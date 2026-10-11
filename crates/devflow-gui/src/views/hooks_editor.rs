//! The Setup tab: the project's hooks and `[env]` as forms, not as TOML.
//!
//! Hooks are three short lists (what runs when a workspace is created, when
//! you switch into it, and before it is removed); the environment is the
//! framework presets and the `NAME = value` recipes written to
//! `.devflow/env`. Every change goes through `devflow_core::config_edit`: the
//! file keeps its comments and order, the result is validated before it is
//! written, and a file that changed since this view read it is never
//! overwritten. This module draws forms and calls core; it decides nothing.
use super::{
    onboarding,
    ui::{self, Tone},
    widgets::{banner, card, chip, empty_row, hoverable, mono, section_header},
};
use crate::{
    jobs::{self, FailureKind},
    model::shorten_home,
    state::AppState,
};
use devflow_core::{
    config::{EnvPreset, HookEntry},
    config_edit::{
        self, Applied, ConfigFiles, Edit, EnvPreview, EnvView, HookRef, HooksView, Placement,
        RecipeRow, Target,
    },
    hooks::HookPhase,
    project::Project,
    propose::{self, ItemKind, PatchOp, PlanOptions, Proposal},
    redact,
    trust::{self, TrustState},
    worktree,
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        h_flex,
        input::{Input, InputEvent, InputState, Textarea, TextareaState},
        notification::Notification,
        radio::{Radio, RadioGroup},
        v_flex,
    },
    prelude::*,
    *,
};
use std::path::Path;

// ---- suggestions from detection ---------------------------------------------------

/// A hook the proposal would add that the configuration does not have yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub id: String,
    pub phase: HookPhase,
    pub name: Option<String>,
    pub run: String,
    pub timeout: Option<String>,
    /// Why it is suggested.
    pub reason: String,
}

fn phase_of(name: &str) -> Option<HookPhase> {
    match name {
        "setup" => Some(HookPhase::Setup),
        "switch" => Some(HookPhase::Switch),
        "teardown" => Some(HookPhase::Teardown),
        _ => None,
    }
}

/// The hook items of a proposal that can be added as they are: they have a
/// patch, nothing in the way, and need no other item first.
///
/// A suggestion is one hook added on its own, with the services devflow only
/// proposes to run itself left out, so the proposal is resolved for that
/// selection first (the hooks accepted, no `service:` item): a compose hook
/// then starts every compose service devflow does not branch, a service the
/// person has not accepted included, and one with nothing left to start drops
/// out. The unresolved patch names only the services devflow never offers.
pub fn hook_suggestions(plan: &Proposal) -> Vec<Suggestion> {
    let hooks: Vec<String> = plan
        .items
        .iter()
        .filter(|item| item.kind == ItemKind::Hook)
        .map(|item| item.id.clone())
        .collect();
    let plan = &plan.resolved(&hooks);
    let present: Vec<&str> = plan.items.iter().map(|i| i.id.as_str()).collect();
    plan.items
        .iter()
        .filter(|item| item.kind == ItemKind::Hook && item.is_actionable())
        .filter(|item| item.conflicts.is_empty())
        .filter(|item| {
            !item
                .depends_on
                .iter()
                .any(|d| present.contains(&d.as_str()))
        })
        .filter_map(|item| {
            let op = item.patch.ops.first()?;
            let PatchOp::Append { path, values, .. } = op else {
                return None;
            };
            let phase = path.get(1).and_then(|p| phase_of(p))?;
            let value = values.first()?;
            let (name, run, timeout) = match value {
                serde_json::Value::String(run) => (None, run.clone(), None),
                serde_json::Value::Object(map) => {
                    let text =
                        |key: &str| map.get(key).and_then(|v| v.as_str()).map(str::to_string);
                    (text("name"), text("run")?, text("timeout"))
                }
                _ => return None,
            };
            Some(Suggestion {
                id: item.id.clone(),
                phase,
                name,
                run,
                timeout,
                reason: item.title.clone(),
            })
        })
        .collect()
}

/// The presets the proposal would add.
pub fn preset_suggestions(plan: &Proposal) -> Vec<EnvPreset> {
    plan.items
        .iter()
        .filter(|item| item.kind == ItemKind::Env && item.is_actionable())
        .filter(|item| item.conflicts.is_empty())
        .filter_map(|item| EnvPreset::parse(item.id.strip_prefix("env:preset:")?))
        .collect()
}

// ---- what the tab shows -------------------------------------------------------------

/// Everything the tab draws, read in one background call.
#[derive(Clone)]
pub struct Loaded {
    pub files: ConfigFiles,
    pub hooks: HooksView,
    pub env: EnvView,
    pub references: Vec<String>,
    pub suggestions: Vec<Suggestion>,
    pub preset_suggestions: Vec<EnvPreset>,
    pub preview: Option<EnvPreview>,
    pub preview_workspace: Option<String>,
    pub trust: TrustState,
}

/// Read the files, the views and the detection hints of the project at `root`.
pub fn load(root: &Path, workspace: Option<&str>) -> anyhow::Result<Loaded> {
    let files = ConfigFiles::read(root)?;
    let hooks = files.hooks()?;
    let env = files.env()?;
    let project = Project::open(root)?;
    let trust = trust::state(&project)?;
    let (preview, preview_workspace) =
        match workspace.and_then(|name| worktree::find(&project, name).ok().flatten()) {
            Some(ws) => (
                config_edit::preview_env(&project, &ws).ok().flatten(),
                Some(ws.name),
            ),
            None => (None, None),
        };
    // Detection is a hint: the tab works without it.
    let plan = propose::propose(root, &PlanOptions::default()).ok();
    Ok(Loaded {
        references: config_edit::available_references(&project.config),
        suggestions: plan.as_ref().map(hook_suggestions).unwrap_or_default(),
        preset_suggestions: plan.as_ref().map(preset_suggestions).unwrap_or_default(),
        files,
        hooks,
        env,
        preview,
        preview_workspace,
        trust,
    })
}

fn phase_title(phase: HookPhase) -> &'static str {
    match phase {
        HookPhase::Setup => "Setup",
        HookPhase::Switch => "Switch",
        HookPhase::Teardown => "Teardown",
    }
}

fn phase_hint(phase: HookPhase) -> &'static str {
    match phase {
        HookPhase::Setup => {
            "Runs once in each new workspace, after its files, data and environment are in place."
        }
        HookPhase::Switch => "Runs every time you switch into an existing workspace.",
        HookPhase::Teardown => "Runs before a workspace is removed; a failure stops the removal.",
    }
}

fn phase_icon(phase: HookPhase) -> Lucide {
    match phase {
        HookPhase::Setup => Lucide::Rocket,
        HookPhase::Switch => Lucide::GitBranch,
        HookPhase::Teardown => Lucide::Trash,
    }
}

/// A recipe's template as the tab shows it: core's masked template, and the
/// raw text only when the person asked to reveal values (the edit form always
/// holds the raw text). The masking rule is core's (`config_edit::masked_template`).
pub fn template_text(recipe: &RecipeRow, reveal: bool) -> String {
    if reveal {
        recipe.reveal().to_string()
    } else {
        recipe.template.clone()
    }
}

