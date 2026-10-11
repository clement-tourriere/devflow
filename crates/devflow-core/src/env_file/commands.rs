//! What a hook's shell command runs: where it reaches mise, and whether
//! `devflow exec` has the last word.
//!
//! mise applies its `[env]` over the variables a command inherits, so a command
//! that goes through mise only gets the workspace's variables when devflow
//! sets them *after* mise has run: `mise exec -- devflow exec -- CMD`.
//! This module reads a shell command string (best effort, not a shell parser)
//! and reports each use of mise, whether `devflow exec` runs inside it, and
//! when a check made in the worktree cannot stand for the use.
use serde::Serialize;

/// Why a check made at the worktree root cannot stand for a use of mise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unfaithful {
    /// `cd DIR` ahead of mise: mise reads that directory's configuration.
    ChangesDirectory,
    /// `mise -C DIR`, `--cd`, `-E`/`--env` or `--profile` choose the
    /// directory or the environment mise reads.
    Selects,
    /// A `MISE_*` variable assigned in the command itself.
    Assigns,
    /// A mise task: its own `env` is applied after anything set before it.
    Task,
}

impl Unfaithful {
    /// A phrase for "this hook ...".
    pub fn describe(self) -> &'static str {
        match self {
            Unfaithful::ChangesDirectory => {
                "changes directory before mise, which reads that directory's configuration"
            }
            Unfaithful::Selects => {
                "tells mise which directory or environment to read (-C, -E, --profile)"
            }
            Unfaithful::Assigns => "sets a MISE_ variable for mise in the command itself",
            Unfaithful::Task => "runs a mise task, whose own `env` is applied after devflow's",
        }
    }
}

/// One place a shell command string runs `mise`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiseUse {
    /// `exec`, `x`, `run`, `install`, ... (empty for a bare `mise`).
    pub subcommand: String,
    /// `devflow exec` runs inside this use (`mise exec -- devflow exec -- CMD`),
    /// so what mise sets cannot win.
    pub through_devflow_exec: bool,
    /// The subcommand starts repository code (`exec`, `run`, `watch`, a task)
    /// rather than managing tools (`install`, `trust`, ...).
    pub runs_code: bool,
    /// Set when a probe in the worktree cannot stand for this use.
    pub unfaithful: Option<Unfaithful>,
}

/// Subcommands that manage mise or its tools and start no repository command.
const NON_RUNNING: [&str; 41] = [
    "activate",
    "bin-paths",
    "cache",
    "completion",
    "config",
    "cfg",
    "deactivate",
    "doctor",
    "dr",
    "edit",
    "env",
    "e",
    "fmt",
    "generate",
    "g",
    "implode",
    "install",
    "i",
    "latest",
    "link",
    "ln",
    "lock",
    "ls",
    "list",
    "ls-remote",
    "outdated",
    "plugins",
    "p",
    "prune",
    "registry",
    "reshim",
    "set",
    "settings",
    "trust",
    "uninstall",
    "unset",
    "upgrade",
    "up",
    "use",
    "u",
    "which",
];

/// Every use of mise in `command`, in order.
///
/// Best effort, not a shell parser: the string is split at unquoted `;`, `&`,
/// `|`, `(`, `)`, `{`, `}`, backticks and newlines, leading `NAME=value`
/// assignments and the wrappers `sudo`, `env`, `time`, `command`, `exec`,
/// `nohup`, `nice` and the shell keywords `if`, `then`, `do`, `else`, `elif`,
/// `while`, `until` and `!` are skipped, and a segment counts when its
/// command word is `mise` (or a path ending in `/mise`). A wrapper that takes
/// options of its own (`env -u X`, `sudo -E`, `timeout 5`, `nice -n 5`,
/// `xargs -n1`) does not hide what it runs: the words after it are tried one by
/// one for a command the scanner knows. The argument of `sh -c`, `bash -c` and
/// the like, and of `eval`, is examined the same way, and so is what comes
/// after `devflow exec --`.
///
/// What runs after `devflow exec --` is held to a stricter rule, because
/// nothing there is allowed to start mise: when the scanner finds no use in it
/// but `mise` appears in it as a word anywhere (in a quoted string, behind a
/// wrapper it cannot read, in a path such as `$HOME/.local/bin/mise`), that
/// counts as an unprotected use. So does a string with a protected use
/// (`mise exec -- devflow exec -- CMD`) and one more occurrence of `mise` as a
/// word than the scanner classified: a protected use never vouches for the
/// rest of the string. A command built at run time (`$TOOL exec`)
/// or mise started from a script file is not seen: a hook that hides mise like
/// that is not detected (but `devflow exec -- $TOOL …` is not wrapped, see
/// [`command_is_wholly_devflow_exec`]).
pub fn mise_uses(command: &str) -> Vec<MiseUse> {
    let mut state = State::default();
    scan(command, 0, &mut state);
    // A protected use must not vouch for the rest of the string. Every `mise`
    // the scanner classified accounts for one occurrence of the word; one more
    // than that is a use it did not see (`echo "mise exec -- bun run migrate"
    // | sh`, a path, an argument) and counts as an unprotected one.
    if state.uses.iter().any(|found| found.through_devflow_exec)
        && mise_occurrences(command) > state.uses.len()
    {
        state.uses.push(MiseUse {
            subcommand: String::new(),
            through_devflow_exec: false,
            runs_code: true,
            unfaithful: state.cd_seen.then_some(Unfaithful::ChangesDirectory),
        });
    }
    state.uses
}

