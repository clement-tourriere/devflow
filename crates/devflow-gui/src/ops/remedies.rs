//! The actions behind remedies that had no button of their own: install the Git
//! hook, finish interrupted PostgreSQL work or drop the databases nothing
//! uses, forget the registrations of worktrees whose folder is gone, and add
//! an `[env]` preset. Each calls the core function the CLI calls (`githook`,
//! `db::Server`, `worktree`, `config_edit`); nothing here shells out.
//!
//! A remedy names its project by root. The project must be the open one: the
//! selection may have moved since the button was drawn, and an action never
//! lands on a project it was not about.
use super::{Done, FailureHint, fresh_server_of};
use crate::{jobs, state::AppState};
use devflow_core::setup::Progress as _;
use devflow_core::{
    config::EnvPreset,
    config_edit::{self, ConfigFiles, Edit, Target},
    githook, worktree,
};
use gpui_kit::{
    Context, Window,
    component::{WindowExt as _, notification::Notification},
};
use std::path::{Path, PathBuf};

impl AppState {
    /// Whether the open project is the one at `root`; otherwise says so.
    pub(crate) fn open_project_at(
        &self,
        root: &Path,
        window: &mut Window,
        cx: &mut gpui_kit::App,
        title: &str,
    ) -> bool {
        let open = self
            .selection
            .current()
            .is_some_and(|m| m.entry.root == root && m.project.is_some());
        if !open {
            Self::notify_error(
                window,
                cx,
                title,
                "That is about a project that is not the open one. Open it, then try again.",
            );
        }
        open
    }

    /// Install the post-checkout Git hook that sets up worktrees other tools
    /// create.
    pub fn install_git_hook(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let title = "Install the Git hook";
        if !self.open_project_at(&root, window, cx, title) {
            return;
        }
        // Shown as an activity: a hook somebody else wrote is refused there,
        // with the way on as buttons under the message.
        self.start_job(
            title.into(),
            true,
            FailureHint::None,
            window,
            cx,
            |project, _| {
                let path = githook::install(&project)?;
                Ok(Done {
                    summary: format!(
                        "Worktrees other tools create are set up by the hook at {}",
                        crate::model::shorten_home(&path)
                    ),
                    ..Done::default()
                })
            },
        );
    }

    /// Finish interrupted PostgreSQL work; with `drop_orphans`, also drop the
    /// scratch databases the configuration no longer lists (by recorded
    /// identity, only the ones reconcile reported).
    pub fn reconcile_databases(
        &mut self,
        root: PathBuf,
        drop_orphans: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = if drop_orphans {
            "Drop unused databases"
        } else {
            "Finish interrupted database work"
        };
        if !self.open_project_at(&root, window, cx, title) {
            return;
        }
        self.start_job(
            title.into(),
            true,
            FailureHint::None,
            window,
            cx,
            move |project, progress| {
                let server = fresh_server_of(&project)?;
                progress.output("Looking at the records of interrupted work…");
                let mut report = server.reconcile()?;
                if drop_orphans {
                    report.actions.extend(server.drop_orphans()?);
                    report.orphans.clear();
                }
                for action in &report.actions {
                    progress.output(&format!("- {action}"));
                }
                let needs_attention = !report.problems.is_empty();
                Ok(Done {
                    summary: if report.actions.is_empty() {
                        "Nothing to finish".to_string()
                    } else {
                        format!("Settled {} item(s)", report.actions.len())
                    },
                    warnings: report.problems,
                    needs_attention,
                    ..Done::default()
                })
            },
        );
    }

    /// Look at the worktrees whose folder is gone (read-only) and ask before
    /// forgetting any: the dialog lists each registration that would be
    /// forgotten and each one that is kept, with why. A worktree on a drive
    /// that is not connected looks the same as a deleted one, and a
    /// registration holds the receipt of the workspace's data.
    pub fn review_missing_worktrees(
        &mut self,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = "Review missing worktrees";
        if !self.open_project_at(&root, window, cx, title) {
            return;
        }
        let Some(project) = self
            .selection
            .current()
            .and_then(|model| model.project.clone())
        else {
            return;
        };
        self.spawn_light(
            window,
            cx,
            move || jobs::catch(|| worktree::prune_candidates(&project)),
            move |_, result, window, cx| match result {
                Err(message) => Self::notify_error(window, cx, title, &message),
                Ok(candidates) => {
                    let (forgettable, kept): (Vec<_>, Vec<_>) =
                        candidates.into_iter().partition(|c| c.refusal.is_none());
                    if forgettable.is_empty() {
                        let text = crate::views::remedies::missing_worktrees_note(&kept);
                        window.push_notification(
                            if kept.is_empty() {
                                Notification::success(text)
                            } else {
                                Notification::warning(text)
                            }
                            .title(title),
                            cx,
                        );
                        return;
                    }
                    let app = cx.entity();
                    crate::views::remedies::open_forget_missing(
                        &app,
                        root,
                        &forgettable,
                        &kept,
                        window,
                        cx,
                    );
                }
            },
        );
    }

    /// Forget the registrations the person confirmed, and no others. Core
    /// looks at each again under the workspace's locks and keeps what owns
    /// data, what is busy and what is on a folder that is gone too.
    pub fn prune_worktrees(
        &mut self,
        root: PathBuf,
        targets: Vec<worktree::PruneTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = "Forget missing worktrees";
        if !self.open_project_at(&root, window, cx, title) {
            return;
        }
        self.start_job(
            title.into(),
            true,
            FailureHint::None,
            window,
            cx,
            move |project, _| {
                let outcome = worktree::prune(&project, &targets)?;
                let mut warnings: Vec<String> = outcome
                    .refused
                    .iter()
                    .map(|r| {
                        format!(
                            "kept {} at {}: {}",
                            r.name,
                            crate::model::shorten_home(&r.path),
                            r.refusal.message()
                        )
                    })
                    .collect();
                warnings.extend(outcome.changed.iter().map(|id| {
                    format!("{id} is not a missing worktree any more; it was left alone")
                }));
                Ok(Done {
                    summary: if outcome.pruned.is_empty() {
                        "No registration was forgotten".to_string()
                    } else {
                        format!(
                            "Forgot the registration of {}. Their branches are kept.",
                            outcome.pruned.join(", ")
                        )
                    },
                    needs_attention: !outcome.refused.is_empty(),
                    warnings,
                    ..Done::default()
                })
            },
        );
    }

    /// Name `preset` in the project's `[env]` (the committed file), as the
    /// Setup tab's checkbox does.
    pub fn add_env_preset(
        &mut self,
        root: PathBuf,
        preset: EnvPreset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = "Add the preset";
        if !self.open_project_at(&root, window, cx, title) {
            return;
        }
        self.spawn_light(
            window,
            cx,
            move || {
                jobs::catch(|| {
                    let files = ConfigFiles::read(&root)?;
                    let expected = files.file(Target::Project).hash.clone();
                    config_edit::apply(
                        &root,
                        Target::Project,
                        expected.as_deref(),
                        &Edit::AddPreset(preset),
                    )
                    .map(|_| ())
                })
            },
            move |this, result, window, cx| match result {
                Ok(()) => {
                    window.push_notification(
                        Notification::success(format!("Added the {} preset.", preset.as_str()))
                            .title(title),
                        cx,
                    );
                    this.refresh(cx);
                }
                Err(message) => Self::notify_error(window, cx, title, &message),
            },
        );
    }
}
