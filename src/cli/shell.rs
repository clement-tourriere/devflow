//! Shell integration: the `devflow` wrapper function and the directory handoff
//! that lets `devflow switch` move the caller's shell.
//!
//! A child process cannot change its parent shell's directory. The wrapper
//! from `devflow shell-init` creates a private temporary file, passes its path
//! in `DEVFLOW_CD_FILE`, and changes into the directory devflow writes there
//! once the command exits. devflow's output stays attached to the terminal, so
//! prompts and streamed hook output work through the wrapper.
use super::output;
use anyhow::{Result, bail};
use clap::{Args, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;

/// File the wrapper reads the target directory from.
const CD_FILE_VAR: &str = "DEVFLOW_CD_FILE";

static CD_FILE: OnceLock<Option<PathBuf>> = OnceLock::new();

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
    let shell = ShellKind::detect().map_or("zsh", ShellKind::name);
    format!(
        "tip: add `eval \"$(devflow shell-init {shell})\"` to your shell profile so `devflow switch` changes directory"
    )
}

/// Wrapper for bash and zsh. `{shell}` is replaced with the shell name.
const POSIX_WRAPPER: &str = r#"# devflow shell integration ({shell})
# Runs devflow attached to the terminal, then changes directory when a command
# asks for it (`devflow switch`).
devflow() {
    local cd_file target exit_code
    cd_file="$(mktemp "${TMPDIR:-/tmp}/devflow-cd.XXXXXX")" || {
        command devflow "$@"
        return
    }
    DEVFLOW_CD_FILE="$cd_file" command devflow "$@"
    exit_code=$?
    target="$(cat -- "$cd_file" 2>/dev/null)"
    rm -f -- "$cd_file"
    if [ -n "$target" ] && [ -d "$target" ] && [ "$target" != "$PWD" ]; then
        builtin cd -- "$target" || return 1
        echo "Changed directory to: $target"
    fi
    return "$exit_code"
}
"#;

const FISH_WRAPPER: &str = r#"# devflow shell integration (fish)
# Runs devflow attached to the terminal, then changes directory when a command
# asks for it (`devflow switch`).
function devflow --wraps devflow --description "devflow with auto-cd"
    set -l tmpdir /tmp
    set -q TMPDIR[1]; and set tmpdir $TMPDIR
    set -l cd_file (mktemp "$tmpdir/devflow-cd.XXXXXX")
    or begin
        command devflow $argv
        return
    end
    DEVFLOW_CD_FILE=$cd_file command devflow $argv
    set -l exit_code $status
    set -l target (string collect < $cd_file)
    rm -f -- $cd_file
    if test -n "$target"; and test -d "$target"; and test "$target" != "$PWD"
        cd -- $target
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
            assert!(text.contains("command devflow"), "{}", shell.name());
        }
        assert!(script(ShellKind::Zsh).contains("(zsh)"));
    }
}
