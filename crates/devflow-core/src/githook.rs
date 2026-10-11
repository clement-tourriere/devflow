//! The optional `post-checkout` Git hook that sets up worktrees created by
//! other tools (`git worktree add`, IDEs, agents).
//!
//! A hook somebody else wrote is never edited, moved or wrapped: installing
//! over it is a typed [`ForeignHook`] refusal, and the person adds
//! [`HOOK_LINE`] to it themselves (core owns the text, so the CLI and the app
//! print the same line).
use crate::project::Project;
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

const HOOK_NAME: &str = "post-checkout";
/// Identifies a hook devflow wrote (any version) so it is safe to replace or remove.
const MARKER_PREFIX: &str = "# devflow-managed-hook";
/// Marks the wrapper that unreleased builds installed over a hook somebody
/// else wrote (they moved that hook to [`LEGACY_ORIGINAL_NAME`]). Nothing
/// writes it any more; it is only recognised so it can be undone.
const LEGACY_WRAPS_PREFIX: &str = "# devflow-wraps-original";
/// Where those builds kept the hook they moved aside (next to the hook).
const LEGACY_ORIGINAL_NAME: &str = "post-checkout.devflow-original";
#[cfg(test)]
const MARKER: &str = "# devflow-managed-hook v2";

/// The one line devflow's own hook is made of, and the one to add to a
/// `post-checkout` hook somebody else wrote. It runs `devflow setup` only for a
/// branch checkout (flag 1) from the all-zero previous HEAD, which is how Git
/// reports a fresh worktree or clone (zeros of any length, so SHA-1 and SHA-256
/// repositories both match); a file checkout (flag 0) runs nothing. Every
/// variable has a default, so it runs under `set -u`. A failing setup prints how
/// to retry and never fails the hook (Git would report that as a failed
/// `worktree add`), and the line ends with the status the hook had when it
/// started (`$?` of the command before it), so it does not change what the hook
/// would have returned. It belongs at the end of the hook: it does not alter an
/// earlier `exit`, and it never runs after one.
pub const HOOK_LINE: &str = r#"devflow_status=$?; if [ -n "${1:-}" ] && [ "${1#*[!0]}" = "$1" ] && [ "${3:-}" = 1 ] && [ -z "${DEVFLOW_SETUP_ACTIVE:-}" ] && command -v devflow >/dev/null 2>&1; then devflow setup --if-new --from-git-hook || echo "devflow: setup failed; run \`devflow setup\` in $(pwd) to retry" >&2; fi; (exit "$devflow_status")"#;

fn script() -> String {
    format!(
        r#"#!/bin/sh
# devflow-managed-hook v2
# Installed by `devflow hook install`; remove it with `devflow hook uninstall`.
# Sets up worktrees created by other tools: a branch checkout (flag 1) whose
# previous HEAD is all zeros.
{HOOK_LINE}
exit 0
"#
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InstallState {
    NotInstalled,
    /// devflow's own hook.
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

/// The hook somebody else wrote and an unreleased build of devflow moved
/// aside, when `hook` is the wrapper that build installed in its place and the
/// original is still beside it.
pub fn legacy_original(hook: &Path) -> Option<PathBuf> {
    let text = std::fs::read(hook).ok()?;
    let wrapper = String::from_utf8_lossy(&text)
        .lines()
        .any(|l| l.starts_with(LEGACY_WRAPS_PREFIX));
    let original = hook.with_file_name(LEGACY_ORIGINAL_NAME);
    (wrapper && std::fs::symlink_metadata(&original).is_ok()).then_some(original)
}

/// A `post-checkout` hook that devflow did not write is in the way of
/// installing its own. It was left alone: devflow never edits another tool's
/// hook. What to do about it travels as remedies
/// ([`crate::remedy::remedies_of`]): open the hook, and copy [`HOOK_LINE`] into
/// it. The message names the file and never quotes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignHook {
    /// The hook that is there.
    pub path: PathBuf,
}

impl std::fmt::Display for ForeignHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "This repository already has a post-checkout hook from another tool ({}); devflow does not edit it. Add devflow's line to it so new worktrees are set up.",
            self.path.display()
        )
    }
}

impl std::error::Error for ForeignHook {}

fn write_hook(path: &Path, text: &str) -> Result<()> {
    crate::fsutil::write_atomic(path, text.as_bytes(), false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("cannot make {} executable", path.display()))?;
    }
    Ok(())
}

fn hook_lock(project: &Project) -> PathBuf {
    project.git_common_dir.join("devflow").join("hook.lock")
}

