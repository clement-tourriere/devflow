//! Whether mise hands a process the variables devflow wrote to
//! `.devflow/env`.
//!
//! Static reading of `mise.toml` keeps missing cases: a later layer
//! (`mise.local.toml`), a duplicate `_.file` entry, a plain `[env]` key, a
//! parent directory or the global configuration can all replace what
//! `.devflow/env` loaded. Two checks ask mise itself and compare with the
//! variables devflow has:
//!
//! - [`check_mise_delivery`] runs `mise env --json` in the worktree: what mise
//!   resolves on its own (what a shell or an editor, or a pitchfork daemon
//!   started with `mise = true`, gets).
//! - [`check_command_env`] runs `mise exec -- sh -c …` in the worktree with
//!   exactly the environment a hook gets (devflow's variables injected,
//!   `MISE_ENV` included) and reads the variables the final process sees: what
//!   a hook that goes through mise gets. A variable that is missing was
//!   removed, one that differs was overridden.
//!
//! Only key names ever leave this module; no value is returned, logged or put
//! in an error, because the variables hold credentials.
use crate::hooks::HookPhase;
use serde::Serialize;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A timeout that fits a check made before running hooks: `mise env` reads
/// configuration and, with `_.source`, runs the scripts it names.
pub const DEFAULT_MISE_TIMEOUT: Duration = Duration::from_secs(20);

/// Most bytes read from each of mise's output streams.
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;
/// How long to wait for the readers after the process is gone.
const READER_GRACE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);

/// What a process started in the worktree gets from mise, compared with the
/// variables in `.devflow/env`. Names only, never values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MiseDelivery {
    /// mise resolves every checked variable to the value in `.devflow/env`.
    Delivered,
    /// mise resolves these variables to something else: a later
    /// `_.file`/`_.source` entry, a plain `[env]` key, `mise.local.toml`, a
    /// parent or global configuration, a task's `env`, an environment-specific
    /// file. (A check that also saw other variables removed lists those here
    /// too.)
    Overridden { keys: Vec<String> },
    /// mise removes these variables (`[env] NAME = false`) although the file
    /// sets them.
    Removed { keys: Vec<String> },
    /// mise resolves none of the checked variables: it does not load
    /// `.devflow/env` at all.
    NotLoaded,
    /// mise refuses the worktree's configuration until it is trusted
    /// (`mise trust`). devflow never trusts anything by itself.
    Untrusted { path: PathBuf },
    /// mise is not installed (or not on `PATH`).
    Unavailable,
    /// The check could not be made. The reason never holds a value.
    Failed { reason: String },
}

impl MiseDelivery {
    /// A short stable name for the state, for output.
    pub fn state(&self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Overridden { .. } => "overridden",
            Self::Removed { .. } => "removed",
            Self::NotLoaded => "not_loaded",
            Self::Untrusted { .. } => "untrusted",
            Self::Unavailable => "unavailable",
            Self::Failed { .. } => "failed",
        }
    }

    /// Everything is as devflow wrote it.
    pub fn is_delivered(&self) -> bool {
        matches!(self, Self::Delivered)
    }

    /// One sentence for a person: what is wrong and where to look. It names
    /// variables and files, never values.
    pub fn describe(&self) -> String {
        match self {
            Self::Delivered => {
                "mise resolves the variables of .devflow/env as devflow wrote them".to_string()
            }
            Self::Overridden { keys } => format!(
                "mise resolves {} from another source than .devflow/env; check `[env]` keys, \
                 later `_.file`/`_.source` entries, mise.local.toml, environment-specific files \
                 and global mise config",
                key_list(keys)
            ),
            Self::Removed { keys } => format!(
                "mise removes {} although .devflow/env sets it (an `[env]` entry set to \
                 `false`?); check mise.toml, mise.local.toml and global mise config",
                key_list(keys)
            ),
            Self::NotLoaded => "mise does not load .devflow/env: none of its variables resolve \
                 (add `_.file = \".devflow/env\"` under `[env]` in mise.toml)"
                .to_string(),
            Self::Untrusted { path } => format!(
                "mise does not trust {}; review it, then run `mise trust {}`",
                path.display(),
                shell_word(&path.display().to_string())
            ),
            Self::Unavailable => "mise is not installed or not on PATH".to_string(),
            Self::Failed { reason } => format!("could not check what mise resolves: {reason}"),
        }
    }
}

/// Up to five names, then a count.
fn key_list(keys: &[String]) -> String {
    const SHOWN: usize = 5;
    let mut text = keys
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if keys.len() > SHOWN {
        text.push_str(&format!(" and {} more", keys.len() - SHOWN));
    }
    text
}

pub(crate) fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%".contains(c))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// How to start mise. The default is `mise` from `PATH`, as the hooks find it;
/// tests point it at an isolated configuration or a stand-in.
#[derive(Debug, Clone)]
pub struct MiseCommand {
    pub program: OsString,
    /// Arguments before `env --json` (a stand-in script run by a shell).
    pub leading_args: Vec<OsString>,
    /// Variables added to the inherited environment.
    pub envs: Vec<(OsString, OsString)>,
    /// Variables removed from it.
    pub env_remove: Vec<OsString>,
}

impl Default for MiseCommand {
    fn default() -> Self {
        Self {
            program: "mise".into(),
            leading_args: Vec::new(),
            envs: Vec::new(),
            env_remove: Vec::new(),
        }
    }
}

/// Ask mise what it resolves in `worktree` and compare it with `.devflow/env`.
///
/// Runs `mise env --json` (`-J`) in the worktree, killing it (and the processes
/// it started) when it takes longer than `timeout`, and compares each of
/// `generated_keys` that `.devflow/env` holds with the value mise resolves.
/// The result names keys only: values are neither returned nor logged.
///
/// Call it before anything relies on mise to deliver the workspace's
/// variables: `setup` does before the first hook, `doctor` on demand, and a
/// caller that starts pitchfork daemons with `mise = true` should do before
/// starting them (those get `.devflow/env` only through mise, so
/// [`MiseDelivery::NotLoaded`] matters to them as much as an override).
///
/// mise is a process-wide fact of the machine (installed or not, which
/// configuration it reads), so the check inherits the caller's environment.
pub fn check_mise_delivery(
    worktree: &Path,
    generated_keys: &[String],
    timeout: Duration,
) -> MiseDelivery {
    check_mise_delivery_with(worktree, generated_keys, timeout, &MiseCommand::default())
}

