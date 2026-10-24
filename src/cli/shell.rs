//! Shell integration: the `devflow` wrapper function and the directory handoff
//! that lets `devflow switch` move the caller's shell.
//!
//! A child process cannot change its parent shell's directory. The wrapper
//! from `devflow shell-init` creates a private temporary file, passes its path
//! in `DEVFLOW_CD_FILE`, and changes into the directory devflow writes there
//! once the command exits. devflow's output stays attached to the terminal, so
//! prompts and streamed hook output work through the wrapper.
//!
//! The wrapper also marks its calls (`DEVFLOW_WRAPPED=1` in the command's
//! environment, so devflow can tell when it is not active) and remembers the
//! directory it left (`DEVFLOW_PREVIOUS`, exported after each change of
//! directory), which is what `devflow switch -` goes back to.
use super::output;
use anyhow::{Result, bail};
use clap::{Args, ValueEnum};
use devflow_core::remedy::Remedy;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;

/// File the wrapper reads the target directory from.
const CD_FILE_VAR: &str = "DEVFLOW_CD_FILE";

/// Set to `1` by the wrapper in the environment of the command it runs.
const WRAPPED_VAR: &str = "DEVFLOW_WRAPPED";

/// The directory the wrapper left at its last change of directory; a variable
/// of the shell itself, so it stays in the environment.
pub const PREVIOUS_VAR: &str = "DEVFLOW_PREVIOUS";

static CD_FILE: OnceLock<Option<PathBuf>> = OnceLock::new();
static WRAPPED: OnceLock<bool> = OnceLock::new();
static PREVIOUS: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Take the wrapper's handoff variable out of the process environment.
///
/// It is addressed to this process only. Hooks, agents and nested devflow
/// commands inherit the environment and must not move the caller's shell.
/// Call it first in `main`, before any thread starts.
pub fn take_handoff() {
    let file = std::env::var_os(CD_FILE_VAR)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    // SAFETY: called at the top of `main`, before this process starts threads.
    unsafe { std::env::remove_var(CD_FILE_VAR) };
    let _ = CD_FILE.set(file);
    // Whether the wrapper started this very command is as private as the
    // handoff file: a hook or an agent started from here is not wrapped.
    let wrapped = std::env::var_os(WRAPPED_VAR).is_some_and(|value| value == "1");
    // SAFETY: as above.
    unsafe { std::env::remove_var(WRAPPED_VAR) };
    let _ = WRAPPED.set(wrapped);
    let _ = PREVIOUS.set(
        std::env::var_os(PREVIOUS_VAR)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    );
}

/// This command was started by the shell wrapper (`devflow shell-init`), so
/// `switch` can change the shell's directory.
pub fn wrapped() -> bool {
    WRAPPED.get().copied().unwrap_or(false)
}

/// The directory the wrapper last left in this shell, if it ever changed one.
pub fn previous() -> Option<PathBuf> {
    PREVIOUS.get().cloned().flatten()
}

/// What to do when the wrapper is not active.
pub fn remedy() -> Remedy {
    Remedy::ShellIntegration {
        shell: ShellKind::detect().map(|shell| shell.name().to_string()),
    }
}

/// Ask the shell wrapper to change into `target` after devflow exits. Returns
/// false when no wrapper will act on it, so the caller prints the path instead.
pub fn request_cd(target: &Path) -> bool {
    let Some(Some(file)) = CD_FILE.get() else {
        return false;
    };
    // The wrapper created this file; never create one at another path.
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(file)
        .and_then(|mut file| {
            std::io::Write::write_all(&mut file, target.as_os_str().as_encoded_bytes())
        })
        .is_ok()
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

impl ShellKind {
    pub fn name(self) -> &'static str {
        match self {
            ShellKind::Bash => "bash",
            ShellKind::Zsh => "zsh",
            ShellKind::Fish => "fish",
        }
    }

    /// The shell named by `$SHELL`, when it is one we support.
    pub fn detect() -> Option<ShellKind> {
        let shell = std::env::var("SHELL").ok()?;
        match Path::new(&shell).file_name()?.to_str()? {
            "bash" => Some(ShellKind::Bash),
            "zsh" => Some(ShellKind::Zsh),
            "fish" => Some(ShellKind::Fish),
            _ => None,
        }
    }
}