/// The `mise` subcommands a shell command string runs, in order (`exec`,
/// `run`, `install`, ... or an empty string for a bare `mise`).
pub fn mise_subcommands(command: &str) -> Vec<String> {
    mise_uses(command)
        .into_iter()
        .map(|mise_use| mise_use.subcommand)
        .collect()
}

/// Whether a shell command string runs `mise` (see [`mise_uses`]).
pub fn command_uses_mise(command: &str) -> bool {
    !mise_uses(command).is_empty()
}

/// Whether the whole string is one simple command, `devflow exec [OPTIONS] --
/// PROGRAM ARGS…` (optionally behind `NAME=value` assignments and `exec`), and
/// nothing PROGRAM runs reaches mise.
///
/// This is the test for "`devflow exec` has the last word" about a command
/// nobody else wraps, such as a pitchfork daemon's `run`: mentioning `devflow
/// exec` is not enough. The string must be a single command, with no `;`, `&&`,
/// `||`, `|`, `&`, newline, backtick, `$(` or parenthesis outside quotes
/// (redirections such as `> log 2>&1` are fine), because anything beside it
/// runs outside the wrapper: `devflow exec -- true; npm run dev` starts the app
/// without the workspace's variables. And PROGRAM must not be mise, nor reach
/// it (`env mise …`, `sudo -E mise …`, `sh -c 'mise …'`, `xargs mise …`, found
/// as a command word by [`mise_uses`] or as a word named `mise`), because mise
/// applies its `[env]` over what devflow set:
/// `devflow exec -- mise exec -- npm run dev` runs npm with mise's values.
///
/// Conservative: any occurrence of `mise` as a word in what PROGRAM is given
/// counts, whether it is a command word, an argument, a word inside a quoted
/// string for `sh -c`, `bash -c`, `zsh -c` or `eval` (`sh -c 'env -u X mise exec
/// -- bun run dev'`), behind a wrapper with options (`env -u X mise …`,
/// `sudo -E mise …`, `timeout 5 mise …`) or a path ending in `/mise`. A command
/// word that is a shell expansion (`$TOOL run dev`) is not followed either.
/// Anything the scanner does not understand answers `false`, which only ever
/// adds a check. Mise started from a script file, or from the program's own
/// code, is not seen.
pub fn command_is_wholly_devflow_exec(command: &str) -> bool {
    let Some(words) = simple_command(command) else {
        return false;
    };
    let mut rest = words.as_slice();
    while let Some(first) = rest.first() {
        if !is_assignment(first) {
            break;
        }
        rest = &rest[1..];
    }
    if rest.first().map(String::as_str) == Some("exec") {
        rest = &rest[1..];
    }
    let Some((program, args)) = rest.split_first() else {
        return false;
    };
    if base_name(program) != "devflow" {
        return false;
    }
    let Some(inner) = strict_devflow_exec_command(args) else {
        return false;
    };
    exec_has_last_word(inner)
}

