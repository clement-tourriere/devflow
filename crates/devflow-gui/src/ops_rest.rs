//! Resting workspaces and the focus, as jobs on `AppState`: suspend, resume,
//! pin and the focus forwarder. Each calls core
//! ([`devflow_core::suspend`], [`devflow_core::focus`], activation) and decides
//! nothing: what stops, what is recorded and what may be forwarded is core's.
use crate::{
    jobs::Done,
    model::ProjectRef,
    ops::{FailureHint, TrustChoice, done_from},
    redaction::shown,
    state::AppState,
};
use devflow_core::{
    focus,
    remedy::Problem,
    setup::{self, SetupOptions},
    suspend,
    worktree::Workspace,
};
use gpui_kit::{Context, Window};

impl AppState {
    /// Stop the workspace's own containers and daemons and mark it suspended.
    /// Nothing shared is touched, and a daemon core cannot see stopped keeps
    /// the workspace from being marked (with the remedy core names).
    pub fn suspend_workspace(
        &mut self,
        owner: ProjectRef,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = shown(&workspace.name);
        let title = format!("Suspend {label}");
        if let Err(message) = self.open_project_for(&owner) {
            Self::notify_error(window, cx, &title, &message);
            return;
        }
        let pitchfork = self.pitchfork.resolve();
        self.start_job(
            title,
            true,
            FailureHint::None,
            window,
            cx,
            move |project, progress| {
                use devflow_core::setup::Progress as _;
                progress.output(&format!("Stopping the daemons and containers of {label}…"));
                let suspension = suspend::suspend(&project, &workspace, pitchfork.as_ref());
                if let Some(changes) = &suspension.services {
                    for service in &changes.services {
                        progress.output(&format!("{}: {:?}", service.key, service.change));
                    }
                }
                if !suspension.suspended {
                    let problem = Problem::new(
                        suspension
                            .problem
                            .unwrap_or_else(|| "the workspace was not suspended".into()),
                    );
                    return Err(suspension
                        .remedies
                        .into_iter()
                        .fold(problem, Problem::with)
                        .into_error());
                }
                Ok(Done {
                    summary: format!("Suspended {label}; its data is kept"),
                    ..Done::default()
                })
            },
        );
    }

    /// Enter the workspace again, which starts what was suspended: the same
    /// activation as switching to it.
    pub fn resume_workspace(
        &mut self,
        owner: ProjectRef,
        workspace: Workspace,
        trust: TrustChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = format!("Resume {}", shown(&workspace.name));
        if let Err(message) = self.open_project_for(&owner) {
            Self::notify_error(window, cx, &title, &message);
            return;
        }
        if !self.hooks_unchanged(&trust, &workspace.path, &title, window, cx) {
            return;
        }
        let pitchfork = self.pitchfork.resolve();
        self.start_job(
            title,
            true,
            FailureHint::None,
            window,
            cx,
            move |project, progress| {
                let options = SetupOptions {
                    approval: trust.approval(),
                    pitchfork,
                    ..Default::default()
                };
                let outcome = setup::activate(&project, &workspace, options, progress)?;
                Ok(done_from(
                    format!("Resumed {}", shown(&workspace.name)),
                    &outcome,
                ))
            },
        );
    }

    /// Keep the workspace out of (or let it back into) the idle suspension
    /// the command line offers.
    pub fn pin_workspace(
        &mut self,
        owner: ProjectRef,
        workspace: Workspace,
        pinned: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = shown(&workspace.name);
        let title = if pinned {
            format!("Pin {label}")
        } else {
            format!("Unpin {label}")
        };
        if let Err(message) = self.open_project_for(&owner) {
            Self::notify_error(window, cx, &title, &message);
            return;
        }
        self.start_job(
            title,
            false,
            FailureHint::None,
            window,
            cx,
            move |project, _| {
                suspend::set_pinned(&project, &workspace, pinned)?;
                Ok(Done {
                    summary: if pinned {
                        format!("{label} is pinned")
                    } else {
                        format!("{label} is not pinned any more")
                    },
                    ..Done::default()
                })
            },
        );
    }

    /// Point the project's focus (the canonical loopback ports) at the
    /// workspace, starting the forwarder if it is not running.
    pub fn focus_workspace(
        &mut self,
        owner: ProjectRef,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = shown(&workspace.name);
        let title = format!("Focus {label}");
        if let Err(message) = self.open_project_for(&owner) {
            Self::notify_error(window, cx, &title, &message);
            return;
        }
        self.start_job(
            title,
            false,
            FailureHint::None,
            window,
            cx,
            move |project, _| {
                let status = focus::focus(&project, &workspace, &[])?;
                let ports: Vec<String> = status
                    .forwards
                    .iter()
                    .map(|f| format!("127.0.0.1:{}", f.listen))
                    .collect();
                Ok(Done {
                    summary: format!("{} now reaches {label}", ports.join(", ")),
                    ..Done::default()
                })
            },
        );
    }

    /// End the project's focus forwarder.
    pub fn stop_focus(&mut self, owner: ProjectRef, window: &mut Window, cx: &mut Context<Self>) {
        let title = "Stop the focus".to_string();
        if let Err(message) = self.open_project_for(&owner) {
            Self::notify_error(window, cx, &title, &message);
            return;
        }
        self.start_job(
            title,
            false,
            FailureHint::None,
            window,
            cx,
            move |project, _| {
                focus::stop(&project)?;
                Ok(Done {
                    summary: "Focus is off".into(),
                    ..Done::default()
                })
            },
        );
    }
}
