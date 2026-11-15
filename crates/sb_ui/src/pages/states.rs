//! What the main area shows in place of a library page: why the archive didn't open, and the
//! page a session opens on until the reader is built (PLAN §13, phase 4).

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
};
use sb_core::catalogue::Session;

use super::MainArea;
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, button, icon};
use crate::format;
use crate::icons::SbIcon;
use crate::keymap;
use crate::services::ActiveServices as _;
use crate::theme::{SbTheme, StyledText as _};

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

/// The archive didn't open (PLAN §4.4): where it is, why, and that nothing was made in its
/// place. The whole screen of DESIGN §4.14, with locating the archive, comes with Settings.
pub fn archive_failed(
    folder: String,
    why: String,
    theme: &SbTheme,
    cx: &Context<MainArea>,
) -> AnyElement {
    let c = &theme.colors;
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            state(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(theme.sizes.page_tile)
                    .rounded(theme.radii.tile)
                    .bg(c.red_soft)
                    .child(icon(SbIcon::CircleAlert, px(26.), c.red))
                    .into_any_element(),
                "The archive can't be opened",
                theme,
            )
            .child(div().text_spec(&theme.text.path).child(folder))
            .child(div().text_color(c.fg2).child(why))
            .child(div().text_color(c.fg2).child(
                "scrollback won't make a new archive in its place, and syncing is paused until \
                 the archive is back. Nothing at your locations is affected.",
            ))
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

/// The page an opened session shows until the reader is built: a header with Back and where
/// the session was opened from, and what the library knows of the session.
pub fn session(
    session: &Session,
    from: SharedString,
    theme: &SbTheme,
    cx: &Context<MainArea>,
) -> AnyElement {
    let c = &theme.colors;
    let title: SharedString = session
        .title
        .clone()
        .unwrap_or_else(|| "Untitled session".to_string())
        .into();
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
                .h(px(44.))
                .px(theme.spacing.lg)
                .border_b_1()
                .border_color(c.line_subtle)
                .whitespace_nowrap()
                .child(
                    div()
                        .id("session-back")
                        .role(Role::Button)
                        .aria_label(format!("Back to {from} ({})", keymap::BACK.label()))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(theme.sizes.control_height)
                        .rounded(theme.radii.control)
                        .cursor_pointer()
                        .hover(|style| style.bg(c.s2))
                        .on_click(cx.listener(|_, _, _, cx| {
                            let library = cx.services().library.clone();
                            library.update(cx, |library, cx| library.close(cx));
                        }))
                        .child(icon(SbIcon::ArrowLeft, px(16.), c.fg2))
                        .automation_target("session/back"),
                )
                .child(div().flex_none().text_color(c.fg3).child(from))
                .child(icon(SbIcon::ChevronRight, px(12.), c.fg3))
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
                            .child(agents::mark(&session.id.provider, px(16.), cx))
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