/// For the words after `devflow`: the command after `exec [OPTIONS] --`, when
/// the subcommand is `exec`, what stands between `exec` and `--` is an option
/// (or its value) and the `--` is there. `None` for anything else, which
/// `devflow` would not run as a command.
fn strict_devflow_exec_command(args: &[String]) -> Option<&[String]> {
    let mut i = 0;
    while let Some(word) = args.get(i) {
        if word == "-C" {
            i += 2;
        } else if word.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    if args.get(i).map(String::as_str) != Some("exec") {
        return None;
    }
    let after = &args[i + 1..];
    let separator = after.iter().position(|w| w == "--")?;
    let mut k = 0;
    while k < separator {
        let word = &after[k];
        if word == "--workspace" || word == "-C" {
            k += 2;
        } else if word.starts_with('-') {
            k += 1;
        } else {
            return None;
        }
    }
    (k == separator).then(|| &after[separator + 1..])
}

/// Whether running `words` can reach mise: a use [`mise_uses`] would find
/// (through wrappers, `sh -c`, `eval`, `devflow exec`), a command word the
/// scanner cannot follow (`$TOOL run`), or any word that mentions the program
/// ([`mentions_mise`]).
fn reaches_mise(words: &[String]) -> bool {
    let mut state = State::default();
    scan_words(words, 0, &mut state);
    !state.uses.is_empty() || state.opaque || words.iter().any(|word| mentions_mise(word))
}

/// Whether `text` has `mise` in it as a word, however it got there: a whole
/// word, the last component of a path, a word inside a quoted string, nested
/// strings inside it, one spelled with quotes inside it (`m"i"se`).
///
/// A word here is a run of letters, digits and `_ - . /`; anything else
/// (whitespace, quotes, `$ ( ) ; & | < > = , : { } ~`) ends it. The text is
/// split as it stands and again with its quote and backslash characters taken
/// out, which reads a string for a shell the way the shell would after
/// removing quotes and never needs to parse it. So `mise` counts as a word in
/// `env -u X mise exec`, `"$HOME/.local/bin/mise" run`, `FOO=mise` and
/// `--mise`; `mise.toml`, `mise-tools`, `promise` and `MISE_ENV` do not.
fn mentions_mise(text: &str) -> bool {
    mise_occurrences(text) > 0
}

/// How many times `text` has `mise` in it as a word (see [`mentions_mise`] for
/// what a word is): the larger of the count in the text as it stands and in the
/// text with its quote and backslash characters taken out, so a spelling with
/// quotes inside is found and nothing is counted twice.
fn mise_occurrences(text: &str) -> usize {
    fn words(text: &str) -> impl Iterator<Item = &str> {
        text.split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')))
            .filter(|word| !word.is_empty())
    }
    fn count(text: &str) -> usize {
        words(text)
            .filter(|word| {
                let word = word.trim_start_matches('-');
                word == "mise" || word.ends_with("/mise")
            })
            .count()
    }
    let unquoted: String = text
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .collect();
    count(text).max(count(&unquoted))
}

/// The words of `command` when it is one simple command, `None` when anything
/// else could run beside it: an unquoted `;`, `|`, `&` (except in a `>&` or
/// `<&` redirection), parenthesis, backtick, newline or comment, a command
/// substitution (`$(` or a backtick) anywhere but in single quotes, or a quote
/// that is never closed. Quotes and escapes are resolved and a
/// backslash-newline joins lines. Redirections stay among the words.
fn simple_command(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut previous = '\0';
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => current.push(other),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '`' => return None,
                        '$' if chars.peek() == Some(&'(') => return None,
                        '\\' => match chars.next()? {
                            '\n' => {}
                            escaped @ ('$' | '`' | '"' | '\\') => current.push(escaped),
                            other => {
                                current.push('\\');
                                current.push(other);
                            }
                        },
                        other => current.push(other),
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') => {}
                Some(escaped) => {
                    current.push(escaped);
                    in_word = true;
                }
                None => {
                    current.push('\\');
                    in_word = true;
                }
            },
            ';' | '|' | '(' | ')' | '`' | '\n' | '\r' => return None,
            '&' if !matches!(previous, '>' | '<') => return None,
            '$' if chars.peek() == Some(&'(') => return None,
            '#' if !in_word => return None,
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            other => {
                current.push(other);
                in_word = true;
            }
        }
        previous = c;
    }
    if in_word {
        words.push(current);
    }
    Some(words)
}

