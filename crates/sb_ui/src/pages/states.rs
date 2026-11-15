//! What the main area shows in place of a library page: why the archive didn't open, and why
//! the locations couldn't be read.

use gpui::{
    AnyElement, Context, Hsla, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, TextRun, div, font,
    prelude::FluentBuilder as _,
};

use super::MainArea;
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, button, icon};
use crate::icons::SbIcon;
use crate::services::archive::Failure;
use crate::services::{ActiveServices as _, SyncService};
use crate::theme::{SbTheme, StyledText as _};

/// What the screen says when another scrollback has the archive.
const LOCKED: &str = "Another scrollback has this archive open. Close that one, or carry on \
                      there, then try again.";
/// What it says when the archive is not there to open, or is not what it was (DESIGN §4.14).
const NOT_REPLACED: &str = "scrollback won't make a new archive in its place, and syncing is \
                            paused until the archive is back. Nothing at your locations is \
                            affected.";

/// A centred column under a tile and a title, as the edge states are drawn (DESIGN §4.14).
fn state(tile: AnyElement, title: impl Into<SharedString>, theme: &SbTheme) -> gpui::Div {
    div()
        .flex_none()
        .w(theme.sizes.state_width)
        .flex()
        .flex_col()
        .gap(theme.spacing.lg)
        .child(tile)
        .child(div().text_spec(&theme.text.state_title).child(title.into()))
}

/// The `red-soft` tile of a state that is a failure.
fn alert_tile(theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(theme.sizes.page_tile)
        .rounded(theme.radii.tile)
        .bg(c.red_soft)
        .child(icon(SbIcon::CircleAlert, theme.sizes.tile_glyph, c.red))
        .into_any_element()
}

/// `text` in `color`, with every `path` in it set in the code font.
fn with_path(text: &str, path: &str, color: Hsla, theme: &SbTheme) -> StyledText {
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

/**
 * The archive didn't open (PLAN §4.4): where it is, why, and that nothing was made in its
 * place. Its folder is written once: in the reason when the reason names it, on a line of its
 * own when it doesn't. The whole screen of DESIGN §4.14, with locating the archive, comes with
 * Settings.
 */
pub fn archive_failed(
    folder: String,
    failure: &Failure,
    theme: &SbTheme,
    cx: &Context<MainArea>,
) -> AnyElement {
    let c = &theme.colors;
    let names_folder = failure.why.contains(&folder);
    // The archive's words are a sentence here.
    let why = format!("{}.", failure.why.trim_end_matches('.'));
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            state(alert_tile(theme), "The archive can't be opened", theme)
                .when(failure.locked || !names_folder, |this| {
                    this.child(div().text_spec(&theme.text.path).child(folder.clone()))
                })
                .map(|this| {
                    if failure.locked {
                        this.child(div().text_color(c.fg2).child(LOCKED))
                    } else {
                        this.child(
                            div()
                                .text_color(c.fg2)
                                .child(with_path(&why, &folder, c.fg2, theme)),
                        )
                        .child(div().text_color(c.fg2).child(NOT_REPLACED))
                    }
                })
                .child(
                    div().flex().child(
                        button("archive-retry", ButtonKind::Primary, false, true, theme)
                            .on_click(cx.listener(|_, _, _, cx| {
                                let archive = cx.services().archive.clone();
                                archive.update(cx, |archive, cx| archive.retry(cx));
                            }))
                            .child("Try again")
                            .automation_target("archive/retry"),
                    ),
                )
                .automation_target("archive/failed"),
        )
        .into_any_element()
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
            state(alert_tile(theme), "The library can't be read", theme)
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
