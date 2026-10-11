//! The app's accent on top of the component library's neutral themes.
//!
//! `Theme::change` loads the registered theme for a mode and replaces every
//! color, so the accent is applied again after each change
//! (`ThemeChoice::apply` does it).
use gpui_kit::{
    App, Hsla,
    component::{Theme, ThemeMode},
    rgb,
};

/// The colors the accent sets, for one mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Accent {
    pub primary: Hsla,
    pub hover: Hsla,
    pub active: Hsla,
    /// Text and icons on the primary color.
    pub on_primary: Hsla,
    /// Focus outlines.
    pub ring: Hsla,
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

/// Indigo: deep enough for white text on light backgrounds, light enough for
/// near-black text on dark ones.
pub fn accent(mode: ThemeMode) -> Accent {
    match mode {
        ThemeMode::Light => Accent {
            primary: hex(0x4f46e5),
            hover: hex(0x4338ca),
            active: hex(0x3730a3),
            on_primary: hex(0xffffff),
            ring: hex(0x818cf8),
        },
        ThemeMode::Dark => Accent {
            primary: hex(0x818cf8),
            hover: hex(0xa5b4fc),
            active: hex(0xc7d2fe),
            on_primary: hex(0x0a0a0a),
            ring: hex(0x6366f1),
        },
    }
}

/// Put the accent on the theme that is active now.
pub fn apply_accent(cx: &mut App) {
    Theme::update(cx, |theme| {
        let accent = accent(theme.mode);
        theme.primary = accent.primary;
        theme.primary_hover = accent.hover;
        theme.primary_active = accent.active;
        theme.primary_foreground = accent.on_primary;
        theme.ring = accent.ring;
        theme.link = accent.primary;
        theme.link_hover = accent.hover;
        theme.link_active = accent.active;
        theme.sidebar_primary = accent.primary;
        theme.sidebar_primary_foreground = accent.on_primary;
        theme.progress_bar = accent.primary;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG contrast ratio of two colors.
    fn contrast(a: Hsla, b: Hsla) -> f32 {
        fn luminance(color: Hsla) -> f32 {
            let rgba = color.to_rgb();
            let channel = |c: f32| {
                if c <= 0.03928 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(rgba.r) + 0.7152 * channel(rgba.g) + 0.0722 * channel(rgba.b)
        }
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn text_on_the_accent_stays_readable_in_both_modes() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let accent = accent(mode);
            for background in [accent.primary, accent.hover, accent.active] {
                let ratio = contrast(accent.on_primary, background);
                assert!(ratio >= 4.5, "{mode:?}: {ratio:.2} on {background:?}");
            }
        }
    }
}