/// [`check_mise_delivery`] with a chosen way to start mise.
pub fn check_mise_delivery_with(
    worktree: &Path,
    generated_keys: &[String],
    timeout: Duration,
    mise: &MiseCommand,
) -> MiseDelivery {
    let expected: Vec<(String, String)> = match super::read(worktree) {
        Ok(vars) => vars
            .into_iter()
            .filter(|(key, _)| generated_keys.contains(key))
            .collect(),
        Err(_) => {
            return MiseDelivery::Failed {
                reason: "cannot read .devflow/env".into(),
            };
        }
    };
    if expected.is_empty() {
        return MiseDelivery::Failed {
            reason: ".devflow/env holds none of the variables to check".into(),
        };
    }
    let output = match answer(run(worktree, timeout, mise), "mise env", timeout, worktree) {
        Ok(output) => output,
        Err(delivery) => return delivery,
    };
    let resolved: serde_json::Map<String, serde_json::Value> =
        match serde_json::from_slice(&output.stdout) {
            Ok(map) => map,
            Err(_) => {
                return MiseDelivery::Failed {
                    reason: "`mise env --json` did not print a JSON object".into(),
                };
            }
        };
    compare(&expected, &resolved)
}

/// The output of a mise run that succeeded, or why there is no answer. A
/// failure names the file mise refuses and nothing else of its message, since
/// a parse error can quote the line it choked on.
fn answer(
    result: Result<Output, RunError>,
    what: &str,
    timeout: Duration,
    worktree: &Path,
) -> Result<Output, MiseDelivery> {
    let output = match result {
        Ok(output) => output,
        Err(RunError::NotFound) => return Err(MiseDelivery::Unavailable),
        Err(RunError::TimedOut) => {
            return Err(MiseDelivery::Failed {
                reason: format!(
                    "`{what}` did not finish within {}s and was stopped",
                    timeout.as_secs().max(1)
                ),
            });
        }
        Err(RunError::Other(reason)) => return Err(MiseDelivery::Failed { reason }),
    };
    if output.success {
        return Ok(output);
    }
    if let Some(path) = untrusted_path(&output.stderr, worktree) {
        return Err(MiseDelivery::Untrusted { path });
    }
    Err(MiseDelivery::Failed {
        reason: format!(
            "`{what}` exited with {}; run it in the worktree to see why",
            output
                .code
                .map_or("a signal".to_string(), |c| format!("code {c}"))
        ),
    })
}

/// Prints what a process started by mise sees of the variables named in its
/// arguments, as `NAME=value` entries ended by NUL (values may hold line
/// breaks), after a marker that tells them from anything mise itself prints.
/// Plain POSIX `sh`, like the hooks.
const PROBE: &str = concat!(
    "printf 'devflow-probe\\0'\n",
    "for k in \"$@\"; do\n",
    "  eval \"if [ \\\"\\${$k+x}\\\" = x ]; then printf '%s=%s\\\\0' \\\"$k\\\" \\\"\\$$k\\\"; fi\"\n",
    "done\n",
);
const PROBE_MARKER: &[u8] = b"devflow-probe\0";
/// `mise exec` flags the check wants where the installed mise knows them: no
/// preparing of project dependencies (`[deps]` providers with `auto = true`
/// would run an install before the hooks), and no cached environment, which
/// could answer for configuration that has changed since.
const PROBE_FLAGS: [&str; 2] = ["--no-deps", "--fresh-env"];

/// Ask mise what a hook's command would receive, and compare it with the
/// variables devflow gives that hook.
///
/// Runs `mise exec -- sh -c PROBE …` in `worktree` with exactly the
/// environment a hook of `phase` gets (the inherited one without Git's and
/// libpq's variables, `vars` on top, `MISE_ENV` among them if devflow sets it)
/// and reads the value each variable of `vars` has in the process mise starts.
/// A variable that is not there was removed by mise's configuration, one with
/// another value was overridden, whichever way mise did it: `[env]` keys, a
/// later `_.file`, `mise.local.toml`, an environment-specific file, `conf.d`,
/// a parent or the global configuration. The result names keys only.
///
/// This stands for a command run from the worktree root as `mise exec -- …`.
/// It cannot stand for a `cd` ahead of mise, a task (whose own `env` comes
/// after), `-C` or `-E`: see [`super::mise_uses`]. `mise exec -- devflow exec
/// -- …` needs no check, since devflow sets the variables after mise.
///
/// Tools are not installed and project dependencies are not prepared by the
/// check (`MISE_AUTO_INSTALL=0`, `MISE_EXEC_AUTO_INSTALL=0`, `--no-deps` and
/// `--fresh-env` when the installed mise knows them); tools that are missing
/// do not stop it.
///
/// `mise exec` trusts a configuration it has not been told to trust when it
/// runs without a terminal (mise 2026.10 records the directory as trusted and
/// runs its `_.file`, `_.source` and templates), so this check never starts
/// with it: [`check_mise_trust`] goes first, and a configuration that mise
/// refuses ends the check as [`MiseDelivery::Untrusted`], still untrusted.
pub fn check_command_env(
    worktree: &Path,
    vars: &[(String, String)],
    phase: HookPhase,
    timeout: Duration,
    mise: &MiseCommand,
) -> MiseDelivery {
    let trust = check_mise_trust(worktree, vars, phase, timeout, mise);
    if !trust.is_delivered() {
        return trust;
    }
    let expected: std::collections::BTreeMap<&str, &str> = vars
        .iter()
        .filter(|(key, _)| plain_name(key))
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    if expected.is_empty() {
        return MiseDelivery::Delivered;
    }
    let ask = |flags: bool| {
        let mut args: Vec<OsString> = vec!["exec".into()];
        if flags {
            args.extend(PROBE_FLAGS.map(OsString::from));
        }
        args.extend(["--", "sh", "-c", PROBE, "sh"].map(OsString::from));
        args.extend(expected.keys().map(OsString::from));
        run_invocation(
            worktree,
            timeout,
            mise,
            &Invocation {
                args,
                hook_env: Some((phase, vars)),
                forced_env: &FORCED_ENV,
            },
        )
    };
    let mut result = ask(true);
    // A mise from before one of those flags: ask again without them.
    if let Ok(output) = &result
        && !output.success
        && PROBE_FLAGS
            .iter()
            .any(|flag| String::from_utf8_lossy(&output.stderr).contains(flag))
    {
        result = ask(false);
    }
    let output = match answer(result, "mise exec", timeout, worktree) {
        Ok(output) => output,
        Err(delivery) => return delivery,
    };
    let Some(seen) = parse_probe(&output.stdout) else {
        return MiseDelivery::Failed {
            reason: "`mise exec` did not run the check".into(),
        };
    };
    let mut different = Vec::new();
    let mut removed = Vec::new();
    for (key, value) in &expected {
        match seen.get(*key) {
            None => removed.push((*key).to_string()),
            Some(found) if found == value => {}
            Some(_) => different.push((*key).to_string()),
        }
    }
    if different.is_empty() && removed.is_empty() {
        MiseDelivery::Delivered
    } else if different.is_empty() {
        MiseDelivery::Removed {
            keys: sorted_keys(removed),
        }
    } else {
        different.extend(removed);
        MiseDelivery::Overridden {
            keys: sorted_keys(different),
        }
    }
}

