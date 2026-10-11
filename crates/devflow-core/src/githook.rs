//! The optional `post-checkout` Git hook that sets up worktrees created by
//! other tools (`git worktree add`, IDEs, agents).
//!
//! A hook somebody else wrote is never edited. [`chain`] moves it aside to a
//! sibling file and installs a hook that runs it first, with the same
//! arguments and input, then sets the worktree up, and exits with the
//! original's status; [`uninstall`] puts it back. Nothing parses the person's
//! script, so it can be written in any language.
use crate::project::Project;
use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const HOOK_NAME: &str = "post-checkout";
/// Where [`chain`] keeps the hook it moved aside (next to the hook).
const ORIGINAL_NAME: &str = "post-checkout.devflow-original";
/// Identifies a hook devflow wrote (any version) so it is safe to replace or remove.
const MARKER_PREFIX: &str = "# devflow-managed-hook";
/// Identifies the wrapper among the hooks devflow wrote.
const WRAPS_PREFIX: &str = "# devflow-wraps-original";
#[cfg(test)]
const MARKER: &str = "# devflow-managed-hook v2";

/// Runs `devflow setup` only for a branch checkout (flag 1) from the all-zero
/// previous HEAD, which is how Git reports a fresh worktree or clone. The case
/// pattern accepts zeros of any length (SHA-1 and SHA-256 repositories). Git
/// would report a failing hook as a failed `worktree add`, so it always
/// succeeds. Every variable has a default, so it also runs under `set -u`.
const SETUP_FUNCTION: &str = r#"devflow_setup() {
  case "${1:-}" in
    ''|*[!0]*) return 0 ;;
  esac
  [ "${3:-}" = 1 ] || return 0
  [ -z "${DEVFLOW_SETUP_ACTIVE:-}" ] || return 0
  command -v devflow >/dev/null 2>&1 || return 0
  devflow setup --if-new --from-git-hook ||
    echo "devflow: setup failed; run \`devflow setup\` in $(pwd) to retry" >&2
  return 0
}
"#;

fn script() -> String {
    format!(
        r#"#!/bin/sh
# devflow-managed-hook v2
# Installed by `devflow hook install`; remove it with `devflow hook uninstall`.
# Sets up worktrees created by other tools: a branch checkout (flag 1) whose
# previous HEAD is all zeros.
{SETUP_FUNCTION}devflow_setup "$@"
exit 0
"#
    )
}

