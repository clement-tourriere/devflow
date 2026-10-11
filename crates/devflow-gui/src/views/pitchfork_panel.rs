//! pitchfork in the wizard and the Daemons tab: whether it is installed and
//! has what devflow calls, how to get it, and whether its proxy is on.
//!
//! Installing is the person's click. The panel shows the exact command first;
//! the button runs that command (core's `pitchfork::run_install`) and nothing
//! else, and only when mise is there to run it. The proxy's commands (one of
//! them uses sudo) are only shown, each with a copy button: devflow never runs
//! them.
use super::{
    ui::{self, Tone},
    widgets::{caption, mono},
};
use crate::{
    model::PitchforkOffer,
    state::{AppState, PitchforkInstall},
};
use devflow_core::pitchfork::{self, Detection, ProxyStatus};
use gpui_kit::{
    component::{
        ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        notification::Notification,
        spinner::Spinner,
        v_flex,
    },
    prelude::*,
    *,
};

/// The element ids of one placement of the panel: the Daemons tab and the
/// wizard can be on screen together.
#[derive(Clone, Copy)]
pub(super) struct Ids {
    pub install: &'static str,
    pub run: &'static str,
    pub copy_install: &'static str,
    pub proxy: &'static str,
    pub copy_proxy: &'static str,
}

pub(super) const TAB: Ids = Ids {
    install: "pitchfork-install",
    run: "pitchfork-install-run",
    copy_install: "copy-install",
    proxy: "pitchfork-proxy",
    copy_proxy: "copy-proxy",
};

pub(super) const WIZARD: Ids = Ids {
    install: "wizard-pitchfork-install",
    run: "wizard-pitchfork-install-run",
    copy_install: "wizard-copy-install",
    proxy: "wizard-pitchfork-proxy",
    copy_proxy: "wizard-copy-proxy",
};

/// Why pitchfork cannot be used as it is, for the install card's title.
pub fn problem_title(detection: &Detection) -> String {
    if detection.path.is_none() {
        "pitchfork is not installed".to_string()
    } else if !detection.installed {
        "pitchfork is on PATH but does not run".to_string()
    } else {
        format!(
            "pitchfork {} lacks what devflow uses",
            detection.version.as_deref().unwrap_or("")
        )
        .replace("  ", " ")
    }
}

/// What is missing, in a sentence.
pub fn problem_body(detection: &Detection) -> String {
    let detail = if detection.installed {
        format!("Missing: {}.", detection.missing_features.join(", "))
    } else {
        detection
            .problem
            .clone()
            .map(|p| format!("{p}."))
            .unwrap_or_default()
    };
    format!("devflow does not supervise processes: pitchfork does. {detail}")
}

/// The command devflow runs when asked to install, as the person reads it.
pub fn install_command(offer: &PitchforkOffer) -> Option<String> {
    offer
        .install
        .as_ref()?
        .run
        .as_ref()
        .map(|argv| argv.join(" "))
}

/// A command to copy, with its button.
fn command_row(id: &'static str, index: usize, command: &str, cx: &App) -> Div {
    let copied = command.to_string();
    h_flex()
        .gap_2()
        .items_center()
        .child(
            mono(command.to_string(), cx)
                .flex_1()
                .min_w_0()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(cx.theme().muted)
                .truncate(),
        )
        .child(
            Button::new((id, index))
                .icon(IconName::Copy)
                .ghost()
                .xsmall()
                .tooltip("Copy")
                .on_click(move |_, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                    window.push_notification(Notification::success("Command copied."), cx);
                }),
        )
}

fn notice(id: &'static str, tone: Tone, cx: &App) -> Stateful<Div> {
    let color = ui::tone_color(tone, cx);
    v_flex()
        .id(id)
        .p_3()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(color.opacity(0.4))
        .bg(color.opacity(0.08))
}

