//! Typed, format-preserving edits of `devflow.toml` (or `devflow.local.toml`).
//!
//! Where [`crate::propose`] adds a first configuration, this module changes
//! the one that exists: add, update, remove and move a hook, set the
//! `[hooks]` timeout, set or unset an `[env]` recipe, add or remove a preset.
//! Frontends (the CLI's `hooks` and `env` commands, the desktop app's Setup
//! tab) build an [`Edit`] and call [`apply`]; none of them touches TOML text.
//!
//! # Guarantees
//!
//! - **Format preserving.** The file is edited with `toml_edit`: comments,
//!   ordering, spacing and the entries nobody touched stay as they were.
//!   A hook is written as a string when only `run` is set, and as a
//!   `{ name, run, timeout }` table otherwise.
//! - **Validated before it is written.** The edited text is merged with the
//!   other configuration file and parsed the way [`Config::load`] reads it; an
//!   edit that would make the configuration invalid (a duplicate hook name, a
//!   recipe that clashes with a preset, a bad timeout) is refused and nothing
//!   is written.
//! - **Never over a newer edit.** The caller names the content hash of the
//!   file it looked at ([`FileState::hash`]); if the file differs when the
//!   edit is read, nothing is written and the error is
//!   [`ConfigChanged`](crate::error::ConfigChanged). The check is repeated
//!   at the moment of the write, which can come long after (validating and
//!   computing the trust scope take time on a large project): the file is
//!   replaced through `fsutil::replace_if_unchanged` with the hash the edit
//!   was computed from, so a save that lands in between is neither
//!   overwritten nor lost. A version that had to be moved aside is named in
//!   the error (`ConfigChanged::kept`).
//! - **Atomic.** The new content is staged in a synced temporary sibling and
//!   swapped in; the file is never absent or half-written.
//! - **Quiet about values.** An error names the key path (`hooks.setup[2]`,
//!   `env.DIRECT_URL`), never the command, the recipe or the name it
//!   complains about: configuration may hold credentials.
//! - **Views are masked.** A recipe may be a credential (a literal in
//!   `devflow.local.toml`), so [`RecipeRow`] and [`PreviewRow`] carry text
//!   already masked by [`masked_template`] and [`masked_value`], which decide
//!   by the variable's *name* as well as by the text, plus a `masked` flag. The
//!   real text is behind `reveal()`, for an editor that writes it back or a
//!   person who asked to see it (`devflow env list --show-values`); it is not
//!   serialized and not in `Debug`.
//!
//! # Local overrides
//!
//! `devflow.local.toml` is merged on top of the project file. A table merges
//! key by key, but an *array* replaces: a local `hooks.setup` replaces the
//! committed one whole, and so does a local `env.presets`. Editing such a list
//! in the local file therefore starts from a copy of the committed list (the
//! result says so in [`Applied::notes`]), and editing the committed file while
//! the local file overrides the list is allowed but reported, because it has
//! no effect on this machine. For the same reason a local list that loses its
//! last entry stays in the file as `[]`: without the key the committed list
//! would apply again. (For `env.presets` the empty key is dropped when the
//! committed file has no presets to hide.)
mod env;
mod hooks;
mod mask;
pub mod migrate;
#[cfg(test)]
mod tests;
mod text;
mod views;

pub use env::{
    EnvPreview, PreviewOrigin, PreviewRow, available_references, preset_description, preset_label,
    preview_env,
};
pub(crate) use mask::has_literal_text;
pub use mask::{masked_template, masked_value};
pub use views::{EnvView, HookRow, HooksView, PhaseView, PresetRow, RecipeRow};

use crate::config::{Config, EnvPreset, HookEntry, LOCAL_CONFIG_FILE};
use crate::error::{ConfigChanged, NotInitialized};
use crate::fsutil::ReplaceError;
use crate::hooks::HookPhase;
use crate::scope::Scope;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

