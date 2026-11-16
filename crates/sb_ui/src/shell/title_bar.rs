//! The custom title bar (DESIGN §2, §4.1): 40px of `chrome` holding scrollback's mark, the
//! search field, Sync all, Settings and, on Windows, the caption buttons. The whole bar is a
//! window drag region; its controls occlude it.
//!
//! **Room.** The bar is three groups: the mark's, the search field, and the buttons'. The two
//! outer groups share what the field leaves equally, which centres the field on the window
//! (DESIGN §2). Each is at least as wide as the wider of the two needs, so where the caption
//! buttons leave too little room the field gives up width rather than its place. Each outer
//! group's padding is on an inner box, so the two shares start equal.
//!
//! **States** (DESIGN §4.1). The first run shows the mark alone: no search, Sync all or
//! Settings (DESIGN §4.10). With a library, Sync all syncs every location, and while a pass
//! runs its glyph turns, it reads "Syncing…", and neither a click nor `Tab` stops on it.
//! While the archive is opening or didn't open, it is drawn disabled (DESIGN §4.14), and so is
//! the search field, which otherwise opens the palette. Settings needs its page (phase 7):
//! until then it is drawn disabled and takes no clicks.
//!
//! **Caption buttons** (Windows). Each is a 46px-wide box, the bar's height, marked with gpui's
//! `WindowControlArea` (`Min`, `Max`, `Close`), so Windows itself answers the hit test and
//! handles the click, the hover and Snap Layouts on Maximize; `occlude` keeps the bar's drag
//! region from claiming the same pixels. The arrangement follows gpui-component 0.9.0's
//! `TitleBar` (`src/title_bar.rs`, Apache-2.0), drawn in scrollback's tokens. macOS draws its
//! traffic lights over the left edge, which the bar leaves room for except in fullscreen, where
//! they are hidden.

use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    RenderOnce, Role, StatefulInteractiveElement as _, Styled as _, Window, WindowControlArea, div,
    prelude::FluentBuilder as _, px, svg,
};

use super::{Shell, SidebarPlacement};
use crate::actions::library::SyncAll;
use crate::actions::shell::{Dismiss, Search, ToggleSidebar};
use crate::automation::AutomationTargetExt as _;
use crate::components::{DISABLED_OPACITY, icon, kbd, sync_glyph, tab_stop, text_width};
use crate::icons::SbIcon;
use crate::keymap;
use crate::pages::Showing;
use crate::services::ActiveServices as _;
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// The gap between the bar's items (`design/project/sb.css`, `.titlebar`).
const GAP: Pixels = px(10.);

impl Shell {
    /// The title bar of the main window.
    pub(super) fn render_title_bar(
        &self,
        sidebar: SidebarPlacement,
        window: &mut Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let showing = Showing::now(cx);
        let first_run = showing == Showing::FirstRun;
        let has_library = showing.is_library();
        let syncing = cx.services().sync.read(cx).is_syncing();
        let traffic_lights = cfg!(target_os = "macos") && !window.is_fullscreen();
        let captions = CaptionButton::row(window);
        // The room each outer group needs; both get the larger, so the field stays centred.
        let left = if traffic_lights {
            theme.sizes.traffic_lights_inset
        } else {
            theme.spacing.lg
        } + if sidebar == SidebarPlacement::Docked {
            px(0.)
        } else {
            theme.sizes.control_height + GAP
        } + px(18.)
            + theme.spacing.md
            + text_width("scrollback", &theme.text.title, window);
        let buttons = (!first_run).then(|| {
            sync_all_width(has_library && syncing, theme, window) + GAP + theme.sizes.control_height
        });
        let right = match (buttons, captions) {
            (Some(buttons), Some(_)) => buttons + GAP + theme.sizes.caption_button_width * 3.,
            (None, Some(_)) => theme.sizes.caption_button_width * 3.,
            (Some(buttons), None) => buttons + theme.spacing.lg,
            (None, None) => theme.spacing.lg,
        };
        let side = left.max(right);
        div()
            .id("titlebar")
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(GAP)
            .w_full()
            .h(theme.sizes.title_bar_height)
            .bg(c.chrome)
            .border_b_1()
            .border_color(c.line_subtle)
            .window_control_area(WindowControlArea::Drag)
            // A press on the bar is a press outside the sidebar's overlay; the bar's buttons
            // occlude it, so a press on one of them isn't.
            .when(sidebar == SidebarPlacement::Overlay, |this| {
                this.on_any_mouse_down(
                    cx.listener(|this, _, window, cx| this.dismiss(&Dismiss, window, cx)),
                )
            })
            .debug_selector(|| "titlebar".into())
            .child(
                div().flex_1().flex_basis(px(0.)).min_w(side).flex().child(
                    div()
                        .flex()
                        .items_center()
                        .gap(GAP)
                        .pl(if traffic_lights {
                            theme.sizes.traffic_lights_inset
                        } else {
                            theme.spacing.lg
                        })
                        .when(sidebar != SidebarPlacement::Docked, |this| {
                            this.child(sidebar_button(sidebar, theme, cx))
                        })
                        .child(mark(theme)),
                ),
            )
            .when(!first_run, |this| {
                this.child(search_field(has_library, theme, cx))
            })
            .child(
                div()
                    .flex_1()
                    .flex_basis(px(0.))
                    .min_w(side)
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(GAP)
                            .when(!first_run, |this| {
                                this.child(sync_all_button(has_library, syncing, theme, cx))
                                    .child(settings_button(theme))
                            })
                            // The caption buttons sit side by side, flush with the window's edge.
                            .map(|this| match captions {
                                Some(buttons) => this.child(div().flex().children(buttons)),
                                None => this.pr(theme.spacing.lg),
                            }),
                    ),
            )
            .automation_target("titlebar")
    }
}

