//! Small building blocks shared by the views, drawn from the theme's tokens.

use gpui::{
    Hsla, IntoElement, ParentElement as _, Pixels, SharedString, Styled as _, div, px, svg,
};

use crate::icons::SbIcon;
use crate::theme::{SbTheme, StyledText as _};

/// How a control that can't be used yet is drawn (`design/project/sb.css`, `.btn:disabled`).
pub const DISABLED_OPACITY: f32 = 0.45;

/// `icon` at `size`, in `color`.
pub fn icon(icon: SbIcon, size: Pixels, color: Hsla) -> impl IntoElement {
    svg()
        .flex_none()
        .path(icon.path())
        .size(size)
        .text_color(color)
}

/// A keycap such as `Ctrl K` (DESIGN §5 "kbd": Geist Mono 11px, `line` border, `s2` fill,
/// radius 4).
pub fn kbd(label: impl Into<SharedString>, theme: &SbTheme) -> impl IntoElement {
    let c = &theme.colors;
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.))
        .rounded(theme.radii.small)
        .border_1()
        .border_color(c.line)
        .bg(c.s2)
        .text_color(c.fg2)
        .text_spec(&theme.text.kbd)
        .whitespace_nowrap()
        .child(label.into())
}

/// A section label (DESIGN §1.3 "overline": 11/14, 500, uppercase, `fg3`). gpui has no letter
/// spacing, so DESIGN's .08em is left out.
pub fn overline(label: &str, theme: &SbTheme) -> impl IntoElement {
    div()
        .flex_none()
        .text_color(theme.colors.fg3)
        .text_spec(&theme.text.overline)
        .whitespace_nowrap()
        .child(label.to_uppercase())
}

/// A count at a row's right edge (DESIGN §4.1: Geist Mono 11px, `fg3`).
pub fn count(value: usize, theme: &SbTheme) -> impl IntoElement {
    div()
        .flex_none()
        .ml_auto()
        .text_color(theme.colors.fg3)
        .text_spec(&theme.text.count)
        .child(value.to_string())
}
