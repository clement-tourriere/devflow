//! Installing pitchfork, on the person's click. The command is the one the
//! panel showed (`InstallPlan::run`, core's `pitchfork::run_install`); this
//! only runs it off the UI thread and reports what happened.
use crate::{
    jobs::{self, Failure},
    state::{AppState, PitchforkInstall},
};
use devflow_core::pitchfork::{Detection, InstallPlan};
use gpui_kit::{
    AppContext as _, Context, Window,
    component::{WindowExt as _, notification::Notification},
};

impl AppState {
    /// Run `plan`'s command. A plan without one (no mise to run it) is
    /// refused: those are instructions for the person.
    pub fn install_pitchfork(
        &mut self,
        plan: InstallPlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.pitchfork_install, PitchforkInstall::Running(_)) {
            return;
        }
        let Some(command) = plan.run.as_ref().map(|argv| argv.join(" ")) else {
            return;
        };
        self.pitchfork_install = PitchforkInstall::Running(command);
        let source = self.pitchfork.clone();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    jobs::catch_typed(|| jobs::install_pitchfork(&source, &plan))
                })
                .await;
            this.update_in(cx, |this, window, cx| this.installed(result, window, cx))
                .ok();
        })
        .detach();
    }

    fn installed(
        &mut self,
        result: Result<Detection, Failure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(detection) if detection.usable() => {
                let message = format!(
                    "pitchfork {} is installed.",
                    detection.version.as_deref().unwrap_or("")
                )
                .replace("  ", " ");
                window.push_notification(
                    Notification::success(message.clone()).title("Install pitchfork"),
                    cx,
                );
                self.pitchfork_install = PitchforkInstall::Done(message);
            }
            Ok(detection) => {
                let message = format!(
                    "The command finished, but this app does not find a usable pitchfork yet: {}. \
                     If mise installed it, start devflow from a terminal where mise is activated.",
                    detection.summary()
                );
                window.push_notification(
                    Notification::warning(message.clone()).title("Install pitchfork"),
                    cx,
                );
                self.pitchfork_install = PitchforkInstall::Done(message);
            }
            Err(failure) => {
                Self::notify_error(window, cx, "Install pitchfork", &failure.message);
                self.pitchfork_install = PitchforkInstall::Failed(failure.message);
            }
        }
        // Whoever shows pitchfork's state reads it again.
        self.pitchfork_epoch += 1;
        self.refresh(cx);
        cx.notify();
    }
}