/// A hook command as the tab shows it: credentials in it are masked unless
/// the person asked to reveal values.
pub fn command_text(run: &str, reveal: bool) -> String {
    if reveal {
        run.to_string()
    } else {
        redact::text(run)
    }
}

/// The file an edit is for: the project it belongs to, the file of that
/// project and the hash the file must still have. An edit is made for the
/// file the view shows; its Undo for the file the edit wrote.
#[derive(Clone, Debug)]
struct EditJob {
    root: std::path::PathBuf,
    target: Target,
    expected: Option<String>,
    file: String,
    /// This puts back an earlier edit.
    undoing: bool,
}

/// Which hook form is open.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Form {
    None,
    /// Adding to a phase, or editing the entry at a position of its list.
    Hook {
        phase: HookPhase,
        editing: Option<usize>,
    },
    /// Adding a variable, or editing the recipe of a key.
    Var {
        editing: Option<String>,
    },
}

/// What a form was opened on. Saving sends this hash, never the one of
/// whatever the tab shows by then: a reload that completes while the form is
/// open must not turn a changed file into a save that overwrites it.
#[derive(Clone, Debug)]
struct FormBase {
    /// The hash of the file the form saves to, as the tab showed it when the
    /// form was opened.
    expected: Option<String>,
    /// The command of the hook being edited, as it was when the form was
    /// opened (its position is in [`Form::Hook`]).
    original: Option<String>,
}

pub struct HooksEditorView {
    state: Entity<AppState>,
    /// The selection and document the loaded data belongs to.
    loaded_for: Option<(uuid::Uuid, std::path::PathBuf, Option<u64>)>,
    data: Option<Loaded>,
    loading: bool,
    /// The newest read; an older one that finishes later is dropped.
    reads: u64,
    /// Why the configuration could not be read.
    load_error: Option<String>,
    target: Target,
    form: Form,
    /// What the open form was opened on; `None` when no form is open.
    base: Option<FormBase>,
    name: Entity<InputState>,
    command: Entity<TextareaState>,
    timeout: Entity<InputState>,
    var_key: Entity<InputState>,
    var_value: Entity<InputState>,
    /// Shown with the open form (or at the top when none is open).
    error: Option<String>,
    /// The file changed on disk since it was read: the message to show.
    conflict: Option<String>,
    saving: bool,
    reveal: bool,
    /// [`AppState::refresh_requests`] as last seen: a higher count is a
    /// refresh the user asked for.
    seen_refreshes: u64,
    /// The newest look at the disk; an older one that finishes later is dropped.
    looks: u64,
    _subscriptions: Vec<Subscription>,
}

