//! What the main area shows in place of a library page: why the locations couldn't be read;
//! and the pieces the archive-unavailable page ([`super::unavailable`]) is drawn with.

use gpui::{
    AnyElement, Context, Hsla, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, TextRun, div, font,
};

use super::MainArea;
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, button, icon};
use crate::icons::SbIcon;
use crate::services::{ActiveServices as _, SyncService};
use crate::theme::{SbTheme, StyledText as _};

/// A centred column under a tile and a title, as the edge states are drawn (DESIGN §4.14).
pub(super) fn state(
    tile: AnyElement,
    title: impl Into<SharedString>,
    theme: &SbTheme,
) -> gpui::Div {
    div()
        .flex_none()
        .w(theme.sizes.state_width)
        .flex()
        .flex_col()
        .gap(theme.spacing.lg)
        .child(tile)
        .child(div().text_spec(&theme.text.state_title).child(title.into()))
}

/// The `red-soft` tile of a state that is a failure, with `glyph`.
pub(super) fn alert_tile(glyph: SbIcon, theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(theme.sizes.page_tile)
        .rounded(theme.radii.tile)
        .bg(c.red_soft)
        .child(icon(glyph, theme.sizes.tile_glyph, c.red))
        .into_any_element()
}

/// `text` in `color`, with every `path` in it set in the code font.
pub(super) fn with_path(text: &str, path: &str, color: Hsla, theme: &SbTheme) -> StyledText {
    let run = |len: usize, family: &SharedString| TextRun {
        len,
        font: font(family.clone()),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mut runs = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(path).filter(|_| !path.is_empty()) {
        runs.push(run(at, &theme.fonts.ui));
        runs.push(run(path.len(), &theme.fonts.mono));
        rest = &rest[at + path.len()..];
    }
    runs.push(run(rest.len(), &theme.fonts.ui));
    runs.retain(|run| run.len > 0);
    StyledText::new(text.to_string()).with_runs(runs)
}

/// The archive is open and its locations couldn't be read: why, and a way to read them again.
pub fn locations_failed(why: &str, theme: &SbTheme, cx: &Context<MainArea>) -> AnyElement {
    let c = &theme.colors;
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            state(
                alert_tile(SbIcon::CircleAlert, theme),
                "The library can't be read",
                theme,
            )
            .child(div().text_color(c.fg2).child(why.to_string()))
            .child(
                div().flex().child(
                    button("locations-retry", ButtonKind::Primary, false, true, theme)
                        .on_click(cx.listener(|_, _, _, cx| {
                            let sync = cx.services().sync.clone();
                            sync.update(cx, SyncService::retry);
                        }))
                        .child("Try again")
                        .automation_target("locations/retry"),
                ),
            )
            .automation_target("locations/failed"),
        )
        .into_any_element()
}