/// The hook [`chain`] installs: it runs the original (same arguments, same
/// input), then does what [`script`] does, and exits with the original's status.
fn wrapper_script() -> String {
    format!(
        r#"#!/bin/sh
# devflow-managed-hook v2
{WRAPS_PREFIX} {ORIGINAL_NAME}
# Installed by `devflow hook install --chain`; `devflow hook uninstall` puts the
# original hook back. It runs {ORIGINAL_NAME} first, with the same arguments and
# input, then sets up worktrees created by other tools, and exits with the
# original hook's status.
status=0
dir=${{0%/*}}
[ "$dir" != "$0" ] || dir=.
original="$dir/{ORIGINAL_NAME}"
if [ -f "$original" ] && [ -x "$original" ]; then
  "$original" "$@"
  status=$?
fi
{SETUP_FUNCTION}devflow_setup "$@"
exit "$status"
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
    /// devflow's hook wrapping one somebody else wrote ([`chain`]); the
    /// original is kept at `original`.
    Wrapped {
        path: PathBuf,
        original: PathBuf,
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

fn original_of(hook: &Path) -> PathBuf {
    hook.with_file_name(ORIGINAL_NAME)
}

pub fn state(project: &Project) -> Result<InstallState> {
    let path = hooks_dir(project)?.join(HOOK_NAME);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            if !text.lines().any(|l| l.trim().starts_with(MARKER_PREFIX)) {
                Ok(InstallState::Foreign { path })
            } else if text.lines().any(|l| l.starts_with(WRAPS_PREFIX)) {
                let original = original_of(&path);
                Ok(InstallState::Wrapped { path, original })
            } else {
                Ok(InstallState::Installed { path })
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(InstallState::NotInstalled),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// A `post-checkout` hook that devflow did not write is in the way of
/// installing its own. It was left alone. What to do about it travels as
/// remedies ([`crate::remedy::remedies_of`]): keep it and run it first
/// ([`chain`]) or edit it by hand.
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
/// replace a foreign hook with a [`ForeignHook`] error; [`chain`] keeps it
/// and runs it first instead. A wrapper stays a wrapper (it is upgraded in
/// place).
pub fn install(project: &Project) -> Result<PathBuf> {
    crate::fsutil::with_lock(&hook_lock(project), || {
        let (path, text) = match state(project)? {
            InstallState::Foreign { path } => {
                return Err(ForeignHook {
                    root: project.root.clone(),
                    path,
                }
                .into());
            }
            InstallState::Wrapped { path, .. } => (path, wrapper_script()),
            InstallState::Installed { path } => (path, script()),
            InstallState::NotInstalled => (hooks_dir(project)?.join(HOOK_NAME), script()),
        };
        write_hook(&path, &text)?;
        Ok(path)
    })
}

/// A hook somebody else wrote, as it was when a person was shown it. [`chain`]
/// moves a hook aside only while it is still this file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookFingerprint {
    /// The hook.
    pub path: PathBuf,
    sha256: String,
    len: u64,
    mode: u32,
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn mode_of(_: &std::fs::Metadata) -> u32 {
    0
}

fn fingerprint_of(path: &Path) -> Result<HookFingerprint> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect {}", path.display()))?;
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(HookFingerprint {
        path: path.to_path_buf(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        len: bytes.len() as u64,
        mode: mode_of(&meta),
    })
}

/// Why a hook somebody else wrote cannot be wrapped: the refusal to hand to
/// the caller (message: the path and the reason, never the hook's contents).
fn refuse_hook(path: &Path, why: &str) -> anyhow::Error {
    use crate::remedy::{Problem, Remedy};
    Problem::new(format!("{} was left alone: {why}", path.display()))
        .with(Remedy::OpenFile {
            path: path.to_path_buf(),
        })
        .into_error()
}

/// A symbolic link is not followed: its target may be shared with other
/// repositories, and a hooks directory that is one may lead anywhere.
fn check_wrappable(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent()
        && std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(refuse_hook(
            path,
            "its folder is a symbolic link, which devflow does not follow",
        ));
    }
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect {}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(refuse_hook(
            path,
            "it is a symbolic link, which devflow does not follow",
        ));
    }
    if !meta.is_file() {
        return Err(refuse_hook(path, "it is not a regular file"));
    }
    Ok(())
}

/// The hook [`chain`] would move aside, as the person is about to be shown
/// it: `Some` only for a regular file somebody else wrote. `None` when there is
/// no such hook, and an error (the same refusal [`chain`] gives) for one that
/// cannot be wrapped.
pub fn foreign_fingerprint(project: &Project) -> Result<Option<HookFingerprint>> {
    match state(project)? {
        InstallState::Foreign { path } => {
            check_wrappable(&path)?;
            Ok(Some(fingerprint_of(&path)?))
        }
        _ => Ok(None),
    }
}

/// What [`chain`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Chained {
    /// The hook that runs on checkout.
    pub path: PathBuf,
    /// Where the person's hook is kept, when devflow's hook wraps one.
    pub original: Option<PathBuf>,
    /// `false` when devflow's hook was already there.
    pub added: bool,
}

/// Keep the `post-checkout` hook somebody else wrote and make devflow's hook
/// run it first. The original is moved aside to a sibling file (same inode,
/// so its mode and owner stay) and devflow's hook takes its place: it runs the
/// original with the same arguments and input, sets up the worktree a branch
/// checkout created, and exits with the original's status. The original is
/// never parsed or edited, so it can be written in any language;
/// [`uninstall`] puts it back.
///
/// `confirmed` is the hook as the person was shown it ([`foreign_fingerprint`]).
/// A hook that is not that file any more (its content or mode changed, or
/// `core.hooksPath` points elsewhere) is left alone, and so is one nobody
/// confirmed. Refused too: a symbolic link (or a hooks directory that is
/// one), a hook that is not a regular file, and a sibling
/// `post-checkout.devflow-original` that is already there. Messages name paths
/// and never contain hook contents. Repeating it changes nothing.
pub fn chain(project: &Project, confirmed: Option<&HookFingerprint>) -> Result<Chained> {
    crate::fsutil::with_lock(&hook_lock(project), || chain_locked(project, confirmed))
}

fn chain_locked(project: &Project, confirmed: Option<&HookFingerprint>) -> Result<Chained> {
    use crate::remedy::{Problem, Remedy};
    let path = match state(project)? {
        InstallState::Installed { path } => {
            return Ok(Chained {
                path,
                original: None,
                added: false,
            });
        }
        InstallState::Wrapped { path, original } => {
            return Ok(Chained {
                path,
                original: Some(original),
                added: false,
            });
        }
        InstallState::NotInstalled => {
            return Err(Problem::new(
                "there is no post-checkout hook to keep; install devflow's own",
            )
            .with(Remedy::InstallGitHook {
                root: project.root.clone(),
            })
            .into_error());
        }
        InstallState::Foreign { path } => path,
    };
    check_wrappable(&path)?;
    let aside = original_of(&path);
    if std::fs::symlink_metadata(&aside).is_ok() {
        return Err(refuse_hook(
            &path,
            &format!(
                "{} is already there, and devflow does not replace it",
                aside.display()
            ),
        ));
    }
    let Some(confirmed) = confirmed else {
        return Err(refuse_hook(
            &path,
            "nobody confirmed it, and devflow only moves a hook it was told to",
        ));
    };
    let changed = || refuse_hook(&path, "it changed after it was shown, so look at it again");
    if *confirmed != fingerprint_of(&path)? {
        return Err(changed());
    }
    // A hard link takes the name atomically and never replaces: the original
    // keeps its inode (mode, owner), and it is that inode which is checked
    // again before devflow's hook takes the old name.
    std::fs::hard_link(&path, &aside).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            refuse_hook(&path, "the place for the original hook was taken meanwhile")
        } else {
            anyhow::Error::new(e).context(format!("cannot move {} aside", path.display()))
        }
    })?;
    let settled = (|| -> Result<()> {
        let moved = HookFingerprint {
            path: path.clone(),
            ..fingerprint_of(&aside)?
        };
        if *confirmed != moved || !same_file(&path, &aside) {
            return Err(changed());
        }
        write_hook(&path, &wrapper_script())
    })();
    if let Err(error) = settled {
        let _ = std::fs::remove_file(&aside);
        return Err(error);
    }
    Ok(Chained {
        path,
        original: Some(aside),
        added: true,
    })
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(_: &Path, _: &Path) -> bool {
    true
}

/// What [`uninstall`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uninstalled {
    /// No hook of devflow's was there (a hook somebody else wrote is never touched).
    NothingToDo,
    /// devflow's hook was removed.
    Removed,
    /// devflow's hook wrapped one somebody else wrote: it is back in place.
    Restored,
}

/// Remove the hook only if devflow wrote it; when it wraps the person's hook
/// ([`chain`]), put that hook back (a rename, so its content, mode and owner
/// are exactly what they were).
pub fn uninstall(project: &Project) -> Result<Uninstalled> {
    crate::fsutil::with_lock(&hook_lock(project), || match state(project)? {
        InstallState::Installed { path } => {
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
            Ok(Uninstalled::Removed)
        }
        InstallState::Wrapped { path, original } => {
            if std::fs::symlink_metadata(&original).is_ok() {
                std::fs::rename(&original, &path)
                    .with_context(|| format!("cannot restore {}", path.display()))?;
                Ok(Uninstalled::Restored)
            } else {
                // The original is gone (somebody removed it): nothing to put back.
                std::fs::remove_file(&path)
                    .with_context(|| format!("cannot remove {}", path.display()))?;
                Ok(Uninstalled::Removed)
            }
        }
        InstallState::NotInstalled | InstallState::Foreign { .. } => Ok(Uninstalled::NothingToDo),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::worktree::testutil::{self, TestRepo};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;
    use std::process::{Command, Output, Stdio};

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

        assert_eq!(uninstall(&t.project).unwrap(), Uninstalled::Removed);
        assert!(!path.exists());
        assert_eq!(uninstall(&t.project).unwrap(), Uninstalled::NothingToDo);
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
    fn the_generated_hooks_run_under_set_u_with_nothing_set() {
        let t = testutil::repo(None);
        let hook = installed(&t);
        let fake = FakeDevflow::new();
        // `sh -u` makes every unset variable an error, and a hook may be run
        // with fewer arguments than Git passes.
        for script in [hook.clone(), {
            let wrapped = t.dir.path().join("wrapper");
            std::fs::write(&wrapped, wrapper_script()).unwrap();
            std::fs::write(
                t.dir.path().join("post-checkout.devflow-original"),
                "#!/bin/sh\nexit 0\n",
            )
            .unwrap();
            wrapped
        }] {
            let out = Command::new("sh")
                .arg("-u")
                .arg(&script)
                .env("PATH", fake.path_env(true))
                .env("FAKE_LOG", fake.log())
                .env_remove("DEVFLOW_SETUP_ACTIVE")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            let out = Command::new("sh")
                .arg("-u")
                .arg(&script)
                .args([ZERO40, "x", "1"])
                .env("PATH", fake.path_env(true))
                .env("FAKE_LOG", fake.log())
                .env_remove("DEVFLOW_SETUP_ACTIVE")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }
        assert_eq!(fake.calls().len(), 2, "{:?}", fake.calls());
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
        assert_eq!(uninstall(&t.project).unwrap(), Uninstalled::Removed);
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
        // The message is plain words; the way on travels as remedies.
        let message = format!("{error:#}");
        assert!(message.contains(path.to_str().unwrap()), "{message}");
        assert!(!message.contains("devflow setup"), "{message}");
        assert_eq!(
            crate::remedy::remedies_of(&error),
            [
                crate::remedy::Remedy::ChainGitHook {
                    root: t.root.clone()
                },
                crate::remedy::Remedy::OpenFile { path: path.clone() }
            ]
        );
        assert_eq!(uninstall(&t.project).unwrap(), Uninstalled::NothingToDo);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    fn foreign(t: &TestRepo, text: &str) -> PathBuf {
        let hooks = t.root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let path = hooks.join("post-checkout");
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
        path
    }

    /// Wrap the foreign hook the way a frontend does: look at it, then chain.
    fn wrap(t: &TestRepo) -> Chained {
        let seen = foreign_fingerprint(&t.project).unwrap();
        chain(&t.project, seen.as_ref()).unwrap()
    }

    #[test]
    fn chaining_keeps_the_original_byte_for_byte_and_uninstall_puts_it_back() {
        let t = testutil::repo(None);
        // No newline at the end, and not even valid UTF-8 further on: the
        // script is never read as text.
        let text: &[u8] = b"#!/bin/bash\nset -e\necho mine\n\xff\xfe";
        let path = foreign(&t, "");
        std::fs::write(&path, text).unwrap();
        let before = path.metadata().unwrap();
        let aside = path.with_file_name("post-checkout.devflow-original");

        let done = wrap(&t);
        assert_eq!(
            done,
            Chained {
                path: path.clone(),
                original: Some(aside.clone()),
                added: true
            }
        );
        // The person's file is the same file: bytes, mode and inode.
        assert_eq!(std::fs::read(&aside).unwrap(), text);
        let moved = aside.metadata().unwrap();
        assert_eq!(moved.ino(), before.ino());
        assert_eq!(moved.permissions().mode(), before.permissions().mode());
        // devflow's hook is in its place, and the state says it wraps.
        let wrapper = std::fs::read_to_string(&path).unwrap();
        assert_eq!(wrapper, wrapper_script());
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(
            state(&t.project).unwrap(),
            InstallState::Wrapped {
                path: path.clone(),
                original: aside.clone()
            }
        );

        // Idempotent, and reinstalling keeps the wrapper.
        let again = chain(&t.project, None).unwrap();
        assert_eq!(
            again,
            Chained {
                path: path.clone(),
                original: Some(aside.clone()),
                added: false
            }
        );
        assert_eq!(install(&t.project).unwrap(), path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), wrapper);
        assert_eq!(std::fs::read(&aside).unwrap(), text);

        // Uninstall restores the original exactly.
        assert_eq!(uninstall(&t.project).unwrap(), Uninstalled::Restored);
        assert_eq!(std::fs::read(&path).unwrap(), text);
        let restored = path.metadata().unwrap();
        assert_eq!(restored.ino(), before.ino());
        assert_eq!(restored.permissions().mode(), before.permissions().mode());
        assert!(!aside.exists());
        assert!(matches!(
            state(&t.project).unwrap(),
            InstallState::Foreign { .. }
        ));
        assert_eq!(uninstall(&t.project).unwrap(), Uninstalled::NothingToDo);
    }

    #[test]
    fn a_wrapper_whose_original_is_gone_is_removed_by_uninstall() {
        let t = testutil::repo(None);
        let path = foreign(&t, "#!/bin/sh\nexit 0\n");
        let done = wrap(&t);
        std::fs::remove_file(done.original.unwrap()).unwrap();
        assert_eq!(uninstall(&t.project).unwrap(), Uninstalled::Removed);
        assert!(!path.exists());
    }

    #[test]
    fn the_wrapper_exits_with_the_originals_status_after_setting_up() {
        let t = testutil::repo(None);
        let path = foreign(&t, "#!/bin/sh\necho mine\nexit \"${ORIGINAL_STATUS:-0}\"\n");
        wrap(&t);
        let fake = FakeDevflow::new();
        for (status, setup_fails) in [("0", false), ("1", false), ("3", true), ("0", true)] {
            let mut extra = vec![("ORIGINAL_STATUS", status)];
            if setup_fails {
                extra.push(("FAKE_FAIL", "1"));
            }
            let before = fake.calls().len();
            let out = run_hook(&path, [ZERO40, "x", "1"], &fake, &extra, true);
            assert_eq!(out.status.code(), status.parse::<i32>().ok(), "{out:?}");
            assert!(String::from_utf8_lossy(&out.stdout).contains("mine"));
            assert_eq!(fake.calls().len(), before + 1, "setup ran after the hook");
            assert_eq!(
                String::from_utf8_lossy(&out.stderr).contains("devflow setup"),
                setup_fails
            );
        }
        // Not a fresh branch checkout: the original still runs and decides.
        let before = fake.calls().len();
        let out = run_hook(
            &path,
            [ZERO40, "x", "0"],
            &fake,
            &[("ORIGINAL_STATUS", "5")],
            true,
        );
        assert_eq!(out.status.code(), Some(5));
        assert_eq!(fake.calls().len(), before);
    }

    #[test]
    fn the_wrapper_passes_arguments_and_input_to_the_original() {
        let t = testutil::repo(None);
        let out_file = t.dir.path().join("seen");
        let path = foreign(
            &t,
            "#!/bin/sh\nprintf '%s|' \"$@\" > \"$SEEN\"\ncat >> \"$SEEN\"\n",
        );
        wrap(&t);
        let fake = FakeDevflow::new();
        let mut child = Command::new(&path)
            .args([ZERO40, "new head", "1"])
            .env("PATH", fake.path_env(true))
            .env("FAKE_LOG", fake.log())
            .env("SEEN", &out_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write as _;
            child.stdin.take().unwrap().write_all(b"from git").unwrap();
        }
        assert!(child.wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(&out_file).unwrap(),
            format!("{ZERO40}|new head|1|from git")
        );
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn the_wrapper_works_with_an_original_that_is_not_a_shell_script() {
        let perl = Path::new("/usr/bin/perl");
        let t = testutil::repo(None);
        let out_file = t.dir.path().join("seen");
        // Perl where there is one; otherwise another interpreter's line that
        // a shell cannot read as shell.
        let script = if perl.exists() {
            "#!/usr/bin/perl\nopen(my $f, '>', $ENV{SEEN}) or die; print $f join('|', @ARGV); close($f);\nexit 4;\n"
        } else {
            "#!/usr/bin/env sh\nprintf '%s' \"$1|$2|$3\" > \"$SEEN\"\nexit 4\n"
        };
        let path = foreign(&t, script);
        wrap(&t);
        let fake = FakeDevflow::new();
        let out = run_hook(
            &path,
            [ZERO40, "head", "1"],
            &fake,
            &[("SEEN", out_file.to_str().unwrap())],
            true,
        );
        assert_eq!(out.status.code(), Some(4), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(&out_file).unwrap(),
            format!("{ZERO40}|head|1")
        );
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn chaining_goes_through_a_hooks_path_and_leaves_devflows_own_hook_alone() {
        let t = testutil::repo(None);
        // Nothing there: install is the remedy, and nothing is written.
        let error = chain(&t.project, None).unwrap_err();
        assert_eq!(
            crate::remedy::remedies_of(&error),
            [crate::remedy::Remedy::InstallGitHook {
                root: t.root.clone()
            }]
        );
        assert!(!t.root.join(".git/hooks/post-checkout").exists());
        // devflow's own hook is there already.
        let own = installed(&t);
        let text = std::fs::read_to_string(&own).unwrap();
        assert_eq!(
            chain(&t.project, None).unwrap(),
            Chained {
                path: own.clone(),
                original: None,
                added: false
            }
        );
        assert_eq!(std::fs::read_to_string(&own).unwrap(), text);
        assert!(
            !own.with_file_name("post-checkout.devflow-original")
                .exists()
        );
        assert_eq!(foreign_fingerprint(&t.project).unwrap(), None);

        // Under a hooks path, the original is kept in the same folder.
        std::fs::remove_file(&own).unwrap();
        t.project
            .repo()
            .unwrap()
            .config()
            .unwrap()
            .set_str("core.hooksPath", "tools/hooks")
            .unwrap();
        let dir = t.root.join("tools/hooks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("post-checkout"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            dir.join("post-checkout"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let done = wrap(&t);
        assert_eq!(done.path, dir.join("post-checkout"));
        assert_eq!(
            done.original.as_deref(),
            Some(dir.join("post-checkout.devflow-original").as_path())
        );
    }

    #[test]
    fn chaining_refuses_what_it_should_not_touch_and_never_quotes_the_hook() {
        let t = testutil::repo(None);
        // A hook that carries a credential, as real ones do.
        let secret = "#!/bin/sh\nnotify --password hunter2-secret\n";
        let refused = |what: &str, error: anyhow::Error| {
            let message = format!("{error:#}");
            assert!(message.contains("post-checkout"), "{what}: {message}");
            assert!(!message.contains("hunter2"), "{what}: {message}");
            assert!(!message.contains("notify"), "{what}: {message}");
            assert!(
                matches!(
                    crate::remedy::remedies_of(&error)[..],
                    [crate::remedy::Remedy::OpenFile { .. }]
                ),
                "{what}: {message}"
            );
        };

        // Nobody confirmed it.
        let path = foreign(&t, secret);
        refused("unconfirmed", chain(&t.project, None).unwrap_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), secret);

        // It changed after it was shown (content, then only mode).
        let seen = foreign_fingerprint(&t.project).unwrap().unwrap();
        std::fs::write(&path, format!("{secret}echo more\n")).unwrap();
        refused("content", chain(&t.project, Some(&seen)).unwrap_err());
        std::fs::write(&path, secret).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        refused("mode", chain(&t.project, Some(&seen)).unwrap_err());
        // Shown another hook (core.hooksPath moved): not this one.
        let elsewhere = HookFingerprint {
            path: t.dir.path().join("other/post-checkout"),
            ..seen.clone()
        };
        refused("path", chain(&t.project, Some(&elsewhere)).unwrap_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), secret);
        assert!(
            !path
                .with_file_name("post-checkout.devflow-original")
                .exists()
        );
        assert!(matches!(
            state(&t.project).unwrap(),
            InstallState::Foreign { .. }
        ));

        // The place for the original is taken.
        let seen = foreign_fingerprint(&t.project).unwrap().unwrap();
        let taken = path.with_file_name("post-checkout.devflow-original");
        std::fs::write(&taken, "somebody else's").unwrap();
        refused("taken", chain(&t.project, Some(&seen)).unwrap_err());
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "somebody else's");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), secret);
        std::fs::remove_file(&taken).unwrap();

        // A symbolic link: its target may be shared, so it is not touched.
        let target = t.dir.path().join("shared-hook");
        std::fs::write(&target, secret).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        refused("link", foreign_fingerprint(&t.project).unwrap_err());
        refused("link", chain(&t.project, Some(&seen)).unwrap_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), secret);
        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(!taken.exists());

        // A hooks folder that is a symbolic link.
        std::fs::remove_file(&path).unwrap();
        let hooks = t.root.join(".git/hooks");
        let real = t.dir.path().join("real-hooks");
        std::fs::rename(&hooks, &real).unwrap();
        std::os::unix::fs::symlink(&real, &hooks).unwrap();
        std::fs::write(real.join("post-checkout"), secret).unwrap();
        refused("folder link", foreign_fingerprint(&t.project).unwrap_err());
        refused("folder link", chain(&t.project, Some(&seen)).unwrap_err());
        assert_eq!(
            std::fs::read_to_string(real.join("post-checkout")).unwrap(),
            secret
        );
        assert!(!real.join("post-checkout.devflow-original").exists());
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

    #[test]
    fn git_runs_the_original_and_then_devflow_through_the_wrapper() {
        let t = testutil::repo(None);
        let ran = t.dir.path().join("original-ran");
        foreign(
            &t,
            &format!("#!/bin/sh\necho \"$3\" >> '{}'\n", ran.display()),
        );
        wrap(&t);
        let fake = FakeDevflow::new();
        let target = t.dir.path().join("by-git");
        let out = Command::new("git")
            .args(["worktree", "add", "-b", "by-git", target.to_str().unwrap()])
            .current_dir(&t.root)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", fake.dir.path().display()),
            )
            .env("FAKE_LOG", fake.log())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(std::fs::read_to_string(&ran).unwrap().trim(), "1");
        assert_eq!(fake.calls().len(), 1, "{:?}", fake.calls());
    }
}