/// The card for a pitchfork that is missing or lacks something: the problem,
/// the install command (to run on a click, or to copy) and the other ways to
/// get it. `None` when pitchfork is fine.
pub(super) fn install_card(
    ids: Ids,
    offer: &PitchforkOffer,
    install: &PitchforkInstall,
    state: &Entity<AppState>,
    cx: &App,
) -> Option<AnyElement> {
    let detection = offer.detection.as_ref().filter(|d| !d.usable())?;
    let plan = offer
        .install
        .clone()
        .unwrap_or_else(pitchfork::install_plan);
    // Without mise there is no command devflow runs: the lines are for the person.
    let command = install_command(offer);
    let running = matches!(install, PitchforkInstall::Running(_));
    let run_state = state.clone();
    let run_plan = plan.clone();
    let mut card = notice(ids.install, Tone::Warning, cx)
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(problem_title(detection)),
        )
        .child(
            div()
                .text_sm()
                .text_color(ui::muted(cx))
                .child(problem_body(detection)),
        );
    if let Some(command) = &command {
        card = card
            .child(
                div()
                    .text_sm()
                    .text_color(ui::muted(cx))
                    .child("devflow runs this command only when you click Install; it changes your global mise configuration."),
            )
            .child(command_row(ids.copy_install, 0, command, cx))
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        Button::new(ids.run)
                            .primary()
                            .small()
                            .icon(IconName::Play)
                            .label("Install pitchfork")
                            .tooltip(format!("Runs: {command}"))
                            .disabled(running)
                            .loading(running)
                            .on_click(move |_, window, cx| {
                                let plan = run_plan.clone();
                                run_state.update(cx, |s, cx| s.install_pitchfork(plan, window, cx))
                            }),
                    )
                    .when(running, |row| {
                        row.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .text_sm()
                                .text_color(ui::muted(cx))
                                .child(Spinner::new().small())
                                .child("Installing…"),
                        )
                    }),
            );
    }
    match install {
        PitchforkInstall::Failed(message) => {
            card = card.child(
                div()
                    .id("pitchfork-install-error")
                    .test_support()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(message.clone()),
            );
        }
        PitchforkInstall::Done(message) => {
            card = card.child(
                div()
                    .id("pitchfork-install-done")
                    .test_support()
                    .text_sm()
                    .text_color(cx.theme().success)
                    .child(message.clone()),
            );
        }
        PitchforkInstall::Idle | PitchforkInstall::Running(_) => {}
    }
    // The other ways, for the person to run.
    let others: Vec<&String> = plan
        .commands
        .iter()
        .filter(|c| Some(*c) != command.as_ref())
        .collect();
    if !others.is_empty() {
        card = card.child(caption(
            if command.is_some() {
                "Or by hand"
            } else {
                "Install it with one of these (devflow runs nothing until you ask)"
            },
            cx,
        ));
        let first = usize::from(command.is_some());
        for (index, other) in others.into_iter().enumerate() {
            card = card.child(command_row(ids.copy_install, first + index, other, cx));
        }
    }
    Some(card.test_support().into_any_element())
}

/// What the proxy is doing, in a sentence.
pub fn proxy_summary(proxy: &ProxyStatus) -> String {
    if !proxy.enabled {
        return "The pitchfork proxy is off".to_string();
    }
    let mut text = format!(
        "The pitchfork proxy is on ({}://*.{}{})",
        proxy.scheme.as_deref().unwrap_or("https"),
        proxy.tld.as_deref().unwrap_or("localhost"),
        proxy.port.map(|p| format!(":{p}")).unwrap_or_default()
    );
    if proxy.trusted == Some(false) {
        text.push_str(", but its certificate is not trusted yet");
    }
    text
}

/// The card for the proxy: only while it is off (or its certificate is not
/// trusted), with the commands to turn it on, to copy. devflow never runs
/// them. `None` when the proxy is fine or pitchfork could not be asked.
pub(super) fn proxy_card(ids: Ids, offer: &PitchforkOffer, cx: &App) -> Option<AnyElement> {
    let proxy = offer.proxy.as_ref()?;
    if !offer.detection.as_ref().is_some_and(Detection::usable)
        || (proxy.enabled && proxy.trusted != Some(false))
    {
        return None;
    }
    let mut card = notice(ids.proxy, Tone::Warning, cx)
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(proxy_summary(proxy)),
        )
        .child(div().text_sm().text_color(ui::muted(cx)).child(
            "Daemons with a port get a stable HTTPS URL when it is on. This is optional, and \
             devflow never runs these commands (one of them needs sudo).",
        ));
    for (index, command) in pitchfork::PROXY_COMMANDS.iter().enumerate() {
        card = card.child(command_row(ids.copy_proxy, index, command, cx));
    }
    Some(card.test_support().into_any_element())
}

/// A one-line state of pitchfork for the wizard's section: installed or not,
/// and the proxy.
pub fn state_line(offer: &PitchforkOffer) -> (Tone, String) {
    match &offer.detection {
        None => (Tone::Neutral, "Looking for pitchfork…".to_string()),
        Some(d) if d.usable() => {
            let version = d.version.clone().unwrap_or_default();
            let proxy = match &offer.proxy {
                Some(p) if p.enabled => "proxy on",
                Some(_) => "proxy off",
                None => "proxy unknown",
            };
            (
                Tone::Success,
                format!("pitchfork {version} is installed · {proxy}"),
            )
        }
        Some(d) => (Tone::Warning, problem_title(d)),
    }
}