/// Which file an edit changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// `devflow.toml` (or `.devflow.toml`): committed, shared by the team.
    Project,
    /// `devflow.local.toml`: this machine only, merged on top, not committed.
    Local,
}

/// How to find an existing hook of a phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookRef {
    /// The entry with this `name`.
    Name(String),
    /// The entry at this position, counting from 0.
    Index(usize),
}

impl HookRef {
    /// A reference as a person types it, resolved against the hooks of a
    /// phase: an exact name first (so a hook called `2` stays reachable by
    /// name), then a 1-based position.
    pub fn resolve_text(text: &str, hooks: &[HookEntry]) -> Self {
        if hooks.iter().any(|h| h.name.as_deref() == Some(text)) {
            return Self::Name(text.to_string());
        }
        match text.parse::<usize>() {
            Ok(position) if position >= 1 => Self::Index(position - 1),
            _ => Self::Name(text.to_string()),
        }
    }
}

/// Where a hook goes in its phase's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    End,
    Start,
    Before(HookRef),
    After(HookRef),
    /// The position the entry ends up at, counting from 0 (clamped to the end).
    Index(usize),
}

/// One change to a configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    AddHook {
        phase: HookPhase,
        entry: HookEntry,
        at: Placement,
    },
    /// Replace an entry with `entry` (all of its fields).
    UpdateHook {
        phase: HookPhase,
        hook: HookRef,
        entry: HookEntry,
    },
    RemoveHook {
        phase: HookPhase,
        hook: HookRef,
    },
    MoveHook {
        phase: HookPhase,
        hook: HookRef,
        to: Placement,
    },
    /// `[hooks] timeout`; `None` removes the key (the default applies).
    SetHooksTimeout(Option<String>),
    /// `[env] KEY = "value"`.
    SetEnv {
        key: String,
        value: String,
    },
    UnsetEnv {
        key: String,
    },
    AddPreset(EnvPreset),
    RemovePreset(EnvPreset),
}

/// A configuration file as it is on disk.
#[derive(Clone)]
pub struct FileState {
    pub path: PathBuf,
    /// `None` when the file does not exist. `devflow.local.toml` may hold
    /// credentials: this is for editing, not for display (`Debug` leaves it out).
    pub text: Option<String>,
    /// SHA-256 of `text`; the value to pass as `expected` to [`apply`].
    pub hash: Option<String>,
}

impl std::fmt::Debug for FileState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileState")
            .field("path", &self.path)
            .field("bytes", &self.text.as_ref().map(String::len))
            .field("hash", &self.hash)
            .finish()
    }
}

/// The project's two configuration files.
#[derive(Debug, Clone)]
pub struct ConfigFiles {
    pub root: PathBuf,
    pub project: FileState,
    pub local: FileState,
}

impl ConfigFiles {
    /// Read both files of the project rooted at `root`. Fails with
    /// [`NotInitialized`] when there is no project file.
    pub fn read(root: &Path) -> Result<Self> {
        let project_path = Config::find_file(root).ok_or_else(|| {
            anyhow::Error::new(NotInitialized {
                root: root.to_path_buf(),
            })
        })?;
        Ok(Self {
            root: root.to_path_buf(),
            project: read_state(project_path)?,
            local: read_state(root.join(LOCAL_CONFIG_FILE))?,
        })
    }

    pub fn file(&self, target: Target) -> &FileState {
        match target {
            Target::Project => &self.project,
            Target::Local => &self.local,
        }
    }

    /// The merged configuration, the way [`Config::load`] reads it.
    pub fn config(&self) -> Result<Config> {
        layers(&self.project, self.local.text.as_deref())
    }
}

fn read_state(path: PathBuf) -> Result<FileState> {
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(FileState {
            hash: Some(crate::propose::sha256_hex(&text)),
            text: Some(text),
            path,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileState {
            path,
            text: None,
            hash: None,
        }),
        Err(_) => bail!("cannot read {}", path.display()),
    }
}

