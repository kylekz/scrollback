//! Small building blocks shared by the views, drawn from the theme's tokens
//! (`design/project/sb.css` names each in parentheses).

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, Div, ElementId, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Role, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Transformation, div, percentage,
    prelude::FluentBuilder as _, px, relative, svg,
};
use sb_core::location::LocationKind;

use crate::format;
use crate::icons::SbIcon;
use crate::theme::{SbTheme, StyledText as _};

/// How a control that can't be used yet is drawn (`.btn:disabled`).
pub const DISABLED_OPACITY: f32 = 0.45;

/// `icon` at `size`, in `color`.
pub fn icon(icon: SbIcon, size: Pixels, color: Hsla) -> impl IntoElement {
    svg()
        .flex_none()
        .path(icon.path())
        .size(size)
        .text_color(color)
}

/// The sync glyph at 14px in `color`, turning while a pass runs (DESIGN §4.1).
pub fn sync_glyph(syncing: bool, color: Hsla) -> AnyElement {
    let glyph = svg()
        .flex_none()
        .path(SbIcon::RefreshCw.path())
        .size(px(14.))
        .text_color(color);
    if syncing {
        glyph
            .with_animation(
                "syncing",
                Animation::new(Duration::from_secs(1)).repeat(),
                |glyph, turned| {
                    glyph.with_transformation(Transformation::rotate(percentage(turned)))
                },
            )
            .into_any_element()
    } else {
        glyph.into_any_element()
    }
}

/// What a location's kind is called (DESIGN §4.1: "Local · synced 2 min ago").
pub fn kind_name(kind: LocationKind) -> &'static str {
    match kind {
        LocationKind::Local => "Local",
    }
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

/// A count at a row's right edge (DESIGN §4.1: Geist Mono 11px), in `color`.
pub fn count(value: usize, color: Hsla, theme: &SbTheme) -> impl IntoElement {
    div()
        .flex_none()
        .ml_auto()
        .text_color(color)
        .text_spec(&theme.text.count)
        .child(format::count(value as u64))
}

/// What a tag or a chip says by its colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// `s3` with `fg2`.
    Neutral,
    /// `accent-soft` with `accent-text`: this machine.
    Accent,
    /// `yellow-soft` with `yellow`: a warning.
    Warn,
}

impl Tone {
    fn colors(self, theme: &SbTheme) -> (Hsla, Hsla) {
        let c = &theme.colors;
        match self {
            Self::Neutral => (c.s3, c.fg2),
            Self::Accent => (c.accent_soft, c.accent_text),
            Self::Warn => (c.yellow_soft, c.yellow),
        }
    }
}

/// A tag (`.tag`: 18 high, radius 4, 11px at 500), or the 16px one of a card's last line. It
/// keeps its width unless the caller lets it shrink, and then its text ends in an ellipsis.
pub fn tag(label: impl Into<SharedString>, tone: Tone, small: bool, theme: &SbTheme) -> Div {
    let (fill, text) = tone.colors(theme);
    div()
        .flex_none()
        .h(if small {
            theme.sizes.tag_small_height
        } else {
            theme.sizes.tag_height
        })
        .px(theme.spacing.sm)
        .rounded(theme.radii.small)
        .bg(fill)
        .text_color(text)
        .text_spec(&theme.text.tag)
        // The 14px line centred in the tag's height.
        .line_height(if small {
            theme.sizes.tag_small_height
        } else {
            theme.sizes.tag_height
        })
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
        .child(label.into())
}

/// The tag that names a location, in its kind's colours (`.tag.win` for this machine).
pub fn location_tag(
    name: impl Into<SharedString>,
    kind: LocationKind,
    small: bool,
    theme: &SbTheme,
) -> Div {
    let tone = match kind {
        LocationKind::Local => Tone::Accent,
    };
    tag(name, tone, small, theme)
}

/// A chip that warns (`.chip.warn`), as low as a card's last line.
pub fn warn_chip(label: &'static str, theme: &SbTheme) -> impl IntoElement {
    let (fill, text) = Tone::Warn.colors(theme);
    div()
        .flex_none()
        .flex()
        .items_center()
        .h(theme.sizes.tag_small_height)
        .px(theme.spacing.sm)
        .rounded(theme.radii.pill)
        .bg(fill)
        .text_color(text)
        .text_spec(&theme.text.small)
        .whitespace_nowrap()
        .child(label)
}

/// How a button is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// `.btn`: `s2` with a `line` border.
    Plain,
    /// `.btn.primary`: `accent` with `accent-ink` text.
    Primary,
}

/// A button (`.btn`: 28 high, radius 6, 500), or the small one (`.btn.sm`: 24 high, 12px).
/// The caller gives it its label, its glyph and what a click does. A disabled one is dimmed
/// and takes no clicks.
pub fn button(
    id: impl Into<ElementId>,
    kind: ButtonKind,
    small: bool,
    enabled: bool,
    theme: &SbTheme,
) -> Stateful<Div> {
    let c = &theme.colors;
    let (fill, hover, text, border) = match kind {
        ButtonKind::Plain => (c.s2, c.s3, c.fg, c.line),
        ButtonKind::Primary => (
            c.accent,
            c.accent.opacity(0.9),
            c.accent_ink,
            gpui::transparent_black(),
        ),
    };
    div()
        .id(id)
        .role(Role::Button)
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.spacing.sm)
        .map(|this| {
            if small {
                this.h(theme.sizes.control_small_height)
                    .px(theme.spacing.md)
                    .text_spec(&theme.text.control_small)
            } else {
                this.h(theme.sizes.control_height)
                    .px(px(10.))
                    .text_spec(&theme.text.control)
            }
        })
        .rounded(theme.radii.control)
        .border_1()
        .border_color(border)
        .bg(fill)
        .text_color(text)
        .whitespace_nowrap()
        .map(|this| {
            if enabled {
                this.cursor_pointer().hover(move |style| style.bg(hover))
            } else {
                this.opacity(DISABLED_OPACITY)
            }
        })
}

/// A progress bar (`.bar`: 4 high, `s3` track, `accent` fill), `fraction` of it filled.
pub fn progress_bar(fraction: f32, theme: &SbTheme) -> impl IntoElement {
    let radius = theme.sizes.bar_height / 2.;
    div()
        .flex_none()
        .w_full()
        .h(theme.sizes.bar_height)
        .rounded(radius)
        .bg(theme.colors.s3)
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .w(relative(fraction.clamp(0., 1.)))
                .rounded(radius)
                .bg(theme.colors.accent),
        )
}

/// A checkbox (`.cb`: 14 square, radius 4), filled `accent` with a check when `checked`.
pub fn checkbox(checked: bool, theme: &SbTheme) -> impl IntoElement {
    let c = &theme.colors;
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(theme.sizes.checkbox)
        .rounded(theme.radii.small)
        .border_color(if checked { c.accent } else { c.fg3 })
        .border(px(1.5))
        .when(checked, |this| {
            this.bg(c.accent)
                .child(icon(SbIcon::Check, px(10.), c.accent_ink))
        })
}

/// The icon tile of a page's header (DESIGN §4.4: 56 square, radius 14, a 26px glyph):
/// `accent-soft` for a folder, `s3` for the library's own pages.
pub fn page_tile(glyph: SbIcon, accent: bool, theme: &SbTheme) -> impl IntoElement {
    let c = &theme.colors;
    let (fill, color) = if accent {
        (c.accent_soft, c.accent_text)
    } else {
        (c.s3, c.fg2)
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(theme.sizes.page_tile)
        .rounded(theme.radii.tile)
        .bg(fill)
        .child(icon(glyph, px(26.), color))
}
