//! Modal dialogs: initialize, trust, new workspace, remove, generic confirm.
//! Each form is a small entity; `window.open_dialog` only hosts it.
use crate::{
    model::{DataOption, RequestKey, WorkspaceRef, data_options, shorten_home},
    ops::{Approval, RemoveChoice, TrustChoice, approve_reviewed},
    state::{AppState, TrustPrompt},
};
use devflow_core::{redact, trust::Scope, worktree::Workspace};
use gpui_kit::{
    component::{
        ActiveTheme as _, Disableable as _, IndexPath, WindowExt as _,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        h_flex,
        input::{Input, InputEvent, InputState},
        select::{Select, SelectState},
        switch::Switch,
        v_flex,
    },
    prelude::*,
    *,
};
use std::{path::PathBuf, rc::Rc};

pub(super) type TrustRun = Rc<dyn Fn(TrustChoice, &mut Window, &mut App)>;
type ConfirmRun = Rc<dyn Fn(&mut Window, &mut App)>;

fn host(
    window: &mut Window,
    cx: &mut App,
    title: &'static str,
    width: f32,
    view: impl IntoElement + Clone + 'static,
) {
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(title).w(px(width)).child(view.clone())
    });
}

pub(super) fn cancel_button() -> Button {
    Button::new("cancel")
        .label("Cancel")
        .on_click(|_, window, cx| window.close_dialog(cx))
}

pub(super) fn footer(buttons: impl IntoIterator<Item = Button>) -> Div {
    h_flex().pt_2().gap_2().justify_end().children(buttons)
}

fn code_line(text: &str, cx: &App) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(cx.theme().muted)
        .font_family("Menlo")
        .text_sm()
        .child(text.to_string())
}

// ---- initialize ----------------------------------------------------------------

pub fn open_initialize(state: &Entity<AppState>, root: PathBuf, window: &mut Window, cx: &mut App) {
    let view = cx.new(|_| InitializeDialog {
        state: state.clone(),
        root,
        postgres: true,
    });
    host(window, cx, "Initialize project", 500., view);
}

struct InitializeDialog {
    state: Entity<AppState>,
    root: PathBuf,
    postgres: bool,
}

impl Render for InitializeDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(div().text_sm().child(format!(
                "{} has no devflow.toml. This creates one at the repository root and registers the project.",
                shorten_home(&self.root)
            )))
            .child(
                Switch::new("init-postgres")
                    .label("Enable PostgreSQL branching (managed container)")
                    .checked(self.postgres)
                    .on_change(cx.listener(|this, value: &bool, _, cx| {
                        this.postgres = *value;
                        cx.notify();
                    })),
            )
            .child(footer([
                cancel_button(),
                Button::new("init").primary().label("Initialize").on_click(cx.listener(
                    |this, _, window, cx| {
                        window.close_dialog(cx);
                        let (root, postgres) = (this.root.clone(), this.postgres);
                        this.state
                            .update(cx, |s, cx| s.initialize_project(root, postgres, window, cx));
                    },
                )),
            ]))
    }
}

// ---- trust -------------------------------------------------------------------------

/// Everything one approval covers, as shown to the user: the commands of
/// every hook phase that has any (credentials masked), then the timeout.
pub fn scope_sections(scope: &Scope) -> Vec<(&'static str, Vec<String>)> {
    let masked = |commands: &[String]| commands.iter().map(|c| redact::text(c)).collect::<Vec<_>>();
    let mut sections = Vec::new();
    for (title, commands) in [
        ("Setup", &scope.setup),
        ("Switch", &scope.switch),
        ("Teardown", &scope.teardown),
    ] {
        if !commands.is_empty() {
            sections.push((title, masked(commands)));
        }
    }
    sections.push(("Timeout per command", vec![scope.timeout.clone()]));
    sections
}