fn layers(project: &FileState, local: Option<&str>) -> Result<Config> {
    let project_text = project
        .text
        .as_deref()
        .context("the project file does not exist")?;
    let base_name = file_name(&project.path);
    Config::parse_layers(project_text, &base_name, local, LOCAL_CONFIG_FILE)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// What an edit did.
#[derive(Clone)]
pub struct Applied {
    pub target: Target,
    pub path: PathBuf,
    /// The file's text was changed (an edit whose effect is already there
    /// changes nothing and writes nothing).
    pub changed: bool,
    /// SHA-256 of the file before and after; `after` is the next `expected`.
    pub before: Option<String>,
    pub after: String,
    /// The new content of the file, as written (a local file may hold
    /// credentials: for editing, not for display; `Debug` leaves it out).
    pub text: String,
    /// Things worth telling the person: an override, a copied list.
    pub notes: Vec<String>,
    /// What an approval covers (hooks or `[env]`) is different now: the new
    /// configuration needs a review before it runs (`devflow trust`).
    pub scope_changed: bool,
}

impl std::fmt::Debug for Applied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Applied")
            .field("target", &self.target)
            .field("path", &self.path)
            .field("changed", &self.changed)
            .field("before", &self.before)
            .field("after", &self.after)
            .field("notes", &self.notes)
            .field("scope_changed", &self.scope_changed)
            .finish_non_exhaustive()
    }
}

/// Apply `edit` to `target` and write the file.
///
/// `expected` is the hash of the file the caller looked at
/// ([`FileState::hash`]; `None` for a file that did not exist). If the file
/// is different now, the error is [`ConfigChanged`] and nothing is written.
pub fn apply(root: &Path, target: Target, expected: Option<&str>, edit: &Edit) -> Result<Applied> {
    run(root, target, expected, edit, true)
}

/// [`apply`] without writing: the result a write would have.
pub fn preview(
    root: &Path,
    target: Target,
    expected: Option<&str>,
    edit: &Edit,
) -> Result<Applied> {
    run(root, target, expected, edit, false)
}

fn run(
    root: &Path,
    target: Target,
    expected: Option<&str>,
    edit: &Edit,
    write: bool,
) -> Result<Applied> {
    edit.check_input(target)?;
    // Two editors at once (two terminals, the app) must not interleave their
    // read-check-write.
    let go = || run_locked(root, target, expected, edit, write);
    match git2::Repository::open(root) {
        Ok(repo) => {
            crate::fsutil::with_lock(&repo.commondir().join("devflow/config-edit.lock"), go)
        }
        Err(_) => go(),
    }
}

fn run_locked(
    root: &Path,
    target: Target,
    expected: Option<&str>,
    edit: &Edit,
    write: bool,
) -> Result<Applied> {
    let files = ConfigFiles::read(root)?;
    let file = files.file(target);
    if file.hash.as_deref() != expected {
        return Err(ConfigChanged::new(file.path.clone()).into());
    }
    if std::fs::symlink_metadata(&file.path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!(
            "{} is a symbolic link; devflow will not rewrite it",
            file_name(&file.path)
        );
    }
    let before = layers(&files.project, files.local.text.as_deref()).map_err(|error| {
        // The message is the configuration parser's, which names a key and
        // never repeats a value.
        error.context("the configuration is not valid as it is; fix it before editing it here")
    })?;

    let mut ctx = text::Context::new(target, &files);
    let mut doc: DocumentMut = file
        .text
        .as_deref()
        .unwrap_or("")
        .parse()
        .map_err(|_| anyhow::anyhow!("{} is not valid TOML", file_name(&file.path)))?;
    edit.apply_to(&mut doc, &mut ctx)?;
    let new_text = text::finish(&doc, file.text.as_deref());
    let changed = file.text.as_deref() != Some(new_text.as_str());
    let new_hash = crate::propose::sha256_hex(&new_text);

    // The edited text, merged with the other file, must be a valid
    // configuration.
    let (project_text, local_text) = match target {
        Target::Project => (new_text.clone(), files.local.text.clone()),
        Target::Local => (
            files.project.text.clone().unwrap_or_default(),
            Some(new_text.clone()),
        ),
    };
    let after = Config::parse_layers(
        &project_text,
        &file_name(&files.project.path),
        local_text.as_deref(),
        LOCAL_CONFIG_FILE,
    )
    .context("the edit would leave an invalid configuration; nothing was written")?;

    let scope_changed = {
        let (a, b) = (Scope::build(&before, root), Scope::build(&after, root));
        a.hooks != b.hooks || a.env != b.env
    };
    if changed && write {
        // `expected` is the hash `new_text` was computed from: a save that
        // landed since (validation and the scope can be slow) must not be
        // overwritten.
        write_unless_changed(&file.path, &new_text, file.hash.as_deref())?;
    }
    Ok(Applied {
        target,
        path: file.path.clone(),
        changed,
        before: file.hash.clone(),
        after: new_hash,
        text: new_text,
        notes: ctx.notes,
        scope_changed,
    })
}