/// The button that shows the collapsed sidebar over the main area, or closes it (`Ctrl B`,
/// DESIGN §2).
fn sidebar_button(
    sidebar: SidebarPlacement,
    theme: &SbTheme,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let c = &theme.colors;
    let open = sidebar == SidebarPlacement::Overlay;
    // Open, it is on (`s3`) whether or not the pointer is over it.
    let fill = if open { c.s3 } else { c.s2 };
    tab_stop(div().id("titlebar-sidebar"), theme)
        .debug_selector(|| "titlebar-sidebar".into())
        .role(Role::Button)
        .aria_label(format!(
            "{} sidebar ({})",
            if open { "Hide" } else { "Show" },
            keymap::TOGGLE_SIDEBAR.label()
        ))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(theme.sizes.control_height)
        .rounded(theme.radii.control)
        .border_1()
        .border_color(gpui::transparent_black())
        // A control inside the drag region.
        .occlude()
        .cursor_pointer()
        .when(open, |this| this.bg(fill))
        .hover(move |style| style.bg(fill))
        .on_click(
            cx.listener(|this, _, window, cx| this.toggle_sidebar(&ToggleSidebar, window, cx)),
        )
        .child(icon(
            SbIcon::PanelLeft,
            theme.sizes.icon,
            if open { c.fg } else { c.fg2 },
        ))
        // Resting on it mid-drag shows the sidebar (`crate::shell`'s spring-loading).
        .map(|this| crate::drag::target(this, crate::services::DropTarget::Sidebar))
        .automation_target("titlebar/sidebar")
}

/// scrollback's mark (DESIGN §2): an 18px `accent` tile with three `accent-ink` bars at 100, 70
/// and 45% of its inner width, then the name at 600.
fn mark(theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    let bar = |width: f32| {
        div()
            .w(px(width))
            .h(px(2.))
            .rounded(px(1.))
            .bg(c.accent_ink)
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .debug_selector(|| "titlebar-mark".into())
        .child(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(2.))
                .size(px(18.))
                .px(theme.spacing.xs)
                .rounded(px(5.))
                .bg(c.accent)
                .child(bar(10.))
                .child(bar(7.))
                .child(bar(4.5)),
        )
        .child(
            div()
                .text_spec(&theme.text.title)
                .text_color(c.fg)
                .child("scrollback"),
        )
        .automation_target("titlebar/mark")
}

/// The search field (DESIGN §4.1): a 460-wide button in the `field` style that opens the
/// palette (`Ctrl K`). Disabled without a library (module docs).
fn search_field(enabled: bool, theme: &SbTheme, cx: &mut Context<Shell>) -> AnyElement {
    let c = &theme.colors;
    div()
        .id("titlebar-search")
        .role(Role::Button)
        .aria_label(format!("Search the archive ({})", keymap::SEARCH.label()))
        // A control inside the drag region.
        .occlude()
        .map(|this| {
            if enabled {
                tab_stop(this, theme).cursor_pointer().on_click(cx.listener(
                    |this, _, window, cx| {
                        this.toggle_palette(&Search, window, cx);
                    },
                ))
            } else {
                this.opacity(DISABLED_OPACITY)
            }
        })
        .flex_shrink(1.)
        .min_w_0()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .w(theme.sizes.search_field_width)
        .h(theme.sizes.input_height)
        .px(px(10.))
        .rounded(theme.radii.input)
        .border_1()
        .border_color(c.line)
        .bg(c.s1)
        .text_color(c.fg3)
        .debug_selector(|| "titlebar-search".into())
        .child(icon(SbIcon::Search, theme.sizes.icon_sm, c.fg3))
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .whitespace_nowrap()
                .child("Search titles, prompts, replies and commands"),
        )
        .child(kbd(keymap::SEARCH.label(), theme))
        .automation_target("titlebar/search")
}

/// What Sync all says: whether a pass runs.
fn sync_all_label(syncing: bool) -> &'static str {
    if syncing { "Syncing…" } else { "Sync all" }
}

/// How wide [`sync_all_button`] is: its border, padding, glyph, gap and label.
fn sync_all_width(syncing: bool, theme: &SbTheme, window: &Window) -> Pixels {
    px(2.)
        + theme.spacing.md * 2.
        + theme.sizes.icon_sm
        + theme.spacing.sm
        + text_width(sync_all_label(syncing), &theme.text.control_small, window)
}

