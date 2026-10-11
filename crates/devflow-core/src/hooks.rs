//! Running hook commands: `sh -c` per command, one process group each, a
//! timeout that escalates TERM → KILL, and output teed to a log file.
use anyhow::{Context, Result};
use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// How long a command gets between SIGTERM and SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// How long to keep collecting output after the shell exited, in case a
/// background process still holds the pipe.
const DRAIN: Duration = Duration::from_millis(300);
const POLL: Duration = Duration::from_millis(25);
/// Variables Git sets for its own hooks; they would redirect any `git`
/// command a hook runs to the wrong repository.
const GIT_CONTEXT: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_PREFIX",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
];
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    Setup,
    Switch,
    Teardown,
}

impl HookPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            HookPhase::Setup => "setup",
            HookPhase::Switch => "switch",
            HookPhase::Teardown => "teardown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HookRun {
    pub command: String,
    /// `None` when the command was killed by a signal or never started.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
}

impl HookRun {
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HookReport {
    pub phase: HookPhase,
    pub runs: Vec<HookRun>,
    /// Combined output of this run.
    pub log: Option<PathBuf>,
    pub ok: bool,
}

impl HookReport {
    /// One line describing the first failed command, for receipts and errors.
    pub fn failure(&self) -> Option<String> {
        let run = self.runs.iter().find(|r| !r.succeeded())?;
        let how = if run.timed_out {
            format!("timed out after {}s", run.duration_ms / 1000)
        } else {
            match run.exit_code {
                Some(code) => format!("exited with code {code}"),
                None => "could not run or was killed".to_string(),
            }
        };
        let log = self
            .log
            .as_ref()
            .map(|p| format!(" (log: {})", p.display()))
            .unwrap_or_default();
        Some(format!(
            "{} hook `{}` {how}{log}",
            self.phase.as_str(),
            crate::redact::text(&run.command)
        ))
    }
}

/// Run `commands` in order with `sh -c` in `cwd`, with `env` added to the
/// inherited environment and `DEVFLOW_SETUP_ACTIVE=1`. Stops at the first
/// failure. Each command gets its own process group and is killed (TERM, then
/// KILL) when it exceeds `timeout`. Output lines go to `output` and to
/// `<log_dir>/<timestamp>-<phase>.log`; credentials (secret-named variables and
/// URL passwords in `env`) are masked in both, and in the echoed command,
/// before anything is written.
pub fn run(
    phase: HookPhase,
    commands: &[String],
    cwd: &Path,
    env: &[(String, String)],
    timeout: Duration,
    log_dir: &Path,
    output: &mut dyn FnMut(&str),
) -> Result<HookReport> {
    if commands.is_empty() {
        return Ok(HookReport {
            phase,
            runs: Vec::new(),
            log: None,
            ok: true,
        });
    }
    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("cannot create {}", log_dir.display()))?;
    let log_path = log_dir.join(format!(
        "{}-{}.log",
        chrono::Utc::now().format("%Y%m%d-%H%M%S"),
        phase.as_str()
    ));
    let mut log = open_log(&log_path)?;
    let secrets = secrets(env);
    let mut runs = Vec::new();
    for command in commands {
        let run = run_one(
            phase, command, cwd, env, &secrets, timeout, &mut log, output,
        )?;
        let ok = run.succeeded();
        runs.push(run);
        if !ok {
            break;
        }
    }
    let ok = runs.last().is_some_and(HookRun::succeeded) && runs.len() == commands.len();
    Ok(HookReport {
        phase,
        runs,
        log: Some(log_path),
        ok,
    })
}

fn open_log(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Hook output often contains tokens and connection strings.
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))
}

/// The values that must never reach a log or a screen: secret-named
/// variables (`PGPASSWORD`, …) and the passwords inside URL values
/// (`DATABASE_URL`), both as written in the URL and percent-decoded. Every one
/// is masked, however short; [`crate::redact::text_with`] takes care of ones
/// that contain each other.
fn secrets(env: &[(String, String)]) -> Vec<String> {
    let mut found = Vec::new();
    for (key, value) in env {
        if crate::redact::is_secret_key(key) {
            found.push(value.clone());
        }
        if let Some((encoded, decoded)) = url_password(value) {
            found.push(encoded);
            found.push(decoded);
        }
        // A password given as a query parameter (`?password=…`, possibly with
        // an encoded name) is the connection's password all the same.
        if let Some(password) = crate::conn::Params::parse(value)
            .ok()
            .filter(|_| value.contains("://"))
            .and_then(|p| p.password().map(str::to_string))
        {
            found.push(password);
        }
    }
    found.sort();
    found.dedup();
    found.retain(|secret| !secret.is_empty());
    found
}