/// Variables the checks set whatever the hook gets: no tool is installed.
const FORCED_ENV: [(&str, &str); 2] = [("MISE_AUTO_INSTALL", "0"), ("MISE_EXEC_AUTO_INSTALL", "0")];

/// Whether mise accepts the configuration a command started in `worktree`
/// with the environment of a hook of `phase` would read, without running
/// anything it would not run otherwise.
///
/// Asks `mise env --json`, which refuses a configuration that has not been
/// trusted (`mise trust`) and trusts nothing, unlike `mise exec`, which
/// trusts what it runs when it has no terminal. The answer is
/// [`MiseDelivery::Delivered`] when mise reads the configuration,
/// [`MiseDelivery::Untrusted`] when it refuses it (naming the file),
/// [`MiseDelivery::Unavailable`] without mise and [`MiseDelivery::Failed`]
/// otherwise. devflow never trusts a configuration for the person.
pub fn check_mise_trust(
    worktree: &Path,
    vars: &[(String, String)],
    phase: HookPhase,
    timeout: Duration,
    mise: &MiseCommand,
) -> MiseDelivery {
    let result = run_invocation(
        worktree,
        timeout,
        mise,
        &Invocation {
            args: vec!["env".into(), "--json".into()],
            hook_env: Some((phase, vars)),
            forced_env: &FORCED_ENV,
        },
    );
    match answer(result, "mise env", timeout, worktree) {
        Ok(_) => MiseDelivery::Delivered,
        Err(delivery) => delivery,
    }
}

/// Names go to the probe's shell as arguments; only plain identifiers do.
fn plain_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The `NAME=value` entries the probe printed after its marker.
fn parse_probe(stdout: &[u8]) -> Option<std::collections::BTreeMap<String, String>> {
    let at = stdout
        .windows(PROBE_MARKER.len())
        .position(|window| window == PROBE_MARKER)?;
    let mut seen = std::collections::BTreeMap::new();
    for entry in stdout[at + PROBE_MARKER.len()..].split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(entry);
        let (key, value) = text.split_once('=')?;
        seen.insert(key.to_string(), value.to_string());
    }
    Some(seen)
}

/// The mise configuration files of the worktree root, in the order mise
/// applies them (lowest first): everything the detection layer discovers,
/// which is every location mise reads, `conf.d` and the environment-specific
/// and `.local` files included. Empty when there is none (or the directory
/// cannot be read).
pub fn mise_configs(worktree: &Path) -> Vec<String> {
    crate::detect::mise_layers(worktree).unwrap_or_default()
}

/// Compare what `.devflow/env` holds with what mise resolved.
///
/// A variable mise resolves to another value is overridden. A variable it does
/// not resolve at all is removed when the others resolve (the file is loaded
/// and some of its variables were taken out again); when none does, mise does
/// not load the file.
fn compare(
    expected: &[(String, String)],
    resolved: &serde_json::Map<String, serde_json::Value>,
) -> MiseDelivery {
    let mut different = Vec::new();
    let mut missing = Vec::new();
    let mut delivered = 0;
    for (key, value) in expected {
        match resolved.get(key) {
            None => missing.push(key.clone()),
            Some(found) if found.as_str() == Some(value.as_str()) => delivered += 1,
            Some(_) => different.push(key.clone()),
        }
    }
    if different.is_empty() && missing.is_empty() {
        return MiseDelivery::Delivered;
    }
    if different.is_empty() {
        if delivered == 0 {
            return MiseDelivery::NotLoaded;
        }
        return MiseDelivery::Removed {
            keys: sorted_keys(missing),
        };
    }
    // A variable mise does not resolve at all next to one it resolves wrongly
    // is a file mise loads another way (a `.env` instead of `.devflow/env`),
    // not a second override: only the wrong values are named.
    MiseDelivery::Overridden {
        keys: sorted_keys(different),
    }
}

fn sorted_keys(mut keys: Vec<String>) -> Vec<String> {
    keys.sort();
    keys.dedup();
    keys
}

/// The config file mise says it does not trust: `Config files in PATH are not
/// trusted.` The worktree stands in when the sentence has another shape.
fn untrusted_path(stderr: &[u8], worktree: &Path) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().find(|line| {
        let line = line.to_ascii_lowercase();
        line.contains("not trusted") || line.contains("untrusted")
    })?;
    let path = line
        .split_once("Config files in ")
        .and_then(|(_, rest)| rest.split_once(" are not trusted"))
        .and_then(|(files, _)| files.split(", ").next())
        .map(str::trim)
        .filter(|path| !path.is_empty());
    Some(path.map_or_else(|| worktree.to_path_buf(), PathBuf::from))
}

// -- running mise ------------------------------------------------------------

enum RunError {
    NotFound,
    TimedOut,
    Other(String),
}

struct Output {
    success: bool,
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// What to ask mise, and the environment to ask it in.
struct Invocation<'a> {
    /// The arguments after `leading_args`.
    args: Vec<OsString>,
    /// The environment a hook of this phase gets, on top of `MiseCommand`'s:
    /// the inherited one minus Git's and libpq's, plus these variables.
    hook_env: Option<(HookPhase, &'a [(String, String)])>,
    /// Variables the question needs whatever the hook gets.
    forced_env: &'a [(&'a str, &'a str)],
}

/// `mise env --json` in `worktree`, bounded in time and output.
fn run(worktree: &Path, timeout: Duration, mise: &MiseCommand) -> Result<Output, RunError> {
    run_invocation(
        worktree,
        timeout,
        mise,
        &Invocation {
            args: vec!["env".into(), "--json".into()],
            hook_env: None,
            forced_env: &[],
        },
    )
}

/// Run mise as `invocation` says, in `worktree`, bounded in time and output.
fn run_invocation(
    worktree: &Path,
    timeout: Duration,
    mise: &MiseCommand,
    invocation: &Invocation,
) -> Result<Output, RunError> {
    let mut command = Command::new(&mise.program);
    command
        .args(&mise.leading_args)
        .args(&invocation.args)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1");
    // How mise is isolated first; what a hook gets on top of that, so the
    // variables devflow injects (`MISE_ENV` among them) are not isolated away.
    for name in &mise.env_remove {
        command.env_remove(name);
    }
    for (name, value) in &mise.envs {
        command.env(name, value);
    }
    if let Some((phase, vars)) = invocation.hook_env {
        crate::hooks::apply_env(&mut command, phase, vars);
    }
    for (name, value) in invocation.forced_env {
        command.env(name, value);
    }
    // Its own process group, so a timeout can take the scripts it started too.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => RunError::NotFound,
        std::io::ErrorKind::PermissionDenied => {
            RunError::Other("mise is not executable".to_string())
        }
        _ => RunError::Other("cannot start mise".to_string()),
    })?;
    let readers = (
        child.stdout.take().map(reader),
        child.stderr.take().map(reader),
    );

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => {
                kill(&mut child);
                return Err(RunError::Other("cannot wait for mise".to_string()));
            }
        }
        if Instant::now() >= deadline {
            kill(&mut child);
            return Err(RunError::TimedOut);
        }
        std::thread::sleep(POLL);
    };
    let collect = |receiver: Option<mpsc::Receiver<Vec<u8>>>| {
        receiver
            .and_then(|receiver| receiver.recv_timeout(READER_GRACE).ok())
            .unwrap_or_default()
    };
    Ok(Output {
        success: status.success(),
        code: status.code(),
        stdout: collect(readers.0),
        stderr: collect(readers.1),
    })
}