/// Run `run` now when no review is needed; otherwise ask first. The user
/// reviews the project's whole `[hooks]` section, because that is what an
/// approval covers.
pub fn with_trust(
    state: &Entity<AppState>,
    teardown: bool,
    window: &mut Window,
    cx: &mut App,
    run: impl Fn(TrustChoice, &mut Window, &mut App) + 'static,
) {
    match state.read(cx).trust_prompt(teardown) {
        None => run(TrustChoice::NotNeeded, window, cx),
        Some(prompt) => {
            let view = cx.new(|_| TrustDialog {
                state: state.clone(),
                prompt,
                teardown,
                run: Rc::new(run),
                error: None,
                working: false,
            });
            host(window, cx, "Trust project hooks?", 600., view);
        }
    }
}

struct TrustDialog {
    state: Entity<AppState>,
    /// The project and exact hooks being reviewed.
    prompt: TrustPrompt,
    teardown: bool,
    run: TrustRun,
    /// Why the last approval failed (the hooks may have changed).
    error: Option<String>,
    working: bool,
}

impl TrustDialog {
    /// Close and carry out `choice`, unless the selection moved to another
    /// project while the dialog was open: the answer is for this project only.
    fn answer(&mut self, choice: TrustChoice, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        if self.state.read(cx).selected_key() != Some(self.prompt.key) {
            AppState::notify_error(
                window,
                cx,
                "Project hooks",
                "The selected project changed while this was open. Start the operation again.",
            );
            return;
        }
        (self.run)(choice, window, cx);
    }

    fn run_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let scope = self.prompt.scope.clone();
        self.answer(TrustChoice::Reviewed(scope), window, cx);
    }

    /// Store the approval of exactly the content shown. If the hooks changed
    /// since, nothing is approved: show why and the new content instead.
    fn trust_and_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.working {
            return;
        }
        self.working = true;
        self.error = None;
        cx.notify();
        let (project, scope) = (self.prompt.project.clone(), self.prompt.scope.clone());
        cx.spawn_in(window, async move |this, cx| {
            let approval = cx
                .background_spawn(async move { approve_reviewed(&project, &scope) })
                .await;
            this.update_in(cx, |this, window, cx| this.approved(approval, window, cx))
                .ok();
        })
        .detach();
    }

    fn approved(&mut self, approval: Approval, window: &mut Window, cx: &mut Context<Self>) {
        self.working = false;
        match approval {
            Approval::Approved => self.run_once(window, cx),
            Approval::Changed {
                message,
                project,
                scope,
            } => {
                let key = self.prompt.key;
                self.prompt = TrustPrompt {
                    key,
                    project: project.clone(),
                    scope,
                };
                self.error = Some(message);
                self.state
                    .update(cx, |state, cx| state.adopt_project(key, project, cx));
                cx.notify();
            }
            Approval::Failed(message) => {
                self.error = Some(message);
                cx.notify();
            }
        }
    }
}

impl Render for TrustDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sections = scope_sections(&self.prompt.scope);
        v_flex()
            .gap_3()
            .child(div().text_sm().child(
                "This project's [hooks] changed or were never approved. Trusting approves \
                 everything below; the commands run with your permissions.",
            ))
            .children(self.error.clone().map(|e| {
                div()
                    .id("trust-error")
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().danger.opacity(0.08))
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(e)
                    .test_support()
            }))
            .child(
                v_flex()
                    .gap_2()
                    .id("trust-commands")
                    .max_h(px(300.))
                    .overflow_y_scroll()
                    .children(sections.into_iter().map(|(title, lines)| {
                        v_flex()
                            .id(SharedString::from(format!("trust-{}", title.to_lowercase().replace(' ', "-"))))
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(cx.theme().muted_foreground)
                                    .child(title),
                            )
                            .children(lines.iter().map(|line| code_line(line, cx)))
                            .test_support()
                    })),
            )
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(if self.teardown {
                "Without approval the workspace is not removed."
            } else {
                "Skip leaves the workspace in \"needs attention\" so you can run setup again later."
            }))
            .child(footer([
                if self.teardown {
                    // Removal cannot proceed without teardown hooks unless forced.
                    cancel_button()
                } else {
                    Button::new("skip")
                        .label("Skip")
                        .disabled(self.working)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.answer(TrustChoice::Skip, window, cx)
                        }))
                },
                Button::new("once")
                    .label("Run once")
                    .disabled(self.working)
                    .on_click(cx.listener(|this, _, window, cx| this.run_once(window, cx))),
                Button::new("trust")
                    .primary()
                    .label("Trust and run")
                    .loading(self.working)
                    .on_click(cx.listener(|this, _, window, cx| this.trust_and_run(window, cx))),
            ]))
    }
}