/// Sync all (DESIGN §4.1): a small button that syncs every location (`F5`). While a pass runs
/// it says so and a click does nothing; without a library it is disabled (module docs).
fn sync_all_button(
    enabled: bool,
    syncing: bool,
    theme: &SbTheme,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let c = &theme.colors;
    div()
        .id("titlebar-sync-all")
        .role(Role::Button)
        .aria_label(format!("Sync all locations ({})", keymap::SYNC_ALL.label()))
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.spacing.sm)
        .h(theme.sizes.control_small_height)
        .px(theme.spacing.md)
        .rounded(theme.radii.control)
        .border_1()
        .border_color(c.line)
        .bg(c.s2)
        .text_color(c.fg)
        .text_spec(&theme.text.control_small)
        .whitespace_nowrap()
        // A control inside the drag region.
        .occlude()
        .map(|this| {
            if !enabled {
                this.opacity(DISABLED_OPACITY)
            } else if syncing {
                // It does nothing while a pass runs, so `Tab` passes it by.
                this
            } else {
                tab_stop(this, theme)
                    .cursor_pointer()
                    .hover(|style| style.bg(c.s3))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.sync_all(&SyncAll, window, cx);
                    }))
            }
        })
        .child(sync_glyph(enabled && syncing, c.fg, theme))
        .child(sync_all_label(enabled && syncing))
        .automation_target("titlebar/sync-all")
}

/// Settings (DESIGN §4.1): a ghost icon button that opens Settings in the main area
/// (`Ctrl ,`). Disabled (module docs).
fn settings_button(theme: &SbTheme) -> AnyElement {
    div()
        .id("titlebar-settings")
        .role(Role::Button)
        .aria_label(format!(
            "Settings ({})",
            keymap::label(keymap::SETTINGS_KEYS)
        ))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(theme.sizes.control_height)
        .rounded(theme.radii.control)
        .opacity(DISABLED_OPACITY)
        .child(icon(SbIcon::Settings, theme.sizes.icon, theme.colors.fg2))
        .automation_target("titlebar/settings")
}

/// One of Windows' caption buttons (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoElement)]
enum CaptionButton {
    Minimize,
    Maximize,
    /// What Maximize becomes while the window is maximized.
    Restore,
    Close,
}

impl CaptionButton {
    /// The buttons `window` shows, left to right: all three on Windows outside fullscreen, none
    /// elsewhere.
    fn row(window: &Window) -> Option<[Self; 3]> {
        (cfg!(windows) && !window.is_fullscreen()).then(|| Self::row_when(window.is_maximized()))
    }

    fn row_when(maximized: bool) -> [Self; 3] {
        let zoom = if maximized {
            Self::Restore
        } else {
            Self::Maximize
        };
        [Self::Minimize, zoom, Self::Close]
    }
}

impl RenderOnce for CaptionButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.sb_theme();
        let c = &theme.colors;
        // Maximize and Restore are one button in two states, so they share a target and an area.
        let (label, target, glyph, area) = match self {
            Self::Minimize => (
                "Minimize",
                "titlebar/min",
                SbIcon::WindowMinimize,
                WindowControlArea::Min,
            ),
            Self::Maximize => (
                "Maximize",
                "titlebar/max",
                SbIcon::WindowMaximize,
                WindowControlArea::Max,
            ),
            Self::Restore => (
                "Restore",
                "titlebar/max",
                SbIcon::WindowRestore,
                WindowControlArea::Max,
            ),
            Self::Close => (
                "Close",
                "titlebar/close",
                SbIcon::WindowClose,
                WindowControlArea::Close,
            ),
        };
        // Close turns Windows' red under the pointer, with a white glyph; the others take `s2`.
        let (hover_fill, hover_glyph) = if self == Self::Close {
            (c.caption_close_hover, c.caption_close_fg)
        } else {
            (c.s2, c.fg2)
        };
        div()
            .id(target)
            .role(Role::Button)
            .aria_label(label)
            .group(target)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .w(theme.sizes.caption_button_width)
            .h(theme.sizes.title_bar_height)
            .occlude()
            .window_control_area(area)
            .hover(move |style| style.bg(hover_fill))
            .active(move |style| style.bg(hover_fill.opacity(0.8)))
            .child(
                // An svg takes its own text colour, not its parent's, so the hover reaches the
                // glyph through the button's group.
                svg()
                    .path(glyph.path())
                    .size(px(10.))
                    .text_color(c.fg2)
                    .group_hover(target, move |style| style.text_color(hover_glyph)),
            )
            .automation_target(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximize_becomes_restore_while_the_window_is_maximized() {
        use CaptionButton::{Close, Maximize, Minimize, Restore};

        assert_eq!(CaptionButton::row_when(false), [Minimize, Maximize, Close]);
        assert_eq!(CaptionButton::row_when(true), [Minimize, Restore, Close]);
    }
}