/// Read a stream on its own thread, keeping the first [`MAX_OUTPUT`] bytes and
/// draining the rest so the writer never blocks on a full pipe.
fn reader<R: Read + Send + 'static>(stream: R) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stream = stream;
        let mut kept = Vec::new();
        let _ = (&mut stream).take(MAX_OUTPUT).read_to_end(&mut kept);
        let _ = std::io::copy(&mut stream, &mut std::io::sink());
        let _ = sender.send(kept);
    });
    receiver
}

/// Stop mise and everything it started.
fn kill(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: plain signal delivery to the process group created for it.
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_names_keys_and_files_and_never_more() {
        let overridden = MiseDelivery::Overridden {
            keys: (1..=7).map(|n| format!("KEY_{n}")).collect(),
        };
        let text = overridden.describe();
        assert!(text.contains("KEY_1") && text.contains("KEY_5"), "{text}");
        assert!(
            !text.contains("KEY_6") && text.contains("and 2 more"),
            "{text}"
        );
        assert!(text.contains("mise.local.toml"), "{text}");
        let untrusted = MiseDelivery::Untrusted {
            path: PathBuf::from("/work/my app/mise.toml"),
        };
        assert!(
            untrusted
                .describe()
                .contains("mise trust '/work/my app/mise.toml'"),
            "{}",
            untrusted.describe()
        );
        assert_eq!(
            serde_json::to_value(&overridden).unwrap()["state"],
            "overridden"
        );
    }

    #[test]
    fn untrusted_files_are_read_from_mises_sentence() {
        let stderr = b"mise ERROR error parsing config file: /w/mise.toml\n\
            mise ERROR Config files in /w/mise.local.toml, /w/mise.toml are not trusted.\n\
            Trust them with `mise trust`.\n";
        assert_eq!(
            untrusted_path(stderr, Path::new("/w")),
            Some(PathBuf::from("/w/mise.local.toml"))
        );
        // Another shape: the worktree stands in.
        assert_eq!(
            untrusted_path(b"the config is not trusted\n", Path::new("/w")),
            Some(PathBuf::from("/w"))
        );
        assert_eq!(untrusted_path(b"boom\n", Path::new("/w")), None);
    }

    #[test]
    fn comparing_never_reports_a_value() {
        let expected = vec![
            ("A".to_string(), "secret-a".to_string()),
            ("B".to_string(), "secret-b".to_string()),
            ("C".to_string(), "secret-c".to_string()),
        ];
        let resolved = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| {
                    (
                        (*k).to_string(),
                        serde_json::Value::String((*v).to_string()),
                    )
                })
                .collect::<serde_json::Map<_, _>>()
        };
        assert_eq!(
            compare(
                &expected,
                &resolved(&[("A", "secret-a"), ("B", "secret-b"), ("C", "secret-c")])
            ),
            MiseDelivery::Delivered
        );
        let overridden = compare(
            &expected,
            &resolved(&[("A", "other-a"), ("B", "secret-b"), ("Z", "x")]),
        );
        assert_eq!(
            overridden,
            MiseDelivery::Overridden {
                keys: vec!["A".into()]
            }
        );
        let shown = format!("{overridden:?} {}", overridden.describe());
        assert!(
            !shown.contains("secret") && !shown.contains("other-a"),
            "{shown}"
        );
        // Nothing resolves: mise does not load the file.
        assert_eq!(
            compare(&expected, &resolved(&[("Z", "x")])),
            MiseDelivery::NotLoaded
        );
        // Everything resolves, differently.
        assert!(matches!(
            compare(
                &expected,
                &resolved(&[("A", "1"), ("B", "2"), ("C", "3")])
            ),
            MiseDelivery::Overridden { keys } if keys == ["A", "B", "C"]
        ));
    }
}

/// The check against a real mise, in directories and with state of their own
/// (`MISE_*` pointed into a temporary directory; the temporary tree is the
/// only trusted path), and against stand-in scripts for the failure shapes.
/// Tests that need mise skip when it is not installed.
#[cfg(all(test, unix))]
mod runtime {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    const DB: &str = "postgres://devflow:db-password-1@localhost:5432/branch_db"; // kingfisher:ignore
    const MAIN_DB: &str = "postgres://main:main-password-2@localhost:5432/main_db"; // kingfisher:ignore
    const KEYS: [&str; 2] = ["DATABASE_URL", "PGHOST"];

    fn keys() -> Vec<String> {
        KEYS.iter().map(|k| (*k).to_string()).collect()
    }

    struct Project {
        _dir: tempfile::TempDir,
        root: PathBuf,
        command: MiseCommand,
    }

    impl Project {
        fn write(&self, name: &str, content: &str) {
            let path = self.root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        fn check(&self) -> MiseDelivery {
            check_mise_delivery_with(&self.root, &keys(), Duration::from_secs(60), &self.command)
        }
    }

    /// A worktree with `.devflow/env` and mise state of its own, or `None`
    /// (test skipped) when mise is not installed.
    fn project() -> Option<Project> {
        if Command::new("mise")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("skipped: mise is not installed");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("worktree");
        std::fs::create_dir_all(&root).unwrap();
        let iso = base.join("mise-state");
        let set = |name: &str, value: &Path| (OsString::from(name), value.as_os_str().to_owned());
        let command = MiseCommand {
            envs: vec![
                set("HOME", &iso.join("home")),
                set("MISE_DATA_DIR", &iso.join("data")),
                set("MISE_CONFIG_DIR", &iso.join("config")),
                set("MISE_CACHE_DIR", &iso.join("cache")),
                set("MISE_STATE_DIR", &iso.join("state")),
                set("MISE_GLOBAL_CONFIG_FILE", &iso.join("config/config.toml")),
                set("MISE_SYSTEM_CONFIG_DIR", &iso.join("system-config")),
                set("MISE_SYSTEM_DATA_DIR", &iso.join("system-data")),
                // Isolated trust: only this temporary tree.
                set("MISE_TRUSTED_CONFIG_PATHS", &base),
            ],
            env_remove: ["MISE_ENV", "MISE_PROFILE", "MISE_YES", "MISE_CEILING_PATHS"]
                .iter()
                .map(OsString::from)
                .collect(),
            ..MiseCommand::default()
        };
        let project = Project {
            _dir: dir,
            root,
            command,
        };
        super::super::write(
            &project.root,
            &[
                ("DATABASE_URL".to_string(), DB.to_string()),
                ("PGHOST".to_string(), "localhost".to_string()),
            ],
        )
        .unwrap();
        project.write(".env", &format!("DATABASE_URL={MAIN_DB}\n"));
        Some(project)
    }