fn url_password(value: &str) -> Option<(String, String)> {
    let rest = value.split_once("://")?.1;
    // Credentials run to the first `@` before any `/`, as libpq reads them.
    let at = rest
        .find(['@', '/'])
        .filter(|&i| rest.as_bytes()[i] == b'@')?;
    let userinfo = &rest[..at];
    let encoded = userinfo.split_once(':')?.1;
    let decoded = percent_encoding::percent_decode_str(encoded)
        .decode_utf8_lossy()
        .into_owned();
    Some((encoded.to_string(), decoded))
}

fn mask(line: &str, secrets: &[String]) -> String {
    let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
    crate::redact::text_with(line, &secrets)
}

fn emit(log: &mut std::fs::File, output: &mut dyn FnMut(&str), secrets: &[String], line: &str) {
    let line = mask(line, secrets);
    let _ = writeln!(log, "{line}");
    output(&line);
}

/// Hook processes inherit our environment minus Git's hook context; the
/// generated variables are applied on top. A workspace with a database also
/// loses every inherited libpq variable that selects or authenticates a
/// connection ([`crate::conn::libpq_env_vars`]) first: the generated set only
/// names what the URL specifies, and an inherited `PGPORT`, `PGSERVICE` or
/// `PGSSLROOTCERT` would otherwise silently redirect a hook's `psql`.
fn apply_env(cmd: &mut Command, phase: HookPhase, env: &[(String, String)]) {
    for var in GIT_CONTEXT {
        cmd.env_remove(var);
    }
    if env.iter().any(|(key, _)| key == "DATABASE_URL") {
        for var in crate::conn::libpq_env_vars() {
            cmd.env_remove(var);
        }
    }
    cmd.envs(env.iter().map(|(k, v)| (k, v)))
        .env("DEVFLOW_SETUP_ACTIVE", "1")
        .env("DEVFLOW_PHASE", phase.as_str());
}

#[allow(clippy::too_many_arguments)]
fn run_one(
    phase: HookPhase,
    command: &str,
    cwd: &Path,
    env: &[(String, String)],
    secrets: &[String],
    timeout: Duration,
    log: &mut std::fs::File,
    output: &mut dyn FnMut(&str),
) -> Result<HookRun> {
    let started = Instant::now();
    emit(log, output, secrets, &format!("$ {command}"));
    let failed = |duration: Duration| HookRun {
        command: command.to_string(),
        exit_code: None,
        timed_out: false,
        duration_ms: duration.as_millis() as u64,
    };

    // One pipe for both streams keeps their relative order.
    let (reader, writer) = std::io::pipe()?;
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    apply_env(&mut cmd, phase, env);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            emit(log, output, secrets, &format!("cannot start `sh`: {e}"));
            return Ok(failed(started.elapsed()));
        }
    };
    // Our copies of the write end must close or the reader never sees EOF.
    drop(cmd);

    // The reader owns its own log handle and keeps draining until EOF, even
    // after we stop listening: a background process the hook started must not
    // get SIGPIPE on its next write.
    let (tx, rx) = mpsc::channel::<String>();
    let mut thread_log = log.try_clone()?;
    let thread_secrets = secrets.to_vec();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = mask(
                        String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']),
                        &thread_secrets,
                    );
                    let _ = writeln!(thread_log, "{line}");
                    let _ = tx.send(line);
                }
            }
        }
    });

    let deadline = started + timeout;
    let mut timed_out = false;
    let mut eof = false;
    let status = loop {
        if eof {
            std::thread::sleep(POLL);
        } else {
            match rx.recv_timeout(POLL) {
                Ok(line) => output(&line),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => eof = true,
            }
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break terminate(&mut child)?;
        }
    };
    // Whatever the shell wrote just before exiting.
    let drain_until = Instant::now() + DRAIN;
    while let Some(left) = drain_until.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(line) => output(&line),
            Err(_) => break,
        }
    }
    let run = HookRun {
        command: command.to_string(),
        exit_code: status.code(),
        timed_out,
        duration_ms: started.elapsed().as_millis() as u64,
    };
    if timed_out {
        emit(
            log,
            output,
            secrets,
            &format!("timed out after {}s; killed", timeout.as_secs()),
        );
    } else if let Some(code) = status.code().filter(|c| *c != 0) {
        emit(log, output, secrets, &format!("exit code {code}"));
    } else if status.code().is_none() {
        emit(log, output, secrets, "terminated by a signal");
    }
    Ok(run)
}