/// What one setup hook does with mise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookMise {
    /// How to name the hook in a message, never its command: `the setup hook
    /// 'install'` or `setup hook #2`.
    pub label: String,
    /// The command runs mise at all (so it needs mise, and mise reads the
    /// worktree's configuration), `devflow exec` inside it or not.
    pub uses_mise: bool,
    /// Some use of mise is not followed by `devflow exec` (so mise's
    /// environment can win), whatever the subcommand.
    pub unprotected: bool,
    /// An unprotected use starts repository code (`exec`, `run`, ...): the
    /// hook should go through `devflow exec`.
    pub unrouted: bool,
    /// An unprotected use is one a check at the worktree root stands for:
    /// `mise exec -- CMD` from the root.
    pub checked: bool,
    /// Why a check at the worktree root cannot stand for an unprotected use
    /// that starts repository code, when that is so.
    pub unchecked: Option<Unfaithful>,
}

/// Classify the setup hooks `commands` by how they use mise. Hooks that do
/// not run mise come out with every flag clear.
pub fn classify_hooks(commands: &[crate::hooks::HookCommand]) -> Vec<HookMise> {
    commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let label = match &command.name {
                Some(name) => format!("the setup hook '{name}'"),
                None => format!("setup hook #{}", index + 1),
            };
            let mut hook = HookMise {
                label,
                uses_mise: false,
                unprotected: false,
                unrouted: false,
                checked: false,
                unchecked: None,
            };
            for found in mise_uses(&command.run) {
                hook.uses_mise = true;
                if found.through_devflow_exec {
                    continue;
                }
                hook.unprotected = true;
                hook.unrouted |= found.runs_code;
                match found.unfaithful {
                    None => hook.checked = true,
                    Some(reason) if found.runs_code => {
                        hook.unchecked.get_or_insert(reason);
                    }
                    Some(_) => {}
                }
            }
            hook
        })
        .collect()
}

#[derive(Default)]
struct State {
    uses: Vec<MiseUse>,
    /// A `cd` came before this point.
    cd_seen: bool,
    /// A command word was a shell expansion (`$TOOL run`): what runs is
    /// decided at run time, which the scanner cannot follow.
    opaque: bool,
    /// Trying the words after a wrapper one by one for a command: a `$` word
    /// there is as likely an argument as a command.
    guessing: bool,
}

const WRAPPERS: [&str; 17] = [
    "sudo", "env", "time", "command", "exec", "nohup", "nice", "then", "do", "else", "elif", "if",
    "while", "until", "!", "builtin", "doas",
];
/// Commands that run the command they are given after options and values of
/// their own (`timeout -k 5 10 CMD`), which the scanner does not know.
const OPTION_WRAPPERS: [&str; 10] = [
    "timeout",
    "gtimeout",
    "xargs",
    "stdbuf",
    "setsid",
    "ionice",
    "flock",
    "caffeinate",
    "watch",
    "parallel",
];
const SHELLS: [&str; 6] = ["sh", "bash", "zsh", "dash", "ksh", "ash"];
/// Builtins that run nothing: they may share a string with `devflow exec`.
const NEUTRAL: [&str; 8] = ["cd", "pushd", "popd", "export", "set", "unset", ":", "true"];
const MAX_DEPTH: usize = 3;

fn scan(command: &str, depth: usize, state: &mut State) {
    if depth > MAX_DEPTH {
        return;
    }
    for segment in split_segments(command) {
        let words = split_words(&segment);
        scan_words(&words, depth, state);
    }
}

