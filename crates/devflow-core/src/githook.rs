//! The optional `post-checkout` Git hook that sets up worktrees created by
//! other tools (`git worktree add`, IDEs, agents).
use crate::project::Project;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::PathBuf;

const HOOK_NAME: &str = "post-checkout";
/// Identifies a hook devflow wrote (any version) so it is safe to replace or remove.
const MARKER_PREFIX: &str = "# devflow-managed-hook";
#[cfg(test)]
const MARKER: &str = "# devflow-managed-hook v1";

/// Runs `devflow setup` only for a branch checkout (flag 1) from the all-zero
/// previous HEAD, which is how Git reports a fresh worktree or clone. The case
/// pattern accepts zeros of any length (SHA-1 and SHA-256 repositories). Git
/// would report a failing hook as a failed `worktree add`, so it always exits 0.
const SCRIPT: &str = r#"#!/bin/sh
# devflow-managed-hook v1
# Installed by `devflow hook install`; remove it with `devflow hook uninstall`.
# Sets up worktrees created by other tools: a branch checkout (flag 1) whose
# previous HEAD is all zeros.
case "$1" in
  ''|*[!0]*) exit 0 ;;
esac
[ "$3" = 1 ] || exit 0
[ -n "$DEVFLOW_SETUP_ACTIVE" ] && exit 0
command -v devflow >/dev/null 2>&1 || exit 0
devflow setup --if-new --from-git-hook ||
  echo "devflow: setup failed; run \`devflow setup\` in $(pwd) to retry" >&2
exit 0
"#;

/// The same logic as one line, for adding to a hook devflow did not write
/// (a POSIX shell script; it runs in a subshell and never changes the exit status).
pub const CHAIN_LINE: &str = r#"( [ "$3" = 1 ] && case "$1" in ''|*[!0]*) exit 0 ;; esac && [ -z "$DEVFLOW_SETUP_ACTIVE" ] && command -v devflow >/dev/null 2>&1 && { devflow setup --if-new --from-git-hook || echo "devflow: setup failed; run 'devflow setup' in $(pwd) to retry" >&2; } ) || true"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InstallState {
    NotInstalled,
    Installed {
        path: PathBuf,
    },
    /// A post-checkout hook that devflow did not write.
    Foreign {
        path: PathBuf,
    },
}

/// `core.hooksPath` (relative paths are relative to the main checkout), else
/// `<common git dir>/hooks`.
pub fn hooks_dir(project: &Project) -> Result<PathBuf> {
    let configured = project
        .repo()?
        .config()?
        .get_path("core.hooksPath")
        .ok()
        .filter(|p| !p.as_os_str().is_empty());
    Ok(match configured {
        Some(path) if path.is_absolute() => path,
        Some(path) => project.root.join(path),
        None => project.git_common_dir.join("hooks"),
    })
}

