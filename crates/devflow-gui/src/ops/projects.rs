//! Mending the list of projects from the app: forget one, forget the ones
//! whose folder is gone, point one at the folder it moved to, and finish a
//! configuration migration. Each is a core call (`registry`,
//! `config_edit::migrate`) on the background executor; nothing here knows how
//! a project is repaired or shells out to the CLI.
use crate::{jobs, state::AppState};
use devflow_core::registry::Entry;
use gpui_kit::{
    Context, PathPromptOptions, Window,
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

    /// Take every project whose folder no longer exists off the list.
    pub fn forget_missing_projects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.spawn_light(
            window,
            cx,
            jobs::forget_missing_projects,
            |this, result, window, cx| match result {
                Ok(removed) => {
                    window.push_notification(
                        Notification::success(format!(
                            "Removed {} from the list. Nothing on disk was touched.",
                            devflow_core::text::counted(removed.len() as u64, "project")
                        ))
                        .title("Remove missing projects"),
                        cx,
                    );
                    this.reload_entries(None, None, cx);
                }
                Err(message) => Self::notify_error(window, cx, "Remove missing projects", &message),
            },
        );
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