// ---- new workspace -------------------------------------------------------------------

pub fn open_new_workspace(state: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    let Some(view) = NewWorkspaceDialog::build(state, window, cx) else {
        return;
    };
    let name = view.read(cx).name.clone();
    host(window, cx, "New workspace", 520., view);
    // After the dialog took focus, put the cursor in the name field.
    window.defer(cx, move |window, cx| {
        name.update(cx, |input, cx| input.focus(window, cx))
    });
}

struct NewWorkspaceDialog {
    state: Entity<AppState>,
    /// The selection this dialog was opened for.
    key: Option<RequestKey>,
    name: Entity<InputState>,
    from: Entity<SelectState<Vec<SharedString>>>,
    from_names: Vec<String>,
    default_name: Option<String>,
    data: Option<Entity<SelectState<Vec<SharedString>>>>,
    data_options: Vec<DataOption>,
    error: Option<SharedString>,
    _subscription: Subscription,
}

impl NewWorkspaceDialog {
    fn build(state: &Entity<AppState>, window: &mut Window, cx: &mut App) -> Option<Entity<Self>> {
        let (from_names, default_name, data_options) = {
            let model = state.read(cx).selection.current()?;
            let default_name = model.default_workspace().map(|w| w.name.clone());
            let names: Vec<String> = model
                .workspaces
                .iter()
                .map(|w| w.workspace.name.clone())
                .collect();
            let options = model.database.configured.then(|| {
                data_options(
                    "Same as code source",
                    &model.workspaces,
                    &model.database.snapshots,
                )
            });
            (names, default_name, options)
        };
        let default_ix = default_name
            .as_ref()
            .and_then(|d| from_names.iter().position(|n| n == d))
            .unwrap_or(0);
        let key = state.read(cx).selected_key();
        let state = state.clone();
        Some(cx.new(|cx| {
            let name = cx.new(|cx| InputState::new(window, cx).placeholder("feature/my-task"));
            let items: Vec<SharedString> = from_names.iter().cloned().map(Into::into).collect();
            let from =
                cx.new(|cx| SelectState::new(items, Some(IndexPath::new(default_ix)), window, cx));
            let data = data_options.as_ref().map(|options| {
                let items: Vec<SharedString> =
                    options.iter().map(|o| o.label.clone().into()).collect();
                cx.new(|cx| SelectState::new(items, Some(IndexPath::new(0)), window, cx))
            });
            let subscription = cx.subscribe_in(
                &name,
                window,
                |this: &mut Self, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.submit(window, cx);
                    }
                },
            );
            Self {
                state,
                key,
                name,
                from,
                from_names,
                default_name,
                data,
                data_options: data_options.unwrap_or_default(),
                error: None,
                _subscription: subscription,
            }
        }))
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.read(cx).selected_key() != self.key {
            self.error =
                Some("The selected project changed; close this dialog and open it again.".into());
            cx.notify();
            return;
        }
        let name = self.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.error = Some("Enter a name for the new workspace.".into());
            cx.notify();
            return;
        }
        let from = self
            .from
            .read(cx)
            .selected_index(cx)
            .and_then(|ix| self.from_names.get(ix.row).cloned())
            .filter(|chosen| Some(chosen) != self.default_name.as_ref());
        let data_from = self.data.as_ref().and_then(|select| {
            let ix = select.read(cx).selected_index(cx)?;
            self.data_options.get(ix.row)?.source.clone()
        });
        window.close_dialog(cx);
        let state = self.state.clone();
        with_trust(
            &self.state.clone(),
            false,
            window,
            cx,
            move |trust, window, cx| {
                let (name, from, data_from) = (name.clone(), from.clone(), data_from.clone());
                state.update(cx, |s, cx| {
                    s.create_workspace(name, from, data_from, trust, window, cx)
                });
            },
        );
    }
}

