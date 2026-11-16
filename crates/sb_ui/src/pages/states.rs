//! What the main area shows in place of a library page: why the locations couldn't be read,
//! and the page a session opens on until the reader is built (PLAN §13, phase 4); and the
//! pieces the archive-unavailable page ([`super::unavailable`]) is drawn with.

use gpui::{
    AnyElement, Context, Hsla, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, TextRun, div, font,
};
use sb_core::catalogue::Session;

use super::MainArea;
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, button, icon, icon_button};
use crate::format;
use crate::icons::SbIcon;
use crate::keymap;
use crate::services::library::title_of;
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

/// The page an opened session shows until the reader is built: a header with Back and where
/// the session was opened from, and what the library knows of the session.
pub fn session(
    session: &Session,
    from: SharedString,
    theme: &SbTheme,
    cx: &Context<MainArea>,
) -> AnyElement {
    let c = &theme.colors;
    let title: SharedString = title_of(session).into();
    let agent = agents::name(&session.id.provider, cx);
    let model = agents::model(&session.id.provider, &session.models, cx);
    let when = session
        .ended
        .or(session.started)
        .map(|ended| format!("{} · ", format::day(ended, format::now())))
        .unwrap_or_default();
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .h(theme.sizes.reader_header_height)
                .px(theme.spacing.lg)
                .border_b_1()
                .border_color(c.line_subtle)
                .whitespace_nowrap()
                .child(
                    icon_button("session-back", theme.sizes.control_height, theme)
                        .aria_label(format!("Back to {from} ({})", keymap::BACK.label()))
                        .on_click(cx.listener(|_, _, _, cx| {
                            let library = cx.services().library.clone();
                            library.update(cx, |library, cx| library.close(cx));
                        }))
                        .child(icon(SbIcon::ArrowLeft, theme.sizes.icon, c.fg2))
                        .automation_target("session/back"),
                )
                .child(div().flex_none().text_color(c.fg3).child(from))
                .child(icon(SbIcon::ChevronRight, theme.sizes.icon_xs, c.fg3))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_spec(&theme.text.title)
                        .child(title.clone())
                        .automation_target("session/title"),
                ),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .justify_center()
                .pt(theme.sizes.first_run_top)
                .child(
                    state(
                        div()
                            .flex()
                            .items_center()
                            .gap(theme.spacing.md)
                            .text_color(c.fg2)
                            .child(agents::mark(&session.id.provider, theme.sizes.icon, cx))
                            .child(
                                std::iter::once(agent)
                                    .chain(model)
                                    .collect::<Vec<_>>()
                                    .join(" · "),
                            )
                            .into_any_element(),
                        title,
                        theme,
                    )
                    .children(session.project_path.clone().map(|path| {
                        div()
                            .text_color(c.fg2)
                            .text_spec(&theme.text.path)
                            .child(path)
                    }))
                    .child(
                        div()
                            .text_color(c.fg3)
                            .text_spec(&theme.text.number)
                            .child(format!(
                                "{when}{}",
                                format::plural(session.all.calls as usize, "call")
                            )),
                    )
                    .child(
                        div().text_color(c.fg2).child(
                            "This session is archived. Reading it here comes with the reader.",
                        ),
                    ),
                ),
        )
        .automation_target("session")
}