fn base_name(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Skip leading assignments and wrappers. Returns whether one of the
/// assignments sets a `MISE_*` variable, and the words that follow.
fn command_words(words: &[String]) -> (bool, &[String]) {
    let mut assigns_mise = false;
    let mut rest = words;
    while let Some(first) = rest.first() {
        if is_assignment(first) {
            assigns_mise |= first.starts_with("MISE_");
        } else if !WRAPPERS.contains(&first.as_str()) {
            break;
        }
        rest = &rest[1..];
    }
    (assigns_mise, rest)
}

fn scan_words(words: &[String], depth: usize, state: &mut State) {
    let (assigns_mise, rest) = command_words(words);
    let Some(program) = rest.first() else {
        return;
    };
    let name = base_name(program);
    if matches!(name, "cd" | "pushd") {
        state.cd_seen = true;
    } else if name == "mise" {
        let (found, inner) = analyze_mise(&rest[1..], state.cd_seen, assigns_mise);
        state.uses.push(found);
        // The command mise starts is a command too: `devflow exec` in it, or
        // another use of mise.
        if depth < MAX_DEPTH {
            match inner {
                Some(Inner::Words(words)) => scan_words(words, depth + 1, state),
                Some(Inner::Script(script)) => scan(script, depth + 1, state),
                None => {}
            }
        }
    } else if name == "devflow" {
        if let Some(inner) = devflow_exec_command(&rest[1..]) {
            // What runs after devflow exec has devflow's variables only until
            // it starts mise: a mise inside is not protected by it. Whatever
            // the scanner cannot read there is held to the same rule: `mise`
            // as a word anywhere in it is an unprotected use.
            let before = state.uses.len();
            if depth < MAX_DEPTH {
                scan_words(inner, depth + 1, state);
            }
            if state.uses.len() == before && inner.iter().any(|word| mentions_mise(word)) {
                state.uses.push(MiseUse {
                    subcommand: String::new(),
                    through_devflow_exec: false,
                    runs_code: true,
                    unfaithful: state.cd_seen.then_some(Unfaithful::ChangesDirectory),
                });
            }
        }
    } else if SHELLS.contains(&name)
        && let Some(script) = shell_script(&rest[1..])
    {
        scan(script, depth + 1, state);
    } else if name == "eval" {
        scan(&rest[1..].join(" "), depth + 1, state);
    } else if program.starts_with('-') || OPTION_WRAPPERS.contains(&name) {
        // A wrapper took options (`env -u X`, `sudo -E`, `nice -n 5`) or takes
        // them (`timeout 5`, `xargs -n1`): the command it runs starts at a word
        // the scanner cannot tell by itself. Try each word in turn and keep the
        // first that is a command it knows.
        let was = std::mem::replace(&mut state.guessing, true);
        for start in 1..rest.len() {
            let before = state.uses.len();
            scan_words(&rest[start..], depth, state);
            if state.uses.len() > before {
                break;
            }
        }
        state.guessing = was;
    } else if program.starts_with('$') && !state.guessing {
        state.opaque = true;
    }
}

/// The script of `sh -c SCRIPT` (the words after the shell's name).
fn shell_script(args: &[String]) -> Option<&str> {
    // `-c`, or a cluster of single-letter flags with a `c` in it (`-lc`).
    let index = args
        .iter()
        .position(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c'))?;
    args.get(index + 1).map(String::as_str)
}

/// For the words after `devflow`: the command after `exec --`, when the
/// subcommand is `exec` (empty when there is none).
fn devflow_exec_command(args: &[String]) -> Option<&[String]> {
    let mut i = 0;
    while let Some(word) = args.get(i) {
        if word == "-C" {
            i += 2;
        } else if word.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    if args.get(i).map(String::as_str) != Some("exec") {
        return None;
    }
    let after = &args[i + 1..];
    Some(
        after
            .iter()
            .position(|w| w == "--")
            .map_or(&after[after.len()..], |at| &after[at + 1..]),
    )
}

/// Whether the command that mise runs (the words after `--`) starts with
/// `devflow exec` and the command that runs reaches no mise: a mise inside
/// applies its `[env]` after devflow set the variables, so devflow no longer
/// has the last word (`mise exec -- devflow exec -- mise exec -- CMD`).
fn inner_is_devflow_exec(words: &[String], depth: usize) -> bool {
    let (_, rest) = command_words(words);
    let Some(program) = rest.first() else {
        return false;
    };
    let name = base_name(program);
    if name == "devflow" {
        return devflow_exec_command(&rest[1..]).is_some_and(exec_has_last_word);
    }
    SHELLS.contains(&name)
        && shell_script(&rest[1..]).is_some_and(|script| string_is_devflow_exec(script, depth + 1))
}

/// The command after `devflow exec --` is there and starts no mise.
fn exec_has_last_word(inner: &[String]) -> bool {
    !inner.is_empty() && !reaches_mise(inner)
}

/// A shell string that runs `devflow exec` and nothing else that could start
/// a command (builtins such as `cd` aside).
fn string_is_devflow_exec(script: &str, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let mut seen = false;
    for segment in split_segments(script) {
        let words = split_words(&segment);
        let (_, rest) = command_words(&words);
        let Some(program) = rest.first() else {
            continue;
        };
        let name = base_name(program);
        if NEUTRAL.contains(&name) {
            continue;
        }
        if name == "devflow" && devflow_exec_command(&rest[1..]).is_some_and(exec_has_last_word) {
            seen = true;
        } else {
            return false;
        }
    }
    seen
}

fn takes_value(flag: &str) -> bool {
    matches!(
        flag,
        "-C" | "--cd"
            | "-E"
            | "--env"
            | "--profile"
            | "-P"
            | "-j"
            | "--jobs"
            | "--allow-env"
            | "--allow-read"
            | "--allow-write"
            | "--allow-net"
            | "--secrets"
    )
}

/// What `mise exec` starts.
enum Inner<'a> {
    /// The words after `--`.
    Words(&'a [String]),
    /// The string of `-c`.
    Script(&'a str),
}

fn analyze_mise<'a>(
    args: &'a [String],
    cd_seen: bool,
    assigns_mise: bool,
) -> (MiseUse, Option<Inner<'a>>) {
    let mut selects = false;
    let mut i = 0;
    while let Some(word) = args.get(i) {
        if !word.starts_with('-') {
            break;
        }
        let long_value = ["--cd=", "--env=", "--profile="]
            .iter()
            .any(|prefix| word.starts_with(prefix));
        let glued = (word.starts_with("-C") || word.starts_with("-E")) && word.len() > 2;
        if matches!(
            word.as_str(),
            "-C" | "--cd" | "-E" | "--env" | "--profile" | "-P"
        ) || long_value
            || glued
        {
            selects = true;
        }
        i += if takes_value(word) { 2 } else { 1 };
    }
    let sub = args.get(i).map(String::as_str).unwrap_or("");
    let after = args.get(i + 1..).unwrap_or(&[]);
    let mut inner = None;
    let (through, runs_code, task) = match sub {
        "" => (false, false, false),
        "exec" | "x" => {
            inner = exec_inner(after);
            let through = match inner {
                Some(Inner::Words(words)) => inner_is_devflow_exec(words, 0),
                Some(Inner::Script(script)) => string_is_devflow_exec(script, 1),
                None => false,
            };
            (through, true, false)
        }
        "tasks" | "t" => {
            let runs = !matches!(
                after.first().map(String::as_str),
                None | Some("ls" | "list" | "info" | "add" | "edit" | "deps" | "validate")
            );
            (false, runs, runs)
        }
        other if NON_RUNNING.contains(&other) => (false, false, false),
        // `run`, `watch`, `shell`, or a task named directly (`mise build`).
        _ => (false, true, true),
    };
    let unfaithful = if task {
        Some(Unfaithful::Task)
    } else if through {
        None
    } else if selects {
        Some(Unfaithful::Selects)
    } else if cd_seen {
        Some(Unfaithful::ChangesDirectory)
    } else if assigns_mise {
        Some(Unfaithful::Assigns)
    } else {
        None
    };
    let found = MiseUse {
        subcommand: sub.to_string(),
        through_devflow_exec: through,
        runs_code,
        unfaithful,
    };
    (found, inner)
}

/// The command `mise exec` starts, from the words after `exec`.
fn exec_inner(after: &[String]) -> Option<Inner<'_>> {
    let mut i = 0;
    while let Some(word) = after.get(i) {
        if word == "--" {
            return Some(Inner::Words(&after[i + 1..]));
        }
        if word == "-c" || word == "--command" {
            return after.get(i + 1).map(|script| Inner::Script(script));
        }
        if let Some(script) = word.strip_prefix("--command=") {
            return Some(Inner::Script(script));
        }
        i += if takes_value(word) { 2 } else { 1 };
    }
    None
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Split at unquoted command separators. Quotes are kept in the pieces.
fn split_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == '\\' && q == '"' {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    current.push(c);
                }
                '\\' => {
                    current.push(c);
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                ';' | '&' | '|' | '(' | ')' | '{' | '}' | '`' | '\n' | '\r' => {
                    segments.push(std::mem::take(&mut current));
                }
                '$' if chars.peek() == Some(&'(') => {
                    segments.push(std::mem::take(&mut current));
                }
                _ => current.push(c),
            },
        }
    }
    segments.push(current);
    segments
}

/// Split a simple command into words, removing quotes.
fn split_words(segment: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                } else {
                    current.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                        in_word = true;
                    }
                }
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut current));
                        in_word = false;
                    }
                }
                _ => {
                    current.push(c);
                    in_word = true;
                }
            },
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests;