impl Render for NewWorkspaceDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label =
            |text: &'static str| div().text_sm().font_weight(FontWeight::MEDIUM).child(text);
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(label("Name"))
                    .child(Input::new(&self.name)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(label("Code from"))
                    .child(Select::new(&self.from).w_full()),
            )
            .children(self.data.as_ref().map(|data| {
                v_flex()
                    .gap_1()
                    .child(label("Data from"))
                    .child(Select::new(data).w_full())
            }))
            .children(
                self.error
                    .clone()
                    .map(|e| div().text_sm().text_color(cx.theme().danger).child(e)),
            )
            .child(footer([
                cancel_button(),
                Button::new("create")
                    .primary()
                    .label("Create")
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            ]))
    }
}

// ---- remove ----------------------------------------------------------------------------

pub fn open_remove(
    state: &Entity<AppState>,
    workspace: &Workspace,
    window: &mut Window,
    cx: &mut App,
) {
    let target = state
        .read(cx)
        .selection
        .current()
        .and_then(|model| model.workspace_ref(&workspace.name));
    match target {
        Some(target) => open_remove_with(state, target, false, false, false, window, cx),
        None => stale_target(window, cx),
    }
}

/// Reopened after a failed removal; protections are bypassed only here, and
/// only for the workspace of the project the failed attempt was about.
pub fn open_remove_forced(
    state: &Entity<AppState>,
    target: WorkspaceRef,
    keep_data: bool,
    delete_branch: bool,
    window: &mut Window,
    cx: &mut App,
) {
    open_remove_with(state, target, true, keep_data, delete_branch, window, cx);
}

fn stale_target(window: &mut Window, cx: &mut App) {
    AppState::notify_error(
        window,
        cx,
        "Remove workspace",
        "That workspace is no longer part of the selected project.",
    );
}

fn open_remove_with(
    state: &Entity<AppState>,
    target: WorkspaceRef,
    force: bool,
    keep_data: bool,
    delete_branch: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let plan = {
        let Some(model) = state.read(cx).selection.current() else {
            return;
        };
        let Some(status) = model.resolve(&target) else {
            stale_target(window, cx);
            return;
        };
        let database = status
            .receipt
            .as_ref()
            .and_then(|r| r.data.as_ref())
            .map(|d| (d.database.clone(), d.owned && d.branch_id.is_some()));
        RemovePlan {
            path: shorten_home(&status.workspace.path),
            branch: status.workspace.branch.clone(),
            database,
            teardown: model
                .project
                .as_ref()
                .map(|p| p.config.hooks.teardown.clone())
                .unwrap_or_default(),
        }
    };
    let view = cx.new(|_| RemoveDialog {
        state: state.clone(),
        target,
        plan,
        choice: RemoveChoice {
            keep_data,
            delete_branch,
            force,
        },
    });
    host(window, cx, "Remove workspace", 560., view);
}

struct RemovePlan {
    path: String,
    branch: Option<String>,
    /// Database name and whether this workspace created (and may drop) it.
    database: Option<(String, bool)>,
    teardown: Vec<String>,
}

struct RemoveDialog {
    state: Entity<AppState>,
    target: WorkspaceRef,
    plan: RemovePlan,
    choice: RemoveChoice,
}

impl RemoveDialog {
    fn effects(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.plan.teardown.is_empty() {
            lines.push(format!(
                "Run {} teardown hook(s) first",
                self.plan.teardown.len()
            ));
        }
        lines.push(match &self.plan.database {
            Some((db, true)) if !self.choice.keep_data => {
                format!("Drop database branch {db} (created by this workspace)")
            }
            Some((db, true)) => format!("Keep database branch {db}"),
            Some((db, false)) => {
                format!("Keep database {db} (attached, not created by this workspace)")
            }
            None => "No database is attached".into(),
        });
        lines.push(format!("Delete the worktree directory {}", self.plan.path));
        lines.push(match (&self.plan.branch, self.choice.delete_branch) {
            (Some(b), true) => format!("Delete Git branch {b}"),
            (Some(b), false) => format!("Keep Git branch {b}"),
            (None, _) => "The worktree has no branch".into(),
        });
        lines
    }
}