    fn overridden(keys: &[&str]) -> MiseDelivery {
        MiseDelivery::Overridden {
            keys: keys.iter().map(|k| (*k).to_string()).collect(),
        }
    }

    #[test]
    fn mise_that_loads_the_file_last_delivers_it() {
        let Some(p) = project() else { return };
        p.write(
            "mise.toml",
            "[env]\n_.file = [\".env\", \".devflow/env\"]\n",
        );
        assert_eq!(p.check(), MiseDelivery::Delivered);
        // Written the way init proposes it.
        p.write("mise.toml", "[env]\n_.file = \".devflow/env\"\n");
        assert_eq!(p.check(), MiseDelivery::Delivered);
    }

    #[test]
    fn round_2_finding_1_a_duplicate_entry_after_devflow_env_wins() {
        let Some(p) = project() else { return };
        p.write(
            "mise.toml",
            "[env]\n_.file = [\".env\", \".devflow/env\", \".env\"]\n",
        );
        assert_eq!(p.check(), overridden(&["DATABASE_URL"]));
        // The same list without the repeated entry is fine.
        p.write(
            "mise.toml",
            "[env]\n_.file = [\".env\", \".devflow/env\"]\n",
        );
        assert_eq!(p.check(), MiseDelivery::Delivered);
    }

    #[test]
    fn round_2_finding_2_mise_local_over_the_base_wiring_wins() {
        let Some(p) = project() else { return };
        p.write("mise.toml", "[env]\n_.file = \".devflow/env\"\n");
        assert_eq!(p.check(), MiseDelivery::Delivered);
        p.write("mise.local.toml", "[env]\n_.file = \".env\"\n");
        assert_eq!(p.check(), overridden(&["DATABASE_URL"]));
        // The file loaded last decides.
        p.write(
            "mise.local.toml",
            "[env]\n_.file = [\".env\", \".devflow/env\"]\n",
        );
        assert_eq!(p.check(), MiseDelivery::Delivered);
        // `.mise.local.toml` sits above `mise.local.toml`.
        p.write(".mise.local.toml", "[env]\n_.file = \".env\"\n");
        assert_eq!(p.check(), overridden(&["DATABASE_URL"]));
    }

    #[test]
    fn round_2_finding_3_mise_loading_another_file_is_not_the_workspaces_database() {
        // `mise exec -- npm install` runs postinstall scripts under mise's
        // environment. A mise that loads `.env` and not `.devflow/env` hands
        // them the main checkout's database.
        let Some(p) = project() else { return };
        p.write("mise.toml", "[env]\n_.file = \".env\"\n");
        assert_eq!(p.check(), overridden(&["DATABASE_URL"]));
    }

    #[test]
    fn a_plain_env_key_after_the_file_wins_and_one_before_loses() {
        let Some(p) = project() else { return };
        p.write(
            "mise.toml",
            &format!("[env]\n_.file = \".devflow/env\"\nDATABASE_URL = \"{MAIN_DB}\"\n"),
        );
        assert_eq!(p.check(), overridden(&["DATABASE_URL"]));
        p.write(
            "mise.toml",
            &format!("[env]\nDATABASE_URL = \"{MAIN_DB}\"\n_.file = \".devflow/env\"\n"),
        );
        assert_eq!(p.check(), MiseDelivery::Delivered);
    }

    #[test]
    fn mise_that_does_not_load_the_file_at_all() {
        let Some(p) = project() else { return };
        p.write("mise.toml", "[tools]\n\n[env]\nUNRELATED = \"1\"\n");
        assert_eq!(p.check(), MiseDelivery::NotLoaded);
    }

    #[test]
    fn a_configuration_mise_does_not_trust_names_the_file() {
        let Some(mut p) = project() else { return };
        p.write("mise.toml", "[env]\n_.file = \".devflow/env\"\n");
        // Trust points somewhere else: this tree is not trusted.
        let elsewhere = tempfile::tempdir().unwrap();
        for (name, value) in &mut p.command.envs {
            if name == "MISE_TRUSTED_CONFIG_PATHS" {
                *value = elsewhere.path().canonicalize().unwrap().into_os_string();
            }
        }
        match p.check() {
            MiseDelivery::Untrusted { path } => {
                assert!(path.starts_with(&p.root), "{}", path.display());
                assert!(
                    path.file_name()
                        .is_some_and(|n| n.as_bytes().ends_with(b".toml")),
                    "{}",
                    path.display()
                );
                let text = MiseDelivery::Untrusted { path: path.clone() }.describe();
                assert!(text.contains("mise trust"), "{text}");
            }
            other => panic!("expected Untrusted, got {other:?}"),
        }
    }

    #[test]
    fn variables_the_file_does_not_hold_are_not_checked() {
        let Some(p) = project() else { return };
        p.write("mise.toml", "[env]\n_.file = \".devflow/env\"\n");
        let only_unknown = ["NOT_IN_THE_FILE".to_string()];
        assert!(matches!(
            check_mise_delivery_with(&p.root, &only_unknown, Duration::from_secs(60), &p.command),
            MiseDelivery::Failed { .. }
        ));
    }

    /// The static reading of the layers and the real mise agree, pair by
    /// pair: with `.devflow/env` in one file and `.env` in the next one up,
    /// mise resolves the main database exactly when detection says an entry
    /// comes after `.devflow/env`, and the other way round it resolves the
    /// workspace's. This guards the precedence table against the installed
    /// mise (and against a future one that reorders its files).
    #[test]
    fn the_static_layer_order_agrees_with_mise() {
        use crate::detect::toolchain::tests::VERIFIED_ORDER;
        let Some(mut p) = project() else { return };
        // The environment-specific files are read for `dev`.
        p.command
            .envs
            .push((OsString::from("MISE_ENV"), OsString::from("dev")));
        p.command.env_remove.retain(|name| name != "MISE_ENV");
        let wire = |file: &str, entry: &str| {
            p.write(file, &format!("[env]\n_.file = \"{entry}\"\n"));
        };
        let statically_overridden = |p: &Project| {
            let facts = crate::detect::scan(&p.root).unwrap();
            let wiring = &facts.wiring;
            wiring.mise_loads_devflow_env
                && (!wiring.mise_env_overridden_by.is_empty()
                    || !wiring.mise_env_overridden_when_selected.is_empty())
        };
        let mut compared = 0;
        // `.tool-versions` holds no environment; the first two `conf.d` files
        // share a rank (they are ordered by name, which this pairs by hand).
        for pair in VERIFIED_ORDER[1..].windows(2) {
            let (low, high) = (pair[0], pair[1]);
            let _ = std::fs::remove_dir_all(p.root.join(".config"));
            for name in VERIFIED_ORDER.iter().skip(1) {
                let _ = std::fs::remove_file(p.root.join(name));
            }
            // `.devflow/env` below `.env`: `.env` wins.
            wire(low, ".devflow/env");
            wire(high, ".env");
            assert_eq!(
                p.check(),
                overridden(&["DATABASE_URL"]),
                "{high} is applied over {low}"
            );
            // A file for one environment is never counted as loading the file
            // (it is not read unless selected), so only the others compare.
            let for_one_environment = low.contains(".dev.");
            assert_eq!(
                statically_overridden(&p),
                !for_one_environment,
                "{high} over {low}, statically"
            );
            // `.devflow/env` above `.env`: it wins.
            wire(low, ".env");
            wire(high, ".devflow/env");
            assert_eq!(
                p.check(),
                MiseDelivery::Delivered,
                "{low} is applied under {high}"
            );
            assert!(!statically_overridden(&p), "{low} under {high}, statically");
            compared += 1;
        }
        assert_eq!(compared, VERIFIED_ORDER.len() - 2);
    }