/// SIGTERM the command's process group, give it [`KILL_GRACE`] to leave, then
/// SIGKILL whatever remains. Returns the shell's exit status.
#[cfg(unix)]
fn terminate(child: &mut std::process::Child) -> Result<std::process::ExitStatus> {
    let group = -(child.id() as i32);
    // SAFETY: plain signal delivery to a process group this function created.
    let signal = |sig: i32| unsafe { libc::kill(group, sig) };
    let alive = || {
        let rc = unsafe { libc::kill(group, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };
    signal(libc::SIGTERM);
    let mut status = None;
    let grace_end = Instant::now() + KILL_GRACE;
    while Instant::now() < grace_end {
        if status.is_none() {
            status = child.try_wait()?;
        }
        if status.is_some() && !alive() {
            break;
        }
        std::thread::sleep(POLL);
    }
    if status.is_none() || alive() {
        signal(libc::SIGKILL);
    }
    match status {
        Some(status) => Ok(status),
        None => Ok(child.wait()?),
    }
}

#[cfg(not(unix))]
fn terminate(child: &mut std::process::Child) -> Result<std::process::ExitStatus> {
    let _ = child.kill();
    Ok(child.wait()?)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct Run {
        report: HookReport,
        lines: Vec<String>,
    }

    fn run_in(dir: &Path, commands: &[&str], env: &[(&str, &str)], timeout: Duration) -> Run {
        let commands: Vec<String> = commands.iter().map(|s| s.to_string()).collect();
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut lines = Vec::new();
        let report = run(
            HookPhase::Setup,
            &commands,
            dir,
            &env,
            timeout,
            &dir.join("logs"),
            &mut |l| lines.push(l.to_string()),
        )
        .unwrap();
        Run { report, lines }
    }

    fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn wait_dead(pid: i32) -> bool {
        for _ in 0..80 {
            if !pid_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    #[test]
    fn runs_in_order_in_cwd_and_logs_merged_output() {
        let dir = tempfile::tempdir().unwrap();
        let r = run_in(
            dir.path(),
            &["echo one", "echo two >&2", "pwd"],
            &[],
            Duration::from_secs(30),
        );
        assert!(r.report.ok);
        assert_eq!(r.report.runs.len(), 3);
        assert!(r.report.runs.iter().all(HookRun::succeeded));
        assert_eq!(&r.lines[..2], ["$ echo one", "one"]);
        assert!(r.lines.contains(&"two".to_string()));
        let pwd = r.lines.last().unwrap();
        assert_eq!(
            Path::new(pwd).canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );

        let log = r.report.log.unwrap();
        assert!(log.starts_with(dir.path().join("logs")));
        let name = log.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.ends_with("-setup.log"), "{name}");
        let text = std::fs::read_to_string(&log).unwrap();
        for line in &r.lines {
            assert!(text.contains(line.as_str()), "{line} missing from log");
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(log.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn a_failing_command_stops_the_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let r = run_in(
            dir.path(),
            &["true", "echo boom; exit 3", "touch never"],
            &[],
            Duration::from_secs(30),
        );
        assert!(!r.report.ok);
        assert_eq!(r.report.runs.len(), 2);
        assert_eq!(r.report.runs[1].exit_code, Some(3));
        assert!(!r.report.runs[1].timed_out);
        assert!(!dir.path().join("never").exists());
        let failure = r.report.failure().unwrap();
        assert!(
            failure.contains("exit 3") && failure.contains("code 3"),
            "{failure}"
        );
        assert!(r.lines.contains(&"boom".to_string()));
    }

    #[test]
    fn environment_is_added_and_git_context_removed() {
        let dir = tempfile::tempdir().unwrap();
        let r = run_in(
            dir.path(),
            &["echo \"$FOO|$DEVFLOW_SETUP_ACTIVE|$DEVFLOW_PHASE|${GIT_DIR-unset}\""],
            &[("FOO", "bar baz")],
            Duration::from_secs(30),
        );
        assert!(r.report.ok);
        assert_eq!(r.lines[1], "bar baz|1|setup|unset");
    }

    #[test]
    fn timeout_kills_the_whole_process_group_quickly() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let command = format!("sleep 600 & echo $! > {}; wait", pidfile.display());
        let started = Instant::now();
        let r = run_in(
            dir.path(),
            &[&command, "touch never"],
            &[],
            Duration::from_millis(400),
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        assert!(!r.report.ok);
        assert_eq!(r.report.runs.len(), 1, "later commands do not run");
        assert!(r.report.runs[0].timed_out);
        assert!(r.report.failure().unwrap().contains("timed out"));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(wait_dead(pid), "the sleep child must be gone");
    }

    #[test]
    fn timeout_escalates_to_kill_when_term_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let command = format!(
            "trap '' TERM; sleep 600 & echo $! > {}; wait",
            pidfile.display()
        );
        let started = Instant::now();
        let r = run_in(dir.path(), &[&command], &[], Duration::from_millis(300));
        let elapsed = started.elapsed();
        assert!(
            elapsed >= KILL_GRACE && elapsed < KILL_GRACE + Duration::from_secs(4),
            "{elapsed:?}"
        );
        assert!(r.report.runs[0].timed_out);
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            wait_dead(pid),
            "SIGKILL must remove the TERM-ignoring child"
        );
    }

    #[test]
    fn background_processes_do_not_block_completion() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let r = run_in(
            dir.path(),
            &["sleep 5 & echo started"],
            &[],
            Duration::from_secs(30),
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(r.report.ok);
        assert!(r.lines.contains(&"started".to_string()));
    }

    #[test]
    fn empty_command_lists_and_missing_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let r = run_in(dir.path(), &[], &[], Duration::from_secs(1));
        assert!(r.report.ok && r.report.runs.is_empty() && r.report.log.is_none());
        assert!(!dir.path().join("logs").exists());

        let mut lines = Vec::new();
        let report = run(
            HookPhase::Teardown,
            &["true".to_string()],
            &dir.path().join("missing"),
            &[],
            Duration::from_secs(1),
            &dir.path().join("logs"),
            &mut |l| lines.push(l.to_string()),
        )
        .unwrap();
        assert!(!report.ok);
        assert_eq!(report.runs[0].exit_code, None);
        assert!(
            lines.iter().any(|l| l.contains("cannot start")),
            "{lines:?}"
        );
    }

    #[test]
    fn credentials_are_masked_in_the_log_the_callback_and_the_command_header() {
        let dir = tempfile::tempdir().unwrap();
        let env = [
            ("PGPASSWORD", "s3cretpw1"),
            ("DATABASE_URL", "postgres://u:p%40ss9@h:1/d"), // kingfisher:ignore
            ("PGUSER", "u"),
        ];
        let commands = [
            "echo \"pw=$PGPASSWORD\"",
            "echo \"$DATABASE_URL\"",
            "echo decoded p@ss9 >&2",
            "echo PGPASSWORD=literal1234 psql",
            "echo \"user=$PGUSER\"",
        ];
        let r = run_in(dir.path(), &commands, &env, Duration::from_secs(30));
        assert!(r.report.ok);
        let log = std::fs::read_to_string(r.report.log.as_ref().unwrap()).unwrap();
        for secret in ["s3cretpw1", "p%40ss9", "p@ss9", "literal1234"] {
            assert!(
                !log.contains(secret),
                "{secret} leaked into the log:\n{log}"
            );
            assert!(
                r.lines.iter().all(|l| !l.contains(secret)),
                "{secret} leaked into the output: {:?}",
                r.lines
            );
        }
        assert!(r.lines.contains(&"pw=***".to_string()), "{:?}", r.lines);
        assert!(
            r.lines.contains(&"postgres://u:***@h:1/d".to_string()),
            "{:?}",
            r.lines
        );
        assert!(
            r.lines.contains(&"$ echo PGPASSWORD=***".to_string()),
            "the echoed command is masked: {:?}",
            r.lines
        );
        assert!(
            r.lines.contains(&"user=u".to_string()),
            "short plain values stay readable"
        );
        // Log and callback agree line for line.
        for line in &r.lines {
            assert!(log.contains(line.as_str()), "{line}");
        }
    }

    #[test]
    fn a_workspace_with_a_database_clears_inherited_libpq_variables() {
        let env_of = |env: &[(&str, &str)]| {
            let env: Vec<(String, String)> = env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let mut cmd = Command::new("sh");
            apply_env(&mut cmd, HookPhase::Setup, &env);
            cmd.get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect::<std::collections::HashMap<_, _>>()
        };
        let with_db = env_of(&[
            ("DATABASE_URL", "postgres://u:p@h:5434/d"),
            ("PGHOST", "h"),
            ("PGPORT", "5434"),
        ]);
        assert_eq!(with_db["PGPORT"].as_deref(), Some("5434"), "generated wins");
        for inherited in
            crate::conn::libpq_env_vars().filter(|v| !matches!(*v, "PGHOST" | "PGPORT"))
        {
            assert_eq!(with_db[inherited], None, "{inherited} must be cleared");
        }
        assert_eq!(with_db["GIT_DIR"], None);
        // Without a database there is nothing to disagree with: the user's
        // environment is left alone.
        let without_db = env_of(&[("DEVFLOW_WORKSPACE", "w")]);
        assert!(!without_db.contains_key("PGPORT"));
        assert!(without_db.contains_key("GIT_DIR"));
    }

    #[test]
    fn short_and_nested_secrets_are_all_masked() {
        let dir = tempfile::tempdir().unwrap();
        // `abcd` is a prefix of the URL password `abcdef`; `pw` is shorter than
        // the old four-byte floor; the query password has an encoded name.
        let env = [
            ("PGPASSWORD", "pw"),
            ("API_SECRET", "abcd"),
            ("DATABASE_URL", "postgres://u:abcdef@h:1/d"), // kingfisher:ignore
            ("OTHER_URL", "postgres://u@h/d?%70assword=qry9"),
        ];
        let commands = [
            "echo abcdef abcd pw qry9",
            "echo \"x${PGPASSWORD}y $DATABASE_URL\"",
        ];
        let r = run_in(dir.path(), &commands, &env, Duration::from_secs(30));
        assert!(r.report.ok);
        let log = std::fs::read_to_string(r.report.log.as_ref().unwrap()).unwrap();
        for secret in ["abcdef", "abcd", "ef", "qry9"] {
            assert!(
                !log.contains(secret) && r.lines.iter().all(|l| !l.contains(secret)),
                "{secret} leaked:\n{log}"
            );
        }
        assert!(
            r.lines.contains(&"*** *** *** ***".to_string()),
            "{:?}",
            r.lines
        );
        assert!(
            r.lines
                .contains(&"x***y postgres://u:***@h:1/d".to_string()),
            "{:?}",
            r.lines
        );
    }

    #[test]
    fn the_secret_list_has_no_empty_entries_and_includes_query_passwords() {
        let env = vec![
            ("PGPASSWORD".to_string(), String::new()),
            (
                "DATABASE_URL".to_string(),
                "postgres://u:p%40ss@h/d?sslmode=disable".to_string(), // kingfisher:ignore
            ),
            (
                "ALT".to_string(),
                "postgresql://u@h/d?%70assword=viaquery".to_string(),
            ),
            ("PLAIN".to_string(), "not a url password=x".to_string()),
        ];
        let found = secrets(&env);
        assert!(found.iter().all(|s| !s.is_empty()), "{found:?}");
        for expected in ["p%40ss", "p@ss", "viaquery"] {
            assert!(
                found.iter().any(|s| s == expected),
                "{expected} in {found:?}"
            );
        }
    }
}