#[derive(Args)]
pub struct ShellInitArgs {
    /// Shell to generate for (default: detected from $SHELL)
    pub shell: Option<ShellKind>,
}

pub fn init(args: ShellInitArgs) -> Result<ExitCode> {
    let shell = match args.shell.or_else(ShellKind::detect) {
        Some(shell) => shell,
        None => {
            bail!("cannot detect the shell from $SHELL; use `devflow shell-init <bash|zsh|fish>`")
        }
    };
    output::say_raw(script(shell));
    Ok(ExitCode::SUCCESS)
}

/// Hint printed (to a terminal only) when `switch` could not change directory.
pub fn manual_hint() -> String {
    remedy()
        .hint()
        .map_or_else(String::new, |hint| format!("tip: {hint}"))
}

/// Wrapper for bash and zsh. `{shell}` is replaced with the shell name.
const POSIX_WRAPPER: &str = r#"# devflow shell integration ({shell})
# Runs devflow attached to the terminal, then changes directory when a command
# asks for it (`devflow switch`), remembering where it came from for `switch -`.
devflow() {
    local cd_file target exit_code before
    # Completion requests (COMPLETE=<shell>) need no handoff file.
    if [ -n "${COMPLETE-}" ]; then
        command devflow "$@"
        return
    fi
    cd_file="$(mktemp "${TMPDIR:-/tmp}/devflow-cd.XXXXXX")" || {
        command devflow "$@"
        return
    }
    DEVFLOW_WRAPPED=1 DEVFLOW_CD_FILE="$cd_file" command devflow "$@"
    exit_code=$?
    target="$(cat -- "$cd_file" 2>/dev/null)"
    rm -f -- "$cd_file"
    if [ -n "$target" ] && [ -d "$target" ] && [ "$target" != "$PWD" ]; then
        before="$PWD"
        builtin cd -- "$target" || return 1
        export DEVFLOW_PREVIOUS="$before"
        echo "Changed directory to: $target"
    fi
    return "$exit_code"
}
"#;

const FISH_WRAPPER: &str = r#"# devflow shell integration (fish)
# Runs devflow attached to the terminal, then changes directory when a command
# asks for it (`devflow switch`), remembering where it came from for `switch -`.
function devflow --wraps devflow --description "devflow with auto-cd"
    # Completion requests (COMPLETE=<shell>) need no handoff file.
    if set -q COMPLETE[1]; and test -n "$COMPLETE"
        command devflow $argv
        return
    end
    set -l tmpdir /tmp
    set -q TMPDIR[1]; and set tmpdir $TMPDIR
    set -l cd_file (mktemp "$tmpdir/devflow-cd.XXXXXX")
    or begin
        command devflow $argv
        return
    end
    DEVFLOW_WRAPPED=1 DEVFLOW_CD_FILE=$cd_file command devflow $argv
    set -l exit_code $status
    set -l target (string collect < $cd_file)
    rm -f -- $cd_file
    if test -n "$target"; and test -d "$target"; and test "$target" != "$PWD"
        set -l before $PWD
        cd -- $target
        and set -gx DEVFLOW_PREVIOUS $before
        and echo "Changed directory to: $target"
    end
    return $exit_code
end
"#;

pub fn script(shell: ShellKind) -> String {
    match shell {
        ShellKind::Bash | ShellKind::Zsh => POSIX_WRAPPER.replace("{shell}", shell.name()),
        ShellKind::Fish => FISH_WRAPPER.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_pass_the_handoff_file() {
        for shell in [ShellKind::Bash, ShellKind::Zsh, ShellKind::Fish] {
            let text = script(shell);
            assert!(text.contains("DEVFLOW_CD_FILE"), "{}", shell.name());
            assert!(text.contains("DEVFLOW_WRAPPED=1"), "{}", shell.name());
            assert!(text.contains("DEVFLOW_PREVIOUS"), "{}", shell.name());
            assert!(text.contains("command devflow"), "{}", shell.name());
        }
        assert!(script(ShellKind::Zsh).contains("(zsh)"));
    }
}