pub fn state(project: &Project) -> Result<InstallState> {
    let path = hooks_dir(project)?.join(HOOK_NAME);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            if text.lines().any(|l| l.trim().starts_with(MARKER_PREFIX)) {
                Ok(InstallState::Installed { path })
            } else {
                Ok(InstallState::Foreign { path })
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(InstallState::NotInstalled),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Install into the hooks directory (honours `core.hooksPath`). Refuses to
/// replace a foreign hook and prints the line to add instead.
pub fn install(project: &Project) -> Result<PathBuf> {
    let path = match state(project)? {
        InstallState::Foreign { path } => bail!(
            "{} already exists and was not written by devflow, so it was left alone.\n\
             To set up worktrees created by other tools, add this line to it:\n\n    {CHAIN_LINE}\n",
            path.display()
        ),
        InstallState::Installed { path } => path,
        InstallState::NotInstalled => hooks_dir(project)?.join(HOOK_NAME),
    };
    crate::fsutil::write_atomic(&path, SCRIPT.as_bytes(), false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("cannot make {} executable", path.display()))?;
    }
    Ok(path)
}

/// Remove the hook only if devflow wrote it.
pub fn uninstall(project: &Project) -> Result<bool> {
    match state(project)? {
        InstallState::Installed { path } => {
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
            Ok(true)
        }
        InstallState::NotInstalled | InstallState::Foreign { .. } => Ok(false),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::worktree::testutil::{self, TestRepo};
    use std::path::Path;
    use std::process::{Command, Output};

    const ZERO40: &str = "0000000000000000000000000000000000000000";
    const ZERO64: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    /// A directory holding a fake `devflow` that records its invocations.
    struct FakeDevflow {
        dir: tempfile::TempDir,
    }

    impl FakeDevflow {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("devflow");
            std::fs::write(
                &script,
                "#!/bin/sh\necho \"$* | $(pwd -P) | active=$DEVFLOW_SETUP_ACTIVE\" >> \"$FAKE_LOG\"\n[ -z \"$FAKE_FAIL\" ]\n",
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self { dir }
        }

        fn log(&self) -> PathBuf {
            self.dir.path().join("calls.log")
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.log())
                .map(|t| t.lines().map(str::to_string).collect())
                .unwrap_or_default()
        }

        fn path_env(&self, with_fake: bool) -> String {
            if with_fake {
                format!("{}:/usr/bin:/bin", self.dir.path().display())
            } else {
                "/usr/bin:/bin".to_string()
            }
        }
    }

    fn run_hook(
        hook: &Path,
        args: [&str; 3],
        fake: &FakeDevflow,
        extra: &[(&str, &str)],
        with_fake: bool,
    ) -> Output {
        let mut cmd = Command::new(hook);
        cmd.args(args)
            .env("PATH", fake.path_env(with_fake))
            .env("FAKE_LOG", fake.log());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn installed(t: &TestRepo) -> PathBuf {
        install(&t.project).unwrap()
    }

    #[test]
    fn installs_an_executable_marked_script_and_uninstalls_it() {
        let t = testutil::repo(None);
        assert_eq!(state(&t.project).unwrap(), InstallState::NotInstalled);
        let path = installed(&t);
        assert_eq!(path, t.root.join(".git/hooks/post-checkout"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("#!/bin/sh\n"));
        assert_eq!(text.lines().nth(1), Some(MARKER));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o111, 0o111);
        assert_eq!(
            state(&t.project).unwrap(),
            InstallState::Installed { path: path.clone() }
        );
        // Reinstalling upgrades in place.
        assert_eq!(install(&t.project).unwrap(), path);

        assert!(uninstall(&t.project).unwrap());
        assert!(!path.exists());
        assert!(!uninstall(&t.project).unwrap());
    }

    #[test]
    fn only_a_fresh_branch_checkout_runs_setup() {
        let t = testutil::repo(None);
        let hook = installed(&t);
        let fake = FakeDevflow::new();
        let call = |args: [&str; 3], extra: &[(&str, &str)], with_fake: bool| {
            let out = run_hook(&hook, args, &fake, extra, with_fake);
            assert!(out.status.success(), "the hook never fails: {out:?}");
            out
        };

        call([ZERO40, "abc123", "1"], &[], true);
        call([ZERO64, "abc123", "1"], &[], true);
        call(["0", "abc123", "1"], &[], true);
        assert_eq!(fake.calls().len(), 3, "zeros of any length count");
        assert!(fake.calls()[0].starts_with("setup --if-new --from-git-hook | "));
        assert!(fake.calls()[0].ends_with("| active="), "{:?}", fake.calls());

        // Not a branch checkout, not a fresh HEAD, re-entry, empty, no devflow.
        call([ZERO40, "abc123", "0"], &[], true);
        call(["abc123", "def456", "1"], &[], true);
        call(
            ["000000000000000000000000000000000000000x", "abc", "1"],
            &[],
            true,
        );
        call(["", "abc", "1"], &[], true);
        call(
            [ZERO40, "abc123", "1"],
            &[("DEVFLOW_SETUP_ACTIVE", "1")],
            true,
        );
        call([ZERO40, "abc123", "1"], &[], false);
        assert_eq!(fake.calls().len(), 3, "{:?}", fake.calls());
    }

    #[test]
    fn setup_failures_print_a_hint_and_still_exit_zero() {
        let t = testutil::repo(None);
        let hook = installed(&t);
        let fake = FakeDevflow::new();
        let out = run_hook(
            &hook,
            [ZERO40, "abc", "1"],
            &fake,
            &[("FAKE_FAIL", "1")],
            true,
        );
        assert!(out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("devflow setup"), "{stderr}");
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn honours_core_hooks_path_relative_and_absolute() {
        let t = testutil::repo(None);
        t.project
            .repo()
            .unwrap()
            .config()
            .unwrap()
            .set_str("core.hooksPath", "tools/hooks")
            .unwrap();
        let path = installed(&t);
        assert_eq!(path, t.root.join("tools/hooks/post-checkout"));
        assert!(path.is_file());
        assert!(!t.root.join(".git/hooks/post-checkout").exists());

        let absolute = t.dir.path().join("shared-hooks");
        t.project
            .repo()
            .unwrap()
            .config()
            .unwrap()
            .set_str("core.hooksPath", absolute.to_str().unwrap())
            .unwrap();
        assert_eq!(state(&t.project).unwrap(), InstallState::NotInstalled);
        let path = installed(&t);
        assert_eq!(path, absolute.join("post-checkout"));
        assert!(uninstall(&t.project).unwrap());
    }

    #[test]
    fn a_foreign_hook_is_never_replaced_and_the_error_says_what_to_add() {
        let t = testutil::repo(None);
        let hooks = t.root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let path = hooks.join("post-checkout");
        let original = "#!/bin/sh\necho mine\n";
        std::fs::write(&path, original).unwrap();

        assert_eq!(
            state(&t.project).unwrap(),
            InstallState::Foreign { path: path.clone() }
        );
        let error = install(&t.project).unwrap_err().to_string();
        assert!(error.contains(CHAIN_LINE), "{error}");
        assert!(error.contains(path.to_str().unwrap()), "{error}");
        assert!(!uninstall(&t.project).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn the_chain_line_behaves_like_the_installed_hook() {
        let t = testutil::repo(None);
        let foreign = t.dir.path().join("foreign-hook");
        // `set -e` is the hostile case: the line must never abort the script.
        std::fs::write(
            &foreign,
            format!("#!/bin/sh\nset -e\n{CHAIN_LINE}\necho after\n"),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&foreign, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fake = FakeDevflow::new();

        for (args, calls) in [
            ([ZERO40, "x", "1"], 1),
            ([ZERO64, "x", "1"], 2),
            (["abc", "x", "1"], 2),
            ([ZERO40, "x", "0"], 2),
        ] {
            let out = run_hook(&foreign, args, &fake, &[], true);
            assert!(out.status.success(), "{out:?}");
            assert!(String::from_utf8_lossy(&out.stdout).contains("after"));
            assert_eq!(fake.calls().len(), calls);
        }
        let out = run_hook(
            &foreign,
            [ZERO40, "x", "1"],
            &fake,
            &[("FAKE_FAIL", "1")],
            true,
        );
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("after"));
        let out = run_hook(
            &foreign,
            [ZERO40, "x", "1"],
            &fake,
            &[("DEVFLOW_SETUP_ACTIVE", "1")],
            true,
        );
        assert!(out.status.success());
        assert_eq!(fake.calls().len(), 3);
    }

    #[test]
    fn git_worktree_add_triggers_the_hook_in_the_new_worktree() {
        let t = testutil::repo(None);
        installed(&t);
        let fake = FakeDevflow::new();
        let target = t.dir.path().join("by-git");
        let git = |args: &[&str], cwd: &Path| {
            let out = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env(
                    "PATH",
                    format!(
                        "{}:/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin",
                        fake.dir.path().display()
                    ),
                )
                .env("FAKE_LOG", fake.log())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(
            &["worktree", "add", "-b", "by-git", target.to_str().unwrap()],
            &t.root,
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].starts_with("setup --if-new --from-git-hook | "));
        assert!(
            calls[0].contains(target.canonicalize().unwrap().to_str().unwrap()),
            "runs inside the new worktree: {calls:?}"
        );
        // An ordinary branch switch in the main checkout does not.
        git(&["checkout", "-b", "other"], &t.root);
        assert_eq!(fake.calls().len(), 1);
    }
}