    // -- stand-ins (no mise needed) ---------------------------------------------

    /// A command that runs `script` with `sh` in place of mise.
    fn stand_in(dir: &Path, script: &str, envs: &[(&str, &Path)]) -> MiseCommand {
        let path = dir.join("fake-mise.sh");
        std::fs::write(&path, script).unwrap();
        MiseCommand {
            program: "/bin/sh".into(),
            leading_args: vec![path.into_os_string()],
            envs: envs
                .iter()
                .map(|(k, v)| (OsString::from(k), v.as_os_str().to_owned()))
                .collect(),
            env_remove: Vec::new(),
        }
    }

    fn worktree_with_env(dir: &Path) -> PathBuf {
        let root = dir.join("worktree");
        std::fs::create_dir_all(&root).unwrap();
        super::super::write(&root, &[("DATABASE_URL".to_string(), DB.to_string())]).unwrap();
        root
    }

    #[test]
    fn mise_that_is_not_installed_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktree_with_env(dir.path());
        let command = MiseCommand {
            program: dir.path().join("no-such-mise").into_os_string(),
            ..MiseCommand::default()
        };
        assert_eq!(
            check_mise_delivery_with(
                &root,
                &["DATABASE_URL".to_string()],
                Duration::from_secs(5),
                &command
            ),
            MiseDelivery::Unavailable
        );
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only probes for the process.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn a_mise_that_hangs_is_killed_with_what_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktree_with_env(dir.path());
        let parent_pid = dir.path().join("parent.pid");
        let child_pid = dir.path().join("child.pid");
        let command = stand_in(
            dir.path(),
            "echo $$ > \"$PARENT_PID\"\nsleep 60 &\necho $! > \"$CHILD_PID\"\nwait\n",
            &[("PARENT_PID", &parent_pid), ("CHILD_PID", &child_pid)],
        );
        let started = Instant::now();
        let result = check_mise_delivery_with(
            &root,
            &["DATABASE_URL".to_string()],
            Duration::from_millis(400),
            &command,
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        match result {
            MiseDelivery::Failed { reason } => {
                assert!(reason.contains("did not finish"), "{reason}")
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let read = |path: &Path| -> i32 {
            std::fs::read_to_string(path)
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        let (parent, child) = (read(&parent_pid), read(&child_pid));
        // Reaping of the orphaned sleep is asynchronous.
        let deadline = Instant::now() + Duration::from_secs(5);
        while (alive(parent) || alive(child)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(!alive(parent), "the shell was killed");
        assert!(!alive(child), "what it started was killed too");
    }

    #[test]
    fn failures_and_odd_output_never_leak_values() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktree_with_env(dir.path());
        let keys = ["DATABASE_URL".to_string()];
        let check = |script: &str| {
            let command = stand_in(dir.path(), script, &[]);
            check_mise_delivery_with(&root, &keys, Duration::from_secs(10), &command)
        };

        // A failure whose message quotes a secret.
        let result = check("echo 'mise ERROR line: PASSWORD=stderr-secret-3' >&2\nexit 1\n"); // kingfisher:ignore
        match &result {
            MiseDelivery::Failed { reason } => {
                assert!(reason.contains("code 1"), "{reason}");
                assert!(!reason.contains("stderr-secret"), "{reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(!format!("{result:?} {}", result.describe()).contains("stderr-secret"));

        // Output that is not JSON.
        assert!(matches!(
            check("echo 'DATABASE_URL=stdout-secret-4'\n"), // kingfisher:ignore
            MiseDelivery::Failed { reason } if !reason.contains("stdout-secret")
        ));

        // JSON with the wrong value and one that is right: names only.
        let wrong = check(&format!(
            "echo '{{\"DATABASE_URL\": \"{MAIN_DB}\", \"OTHER\": \"x\"}}'\n"
        ));
        assert_eq!(wrong, overridden(&["DATABASE_URL"]));
        assert!(!format!("{wrong:?} {}", wrong.describe()).contains("main-password"));
        let right = check(&format!("echo '{{\"DATABASE_URL\": \"{DB}\"}}'\n"));
        assert_eq!(right, MiseDelivery::Delivered);

        // mise says the configuration is not trusted.
        let untrusted =
            check("echo 'mise ERROR Config files in /w/mise.toml are not trusted.' >&2\nexit 1\n");
        assert_eq!(
            untrusted,
            MiseDelivery::Untrusted {
                path: PathBuf::from("/w/mise.toml")
            }
        );
    }

    #[test]
    fn a_worktree_without_the_file_cannot_be_checked() {
        let dir = tempfile::tempdir().unwrap();
        let command = stand_in(dir.path(), "echo '{}'\n", &[]);
        assert!(matches!(
            check_mise_delivery_with(
                dir.path(),
                &["DATABASE_URL".to_string()],
                Duration::from_secs(5),
                &command
            ),
            MiseDelivery::Failed { .. }
        ));
    }

    // -- the check of what a hook's process receives ---------------------------

    fn probe_vars(vars: &[(&str, &str)]) -> Vec<(String, String)> {
        vars.iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// A stand-in for `mise exec`: drops the arguments up to `--`, runs
    /// `body` (shell, to change the environment as a configuration would) and
    /// runs the rest. Anything else exits 64.
    fn exec_stand_in(dir: &Path, body: &str) -> MiseCommand {
        stand_in(
            dir,
            &format!(
                "case \"$1\" in env) echo '{{}}'; exit 0;; \
                 exec) shift; while [ \"$1\" != -- ]; do shift; done; shift;; \
                 *) exit 64;; esac\n{body}\nexec \"$@\"\n"
            ),
            &[],
        )
    }

    fn probe_at(dir: &Path, vars: &[(String, String)], command: &MiseCommand) -> MiseDelivery {
        check_command_env(
            &dir.join("worktree"),
            vars,
            HookPhase::Setup,
            Duration::from_secs(10),
            command,
        )
    }

    fn overridden_keys(keys: &[&str]) -> MiseDelivery {
        MiseDelivery::Overridden {
            keys: keys.iter().map(|k| (*k).to_string()).collect(),
        }
    }

    #[test]
    fn the_probe_reads_what_the_process_sees_and_reports_keys_only() {
        let dir = tempfile::tempdir().unwrap();
        worktree_with_env(dir.path());
        let vars = probe_vars(&[
            ("DATABASE_URL", DB),
            ("PGHOST", "localhost"),
            ("KEPT", "a value\nwith a line break and 'quotes'"),
        ]);
        let probe = |body: &str| probe_at(dir.path(), &vars, &exec_stand_in(dir.path(), body));
        assert_eq!(probe("true"), MiseDelivery::Delivered);
        // Another value is an override; nothing but the name comes back.
        let overridden = probe(&format!("DATABASE_URL='{MAIN_DB}'; export DATABASE_URL"));
        assert_eq!(overridden, overridden_keys(&["DATABASE_URL"]));
        // A variable that is gone was removed.
        let removed = probe("unset PGHOST");
        assert_eq!(
            removed,
            MiseDelivery::Removed {
                keys: vec!["PGHOST".into()]
            }
        );
        // Both at once are all named as overridden.
        let both = probe(&format!(
            "DATABASE_URL='{MAIN_DB}'; export DATABASE_URL; unset PGHOST"
        ));
        assert_eq!(both, overridden_keys(&["DATABASE_URL", "PGHOST"]));
        // A value with a line break is compared whole.
        assert_eq!(
            probe("KEPT='a value'; export KEPT"),
            overridden_keys(&["KEPT"])
        );
        for shown in [&overridden, &removed, &both] {
            let text = format!("{shown:?} {}", shown.describe());
            assert!(
                !text.contains("main-password") && !text.contains("db-password"),
                "{text}"
            );
        }
        // A key that is not a plain name never reaches the shell.
        let odd = probe_vars(&[("A B", "x"), ("$(touch pwned)", "y"), ("OK", "z")]);
        let delivered = probe_at(dir.path(), &odd, &exec_stand_in(dir.path(), "true"));
        assert_eq!(delivered, MiseDelivery::Delivered);
        assert!(!dir.path().join("worktree/pwned").exists());
        // Nothing to check.
        assert_eq!(
            probe_at(dir.path(), &[], &exec_stand_in(dir.path(), "false")),
            MiseDelivery::Delivered
        );
    }

    #[test]
    fn the_probe_is_not_confused_by_what_mise_prints_and_asks_older_mises_again() {
        let dir = tempfile::tempdir().unwrap();
        worktree_with_env(dir.path());
        let vars = probe_vars(&[("DATABASE_URL", DB)]);
        // Noise on stdout before the marker.
        let noisy = stand_in(
            dir.path(),
            "[ \"$1\" = env ] && { echo '{}'; exit 0; }\necho 'mise WARN something on stdout'\nshift; while [ \"$1\" != -- ]; do shift; done; shift\nexec \"$@\"\n",
            &[],
        );
        assert_eq!(probe_at(dir.path(), &vars, &noisy), MiseDelivery::Delivered);
        // A mise from before `--no-deps` refuses the flag; the probe asks again without it.
        let old = stand_in(
            dir.path(),
            "[ \"$1\" = env ] && { echo '{}'; exit 0; }\nfor a in \"$@\"; do [ \"$a\" = --no-deps ] && { echo \"error: unexpected argument '--no-deps' found\" >&2; exit 2; }; done\nshift; while [ \"$1\" != -- ]; do shift; done; shift\nexec \"$@\"\n",
            &[],
        );
        assert_eq!(probe_at(dir.path(), &vars, &old), MiseDelivery::Delivered);
        // Not a mise that ran the check.
        let silent = stand_in(dir.path(), "exit 0\n", &[]);
        assert!(matches!(
            probe_at(dir.path(), &vars, &silent),
            MiseDelivery::Failed { .. }
        ));
    }

    #[test]
    fn the_probe_fails_and_is_unavailable_like_the_other_check() {
        let dir = tempfile::tempdir().unwrap();
        worktree_with_env(dir.path());
        let vars = probe_vars(&[("DATABASE_URL", DB)]);
        let missing = MiseCommand {
            program: dir.path().join("no-such-mise").into_os_string(),
            ..MiseCommand::default()
        };
        assert_eq!(
            probe_at(dir.path(), &vars, &missing),
            MiseDelivery::Unavailable
        );
        let untrusted = stand_in(
            dir.path(),
            "echo 'mise ERROR Config files in /w/mise.toml are not trusted.' >&2\nexit 1\n",
            &[],
        );
        assert_eq!(
            probe_at(dir.path(), &vars, &untrusted),
            MiseDelivery::Untrusted {
                path: PathBuf::from("/w/mise.toml")
            }
        );
        let broken = stand_in(
            dir.path(),
            "echo 'PASSWORD=stderr-secret-3' >&2\nexit 3\n", // kingfisher:ignore
            &[],
        );
        match probe_at(dir.path(), &vars, &broken) {
            MiseDelivery::Failed { reason } => {
                assert!(reason.contains("code 3"), "{reason}");
                assert!(!reason.contains("stderr-secret"), "{reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let hanging = stand_in(dir.path(), "sleep 30\n", &[]);
        let started = Instant::now();
        let result = check_command_env(
            &dir.path().join("worktree"),
            &vars,
            HookPhase::Setup,
            Duration::from_millis(300),
            &hanging,
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            matches!(result, MiseDelivery::Failed { reason } if reason.contains("did not finish"))
        );
    }

    // -- the reviewers' reproductions, against a real mise ----------------------

    impl Project {
        fn vars(&self) -> Vec<(String, String)> {
            super::super::read(&self.root).unwrap()
        }

        fn probe(&self) -> MiseDelivery {
            self.probe_with(&self.vars())
        }

        fn probe_with(&self, vars: &[(String, String)]) -> MiseDelivery {
            check_command_env(
                &self.root,
                vars,
                HookPhase::Setup,
                Duration::from_secs(60),
                &self.command,
            )
        }

        /// mise run as a hook would run it (`args`), in `dir`, with the
        /// environment of a hook; its standard output.
        fn run_as_hook(&self, dir: &Path, args: &[&str]) -> String {
            let mut command = Command::new(&self.command.program);
            command.args(args).current_dir(dir).stdin(Stdio::null());
            for name in &self.command.env_remove {
                command.env_remove(name);
            }
            for (name, value) in &self.command.envs {
                command.env(name, value);
            }
            crate::hooks::apply_env(&mut command, HookPhase::Setup, &self.vars());
            String::from_utf8(command.output().unwrap().stdout).unwrap()
        }
    }

    #[test]
    fn a_real_mise_that_leaves_the_variables_alone_delivers_them() {
        let Some(p) = project() else { return };
        assert_eq!(p.probe(), MiseDelivery::Delivered, "no configuration");
        p.write("mise.toml", "[env]\n_.file = \".devflow/env\"\n");
        assert_eq!(p.probe(), MiseDelivery::Delivered);
        // Missing tools are not installed (no network, no wait) and do not stop it.
        p.write(
            "mise.toml",
            "[tools]\nnode = \"99.9.9\"\n\n[env]\n_.file = \".devflow/env\"\n",
        );
        assert_eq!(p.probe(), MiseDelivery::Delivered);
    }

    #[test]
    fn review_final_c_a_generated_mise_env_selects_other_configuration() {
        // The probe has to run with the `MISE_ENV` the hook will get: the
        // environment-specific file overrides the database for it alone.
        let Some(p) = project() else { return };
        p.write("mise.toml", "[env]\n_.file = \".devflow/env\"\n");
        p.write(
            "mise.production.toml",
            &format!("[env]\nDATABASE_URL = \"{MAIN_DB}\"\n"),
        );
        assert_eq!(p.probe(), MiseDelivery::Delivered, "MISE_ENV is not set");
        let mut vars = p.vars();
        vars.push(("MISE_ENV".into(), "production".into()));
        assert_eq!(p.probe_with(&vars), overridden_keys(&["DATABASE_URL"]));
        // Another environment leaves the file alone.
        vars.last_mut().unwrap().1 = "staging".into();
        assert_eq!(p.probe_with(&vars), MiseDelivery::Delivered);
    }

    #[test]
    fn review_final_d_configuration_in_places_a_short_list_of_names_misses() {
        let Some(p) = project() else { return };
        for (file, env) in [
            (".config/mise/conf.d/a.toml", None),
            (".config/mise/config.dev.toml", Some("dev")),
            ("mise.dev.local.toml", Some("dev")),
            (".mise/config.local.toml", None),
        ] {
            let _ = std::fs::remove_dir_all(p.root.join(".config"));
            let _ = std::fs::remove_dir_all(p.root.join(".mise"));
            let _ = std::fs::remove_file(p.root.join("mise.dev.local.toml"));
            p.write(file, &format!("[env]\nDATABASE_URL = \"{MAIN_DB}\"\n"));
            // Found by the detection layer's discovery...
            assert_eq!(mise_configs(&p.root), vec![file.to_string()], "{file}");
            // ...and seen by the probe, with the environment the hook gets.
            let mut vars = p.vars();
            if let Some(env) = env {
                vars.push(("MISE_ENV".into(), env.into()));
            }
            assert_eq!(
                p.probe_with(&vars),
                overridden_keys(&["DATABASE_URL"]),
                "{file}"
            );
        }
    }

    #[test]
    fn review_final_e_a_variable_set_to_false_is_removed_even_when_the_file_is_not_loaded() {
        let Some(p) = project() else { return };
        // mise does not load `.devflow/env`, and removes DATABASE_URL.
        p.write("mise.toml", "[env]\nDATABASE_URL = false\n");
        assert_eq!(
            p.probe(),
            MiseDelivery::Removed {
                keys: vec!["DATABASE_URL".into()]
            }
        );
        // `mise env` on its own sees a mise that loads nothing: the old check
        // let this through, and `mise exec` then removed the injected variable.
        assert_eq!(p.check(), MiseDelivery::NotLoaded);
    }

    #[test]
    fn review_final_a_and_b_a_subdirectory_and_a_task_are_what_a_root_probe_misses() {
        // The probe is made at the worktree root. A `cd app` ahead of mise,
        // and a task's own `env`, change what the command receives, which is
        // why such hooks are named in a warning instead (see `mise_uses`).
        let Some(p) = project() else { return };
        p.write("mise.toml", "[env]\n_.file = \".devflow/env\"\n");
        p.write(
            "app/mise.toml",
            &format!("[env]\nDATABASE_URL = \"{MAIN_DB}\"\n"),
        );
        assert_eq!(p.probe(), MiseDelivery::Delivered, "the root is fine");
        let print = ["exec", "--", "sh", "-c", "printf %s \"$DATABASE_URL\""];
        assert_eq!(p.run_as_hook(&p.root, &print), DB);
        assert_eq!(
            p.run_as_hook(&p.root.join("app"), &print),
            MAIN_DB,
            "(a) cd app && mise exec"
        );
        for command in ["cd app && mise exec -- npm ci", "mise run install"] {
            let found = crate::env_file::mise_uses(command);
            assert!(found.iter().all(|u| u.unfaithful.is_some()), "{command}");
        }
        // (b) A task's own `env` is applied after anything set before it.
        p.write(
            "mise.toml",
            &format!(
                "[env]\n_.file = \".devflow/env\"\n\n[tasks.install]\nrun = \"printf %s $DATABASE_URL\"\n\n\
                 [tasks.install.env]\nDATABASE_URL = \"{MAIN_DB}\"\n"
            ),
        );
        let output = p.run_as_hook(&p.root, &["run", "install"]);
        assert!(output.ends_with(MAIN_DB), "(b) mise run: {output}");
    }

    #[test]
    fn every_configuration_file_mise_lists_is_discovered() {
        use crate::detect::toolchain::tests::VERIFIED_ORDER;
        let Some(mut p) = project() else { return };
        p.command
            .envs
            .push((OsString::from("MISE_ENV"), OsString::from("dev")));
        p.command.env_remove.retain(|name| name != "MISE_ENV");
        for file in VERIFIED_ORDER {
            p.write(file, "");
        }
        let found = mise_configs(&p.root);
        assert_eq!(found, VERIFIED_ORDER.to_vec(), "in mise's order");
        // And mise itself lists exactly these.
        let mut command = Command::new(&p.command.program);
        command
            .args(["config", "ls", "--json"])
            .current_dir(&p.root)
            .stdin(Stdio::null());
        for name in &p.command.env_remove {
            command.env_remove(name);
        }
        for (name, value) in &p.command.envs {
            command.env(name, value);
        }
        let output = command.output().unwrap();
        let listed: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        let mut theirs: Vec<String> = listed
            .iter()
            .filter_map(|entry| entry["path"].as_str())
            .filter_map(|path| Path::new(path).strip_prefix(&p.root).ok())
            .map(|path| path.display().to_string())
            .collect();
        theirs.sort();
        let mut ours = found.clone();
        ours.sort();
        assert_eq!(theirs, ours);
        // A worktree without any is empty.
        assert!(mise_configs(tempfile::tempdir().unwrap().path()).is_empty());
    }
}
