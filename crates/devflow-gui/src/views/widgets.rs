//! Shared building blocks of the setup views (the Setup tab, the workspace
//! detail and the wizard): section headers, cards, banners, chips and
//! monospaced text. Colors come from the theme only, so light and dark both
//! work; nothing here names a color.
use super::ui::{self, Tone};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Icon, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex, v_flex,
    },
    prelude::*,
    *,
};

/// A block of the page: a bordered, slightly raised surface.
pub fn card(cx: &App) -> Div {
    v_flex()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
        // A card is as tall as its content: in a scrolling column it must not
        // shrink to nothing (it clips its own overflow).
        .flex_none()
        .overflow_hidden()
}

/// The title row of a section: an icon, a title and, beside them, what it is
/// for. `right` goes to the far end (buttons).
pub fn section_header(
    icon: Lucide,
    title: impl Into<SharedString>,
    hint: Option<SharedString>,
    right: Option<AnyElement>,
    cx: &App,
) -> Div {
    h_flex()
        .w_full()
        .px_3()
        .py_2()
        .gap_3()
        .items_center()
        .justify_between()
        .bg(cx.theme().muted.opacity(0.4))
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            h_flex()
                .gap_2()
                .min_w_0()
                .items_center()
                .child(
                    Icon::new(icon)
                        .size_4()
                        .text_color(cx.theme().muted_foreground),
                )
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title.into()))
                .children(hint.map(|hint| {
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(ui::muted(cx))
                        .child(hint)
                })),
        )
        .children(right)
}

/// A small label for a group of fields or facts.
pub fn caption(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(ui::muted(cx))
        .child(text.into())
}

/// Text in the theme's monospaced face.
pub fn mono(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .font_family(cx.theme().mono_font_family.clone())
        .text_sm()
        .child(text.into())
}

/// A pill: short text in a tone, with a quiet fill. Not interactive.
pub fn chip(label: impl Into<SharedString>, tone: Tone, cx: &App) -> Div {
    let color = ui::tone_color(tone, cx);
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .border_1()
        .border_color(color.opacity(0.35))
        .bg(color.opacity(0.1))
        .text_xs()
        .text_color(color)
        .whitespace_nowrap()
        .child(label.into())
}

/// A notice across the page: what happened, in a tone, with the one action
/// that answers it.
pub fn banner(
    tone: Tone,
    icon: Lucide,
    text: impl Into<SharedString>,
    action: Option<Button>,
    cx: &App,
) -> Div {
    let color = ui::tone_color(tone, cx);
    h_flex()
        .w_full()
        .px_3()
        .py_2()
        .gap_3()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(color.opacity(0.4))
        .bg(color.opacity(0.08))
        .child(Icon::new(icon).size_4().text_color(color))
        .child(div().flex_1().min_w_0().text_sm().child(text.into()))
        .children(action.map(|button| button.small().into_any_element()))
}

/// A quiet row of a list: the hover state every row shares.
pub fn hoverable(row: Stateful<Div>, cx: &App) -> Stateful<Div> {
    row.hover(|style| style.bg(cx.theme().muted.opacity(0.5)))
}

/// An empty block inside a card: one sentence and, when there is something to
/// do about it, the one action.
pub fn empty_row(text: impl Into<SharedString>, action: Option<Button>, cx: &App) -> Div {
    h_flex()
        .w_full()
        .px_3()
        .py_3()
        .gap_3()
        .items_center()
        .justify_between()
        .text_sm()
        .text_color(ui::muted(cx))
        .child(div().min_w_0().child(text.into()))
        .children(action.map(|button| button.small().ghost().into_any_element()))
}
