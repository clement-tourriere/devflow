//! Git itself as the oracle for the tests that judge configuration and
//! attributes: the same files are given to the `git` on `PATH`, in an
//! environment that contains nothing else (no user, system or XDG
//! configuration, no system attributes).
use std::path::Path;
use std::process::{Command, Output};

/// Whether a `git` is installed. The tests that need one say so and stop
/// when it is not.
pub fn available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Run `git args` in `cwd` with `home` as `$HOME` and nothing else inherited
/// but `PATH`.
pub fn git(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .output()
        .expect("git runs")
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
