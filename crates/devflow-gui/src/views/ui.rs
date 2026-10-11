//! Small shared building blocks for the views.
use crate::model::{ServerBadge, WorkspaceState, cow_text};
use devflow_core::{config::CowCapability, db::CowSource};
use gpui_kit::{
    component::{ActiveTheme as _, h_flex, spinner::Spinner, tag::Tag, v_flex},
    prelude::*,
    *,
};

#[derive(Clone, Copy)]
pub enum Tone {
    Neutral,
    Success,
    Warning,
    Danger,
    Info,
}

/// A status tag. Outlined, so the label keeps the tone's color on the page
/// background: the filled variants put light text on a light fill in the dark
/// theme.
pub fn tag(label: impl Into<SharedString>, tone: Tone) -> Tag {
    let tag = match tone {
        Tone::Neutral => Tag::secondary(),
        Tone::Success => Tag::success(),
        Tone::Warning => Tag::warning(),
        Tone::Danger => Tag::danger(),
        Tone::Info => Tag::info(),
    };
    tag.outline().child(label.into())
}

pub fn workspace_tag(state: WorkspaceState) -> Tag {
    let tone = match state {
        WorkspaceState::Ready => Tone::Success,
        WorkspaceState::NeedsAttention => Tone::Warning,
        WorkspaceState::InProgress | WorkspaceState::Busy => Tone::Info,
        WorkspaceState::NotSetUp => Tone::Neutral,
        WorkspaceState::Missing => Tone::Danger,
    };
    tag(state.label(), tone)
}

pub fn server_tag(badge: ServerBadge) -> Tag {
    let tone = match badge {
        ServerBadge::Running => Tone::Success,
        ServerBadge::Stopped | ServerBadge::NotConfigured => Tone::Neutral,
        ServerBadge::Unreachable => Tone::Danger,
    };
    tag(badge.label(), tone)
}

pub fn cow_tag(cow: CowCapability, source: Option<CowSource>) -> Tag {
    let tone = match cow {
        CowCapability::Verified => Tone::Success,
        CowCapability::Copy => Tone::Warning,
        CowCapability::Unknown => Tone::Neutral,
    };
    tag(format!("CoW {}", cow_text(cow, source)), tone)
}

pub fn muted(cx: &App) -> Hsla {
    cx.theme().muted_foreground
}

pub fn heading(text: impl Into<SharedString>) -> Div {
    div()
        .text_base()
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

/// A centered title with an explanation, for empty and error states.
pub fn note(title: impl Into<SharedString>, body: impl Into<SharedString>, cx: &App) -> Div {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .p_8()
        .child(heading(title))
        .child(
            div()
                .max_w(px(520.))
                .text_sm()
                .text_center()
                .text_color(muted(cx))
                .child(body.into()),
        )
}

pub fn error_banner(message: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().danger.opacity(0.4))
        .bg(cx.theme().danger.opacity(0.08))
        .text_sm()
        .text_color(cx.theme().danger)
        .child(message.into())
}

pub fn loading(label: &str, cx: &App) -> Div {
    h_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .text_color(muted(cx))
        .child(Spinner::new())
        .child(label.to_string())
}