/// Replace `path` with `text` unless it no longer holds what `expected` (its
/// hash, `None` for a file that did not exist) says it held. A conflict is the
/// typed [`ConfigChanged`], naming the file a displaced save was kept in.
fn write_unless_changed(path: &Path, text: &str, expected: Option<&str>) -> Result<()> {
    #[cfg(test)]
    seams::before_write();
    #[cfg(test)]
    let result = seams::replace(path, text, expected);
    #[cfg(not(test))]
    let result = crate::fsutil::replace_if_unchanged(path, text.as_bytes(), false, expected);
    match result {
        Ok(()) => Ok(()),
        Err(ReplaceError::Failed(error)) => Err(error),
        Err(ReplaceError::Conflict(conflict)) => Err(ConfigChanged {
            path: conflict.path,
            kept: conflict.kept,
        }
        .into()),
    }
}

/// Test-only seams: acting at the moments a concurrent save would land.
#[cfg(test)]
pub(crate) mod seams {
    use crate::fsutil::{ReplaceError, ReplaceSeams, replace_if_unchanged_with};
    use std::cell::RefCell;
    use std::path::Path;

    /// What runs where; set per thread (an edit runs on the caller's thread).
    #[derive(Default)]
    pub(crate) struct Hooks {
        /// After the edit was validated, before the write begins.
        pub before_write: Option<Box<dyn Fn()>>,
        /// After the new content is staged, before it is swapped in.
        pub after_staging: Option<Box<dyn Fn()>>,
        /// Right after the swap, while the displaced content is aside.
        pub after_exchange: Option<Box<dyn Fn()>>,
    }

    thread_local! {
        static HOOKS: RefCell<Hooks> = RefCell::new(Hooks::default());
    }

    /// Install `hooks` for the current thread until the guard is dropped.
    pub(crate) fn install(hooks: Hooks) -> Guard {
        HOOKS.with(|slot| *slot.borrow_mut() = hooks);
        Guard
    }

    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOKS.with(|slot| *slot.borrow_mut() = Hooks::default());
        }
    }

    pub(super) fn before_write() {
        HOOKS.with(|slot| {
            if let Some(hook) = &slot.borrow().before_write {
                hook();
            }
        });
    }

    pub(super) fn replace(
        path: &Path,
        text: &str,
        expected: Option<&str>,
    ) -> Result<(), ReplaceError> {
        HOOKS.with(|slot| {
            let hooks = slot.borrow();
            let seams = ReplaceSeams {
                after_staging: hooks.after_staging.as_deref(),
                after_exchange: hooks.after_exchange.as_deref(),
                ..ReplaceSeams::default()
            };
            replace_if_unchanged_with(path, text.as_bytes(), false, expected, &seams)
        })
    }
}