impl Render for RemoveDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let owned_db = matches!(self.plan.database, Some((_, true)));
        v_flex()
            .gap_3()
            .child(div().text_sm().child(format!("Remove workspace {}? This will:", self.target.name)))
            .child(v_flex().gap_1().pl_2().children(
                self.effects().into_iter().map(|line| div().text_sm().child(format!("• {line}"))),
            ))
            .when(self.choice.force, |col| {
                col.child(
                    div().text_sm().text_color(cx.theme().warning).child(
                        "Force: uncommitted changes are discarded, a locked worktree is removed, and teardown failures are ignored.",
                    ),
                )
            })
            .when(owned_db, |col| {
                col.child(
                    Checkbox::new("keep-data")
                        .label("Keep the database branch")
                        .checked(self.choice.keep_data)
                        .on_click(cx.listener(|this, value: &bool, _, cx| {
                            this.choice.keep_data = *value;
                            cx.notify();
                        })),
                )
            })
            .child(
                Checkbox::new("delete-branch")
                    .label("Also delete the Git branch")
                    .checked(self.choice.delete_branch)
                    .on_click(cx.listener(|this, value: &bool, _, cx| {
                        this.choice.delete_branch = *value;
                        cx.notify();
                    })),
            )
            .child(footer([
                cancel_button(),
                Button::new("remove").danger().label("Remove").on_click(cx.listener(
                    |this, _, window, cx| {
                        window.close_dialog(cx);
                        let (state, target, choice) = (this.state.clone(), this.target.clone(), this.choice.clone());
                        let for_trust = state.clone();
                        with_trust(&for_trust, true, window, cx, move |trust, window, cx| {
                            let (target, choice) = (target.clone(), choice.clone());
                            state.update(cx, |s, cx| s.remove_workspace(target, choice, trust, window, cx));
                        });
                    },
                )),
            ]))
    }
}

// ---- generic confirmation -------------------------------------------------------------

pub fn open_confirm(
    window: &mut Window,
    cx: &mut App,
    title: &'static str,
    lines: Vec<String>,
    label: &'static str,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
) {
    let view = cx.new(|_| ConfirmDialog {
        lines,
        label,
        on_confirm: Rc::new(on_confirm),
    });
    host(window, cx, title, 480., view);
}

struct ConfirmDialog {
    lines: Vec<String>,
    label: &'static str,
    on_confirm: ConfirmRun,
}

impl Render for ConfirmDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let on_confirm = self.on_confirm.clone();
        v_flex()
            .gap_2()
            .children(self.lines.iter().map(|l| div().text_sm().child(l.clone())))
            .child(footer([
                cancel_button(),
                Button::new("confirm")
                    .danger()
                    .label(self.label)
                    .on_click(cx.listener(move |_, _, window, cx| {
                        window.close_dialog(cx);
                        on_confirm(window, cx);
                    })),
            ]))
    }
}

#[cfg(test)]
mod tests {
    use super::{Scope, scope_sections};

    #[test]
    fn the_review_lists_every_hook_phase_with_credentials_masked() {
        let scope = Scope {
            setup: vec![
                "bun install".into(),
                "psql postgres://u:hunter2@h/db -c 'select 1'".into(), // kingfisher:ignore
            ],
            switch: vec!["echo switched".into()],
            teardown: Vec::new(),
            timeout: "5m".into(),
            digest: "d".into(),
        };
        let sections = scope_sections(&scope);
        let titles: Vec<_> = sections.iter().map(|(title, _)| *title).collect();
        assert_eq!(titles, ["Setup", "Switch", "Timeout per command"]);
        assert_eq!(sections[0].1[0], "bun install");
        assert!(!sections[0].1[1].contains("hunter2"), "{:?}", sections[0].1);
        assert!(sections[0].1[1].contains("u:***@h"));
        assert_eq!(sections[2].1, ["5m"]);

        // Teardown is listed too, even when setup has nothing to show.
        let teardown_only = Scope {
            setup: Vec::new(),
            switch: Vec::new(),
            teardown: vec!["make clean".into()],
            ..scope
        };
        let titles: Vec<_> = scope_sections(&teardown_only)
            .iter()
            .map(|(title, _)| *title)
            .collect();
        assert_eq!(titles, ["Teardown", "Timeout per command"]);
    }
}