impl HooksEditorView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("name (optional)"));
        let command = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(3)
                .placeholder("shell command, run with sh -c in the workspace")
        });
        let timeout =
            cx.new(|cx| InputState::new(window, cx).placeholder("timeout, e.g. 5m (optional)"));
        let var_key = cx.new(|cx| InputState::new(window, cx).placeholder("NAME"));
        let var_value =
            cx.new(|cx| InputState::new(window, cx).placeholder("value, e.g. ${DATABASE_URL}"));
        let seen_refreshes = state.read(cx).refresh_requests;
        let mut subscriptions = vec![
            cx.observe_in(&state, window, |this, _, window, cx| {
                this.sync(window, cx);
                let asked = this.state.read(cx).refresh_requests;
                if asked != this.seen_refreshes {
                    this.seen_refreshes = asked;
                    this.look_at_disk(window, cx);
                }
                cx.notify();
            }),
            // Coming back to the window: the files may have been edited
            // meanwhile (an editor, `devflow env preset add`). One small read.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.look_at_disk(window, cx);
                }
            }),
        ];
        for input in [&name, &timeout, &var_key, &var_value] {
            subscriptions.push(cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
        }
        subscriptions.push(cx.subscribe(&command, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        }));
        let mut this = Self {
            state,
            loaded_for: None,
            data: None,
            loading: false,
            reads: 0,
            load_error: None,
            target: Target::Project,
            form: Form::None,
            base: None,
            name,
            command,
            timeout,
            var_key,
            var_value,
            error: None,
            conflict: None,
            saving: false,
            reveal: false,
            seen_refreshes,
            looks: 0,
            _subscriptions: subscriptions,
        };
        this.sync(window, cx);
        this
    }

    // ---- loading ---------------------------------------------------------------------

    /// Read the project's setup again when another project is selected or its
    /// configuration document was replaced (the Config tab saved or reloaded).
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return;
            };
            if model.phase != crate::model::LoadPhase::Ready {
                return;
            }
            (
                model.key.project,
                model.entry.root.clone(),
                model.config.as_ref().map(|c| c.identity.generation),
            )
        };
        let same_project = self
            .loaded_for
            .as_ref()
            .is_some_and(|(p, r, _)| *p == wanted.0 && *r == wanted.1);
        if !same_project {
            // Another project: nothing of the old one may stay on screen.
            self.data = None;
            self.form = Form::None;
            self.error = None;
            self.conflict = None;
            self.load_error = None;
            self.target = Target::Project;
        }
        let document_changed = self
            .loaded_for
            .as_ref()
            .is_some_and(|(_, _, g)| wanted.2.is_some() && g.is_some() && *g != wanted.2);
        let unread = self.data.is_none() && !self.loading && self.load_error.is_none();
        if !same_project || document_changed || unread {
            self.loaded_for = Some(wanted);
            self.read(true, window, cx);
        }
    }

    /// Whether an edit is in progress: a form is open (its fields are not
    /// saved) or a save is running. A reload would throw that away.
    fn editing(&self) -> bool {
        self.form != Form::None || self.saving
    }

    /// Compare the files on disk with the ones the tab shows, and read them
    /// again when they differ and nothing is being edited. With an edit in
    /// progress the tab keeps what it shows and says so, with Reload next to
    /// the message. Called by the global refresh and when the window is
    /// activated; a refresh while a read is already running does nothing.
    pub fn look_at_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, root, _)) = self.loaded_for.clone() else {
            return;
        };
        let Some(shown) = self.data.as_ref().map(|d| {
            (
                d.files.project.path.clone(),
                d.files.project.hash.clone(),
                d.files.local.hash.clone(),
            )
        }) else {
            return;
        };
        if self.loading {
            return;
        }
        self.looks += 1;
        let look = self.looks;
        let reads = self.reads;
        cx.spawn_in(window, async move |this, cx| {
            let on_disk = cx
                .background_spawn(async move {
                    ConfigFiles::read(&root)
                        .map(|f| (f.project.path, f.project.hash, f.local.hash))
                        .ok()
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                // Newer looks and reads win; a failed read changes nothing
                // (Reload says why).
                if this.looks != look || this.reads != reads {
                    return;
                }
                let Some(on_disk) = on_disk else {
                    return;
                };
                if on_disk == shown {
                    return;
                }
                if this.editing() {
                    this.conflict = Some(this.changed_while_editing());
                    cx.notify();
                } else {
                    this.read(true, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// The notice for a file that changed while a form was open.
    fn changed_while_editing(&self) -> String {
        format!(
            "{} changed on disk while you were editing. Your edit is kept; \
             reload to see the new content.",
            self.file_name(self.target)
        )
    }

    /// After the person reloaded with a form open: the form is made on the file
    /// as it is now. A form that edits a hook follows it to where it is now
    /// (the same command, at its position or its only other one); when it is
    /// gone or ambiguous the form is closed with a message, never aimed at
    /// whatever sits at the old position.
    fn rebase_form(&mut self) {
        let Some(data) = &self.data else {
            return;
        };
        let expected = data.files.file(self.target).hash.clone();
        let Some(base) = self.base.as_mut() else {
            return;
        };
        if let Form::Hook {
            phase,
            editing: Some(index),
        } = &mut self.form
        {
            let rows = data.hooks.phase(*phase).rows_for(self.target);
            let is_it = |i: usize| {
                rows.get(i)
                    .is_some_and(|row| Some(&row.run) == base.original.as_ref())
            };
            let others: Vec<usize> = (0..rows.len()).filter(|i| is_it(*i)).collect();
            let found = if is_it(*index) {
                Some(*index)
            } else if others.len() == 1 {
                Some(others[0])
            } else {
                None
            };
            match found {
                Some(found) => *index = found,
                None => {
                    self.form = Form::None;
                    self.base = None;
                    self.error = Some(
                        "The hook you were editing changed on disk, so its form was closed. \
                         Open it again to edit the new version."
                            .into(),
                    );
                    return;
                }
            }
        }
        base.expected = expected;
    }

    /// Read the files again because the person asked (the Reload button) or an
    /// edit was just saved: what comes back is shown.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.read(false, window, cx);
    }

    /// Read the files again. An `automatic` read (the selection or the
    /// document moved, the window was activated) that completes while a form
    /// is open, and finds the files changed, keeps the data the form was
    /// opened on and shows the "changed on disk" notice: swapping the data
    /// (and with it the hash) under the form would let its save overwrite the
    /// change without a conflict.
    fn read(&mut self, automatic: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (root, workspace, project) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return;
            };
            (
                model.entry.root.clone(),
                model.selected_workspace.clone(),
                model.key.project,
            )
        };
        self.loading = true;
        self.reads += 1;
        let read = self.reads;
        cx.spawn_in(window, async move |this, cx| {
            let loaded = cx
                .background_spawn(async move { jobs::catch(|| load(&root, workspace.as_deref())) })
                .await;
            this.update_in(cx, |this, _, cx| {
                if this.reads != read {
                    // A newer read is in flight; its answer is the one to show.
                    return;
                }
                this.loading = false;
                // The selection may have moved on while this was reading.
                let still = this
                    .loaded_for
                    .as_ref()
                    .is_some_and(|(p, _, _)| *p == project);
                if !still {
                    return;
                }
                match loaded {
                    Ok(data) => {
                        let under_a_form = automatic
                            && this.form != Form::None
                            && this.data.as_ref().is_some_and(|shown| {
                                shown.files.project.hash != data.files.project.hash
                                    || shown.files.local.hash != data.files.local.hash
                            });
                        if under_a_form {
                            this.conflict = Some(this.changed_while_editing());
                        } else {
                            // The person asked for this read with a form
                            // open: the form carries on over what was read.
                            let rebase = !automatic && this.form != Form::None;
                            this.data = Some(data);
                            this.load_error = None;
                            this.conflict = None;
                            if rebase {
                                this.rebase_form();
                            }
                        }
                    }
                    Err(message) => this.load_error = Some(message),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn file_name(&self, target: Target) -> String {
        self.data
            .as_ref()
            .and_then(|d| {
                d.files
                    .file(target)
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| match target {
                Target::Project => "devflow.toml".into(),
                Target::Local => "devflow.local.toml".into(),
            })
    }

    // ---- saving ----------------------------------------------------------------------

    /// Apply `edit` to the file this view looked at, then read everything
    /// again. `done` is the sentence of the success notification; `undo`, an
    /// edit that puts things back, is offered with it.
    fn apply_edit(
        &mut self,
        edit: Edit,
        done: String,
        undo: Option<Edit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_edit_on(None, edit, done, undo, window, cx);
    }

    /// [`Self::apply_edit`] for a file that must still have the hash `base`
    /// (what an open form was made on) instead of the one the tab shows now.
    fn apply_edit_on(
        &mut self,
        base: Option<Option<String>>,
        edit: Edit,
        done: String,
        undo: Option<Edit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data) = &self.data else {
            return;
        };
        let target = self.target;
        let job = EditJob {
            root: data.files.root.clone(),
            target,
            expected: base.unwrap_or_else(|| data.files.file(target).hash.clone()),
            file: self.file_name(target),
            undoing: false,
        };
        self.run_edit(job, edit, done, undo, window, cx);
    }

    /// Run `edit` on the file `job` names, provided it is still the one the
    /// job was made for.
    fn run_edit(
        &mut self,
        job: EditJob,
        edit: Edit,
        done: String,
        undo: Option<Edit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        self.saving = true;
        self.error = None;
        cx.notify();
        let (root, target, expected) = (job.root.clone(), job.target, job.expected.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    jobs::catch_typed(|| {
                        config_edit::apply(&root, target, expected.as_deref(), &edit)
                    })
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.edit_done(result, job, done, undo, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// Put back what a notification's edit changed: on the same file, and only
    /// if that file is still as the edit left it. It never follows the
    /// "Save to" choice or the project selected now.
    fn undo(&mut self, job: EditJob, edit: Edit, window: &mut Window, cx: &mut Context<Self>) {
        self.run_edit(job, edit, "Put it back.".into(), None, window, cx);
    }

    fn edit_done(
        &mut self,
        result: Result<Applied, jobs::Failure>,
        job: EditJob,
        done: String,
        undo: Option<Edit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.saving = false;
        let file = job.file.clone();
        match result {
            Ok(applied) => {
                self.form = Form::None;
                self.base = None;
                self.error = None;
                let mut text = done;
                for note in &applied.notes {
                    text.push_str(&format!("\n{note}"));
                }
                let mut note = if applied.notes.is_empty() {
                    Notification::success(text)
                } else {
                    Notification::info(text)
                };
                if let Some(undo) = undo {
                    // The inverse is bound to the file this edit wrote and to
                    // what the file was afterwards.
                    let back = EditJob {
                        expected: Some(applied.after.clone()),
                        undoing: true,
                        ..job.clone()
                    };
                    let view = cx.entity();
                    note = note.action(move |_, _, _| {
                        let (view, back, undo) = (view.clone(), back.clone(), undo.clone());
                        Button::new("undo-edit")
                            .label("Undo")
                            .on_click(move |_, window, cx| {
                                let (back, undo) = (back.clone(), undo.clone());
                                view.update(cx, |this, cx| this.undo(back, undo, window, cx));
                            })
                    });
                }
                window.push_notification(note.title(file), cx);
                // What is on screen is reread only if it is the file just written.
                let shown = self
                    .state
                    .read(cx)
                    .selection
                    .current()
                    .is_some_and(|m| m.entry.root == job.root);
                if shown {
                    self.reload(window, cx);
                    self.state.update(cx, |s, cx| s.config_changed_on_disk(cx));
                }
            }
            Err(failure) if failure.kind == FailureKind::ConfigChanged => {
                if job.undoing {
                    window.push_notification(
                        Notification::warning(format!(
                            "{file} changed since that edit, so it was not undone."
                        ))
                        .title(file),
                        cx,
                    );
                } else {
                    self.conflict = Some(format!("{file} changed on disk — reload"));
                }
            }
            Err(failure) if job.undoing => {
                window.push_notification(
                    Notification::error(failure.message).title(format!("Cannot undo in {file}")),
                    cx,
                );
            }
            Err(failure) => self.error = Some(failure.message),
        }
        cx.notify();
    }

    // ---- forms -----------------------------------------------------------------------

    fn clear_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [&self.name, &self.timeout, &self.var_key, &self.var_value] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.command
            .update(cx, |input, cx| input.set_value("", window, cx));
    }

    fn open_hook_form(
        &mut self,
        phase: HookPhase,
        editing: Option<usize>,
        fill: Option<HookEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(expected) = self
            .data
            .as_ref()
            .map(|d| d.files.file(self.target).hash.clone())
        else {
            return;
        };
        // The entry was drawn from the data shown; if that moved on since, the
        // position no longer names it.
        if let (Some(index), Some(entry)) = (editing, &fill)
            && self.data.as_ref().is_some_and(|d| {
                d.hooks
                    .phase(phase)
                    .rows_for(self.target)
                    .get(index)
                    .is_none_or(|row| row.run != entry.run)
            })
        {
            self.conflict = Some(format!(
                "{} changed on disk — reload",
                self.file_name(self.target)
            ));
            cx.notify();
            return;
        }
        self.clear_inputs(window, cx);
        self.error = None;
        self.base = Some(FormBase {
            expected,
            original: editing.and(fill.as_ref().map(|e| e.run.clone())),
        });
        if let Some(entry) = fill {
            self.name.update(cx, |input, cx| {
                input.set_value(entry.name.clone().unwrap_or_default(), window, cx)
            });
            self.timeout.update(cx, |input, cx| {
                input.set_value(entry.timeout.clone().unwrap_or_default(), window, cx)
            });
            self.command.update(cx, |input, cx| {
                input.set_value(entry.run.clone(), window, cx)
            });
        }
        self.form = Form::Hook { phase, editing };
        cx.notify();
        let command = self.command.clone();
        window.defer(cx, move |window, cx| {
            command.update(cx, |input, cx| input.focus(window, cx))
        });
    }

    fn open_var_form(
        &mut self,
        editing: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(expected) = self
            .data
            .as_ref()
            .map(|d| d.files.file(self.target).hash.clone())
        else {
            return;
        };
        self.clear_inputs(window, cx);
        self.error = None;
        self.base = Some(FormBase {
            expected,
            original: None,
        });
        let key = editing.as_ref().map(|(k, _)| k.clone());
        if let Some((k, v)) = editing {
            self.var_key
                .update(cx, |input, cx| input.set_value(k, window, cx));
            self.var_value
                .update(cx, |input, cx| input.set_value(v, window, cx));
        }
        self.form = Form::Var { editing: key };
        cx.notify();
        let focus = if matches!(&self.form, Form::Var { editing: Some(_) }) {
            self.var_value.clone()
        } else {
            self.var_key.clone()
        };
        window.defer(cx, move |window, cx| {
            focus.update(cx, |input, cx| input.focus(window, cx))
        });
    }

    fn close_form(&mut self, cx: &mut Context<Self>) {
        self.form = Form::None;
        self.base = None;
        self.error = None;
        cx.notify();
    }

    /// The entry the hook form describes.
    fn form_entry(&self, cx: &App) -> HookEntry {
        let text = |value: gpui_kit::SharedString| {
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        };
        HookEntry {
            name: text(self.name.read(cx).value()),
            run: self
                .command
                .read(cx)
                .value()
                .trim_end_matches(['\n', ' '])
                .to_string(),
            timeout: text(self.timeout.read(cx).value()),
        }
    }

    fn save_hook(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Form::Hook { phase, editing } = self.form.clone() else {
            return;
        };
        let entry = self.form_entry(cx);
        if entry.run.trim().is_empty() {
            self.error = Some("Enter the command to run.".into());
            cx.notify();
            return;
        }
        let Some(base) = self.base.clone() else {
            return;
        };
        // The hook being edited must still be the one the form was opened on.
        if let Some(index) = editing
            && self.data.as_ref().is_some_and(|d| {
                d.hooks
                    .phase(phase)
                    .rows_for(self.target)
                    .get(index)
                    .is_none_or(|row| Some(&row.run) != base.original.as_ref())
            })
        {
            self.conflict = Some(self.changed_while_editing());
            cx.notify();
            return;
        }
        let label = entry.name.clone().unwrap_or_else(|| "hook".into());
        let (edit, done) = match editing {
            Some(index) => (
                Edit::UpdateHook {
                    phase,
                    hook: HookRef::Index(index),
                    entry,
                },
                format!("Updated {label} in {}.", phase.as_str()),
            ),
            None => (
                Edit::AddHook {
                    phase,
                    entry,
                    at: Placement::End,
                },
                format!("Added {label} to {}.", phase.as_str()),
            ),
        };
        self.apply_edit_on(Some(base.expected), edit, done, None, window, cx);
    }

    fn save_var(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Form::Var { editing } = self.form.clone() else {
            return;
        };
        let key = match &editing {
            Some(key) => key.clone(),
            None => self.var_key.read(cx).value().trim().to_string(),
        };
        if key.is_empty() {
            self.error = Some("Enter a variable name.".into());
            cx.notify();
            return;
        }
        let Some(base) = self.base.clone() else {
            return;
        };
        let value = self.var_value.read(cx).value().to_string();
        let previous = self
            .data
            .as_ref()
            .and_then(|d| d.env.recipes.iter().find(|r| r.key == key))
            .map(|r| r.reveal().to_string());
        let undo = previous.map(|template| Edit::SetEnv {
            key: key.clone(),
            value: template,
        });
        let done = format!("Set {key}.");
        self.apply_edit_on(
            Some(base.expected),
            Edit::SetEnv { key, value },
            done,
            undo,
            window,
            cx,
        );
    }

    fn move_hook(
        &mut self,
        phase: HookPhase,
        from: usize,
        to: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let undo = Edit::MoveHook {
            phase,
            hook: HookRef::Index(to),
            to: Placement::Index(from),
        };
        let edit = Edit::MoveHook {
            phase,
            hook: HookRef::Index(from),
            to: Placement::Index(to),
        };
        self.apply_edit(edit, "Moved.".into(), Some(undo), window, cx);
    }

    fn remove_hook(
        &mut self,
        phase: HookPhase,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data) = &self.data else {
            return;
        };
        let rows = data.hooks.phase(phase).rows_for(self.target);
        let Some(row) = rows.get(index) else {
            return;
        };
        let entry = HookEntry {
            name: row.name.clone(),
            run: row.run.clone(),
            timeout: row.timeout.clone(),
        };
        let label = entry.name.clone().unwrap_or_else(|| "hook".into());
        let undo = Edit::AddHook {
            phase,
            entry,
            at: Placement::Index(index),
        };
        let edit = Edit::RemoveHook {
            phase,
            hook: HookRef::Index(index),
        };
        self.apply_edit(
            edit,
            format!("Removed {label} from {}.", phase.as_str()),
            Some(undo),
            window,
            cx,
        );
    }

    fn toggle_preset(
        &mut self,
        preset: EnvPreset,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (edit, undo, done) = if on {
            (
                Edit::AddPreset(preset),
                Edit::RemovePreset(preset),
                format!("Added the {} preset.", preset.as_str()),
            )
        } else {
            (
                Edit::RemovePreset(preset),
                Edit::AddPreset(preset),
                format!("Removed the {} preset.", preset.as_str()),
            )
        };
        self.apply_edit(edit, done, Some(undo), window, cx);
    }

    fn remove_var(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let template = self
            .data
            .as_ref()
            .and_then(|d| d.env.recipes.iter().find(|r| r.key == key))
            .map(|r| r.reveal().to_string());
        let undo = template.map(|value| Edit::SetEnv {
            key: key.clone(),
            value,
        });
        let done = format!("Removed {key}.");
        self.apply_edit(Edit::UnsetEnv { key }, done, undo, window, cx);
    }

    // ---- test hooks ------------------------------------------------------------------

    /// Fill the hook form, for tests.
    #[cfg(test)]
    pub(super) fn fill_hook(
        &mut self,
        name: &str,
        command: &str,
        timeout: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.name
            .update(cx, |i, cx| i.set_value(name.to_string(), window, cx));
        self.command
            .update(cx, |i, cx| i.set_value(command.to_string(), window, cx));
        self.timeout
            .update(cx, |i, cx| i.set_value(timeout.to_string(), window, cx));
    }

    /// Fill the variable form, for tests.
    #[cfg(test)]
    pub(super) fn fill_var(
        &mut self,
        key: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.var_key
            .update(cx, |i, cx| i.set_value(key.to_string(), window, cx));
        self.var_value
            .update(cx, |i, cx| i.set_value(value.to_string(), window, cx));
    }

    #[cfg(test)]
    pub(super) fn loaded(&self) -> Option<&Loaded> {
        self.data.as_ref()
    }

    /// Whether a read is in flight, for tests.
    #[cfg(test)]
    pub(super) fn is_loading(&self) -> bool {
        self.loading
    }

    /// Start an automatic read (what the window activation and the document
    /// sync do), for tests.
    #[cfg(test)]
    pub(super) fn start_automatic_read(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.read(true, window, cx);
    }

    /// Open the edit form of the hook at `index` of the phase's shared list, as
    /// its row's Edit button does with what the tab shows, for tests.
    #[cfg(test)]
    pub(super) fn open_edit_form(
        &mut self,
        phase: HookPhase,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row = self
            .data
            .as_ref()
            .and_then(|d| {
                d.hooks
                    .phase(phase)
                    .rows_for(self.target)
                    .get(index)
                    .cloned()
            })
            .expect("a hook at that position");
        let entry = HookEntry {
            name: row.name.clone(),
            run: row.run.clone(),
            timeout: row.timeout.clone(),
        };
        self.open_hook_form(phase, Some(index), Some(entry), window, cx);
    }

    /// The commands of a phase as its rows show them, for tests.
    #[cfg(test)]
    pub(super) fn shown_commands(&self, phase: HookPhase) -> Vec<String> {
        self.data.as_ref().map_or_else(Vec::new, |data| {
            data.hooks
                .phase(phase)
                .rows_for(self.target)
                .iter()
                .map(|row| command_text(&row.run, self.reveal))
                .collect()
        })
    }

    /// The templates of the variables as their rows show them, for tests.
    #[cfg(test)]
    pub(super) fn shown_templates(&self) -> Vec<(String, String)> {
        self.data.as_ref().map_or_else(Vec::new, |data| {
            data.env
                .recipes
                .iter()
                .map(|r| (r.key.clone(), template_text(r, self.reveal)))
                .collect()
        })
    }

    #[cfg(test)]
    pub(super) fn conflict(&self) -> Option<&str> {
        self.conflict.as_deref()
    }

    #[cfg(test)]
    pub(super) fn inline_error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

// ---- drawing ------------------------------------------------------------------------

impl HooksEditorView {
    fn header(&self, data: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        let local_name = self.file_name(Target::Local);
        let project_name = self.file_name(Target::Project);
        let selected = match self.target {
            Target::Project => 0,
            Target::Local => 1,
        };
        h_flex()
            .id("setup-header")
            .w_full()
            // The file names make the second group wide: it wraps below the
            // title instead of running out of the window.
            .flex_wrap()
            .gap_3()
            .items_center()
            .justify_between()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Setup"),
                            )
                            // Quiet once it is in order: a mark, not a banner.
                            .when(matches!(data.trust, TrustState::Trusted), |row| {
                                row.child(
                                    chip("approved", Tone::Success, cx)
                                        .id("setup-approved")
                                        .test_support(),
                                )
                            }),
                    )
                    .child(div().text_sm().text_color(ui::muted(cx)).child(
                        "What every new workspace runs and gets. Edits keep your comments.",
                    )),
            )
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(div().text_sm().text_color(ui::muted(cx)).child("Save to"))
                    .child(
                        RadioGroup::horizontal("edit-target")
                            .selected_index(Some(selected))
                            .child(
                                Radio::new("target-project")
                                    .label(format!("{project_name} (shared)")),
                            )
                            .child(
                                Radio::new("target-local")
                                    .label(format!("{local_name} (this machine)")),
                            )
                            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                this.target = if *ix == 0 {
                                    Target::Project
                                } else {
                                    Target::Local
                                };
                                this.form = Form::None;
                                this.error = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("setup-reload")
                            .small()
                            .icon(IconName::RefreshCw)
                            .label("Reload")
                            .loading(self.loading)
                            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                    ),
            )
    }

    fn banners(&self, data: &Loaded, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some(message) = &self.conflict {
            out.push(
                banner(
                    Tone::Warning,
                    Lucide::TriangleAlert,
                    message.clone(),
                    Some(
                        Button::new("setup-conflict-reload")
                            .label("Reload")
                            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                    ),
                    cx,
                )
                .id("setup-conflict")
                .test_support()
                .into_any_element(),
            );
        }
        if self.form == Form::None
            && let Some(error) = &self.error
        {
            out.push(
                ui::error_banner(error.clone(), cx)
                    .id("setup-error")
                    .test_support()
                    .into_any_element(),
            );
        }
        if let TrustState::Untrusted {
            previously_trusted,
            format_changed,
            ..
        } = &data.trust
        {
            let text = if *format_changed {
                "devflow now approves more than hooks. Review what this project runs again."
            } else if *previously_trusted {
                "The hooks or variables changed since you approved them. New workspaces skip them until you review."
            } else {
                "These hooks and variables have not been approved. New workspaces skip them until you review."
            };
            let state = self.state.clone();
            let view = cx.entity();
            out.push(
                banner(
                    Tone::Warning,
                    Lucide::ShieldAlert,
                    text,
                    Some(
                        Button::new("review-approve")
                            .primary()
                            .label("Review & approve")
                            .on_click(move |_, window, cx| {
                                let view = view.clone();
                                onboarding::open_review(
                                    &state,
                                    move |window, cx| {
                                        view.update(cx, |this, cx| this.reload(window, cx))
                                    },
                                    window,
                                    cx,
                                )
                            }),
                    ),
                    cx,
                )
                .id("setup-trust-banner")
                .test_support()
                .into_any_element(),
            );
        }
        if self.target == Target::Local {
            out.push(
                banner(
                    Tone::Info,
                    Lucide::Link,
                    format!(
                        "Edits go to {}, which is not committed. A list you change there replaces the shared one on this machine, so it starts as a copy of it.",
                        self.file_name(Target::Local)
                    ),
                    None,
                    cx,
                )
                .id("setup-local-note")
                .test_support()
                .into_any_element(),
            );
        }
        out
    }

    fn hook_form(
        &self,
        phase: HookPhase,
        editing: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let saving = self.saving;
        v_flex()
            .id("hook-form")
            .w_full()
            .gap_2()
            .p_3()
            .bg(cx.theme().muted.opacity(0.35))
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(220.))
                            .child(Input::new(&self.name).id("hook-name")),
                    )
                    .child(
                        div()
                            .w(px(200.))
                            .child(Input::new(&self.timeout).id("hook-timeout")),
                    ),
            )
            .child(div().id("hook-command").child(Textarea::new(&self.command)))
            .children(self.error.clone().map(|e| {
                div()
                    .id("form-error")
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(e)
                    .test_support()
            }))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("hook-cancel")
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.close_form(cx))),
                    )
                    .child(
                        Button::new("hook-save")
                            .small()
                            .primary()
                            .label(if editing.is_some() {
                                "Save"
                            } else {
                                "Add hook"
                            })
                            .loading(saving)
                            .disabled(saving)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let _ = phase;
                                this.save_hook(window, cx)
                            })),
                    ),
            )
            .test_support()
            .into_any_element()
    }

    fn hook_row(
        &self,
        phase: HookPhase,
        index: usize,
        count: usize,
        row: &config_edit::HookRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = |kind: &str| SharedString::from(format!("hook-{kind}-{}-{index}", phase.as_str()));
        let entry = HookEntry {
            name: row.name.clone(),
            run: row.run.clone(),
            timeout: row.timeout.clone(),
        };
        let saving = self.saving;
        let shown = command_text(&row.run, self.reveal);
        hoverable(
            h_flex()
                .id(id("row"))
                .w_full()
                .px_3()
                .py_1p5()
                .gap_3()
                .items_center()
                .border_b_1()
                .border_color(cx.theme().border.opacity(0.5)),
            cx,
        )
        .child(
            div()
                .w(px(18.))
                .text_xs()
                .text_color(ui::muted(cx))
                .child(format!("{}", index + 1)),
        )
        .child(match &row.name {
            Some(name) => div()
                .w(px(140.))
                .truncate()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(redact::text(name)),
            None => div()
                .w(px(140.))
                .text_sm()
                .text_color(ui::muted(cx))
                .child("unnamed"),
        })
        .child(
            mono(shown.lines().next().unwrap_or("").to_string(), cx)
                .id(id("command"))
                .flex_1()
                .min_w_0()
                .truncate()
                .when(shown.lines().count() > 1, |d| {
                    d.child(div().text_color(ui::muted(cx)).child(" …"))
                })
                .test_support(),
        )
        .children(
            row.timeout
                .clone()
                .map(|t| chip(format!("timeout {t}"), Tone::Neutral, cx)),
        )
        .child(
            h_flex()
                .gap_0p5()
                .child(
                    Button::new(id("up"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowUp)
                        .tooltip("Run earlier")
                        .disabled(saving || index == 0)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.move_hook(phase, index, index - 1, window, cx)
                        })),
                )
                .child(
                    Button::new(id("down"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowDown)
                        .tooltip("Run later")
                        .disabled(saving || index + 1 >= count)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.move_hook(phase, index, index + 1, window, cx)
                        })),
                )
                .child(
                    Button::new(id("edit"))
                        .ghost()
                        .xsmall()
                        .icon(Lucide::Pencil)
                        .tooltip("Edit")
                        .disabled(saving)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_hook_form(phase, Some(index), Some(entry.clone()), window, cx)
                        })),
                )
                .child(
                    Button::new(id("delete"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Delete)
                        .tooltip("Remove")
                        .disabled(saving)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.remove_hook(phase, index, window, cx)
                        })),
                ),
        )
        .test_support()
        .into_any_element()
    }

    fn hook_card(&self, data: &Loaded, phase: HookPhase, cx: &mut Context<Self>) -> AnyElement {
        let view = data.hooks.phase(phase);
        let rows = view.rows_for(self.target);
        let count = rows.len();
        let overridden = self.target == Target::Project && view.source == Target::Local;
        let adding = self.form
            == (Form::Hook {
                phase,
                editing: None,
            });
        let suggestions: Vec<&Suggestion> = data
            .suggestions
            .iter()
            .filter(|s| s.phase == phase)
            .filter(|s| !rows.iter().any(|r| r.run.trim() == s.run.trim()))
            .collect();
        let add = Button::new(SharedString::from(format!("add-hook-{}", phase.as_str())))
            .small()
            .icon(IconName::Plus)
            .label("Add hook")
            .disabled(self.saving)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_hook_form(phase, None, None, window, cx)
            }));
        // A command that quotes a credential is masked; Reveal shows it.
        let hides = rows.iter().any(|r| command_text(&r.run, false) != r.run);
        let reveal = self.reveal;
        let controls = h_flex()
            .gap_1()
            .when(hides || reveal, |row| {
                row.child(
                    Button::new(SharedString::from(format!(
                        "reveal-hooks-{}",
                        phase.as_str()
                    )))
                    .ghost()
                    .small()
                    .icon(if reveal {
                        IconName::EyeOff
                    } else {
                        IconName::Eye
                    })
                    .tooltip(if reveal {
                        "Hide the values"
                    } else {
                        "Show the values"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reveal = !this.reveal;
                        cx.notify();
                    })),
                )
            })
            .child(add)
            .into_any_element();
        let mut body = v_flex().w_full();
        if overridden {
            body = body.child(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(cx.theme().warning)
                    .border_b_1()
                    .border_color(cx.theme().border.opacity(0.5))
                    .child(format!(
                        "{} replaces this list on this machine, so changes to the shared list have no effect here.",
                        self.file_name(Target::Local)
                    )),
            );
        }
        for (index, row) in rows.iter().enumerate() {
            if self.form
                == (Form::Hook {
                    phase,
                    editing: Some(index),
                })
            {
                body = body.child(self.hook_form(phase, Some(index), cx));
            } else {
                body = body.child(self.hook_row(phase, index, count, row, cx));
            }
        }
        if rows.is_empty() && !adding {
            body = body.child(empty_row(
                match phase {
                    HookPhase::Setup => "Nothing runs when a workspace is created yet.",
                    HookPhase::Switch => "Nothing runs when you switch into a workspace.",
                    HookPhase::Teardown => "Nothing runs before a workspace is removed.",
                },
                None,
                cx,
            ));
        }
        if !suggestions.is_empty() && !adding {
            body = body.child(
                h_flex()
                    .id(SharedString::from(format!(
                        "suggestions-{}",
                        phase.as_str()
                    )))
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(
                        div()
                            .text_xs()
                            .text_color(ui::muted(cx))
                            .child("Detected in your project:"),
                    )
                    .children(suggestions.into_iter().map(|s| {
                        let entry = HookEntry {
                            name: s.name.clone(),
                            run: s.run.clone(),
                            timeout: s.timeout.clone(),
                        };
                        Button::new(SharedString::from(format!("suggest-{}", s.id)))
                            .xsmall()
                            .outline()
                            .icon(Lucide::WandSparkles)
                            .label(
                                s.name
                                    .clone()
                                    .unwrap_or_else(|| command_text(&s.run, self.reveal)),
                            )
                            .tooltip(format!(
                                "{} — {}",
                                s.reason,
                                command_text(&s.run, self.reveal)
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_hook_form(phase, None, Some(entry.clone()), window, cx)
                            }))
                    })),
            );
        }
        if adding {
            body = body.child(self.hook_form(phase, None, cx));
        }
        card(cx)
            .id(SharedString::from(format!("hooks-{}", phase.as_str())))
            .child(section_header(
                phase_icon(phase),
                phase_title(phase),
                Some(phase_hint(phase).into()),
                Some(controls),
                cx,
            ))
            .child(body)
            .test_support()
            .into_any_element()
    }

    fn presets_card(&self, data: &Loaded, cx: &mut Context<Self>) -> AnyElement {
        let on: Vec<EnvPreset> = data.env.presets.iter().map(|p| p.preset).collect();
        let rows = EnvPreset::ALL.into_iter().map(|preset| {
            let checked = on.contains(&preset);
            let detected = data.preset_suggestions.contains(&preset);
            hoverable(
                h_flex()
                    .id(SharedString::from(format!(
                        "preset-row-{}",
                        preset.as_str()
                    )))
                    .w_full()
                    .px_3()
                    .py_1p5()
                    .gap_3()
                    .items_start()
                    .border_b_1()
                    .border_color(cx.theme().border.opacity(0.5)),
                cx,
            )
            .child(
                Checkbox::new(SharedString::from(format!("preset-{}", preset.as_str())))
                    .checked(checked)
                    .on_click(cx.listener(move |this, value: &bool, window, cx| {
                        this.toggle_preset(preset, *value, window, cx)
                    })),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(config_edit::preset_label(preset)),
                            )
                            .when(detected && !checked, |row| {
                                row.child(chip("detected in your project", Tone::Info, cx))
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(ui::muted(cx))
                            .child(config_edit::preset_description(preset)),
                    ),
            )
        });
        card(cx)
            .id("env-presets")
            .child(section_header(
                Lucide::Layers,
                "Presets",
                Some("typed variables for a framework, built from the workspace's database".into()),
                None,
                cx,
            ))
            .children(rows)
            .test_support()
            .into_any_element()
    }

    fn recipes_card(&self, data: &Loaded, cx: &mut Context<Self>) -> AnyElement {
        let reveal = self.reveal;
        let preview = data.preview.as_ref();
        let adding = self.form == (Form::Var { editing: None });
        let controls = h_flex()
            .gap_1()
            .child(
                Button::new("reveal-values")
                    .ghost()
                    .small()
                    .icon(if reveal {
                        IconName::EyeOff
                    } else {
                        IconName::Eye
                    })
                    .tooltip(if reveal {
                        "Hide the values"
                    } else {
                        "Show the values"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reveal = !this.reveal;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("add-var")
                    .small()
                    .icon(IconName::Plus)
                    .label("Add variable")
                    .disabled(self.saving)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_var_form(None, window, cx)),
                    ),
            )
            .into_any_element();
        let mut body = v_flex().w_full();
        for (index, recipe) in data.env.recipes.iter().enumerate() {
            if self.form
                == (Form::Var {
                    editing: Some(recipe.key.clone()),
                })
            {
                body = body.child(self.var_form(data, true, cx));
                continue;
            }
            let resolved = preview
                .and_then(|p| p.rows.iter().find(|r| r.key == recipe.key))
                .map(|row| {
                    if reveal {
                        row.reveal().to_string()
                    } else {
                        row.masked.clone()
                    }
                });
            // The edit form starts from what is written: the person asked to edit it.
            let (key, template) = (recipe.key.clone(), recipe.reveal().to_string());
            let delete_key = recipe.key.clone();
            body = body.child(
                hoverable(
                    h_flex()
                        .id(SharedString::from(format!("var-row-{}", recipe.key)))
                        .w_full()
                        .px_3()
                        .py_1p5()
                        .gap_3()
                        .items_center()
                        .border_b_1()
                        .border_color(cx.theme().border.opacity(0.5)),
                    cx,
                )
                .child(
                    mono(recipe.key.clone(), cx)
                        .w(px(200.))
                        .truncate()
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(
                    mono(template_text(recipe, reveal), cx)
                        .id(SharedString::from(format!("var-template-{}", recipe.key)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .test_support(),
                )
                .child(
                    mono(resolved.unwrap_or_else(|| "—".into()), cx)
                        .id(SharedString::from(format!("var-resolved-{}", recipe.key)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(ui::muted(cx))
                        .test_support(),
                )
                .children((recipe.source == Target::Local).then(|| chip("local", Tone::Info, cx)))
                .child(
                    h_flex()
                        .gap_0p5()
                        .child(
                            Button::new(SharedString::from(format!("var-edit-{index}")))
                                .ghost()
                                .xsmall()
                                .icon(Lucide::Pencil)
                                .tooltip("Edit")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_var_form(
                                        Some((key.clone(), template.clone())),
                                        window,
                                        cx,
                                    )
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("var-delete-{index}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Delete)
                                .tooltip("Remove")
                                .disabled(self.saving)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.remove_var(delete_key.clone(), window, cx)
                                })),
                        ),
                )
                .test_support(),
            );
        }
        if data.env.recipes.is_empty() && !adding {
            body = body.child(empty_row(
                "No variables yet. A variable is NAME = a value that can refer to ${DATABASE_URL} and the other variables devflow generates.",
                None,
                cx,
            ));
        }
        if adding {
            body = body.child(self.var_form(data, false, cx));
        }
        let hint = match (&data.preview_workspace, preview) {
            (Some(name), Some(_)) => Some(SharedString::from(format!(
                "resolved in {name}; credentials are masked"
            ))),
            _ => Some("written to .devflow/env in every workspace".into()),
        };
        card(cx)
            .id("env-recipes")
            .child(section_header(
                Lucide::Variable,
                "Variables",
                hint,
                Some(controls),
                cx,
            ))
            .child(body)
            .test_support()
            .into_any_element()
    }

    fn var_form(&self, data: &Loaded, editing: bool, cx: &mut Context<Self>) -> AnyElement {
        let saving = self.saving;
        v_flex()
            .id("var-form")
            .w_full()
            .gap_2()
            .p_3()
            .bg(cx.theme().muted.opacity(0.35))
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(220.))
                            .child(Input::new(&self.var_key).id("var-key").disabled(editing)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.var_value).id("var-value")),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .items_center()
                    .child(div().text_xs().text_color(ui::muted(cx)).child("Refer to:"))
                    .children(data.references.iter().take(12).map(|name| {
                        let reference = format!("${{{name}}}");
                        let shown = reference.clone();
                        Button::new(SharedString::from(format!("ref-{name}")))
                            .xsmall()
                            .ghost()
                            .label(shown)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let current = this.var_value.read(cx).value();
                                this.var_value.update(cx, |input, cx| {
                                    input.set_value(format!("{current}{reference}"), window, cx)
                                });
                            }))
                    })),
            )
            .children(self.error.clone().map(|e| {
                div()
                    .id("form-error")
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(e)
                    .test_support()
            }))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("var-cancel")
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.close_form(cx))),
                    )
                    .child(
                        Button::new("var-save")
                            .small()
                            .primary()
                            .label(if editing { "Save" } else { "Add variable" })
                            .loading(saving)
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, window, cx| this.save_var(window, cx))),
                    ),
            )
            .test_support()
            .into_any_element()
    }
}

