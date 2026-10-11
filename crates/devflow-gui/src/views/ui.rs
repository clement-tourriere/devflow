//! Small shared building blocks for the views.
use crate::{
    model::{ServerBadge, WorkspaceState, cow_text},
    services::{BranchState, Cow},
};
use devflow_core::{config::CowCapability, db::CowSource};
use gpui_kit::{
    component::{
        ActiveTheme as _, Icon, Sizable as _,
        button::Button,
        empty::{
            Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant,
            EmptyTitle,
        },
        h_flex,
        spinner::Spinner,
        tag::Tag,
        tooltip::Tooltip,
    },
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

/// The color family of a workspace state: ready green, needs attention amber,
/// in progress and busy blue, not set up gray, missing red. The status tags
/// and the sidebar's dots share it.
pub fn workspace_tone(state: WorkspaceState) -> Tone {
    match state {
        WorkspaceState::Ready => Tone::Success,
        WorkspaceState::NeedsAttention => Tone::Warning,
        WorkspaceState::InProgress | WorkspaceState::Busy => Tone::Info,
        WorkspaceState::NotSetUp => Tone::Neutral,
        WorkspaceState::Missing => Tone::Danger,
    }
}

pub fn workspace_tag(state: WorkspaceState) -> impl IntoElement {
    pill(
        "workspace-state",
        state.label(),
        workspace_tone(state),
        state.help(),
    )
}

/// A status pill: a tag with a tooltip that says what the state means, the
/// same everywhere a state is shown. `id` only has to differ from its
/// siblings'.
pub fn pill(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    tone: Tone,
    help: impl Into<SharedString>,
) -> impl IntoElement {
    let help = help.into();
    div()
        .id(id)
        .flex_none()
        .child(tag(label, tone))
        .tooltip(move |window, cx| Tooltip::new(help.clone()).build(window, cx))
        .test_support()
}

pub fn tone_color(tone: Tone, cx: &App) -> Hsla {
    let theme = cx.theme();
    match tone {
        Tone::Neutral => theme.muted_foreground,
        Tone::Success => theme.success,
        Tone::Warning => theme.warning,
        Tone::Danger => theme.danger,
        Tone::Info => theme.info,
    }
}

/// The small status mark of a workspace in a list: a dot in its tone's color,
/// or a spinner while something is running in it.
pub fn workspace_dot(state: WorkspaceState, cx: &App) -> AnyElement {
    let color = tone_color(workspace_tone(state), cx);
    match state {
        WorkspaceState::InProgress | WorkspaceState::Busy => {
            Spinner::new().xsmall().color(color).into_any_element()
        }
        _ => div()
            .flex_shrink_0()
            .size(px(8.))
            .rounded_full()
            .bg(color.opacity(if state == WorkspaceState::NotSetUp {
                0.6
            } else {
                1.
            }))
            .into_any_element(),
    }
}

pub fn server_tag(badge: ServerBadge) -> impl IntoElement {
    let tone = match badge {
        ServerBadge::Running => Tone::Success,
        ServerBadge::Stopped | ServerBadge::NotConfigured => Tone::Neutral,
        ServerBadge::Unreachable => Tone::Danger,
    };
    pill("server-state", badge.label(), tone, badge.help())
}

pub fn cow_tag(cow: CowCapability, source: Option<CowSource>) -> impl IntoElement {
    let tone = match cow {
        CowCapability::Verified => Tone::Success,
        CowCapability::Copy => Tone::Warning,
        CowCapability::Unknown => Tone::Neutral,
    };
    pill(
        "cow-state",
        format!("CoW {}", cow_text(cow, source)),
        tone,
        crate::model::cow_meaning(cow),
    )
}

/// The color family of a container branch's state.
pub fn branch_tone(state: BranchState) -> Tone {
    match state {
        BranchState::Running => Tone::Success,
        BranchState::Missing => Tone::Warning,
        BranchState::Creating | BranchState::Deleting => Tone::Info,
        BranchState::Stopped | BranchState::NotStarted | BranchState::Unknown => Tone::Neutral,
    }
}

/// The copy-on-write result of a container service's Docker engine, as a
/// pill whose tooltip says what the result means.
pub fn cow_pill(id: impl Into<ElementId>, cow: &Cow) -> impl IntoElement {
    let tone = match cow.capability {
        CowCapability::Verified => Tone::Success,
        CowCapability::Copy => Tone::Warning,
        CowCapability::Unknown => Tone::Neutral,
    };
    let label = match (cow.capability, cow.measured_at) {
        (CowCapability::Unknown, None) => "CoW not measured".to_string(),
        (capability, _) => format!("CoW {}", crate::model::cow_label(capability)),
    };
    pill(id, label, tone, cow.help())
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

/// An empty or error state: an icon, a title, an explanation and, when there
/// is something to do about it, the primary action.
pub fn empty_state(
    icon: impl Into<Icon>,
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    action: Option<Button>,
) -> Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_8()
        .child(
            Empty::new()
                .max_w(px(520.))
                .flex_none()
                .header(
                    EmptyHeader::new()
                        .media(
                            EmptyMedia::new()
                                .with_variant(EmptyMediaVariant::Icon)
                                .child(icon.into()),
                        )
                        .title(EmptyTitle::new().child(title.into()))
                        .description(EmptyDescription::new().child(body.into())),
                )
                .when_some(action, |empty, action| {
                    empty.content(EmptyContent::new().child(action))
                }),
        )
}

/// An empty list that has little room: one muted sentence and the one action.
pub fn empty_inline(text: impl Into<SharedString>, action: Option<Button>, cx: &App) -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .text_sm()
        .text_color(muted(cx))
        .child(text.into())
        .children(action)
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

#[cfg(test)]
mod tests {
    use super::{Tone, workspace_tone};
    use crate::model::WorkspaceState;

    #[test]
    fn workspace_states_keep_their_colors() {
        let tone = |state| match workspace_tone(state) {
            Tone::Success => "green",
            Tone::Warning => "amber",
            Tone::Info => "blue",
            Tone::Neutral => "gray",
            Tone::Danger => "red",
        };
        assert_eq!(tone(WorkspaceState::Ready), "green");
        assert_eq!(tone(WorkspaceState::NeedsAttention), "amber");
        assert_eq!(tone(WorkspaceState::InProgress), "blue");
        assert_eq!(tone(WorkspaceState::Busy), "blue");
        assert_eq!(tone(WorkspaceState::NotSetUp), "gray");
        assert_eq!(tone(WorkspaceState::Missing), "red");
    }
}
