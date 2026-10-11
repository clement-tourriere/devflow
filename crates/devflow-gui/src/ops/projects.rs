//! Mending the list of projects from the app: forget one, forget the ones
//! whose folder was not found, point one at the folder it moved to, and finish a
//! configuration migration. Each is a core call (`registry`,
//! `config_edit::migrate`) on the background executor; nothing here knows how
//! a project is repaired or shells out to the CLI.
use crate::{jobs, state::AppState};
use devflow_core::registry::Entry;
use gpui_kit::{
    AppContext as _, Context, PathPromptOptions, Window,
    component::{WindowExt as _, notification::Notification},
};
use std::path::PathBuf;
use uuid::Uuid;

impl AppState {
    /// Take a project off the list. Its folder and data are not touched.
    pub fn forget_project(&mut self, entry: Entry, window: &mut Window, cx: &mut Context<Self>) {
        if entry.id.is_nil() {
            // A folder that was only being looked at: nothing is registered.
            self.remove_entry(entry, cx);
            return;
        }
        let (root, name) = (entry.root.clone(), entry.name.clone());
        self.spawn_light(
            window,
            cx,
            move || jobs::forget_project(root),
            move |this, result, window, cx| match result {
                Ok(()) => {
                    window.push_notification(
                        Notification::success(format!(
                            "'{name}' is no longer in the list. Its folder and data were not touched."
                        ))
                        .title("Remove from list"),
                        cx,
                    );
                    this.reload_entries(None, None, cx);
                }
                Err(message) => Self::notify_error(window, cx, "Remove from list", &message),
            },
        );
    }

    /// Take the `confirmed` projects whose folder is still not found off the
    /// list: the ones the person was shown, and no others.
    pub fn forget_missing_projects(
        &mut self,
        confirmed: Vec<Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let asked = confirmed.len();
        self.spawn_light(
            window,
            cx,
            move || jobs::forget_missing_projects(confirmed),
            move |this, result, window, cx| match result {
                Ok(removed) => {
                    let mut text = format!(
                        "Removed {} from the list. Nothing on disk was touched.",
                        devflow_core::text::counted(removed.len() as u64, "project")
                    );
                    if removed.len() < asked {
                        text.push_str(
                            " The others were kept: their folder was found again or they changed.",
                        );
                    }
                    window.push_notification(
                        Notification::success(text).title("Remove unavailable projects"),
                        cx,
                    );
                    this.reload_entries(None, None, cx);
                }
                Err(message) => {
                    Self::notify_error(window, cx, "Remove unavailable projects", &message)
                }
            },
        );
    }

    /// Look again at one project that could not be opened (recompute its
    /// health from the disk), whichever project is selected. The selected
    /// project is read from scratch; for another one only its mark in the list
    /// changes, and the selection stays where it is.
    pub fn retry_project(&mut self, entry: Entry, cx: &mut Context<Self>) {
        if entry.id.is_nil() {
            // A folder that is only being looked at has no health to recompute.
            if self
                .selection
                .current()
                .is_some_and(|m| m.entry.root == entry.root)
            {
                self.refresh(cx);
            }
            return;
        }
        let id = entry.id;
        cx.spawn(async move |this, cx| {
            let checked = cx
                .background_spawn(async move { jobs::check_projects(&[entry]) })
                .await;
            this.update(cx, |this, cx| {
                match checked.into_iter().find(|(key, _)| *key == id) {
                    Some((_, problem)) => {
                        this.problems.insert(id, problem);
                    }
                    None => {
                        this.problems.remove(&id);
                    }
                }
                if this
                    .selection
                    .current()
                    .is_some_and(|m| m.key.project == id)
                {
                    this.project_changed(id, cx);
                } else {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Ask for the folder a project moved to, then point the project at it.
    pub fn locate_project(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Locate project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.relocate_project(id, path, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// Point the listed project `id` at `new_root` (core checks that the
    /// folder holds that very project).
    pub fn relocate_project(
        &mut self,
        id: Uuid,
        new_root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.spawn_light(
            window,
            cx,
            move || jobs::relocate_project(id, new_root),
            |this, result, window, cx| match result {
                Ok(entry) => {
                    window.push_notification(
                        Notification::success(format!(
                            "'{}' is now at {}.",
                            entry.name,
                            crate::model::shorten_home(&entry.root)
                        ))
                        .title("Locate project"),
                        cx,
                    );
                    this.project_changed(entry.id, cx);
                }
                Err(message) => Self::notify_error(window, cx, "Locate project", &message),
            },
        );
    }

    /// A project's place or configuration changed on disk: read the list and
    /// the project again, from scratch.
    pub fn project_changed(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self
            .selection
            .current()
            .is_some_and(|m| m.key.project == id)
        {
            // Its model holds the old entry; selecting it again reloads it.
            self.selection.clear();
        }
        self.summaries.remove(&id);
        self.reload_entries(Some(id), None, cx);
    }
}