/// Install into the hooks directory (honours `core.hooksPath`). Refuses to
/// replace a foreign hook with a [`ForeignHook`] error. The wrapper an
/// unreleased build installed over somebody's hook is left as it is, so the
/// hook it runs keeps running; [`uninstall`] undoes it.
pub fn install(project: &Project) -> Result<PathBuf> {
    crate::fsutil::with_lock(&hook_lock(project), || {
        let path = match state(project)? {
            InstallState::Foreign { path } => return Err(ForeignHook { path }.into()),
            InstallState::Installed { path } if legacy_original(&path).is_some() => {
                return Ok(path);
            }
            InstallState::Installed { path } => path,
            InstallState::NotInstalled => hooks_dir(project)?.join(HOOK_NAME),
        };
        write_hook(&path, &script())?;
        Ok(path)
    })
}

/// Remove the hook only if devflow wrote it; `true` when it did. A hook
/// somebody else wrote is never touched. When the hook is the wrapper an
/// unreleased build installed over somebody's hook, that hook is renamed back
/// into place instead (its content, mode and owner are exactly what they were).
pub fn uninstall(project: &Project) -> Result<bool> {
    crate::fsutil::with_lock(&hook_lock(project), || match state(project)? {
        InstallState::Installed { path } => {
            match legacy_original(&path) {
                Some(original) => std::fs::rename(&original, &path)
                    .with_context(|| format!("cannot restore {}", path.display()))?,
                None => std::fs::remove_file(&path)
                    .with_context(|| format!("cannot remove {}", path.display()))?,
            }
            Ok(true)
        }
        InstallState::NotInstalled | InstallState::Foreign { .. } => Ok(false),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::worktree::testutil::{self, TestRepo};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
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

    fn write_executable(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
    fn the_generated_hook_runs_under_set_u_with_nothing_set() {
        let t = testutil::repo(None);
        let hook = installed(&t);
        let fake = FakeDevflow::new();
        // `sh -u` makes every unset variable an error, and a hook may be run
        // with fewer arguments than Git passes.
        for args in [&[][..], &[ZERO40, "x", "1"][..]] {
            let out = Command::new("sh")
                .arg("-u")
                .arg(&hook)
                .args(args)
                .env("PATH", fake.path_env(true))
                .env("FAKE_LOG", fake.log())
                .env_remove("DEVFLOW_SETUP_ACTIVE")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }
        assert_eq!(fake.calls().len(), 1, "{:?}", fake.calls());
    }

    #[test]
    fn the_hook_is_the_shared_line_and_the_line_is_what_a_foreign_hook_needs() {
        let t = testutil::repo(None);
        let text = std::fs::read_to_string(installed(&t)).unwrap();
        assert!(text.lines().any(|l| l == HOOK_LINE), "{text}");
        assert!(!HOOK_LINE.contains('\n'));
        assert!(HOOK_LINE.contains("${DEVFLOW_SETUP_ACTIVE:-}"));

        // Appended to somebody's own hook, whose last command ended with a
        // status: setup runs for a fresh branch checkout, a failing setup only
        // prints a hint, and the hook still ends with its own status.
        let fake = FakeDevflow::new();
        let theirs = t.dir.path().join("theirs");
        for shell in ["sh", "bash"] {
            for setup_fails in [false, true] {
                for last_status in [0, 3] {
                    std::fs::write(
                        &theirs,
                        format!("echo mine\n(exit {last_status})\n{HOOK_LINE}\n"),
                    )
                    .unwrap();
                    let calls = fake.calls().len();
                    let mut cmd = Command::new(shell);
                    cmd.arg("-u")
                        .arg(&theirs)
                        .args([ZERO40, "head", "1"])
                        .env("PATH", fake.path_env(true))
                        .env("FAKE_LOG", fake.log());
                    if setup_fails {
                        cmd.env("FAKE_FAIL", "1");
                    }
                    let out = cmd.output().unwrap();
                    assert_eq!(fake.calls().len(), calls + 1, "{shell}");
                    assert_eq!(out.status.code(), Some(last_status), "{shell}: {out:?}");
                    assert_eq!(
                        String::from_utf8_lossy(&out.stderr).contains("devflow setup"),
                        setup_fails
                    );
                }
            }
        }
        // `set -e` hooks that reach the line keep working.
        write_executable(&theirs, &format!("#!/bin/sh\nset -eu\ntrue\n{HOOK_LINE}\n"));
        let calls = fake.calls().len();
        let out = run_hook(&theirs, [ZERO40, "head", "1"], &fake, &[], true);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(fake.calls().len(), calls + 1);
        // An earlier `exit` is not changed, and the line then never runs.
        write_executable(&theirs, &format!("#!/bin/sh\nexit 7\n{HOOK_LINE}\n"));
        let calls = fake.calls().len();
        let out = run_hook(&theirs, [ZERO40, "head", "1"], &fake, &[], true);
        assert_eq!(out.status.code(), Some(7));
        assert_eq!(fake.calls().len(), calls);
        // A file checkout runs nothing.
        write_executable(&theirs, &format!("#!/bin/sh\n{HOOK_LINE}\n"));
        run_hook(&theirs, [ZERO40, "head", "0"], &fake, &[], true);
        assert_eq!(fake.calls().len(), calls);
    }

    /// What an unreleased build wrote over somebody's hook.
    fn legacy_wrapper(t: &TestRepo, original: &str) -> (PathBuf, PathBuf) {
        let hooks = t.root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let (path, aside) = (
            hooks.join("post-checkout"),
            hooks.join("post-checkout.devflow-original"),
        );
        std::fs::write(
            &path,
            format!("#!/bin/sh\n{MARKER}\n# devflow-wraps-original post-checkout.devflow-original\nexit 0\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&aside, original).unwrap();
        std::fs::set_permissions(&aside, std::fs::Permissions::from_mode(0o750)).unwrap();
        (path, aside)
    }

    #[test]
    fn uninstall_puts_back_the_hook_an_unreleased_build_moved_aside() {
        let t = testutil::repo(None);
        let original = "#!/bin/bash\necho mine\n";
        let (path, aside) = legacy_wrapper(&t, original);
        let inode = aside.metadata().unwrap().ino();
        assert_eq!(legacy_original(&path), Some(aside.clone()));

        // Installing keeps the wrapper, so the hook it runs keeps running.
        let before = std::fs::read(&path).unwrap();
        assert_eq!(install(&t.project).unwrap(), path);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(aside.exists());

        assert!(uninstall(&t.project).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let restored = path.metadata().unwrap();
        assert_eq!(restored.ino(), inode);
        assert_eq!(restored.permissions().mode() & 0o777, 0o750);
        assert!(!aside.exists());
        // It is somebody else's hook again, and left alone.
        assert!(matches!(
            state(&t.project).unwrap(),
            InstallState::Foreign { .. }
        ));
        assert!(!uninstall(&t.project).unwrap());
        assert_eq!(legacy_original(&path), None);
    }

    #[test]
    fn a_legacy_wrapper_whose_original_is_gone_is_just_removed() {
        let t = testutil::repo(None);
        let (path, aside) = legacy_wrapper(&t, "#!/bin/sh\n");
        std::fs::remove_file(&aside).unwrap();
        assert_eq!(legacy_original(&path), None);
        assert!(uninstall(&t.project).unwrap());
        assert!(!path.exists());
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
    fn a_foreign_hook_is_never_replaced_and_the_error_is_typed_without_its_contents() {
        let t = testutil::repo(None);
        let hooks = t.root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let path = hooks.join("post-checkout");
        // A hook that carries a credential, as real ones do.
        let original = "#!/bin/sh\nnotify --password hunter2-secret\n"; // kingfisher:ignore
        std::fs::write(&path, original).unwrap();

        assert_eq!(
            state(&t.project).unwrap(),
            InstallState::Foreign { path: path.clone() }
        );
        let error = install(&t.project).unwrap_err();
        let foreign = error.downcast_ref::<ForeignHook>().expect("typed");
        assert_eq!(foreign.path, path);
        // The message is plain words naming the file; the way on travels as
        // remedies.
        let message = format!("{error:#}");
        assert!(message.contains(path.to_str().unwrap()), "{message}");
        assert!(message.contains("devflow does not edit it"), "{message}");
        assert!(!message.contains("devflow setup"), "{message}");
        assert!(!message.contains("hunter2") && !message.contains("notify"));
        assert_eq!(
            crate::remedy::remedies_of(&error),
            [
                crate::remedy::Remedy::CopyHookLine { path: path.clone() },
                crate::remedy::Remedy::OpenFile { path: path.clone() }
            ]
        );
        assert!(!uninstall(&t.project).unwrap());
        assert_eq!(install(&t.project).unwrap_err().to_string(), message);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(
            !path
                .with_file_name("post-checkout.devflow-original")
                .exists()
        );
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
