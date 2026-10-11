//! "Run doctor" for a workspace: the checks `devflow doctor` runs there,
//! failures and warnings first, each with what to do about it. The checks are
//! core's (`devflow_core::doctor::run_in`), run on the background executor;
//! nothing here judges them.
use super::{
    ui::{self, Tone},
    widgets::{card, chip, section_header},
};
use crate::{
    jobs::{self, DoctorSource},
    state::AppState,
};
use devflow_core::{
    doctor::{Check, CheckStatus},
    redact,
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{ActiveTheme as _, Icon, IconName, WindowExt as _, button::Button, h_flex, v_flex},
    prelude::*,
    *,
};
use std::path::PathBuf;

/// One check as the dialog shows it, free text masked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRow {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    pub hint: Option<String>,
}

impl CheckRow {
    pub fn of(check: &Check) -> Self {
        Self {
            name: redact::text(&check.name),
            status: check.status,
            detail: redact::text(&check.detail),
            hint: check.hint.as_deref().map(redact::text),
        }
    }
}

fn severity(status: CheckStatus) -> u8 {
    match status {
        CheckStatus::Fail => 0,
        CheckStatus::Warn => 1,
        CheckStatus::Ok => 2,
    }
}

/// The checks in the order the dialog lists them: what is wrong first, each
/// group in the order core ran them.
pub fn rows(checks: &[Check]) -> Vec<CheckRow> {
    let mut rows: Vec<CheckRow> = checks.iter().map(CheckRow::of).collect();
    rows.sort_by_key(|row| severity(row.status));
    rows
}

enum Run {
    Running,
    Done(Vec<CheckRow>),
    Failed(String),
}

struct DoctorDialog {
    workspace: String,
    run: Run,
}

/// Run the checks for the workspace at `path` of the selected project and
/// show them in a dialog.
pub fn open(state: &Entity<AppState>, path: PathBuf, window: &mut Window, cx: &mut App) {
    let (project, source) = {
        let app = state.read(cx);
        (
            app.selection
                .current()
                .and_then(|model| model.project.clone()),
            app.doctor.clone(),
        )
    };
    let workspace = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let view = cx.new(|_| DoctorDialog {
        workspace,
        run: Run::Running,
    });
    let hosted = view.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title("Doctor").w(px(640.)).child(hosted.clone())
    });
    let Some(project) = project else {
        view.update(cx, |this, cx| {
            this.run = Run::Failed("The project is not loaded.".into());
            cx.notify();
        });
        return;
    };
    cx.spawn(async move |cx| {
        let checks = cx
            .background_spawn(async move {
                jobs::catch_value(|| match source {
                    DoctorSource::Core => devflow_core::doctor::run_in(Some(&project), &path),
                    #[cfg(test)]
                    DoctorSource::Fixed(checks) => checks,
                })
            })
            .await;
        view.update(cx, |this, cx| {
            this.run = match checks {
                Ok(checks) => Run::Done(rows(&checks)),
                Err(message) => Run::Failed(message),
            };
            cx.notify();
        });
    })
    .detach();
}

impl Render for DoctorDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.run {
            Run::Running => ui::loading("Checking this workspace…", cx)
                .h(px(160.))
                .into_any_element(),
            Run::Failed(message) => ui::error_banner(message.clone(), cx).into_any_element(),
            Run::Done(rows) => {
                let failing = rows.iter().filter(|r| r.status != CheckStatus::Ok).count();
                card(cx)
                    .id("doctor-checks")
                    .child(section_header(
                        Lucide::ListChecks,
                        "Checks",
                        Some(
                            if failing == 0 {
                                "everything is in order".to_string()
                            } else {
                                format!("{failing} to look at")
                            }
                            .into(),
                        ),
                        None,
                        cx,
                    ))
                    .child(
                        v_flex()
                            .id("doctor-rows")
                            .max_h(px(380.))
                            .overflow_y_scroll()
                            .children(rows.iter().enumerate().map(|(index, row)| {
                                let (icon, tone) = match row.status {
                                    CheckStatus::Ok => (IconName::CircleCheck, Tone::Success),
                                    CheckStatus::Warn => (IconName::TriangleAlert, Tone::Warning),
                                    CheckStatus::Fail => (IconName::CircleX, Tone::Danger),
                                };
                                h_flex()
                                    .id(SharedString::from(format!("doctor-check-{index}")))
                                    .w_full()
                                    .px_3()
                                    .py_1p5()
                                    .gap_3()
                                    .items_start()
                                    .border_b_1()
                                    .border_color(cx.theme().border.opacity(0.5))
                                    .child(
                                        Icon::new(icon)
                                            .size_4()
                                            .text_color(ui::tone_color(tone, cx)),
                                    )
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .gap_0p5()
                                            .child(
                                                h_flex()
                                                    .gap_2()
                                                    .items_center()
                                                    .child(
                                                        div()
                                                            .text_sm()
                                                            .font_weight(FontWeight::SEMIBOLD)
                                                            .child(row.name.clone()),
                                                    )
                                                    .when(row.status != CheckStatus::Ok, |r| {
                                                        r.child(chip(
                                                            match row.status {
                                                                CheckStatus::Fail => "failed",
                                                                _ => "warning",
                                                            },
                                                            tone,
                                                            cx,
                                                        ))
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(ui::muted(cx))
                                                    .child(row.detail.clone()),
                                            )
                                            .children(row.hint.clone().map(|hint| {
                                                div().text_xs().child(format!("To fix it: {hint}"))
                                            })),
                                    )
                                    .test_support()
                            })),
                    )
                    .test_support()
                    .into_any_element()
            }
        };
        v_flex()
            .gap_3()
            .child(div().text_sm().text_color(ui::muted(cx)).child(format!(
                "What devflow checks for {}.",
                crate::redaction::shown(&self.workspace)
            )))
            .child(body)
            .child(
                h_flex().justify_end().child(
                    Button::new("doctor-close")
                        .label("Close")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` also exports a `test` attribute; these are plain tests.
    use std::prelude::v1::test;

    fn check(name: &str, status: CheckStatus, detail: &str, hint: Option<&str>) -> Check {
        Check {
            name: name.into(),
            status,
            detail: detail.into(),
            hint: hint.map(str::to_string),
        }
    }

    #[test]
    fn what_is_wrong_comes_first_and_the_text_is_masked() {
        // kingfisher:ignore
        let url = "postgres://u:review-password@h/db";
        let checks = vec![
            check("git", CheckStatus::Ok, "git 2.50", None),
            check(
                "mise-env",
                CheckStatus::Fail,
                &format!("mise resolves DATABASE_URL elsewhere ({url})"),
                Some("check later _.file entries"),
            ),
            check("docker", CheckStatus::Warn, "not running", Some("start it")),
            check("hooks", CheckStatus::Ok, "approved", None),
        ];
        let rows = rows(&checks);
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["mise-env", "docker", "git", "hooks"]);
        assert!(!format!("{rows:?}").contains("review-password"));
        assert_eq!(rows[0].hint.as_deref(), Some("check later _.file entries"));
    }
}
