//! The optional `post-checkout` Git hook that sets up worktrees created by
//! other tools (`git worktree add`, IDEs, agents).
use crate::project::Project;
use anyhow::{Context, Result};
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

/// A `post-checkout` hook that devflow did not write is in the way of
/// installing its own. It was left alone. What to do about it travels as
/// remedies ([`crate::remedy::remedies_of`]): add devflow to that hook
/// ([`chain`]) or edit it by hand with [`CHAIN_LINE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignHook {
    /// The project's main checkout.
    pub root: PathBuf,
    /// The hook that is there.
    pub path: PathBuf,
}

impl std::fmt::Display for ForeignHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} already exists and was not written by devflow, so it was left alone",
            self.path.display()
        )
    }
}

impl std::error::Error for ForeignHook {}

/// Install into the hooks directory (honours `core.hooksPath`). Refuses to
/// replace a foreign hook with a [`ForeignHook`] error; [`chain`] adds devflow
/// to it instead.
pub fn install(project: &Project) -> Result<PathBuf> {
    let path = match state(project)? {
        InstallState::Foreign { path } => {
            return Err(ForeignHook {
                root: project.root.clone(),
                path,
            }
            .into());
        }
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

/// What [`chain`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Chained {
    /// The hook the line is in.
    pub path: PathBuf,
    /// `false` when the hook already had it (or is devflow's own).
    pub added: bool,
}

/// Add devflow to the `post-checkout` hook somebody else wrote, so that it also
/// sets up the worktrees other tools create: [`CHAIN_LINE`] is appended to the
/// file itself, with a comment saying where it comes from. Nothing else in the
/// hook changes, and the file keeps its inode, mode and owner (it is appended
/// to, not replaced).
///
/// Repeating it changes nothing (a hook that already runs
/// `devflow setup --from-git-hook` is left as it is), and a hook devflow cannot
/// edit safely is refused with a [`Problem`](crate::remedy::Problem) that says
/// why: a symbolic link (its target may be shared), a file that is not a
/// text script for a POSIX shell, one Git would not run (not executable), and
/// one that `exec`s or `exit`s at the top level, where an appended line would
/// never run. The line is never written into a hook devflow wrote.
pub fn chain(project: &Project) -> Result<Chained> {
    let lock = project.git_common_dir.join("devflow").join("hook.lock");
    crate::fsutil::with_lock(&lock, || chain_locked(project))
}

fn chain_locked(project: &Project) -> Result<Chained> {
    use crate::remedy::{Problem, Remedy};
    let path = match state(project)? {
        InstallState::Installed { path } => return Ok(Chained { path, added: false }),
        InstallState::NotInstalled => {
            return Err(Problem::new(
                "there is no post-checkout hook to add devflow to; install devflow's own",
            )
            .with(Remedy::InstallGitHook {
                root: project.root.clone(),
            })
            .into_error());
        }
        InstallState::Foreign { path } => path,
    };
    let refuse = |why: String| {
        Problem::new(format!(
            "{} was left alone: {why}. Add the line by hand",
            path.display()
        ))
        .with(Remedy::OpenFile { path: path.clone() })
        .into_error()
    };
    let meta = std::fs::symlink_metadata(&path)
        .with_context(|| format!("cannot inspect {}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(refuse(
            "it is a symbolic link, and its target may be shared with other repositories".into(),
        ));
    }
    if !meta.is_file() {
        return Err(refuse("it is not a regular file".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(refuse(
                "it is not executable, so Git does not run it".into(),
            ));
        }
    }
    let bytes = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let Ok(text) = String::from_utf8(bytes) else {
        return Err(refuse("it is not a text script".into()));
    };
    if text.contains("--from-git-hook") {
        return Ok(Chained { path, added: false });
    }
    if let Err(why) = appendable(&text) {
        return Err(refuse(why));
    }
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .with_context(|| format!("cannot write to {}", path.display()))?;
    let lead = if text.ends_with('\n') { "" } else { "\n" };
    file.write_all(
        format!("{lead}# Added by devflow: sets up worktrees other tools create.\n{CHAIN_LINE}\n")
            .as_bytes(),
    )
    .with_context(|| format!("cannot write to {}", path.display()))?;
    Ok(Chained { path, added: true })
}

/// Whether a line appended to the script `text` would run, and the script is
/// one [`CHAIN_LINE`] (POSIX shell) can be added to; otherwise why not.
fn appendable(text: &str) -> std::result::Result<(), String> {
    let first = text.lines().next().unwrap_or_default();
    let Some(interpreter) = first.strip_prefix("#!") else {
        return Err("it does not start with a shell interpreter line (#!/bin/sh)".into());
    };
    let mut words = interpreter.split_whitespace();
    let mut program = words.next().unwrap_or_default();
    if program.rsplit('/').next() == Some("env") {
        program = words.find(|w| !w.starts_with('-')).unwrap_or_default();
    }
    let program = program.rsplit('/').next().unwrap_or_default();
    if !["sh", "bash", "dash", "ash", "ksh", "zsh"].contains(&program) {
        return Err(format!(
            "it is a {} script, and the line is for a POSIX shell",
            if program.is_empty() { "?" } else { program }
        ));
    }
    for line in text.lines() {
        // Not indented: the script's own flow, where `exec` or `exit` ends it.
        if line == "exit" || line.starts_with("exit ") || line.starts_with("exec ") {
            return Err(format!(
                "it has `{}` before the end of the file, so a line added at the end would never run",
                line.trim_end()
            ));
        }
    }
    Ok(())
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
    fn a_foreign_hook_is_never_replaced_and_the_error_is_typed_without_the_snippet() {
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
        let error = install(&t.project).unwrap_err();
        let foreign = error.downcast_ref::<ForeignHook>().expect("typed");
        assert_eq!(foreign.path, path);
        assert_eq!(foreign.root, t.root);
        // The message is plain words: the snippet is data ([`CHAIN_LINE`]), and
        // the way on travels as remedies.
        let message = format!("{error:#}");
        assert!(message.contains(path.to_str().unwrap()), "{message}");
        assert!(!message.contains("devflow setup") && !message.contains(CHAIN_LINE));
        assert_eq!(
            crate::remedy::remedies_of(&error),
            [
                crate::remedy::Remedy::ChainGitHook {
                    root: t.root.clone()
                },
                crate::remedy::Remedy::OpenFile { path: path.clone() }
            ]
        );
        assert!(!uninstall(&t.project).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    fn foreign(t: &TestRepo, text: &str) -> PathBuf {
        let hooks = t.root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let path = hooks.join("post-checkout");
        std::fs::write(&path, text).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
        path
    }

    #[test]
    fn chaining_adds_the_line_once_keeps_the_rest_and_the_file_itself() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let t = testutil::repo(None);
        // No newline at the end: the line still starts on its own.
        let path = foreign(&t, "#!/bin/bash\nset -e\necho mine");
        let before = path.metadata().unwrap();

        let done = chain(&t.project).unwrap();
        assert_eq!(
            done,
            Chained {
                path: path.clone(),
                added: true
            }
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("#!/bin/bash\nset -e\necho mine\n# Added by devflow"));
        assert!(text.ends_with(&format!("{CHAIN_LINE}\n")), "{text}");
        // The same file, with the same mode: appended to, not replaced.
        let after = path.metadata().unwrap();
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.permissions().mode(), before.permissions().mode());
        // Still the other tool's hook (uninstall would not delete it), and
        // the doctor reads it as chained.
        assert!(matches!(
            state(&t.project).unwrap(),
            InstallState::Foreign { .. }
        ));
        assert!(!uninstall(&t.project).unwrap());
        assert!(path.exists());

        // Idempotent.
        let again = chain(&t.project).unwrap();
        assert_eq!(
            again,
            Chained {
                path: path.clone(),
                added: false
            }
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        // The hook still runs, then sets up a fresh worktree.
        let fake = FakeDevflow::new();
        let out = run_hook(&path, [ZERO40, "x", "1"], &fake, &[], true);
        assert!(out.status.success(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("mine"));
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn chaining_goes_through_a_hooks_path_and_leaves_devflows_own_hook_alone() {
        let t = testutil::repo(None);
        // Nothing there: install is the remedy, and nothing is written.
        let error = chain(&t.project).unwrap_err();
        assert_eq!(
            crate::remedy::remedies_of(&error),
            [crate::remedy::Remedy::InstallGitHook {
                root: t.root.clone()
            }]
        );
        assert!(!t.root.join(".git/hooks/post-checkout").exists());
        // devflow's own hook already has the line.
        let own = installed(&t);
        let text = std::fs::read_to_string(&own).unwrap();
        assert_eq!(
            chain(&t.project).unwrap(),
            Chained {
                path: own.clone(),
                added: false
            }
        );
        assert_eq!(std::fs::read_to_string(&own).unwrap(), text);
    }

    #[test]
    fn chaining_refuses_what_it_cannot_edit_safely_and_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let t = testutil::repo(None);
        let refused = |why: &str| {
            let error = chain(&t.project).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains(why), "{message}");
            assert!(!message.contains(CHAIN_LINE), "{message}");
            assert!(matches!(
                crate::remedy::remedies_of(&error)[..],
                [crate::remedy::Remedy::OpenFile { .. }]
            ));
        };
        let content_of = |p: &Path| std::fs::read(p).unwrap();

        for (text, why) in [
            ("#!/usr/bin/env python3\nprint('x')\n", "python3 script"),
            ("echo no interpreter line\n", "interpreter line"),
            (
                "#!/bin/sh\nrun-things\nexec other-hook \"$@\"\n",
                "exec other-hook",
            ),
            ("#!/bin/sh\nrun-things\nexit 0\n", "exit 0"),
        ] {
            let path = foreign(&t, text);
            refused(why);
            assert_eq!(content_of(&path), text.as_bytes(), "{text}");
        }
        // An indented exit is inside a block: the end of the file still runs.
        let path = foreign(&t, "#!/usr/bin/env bash\nif [ x ]; then\n  exit 0\nfi\n");
        assert!(chain(&t.project).unwrap().added);
        assert!(content_of(&path).ends_with(format!("{CHAIN_LINE}\n").as_bytes()));

        // Not UTF-8, not executable.
        let path = foreign(&t, "#!/bin/sh\n");
        std::fs::write(&path, b"#!/bin/sh\n\xff\xfe\n").unwrap();
        refused("not a text script");
        std::fs::write(&path, "#!/bin/sh\necho mine\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        refused("not executable");
        assert_eq!(content_of(&path), b"#!/bin/sh\necho mine\n");

        // A symbolic link: its target may be shared, so it is not touched.
        let target = t.dir.path().join("shared-hook");
        std::fs::write(&target, "#!/bin/sh\necho shared\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        refused("symbolic link");
        assert_eq!(content_of(&target), b"#!/bin/sh\necho shared\n");
        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
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