impl Render for HooksEditorView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(data) = self.data.clone() else {
            return match &self.load_error {
                Some(message) => v_flex()
                    .p_4()
                    .gap_3()
                    .child(ui::error_banner(message.clone(), cx))
                    .child(
                        Button::new("setup-retry")
                            .label("Retry")
                            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                    )
                    .into_any_element(),
                None => ui::loading("Reading the setup…", cx).into_any_element(),
            };
        };
        let banners = self.banners(&data, cx);
        let path = shorten_home(&data.files.project.path);
        v_flex()
            .id("hooks-editor")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_4()
            .child(self.header(&data, cx))
            .children(banners)
            .child(self.hook_card(&data, HookPhase::Setup, cx))
            .child(self.hook_card(&data, HookPhase::Switch, cx))
            .child(self.hook_card(&data, HookPhase::Teardown, cx))
            .child(
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(Lucide::KeyRound).size_4().text_color(ui::muted(cx)))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Environment"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui::muted(cx))
                                    .child("loaded by commands and daemons through .devflow/env"),
                            ),
                    )
                    .child(self.presets_card(&data, cx))
                    .child(self.recipes_card(&data, cx)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(ui::muted(cx))
                    .child(format!(
                        "Editing {path}. A hook is a string until it needs a name or a timeout. Changing what runs asks you to approve it again.",
                    )),
            )
            .test_support()
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` also exports a `test` attribute; these are plain tests.
    use devflow_core::propose::{Confidence, FilePatch, Item};
    use serde_json::json;
    use std::prelude::v1::test;

    fn hook_item(id: &str, phase: &str, value: serde_json::Value, depends: &[&str]) -> Item {
        Item {
            id: id.into(),
            kind: ItemKind::Hook,
            title: format!("run {id}"),
            value: value.clone(),
            confidence: Confidence::High,
            evidence: vec![],
            risks: vec![],
            depends_on: depends.iter().map(|d| d.to_string()).collect(),
            conflicts: vec![],
            patch: FilePatch {
                file: "devflow.toml".into(),
                ops: vec![PatchOp::Append {
                    path: vec!["hooks".into(), phase.into()],
                    values: vec![value],
                    front: false,
                }],
            },
        }
    }

    fn plan(items: Vec<Item>) -> Proposal {
        Proposal {
            proposal_version: 1,
            schema_version: 1,
            input_hashes: Default::default(),
            items,
            already_configured: vec![],
            warnings: vec![],
        }
    }

    #[test]
    fn suggestions_are_the_hooks_a_proposal_could_add_as_they_are() {
        let mut conflicted = hook_item("hook:setup:b", "setup", json!({"run": "b"}), &[]);
        conflicted.conflicts = vec!["a hook named b runs something else".into()];
        let mut manual = hook_item("hook:setup:c", "setup", json!({"run": "c"}), &[]);
        manual.patch.ops.clear();
        let env = Item {
            kind: ItemKind::Env,
            id: "env:preset:prisma".into(),
            ..hook_item("x", "setup", json!({"run": "x"}), &[])
        };
        let plan = plan(vec![
            hook_item(
                "hook:setup:install",
                "setup",
                json!({"name": "install", "run": "bun install"}),
                &[],
            ),
            hook_item(
                "hook:switch:sync",
                "switch",
                json!("bun install --frozen-lockfile"),
                &[],
            ),
            hook_item(
                "hook:setup:migrate",
                "setup",
                json!({"name": "migrate", "run": "prisma migrate deploy", "timeout": "5m"}),
                &["service:db"],
            ),
            conflicted,
            manual,
            env,
            Item {
                id: "service:db".into(),
                kind: ItemKind::Service,
                ..hook_item("service:db", "setup", json!({"run": "-"}), &[])
            },
        ]);
        let suggestions = hook_suggestions(&plan);
        let runs: Vec<_> = suggestions
            .iter()
            .map(|s| (s.phase, s.run.as_str()))
            .collect();
        // `migrate` waits for the service it depends on; conflicts and
        // hand-work items are not offered.
        assert_eq!(
            runs,
            [
                (HookPhase::Setup, "bun install"),
                (HookPhase::Switch, "bun install --frozen-lockfile")
            ]
        );
        assert_eq!(suggestions[0].name.as_deref(), Some("install"));
        assert_eq!(suggestions[1].name, None);
        assert_eq!(preset_suggestions(&plan), [EnvPreset::Prisma]);
    }

    #[test]
    fn commands_are_masked_unless_revealed() {
        // kingfisher:ignore
        let url = "postgres://u:sentinel@h/db";
        assert_eq!(
            command_text(&format!("psql {url} -c 'select 1'"), false),
            "psql postgres://u:***@h/db -c 'select 1'"
        );
        assert_eq!(command_text("bun install", false), "bun install");
        assert_eq!(command_text(url, true), url);
    }

    #[test]
    fn titles_and_hints_exist_for_every_phase() {
        for phase in [HookPhase::Setup, HookPhase::Switch, HookPhase::Teardown] {
            assert!(!phase_title(phase).is_empty() && !phase_hint(phase).is_empty());
        }
        assert_eq!(phase_of("switch"), Some(HookPhase::Switch));
        assert_eq!(phase_of("up"), None);
        let _ = Confidence::High;
    }
}
