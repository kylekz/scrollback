//! The custom title bar (DESIGN §2, §4.1): 40px of `chrome` holding scrollback's mark, the
//! search field, Sync all, Settings and, on Windows, the caption buttons. The whole bar is a
//! window drag region; its controls occlude it.
//!
//! **Room.** The bar is three groups: the mark's, the search field, and the buttons'. The two
//! outer groups share what the field leaves equally, which centres the field on the window; a
//! group that needs more than its half keeps its own width and the field moves over by the
//! difference. Each outer group's padding is on an inner box, so the two shares start equal.
//!
//! Search needs the index (PLAN §13 phase 6), Sync all a location (phase 3) and Settings its
//! page (phase 7): until then each is drawn disabled and takes no clicks.
//!
//! The caption buttons are gpui `WindowControlArea`s, so Windows itself handles their clicks,
//! hover and Snap Layouts on Maximize (the pattern of Zed's `platform_title_bar`). macOS draws
//! its traffic lights over the left edge, which the bar leaves room for.

use gpui::{
    AnyElement, Context, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    RenderOnce, Role, StatefulInteractiveElement as _, Styled as _, Window, WindowControlArea, div,
    prelude::FluentBuilder as _, px,
};

use super::{Shell, SidebarPlacement};
use crate::actions::shell::ToggleSidebar;
use crate::automation::AutomationTargetExt as _;
use crate::components::{DISABLED_OPACITY, icon, kbd};
use crate::icons::SbIcon;
use crate::keymap;
use crate::theme::{SbTheme, StyledText as _};

/// The gap between the bar's items, and its left padding where no traffic lights sit
/// (`design/project/sb.css`, `.titlebar`).
const GAP: Pixels = px(10.);
const EDGE: Pixels = px(12.);

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
        let caption_buttons = caption_buttons(window);
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
            .debug_selector(|| "titlebar".into())
            .child(
                div().flex_1().flex().child(
                    div()
                        .flex()
                        .items_center()
                        .gap(GAP)
                        .pl(if cfg!(target_os = "macos") {
                            theme.sizes.traffic_lights_inset
                        } else {
                            EDGE
                        })
                        .when(sidebar != SidebarPlacement::Docked, |this| {
                            this.child(sidebar_button(sidebar, theme, cx))
                        })
                        .child(mark(theme)),
                ),
            )
            .child(search_field(theme))
            .child(
                div().flex_1().flex().justify_end().child(
                    div()
                        .flex()
                        .items_center()
                        .gap(GAP)
                        // The caption buttons sit flush with the window's edge.
                        .when(caption_buttons.is_empty(), |this| this.pr(EDGE))
                        .child(sync_all_button(theme))
                        .child(settings_button(theme))
                        .when(!caption_buttons.is_empty(), |this| {
                            this.child(WindowControls::new(caption_buttons, theme))
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
    let (hover, pressed) = (c.s2, c.s3);
    div()
        .id("titlebar-sidebar")
        .role(Role::Button)
        .aria_label(format!(
            "{} sidebar ({})",
            if open { "Hide" } else { "Show" },
            keymap::label("secondary-b")
        ))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(theme.sizes.control_height)
        .rounded(theme.radii.control)
        // A control inside the drag region.
        .occlude()
        .cursor_pointer()
        .when(open, |this| this.bg(pressed))
        .hover(move |style| style.bg(hover))
        .on_click(
            cx.listener(|this, _, window, cx| this.toggle_sidebar(&ToggleSidebar, window, cx)),
        )
        .child(icon(
            SbIcon::PanelLeft,
            px(16.),
            if open { c.fg } else { c.fg2 },
        ))
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
/// palette (`Ctrl K`). Disabled (module docs).
fn search_field(theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    div()
        .id("titlebar-search")
        .role(Role::Button)
        .aria_label(format!(
            "Search the archive ({})",
            keymap::label("secondary-k")
        ))
        .flex_none()
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
        .opacity(DISABLED_OPACITY)
        .debug_selector(|| "titlebar-search".into())
        .child(icon(SbIcon::Search, px(14.), c.fg3))
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .whitespace_nowrap()
                .child("Search titles, prompts, replies and commands"),
        )
        .child(kbd(keymap::label("secondary-k"), theme))
        .automation_target("titlebar/search")
}

/// Sync all (DESIGN §4.1): a small button that syncs every location. Disabled (module docs).
fn sync_all_button(theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    div()
        .id("titlebar-sync-all")
        .role(Role::Button)
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
        .opacity(DISABLED_OPACITY)
        .child(icon(SbIcon::RefreshCw, px(14.), c.fg))
        .child("Sync all")
        .automation_target("titlebar/sync-all")
}

/// Settings (DESIGN §4.1): a ghost icon button that opens Settings in the main area
/// (`Ctrl ,`). Disabled (module docs).
fn settings_button(theme: &SbTheme) -> AnyElement {
    div()
        .id("titlebar-settings")
        .role(Role::Button)
        .aria_label(format!("Settings ({})", keymap::label("secondary-,")))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(theme.sizes.control_height)
        .rounded(theme.radii.control)
        .opacity(DISABLED_OPACITY)
        .child(icon(SbIcon::Settings, px(16.), theme.colors.fg2))
        .automation_target("titlebar/settings")
}

/// Minimize, maximize/restore and close: 46 wide and the title bar's height.
#[derive(IntoElement)]
struct WindowControls {
    buttons: Vec<CaptionButton>,
    glyph: Hsla,
    hover: Hsla,
    close_hover: Hsla,
    close_glyph: Hsla,
    width: Pixels,
    height: Pixels,
}

impl WindowControls {
    fn new(buttons: Vec<CaptionButton>, theme: &SbTheme) -> Self {
        let c = &theme.colors;
        Self {
            buttons,
            glyph: c.fg2,
            hover: c.s2,
            close_hover: c.caption_close_hover,
            close_glyph: c.caption_close_fg,
            width: theme.sizes.caption_button_width,
            height: theme.sizes.title_bar_height,
        }
    }

    fn button(&self, button: CaptionButton) -> AnyElement {
        let (hover_bg, hover_glyph) = match button {
            CaptionButton::Close => (self.close_hover, self.close_glyph),
            _ => (self.hover, self.glyph),
        };
        let group = button.id();
        div()
            .id(button.id())
            .role(Role::Button)
            .aria_label(button.label())
            .group(group)
            .flex()
            .items_center()
            .justify_center()
            .w(self.width)
            .h(self.height)
            // Keep the drag region from claiming the hit test.
            .occlude()
            // Windows handles the click through the control area.
            .window_control_area(button.control_area())
            .hover(move |style| style.bg(hover_bg))
            .active(move |style| style.bg(hover_bg.opacity(0.8)))
            .child(
                gpui::svg()
                    .path(button.icon().path())
                    .size(px(10.))
                    .text_color(self.glyph)
                    .group_hover(group, move |style| style.text_color(hover_glyph)),
            )
            .automation_target(button.target())
    }
}

impl RenderOnce for WindowControls {
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        div()
            .id("window-controls")
            .flex_none()
            .flex()
            .flex_row()
            .children(self.buttons.iter().map(|button| self.button(*button)))
    }
}

/// The caption buttons `window` shows ([`caption_buttons_for`] on this OS).
fn caption_buttons(window: &Window) -> Vec<CaptionButton> {
    caption_buttons_for(
        std::env::consts::OS,
        window.is_fullscreen(),
        window.is_maximized(),
    )
}

/// The caption buttons the title bar shows on `os` (`std::env::consts::OS`), left to right: on
/// Windows all three, none in fullscreen; none on macOS, whose traffic lights are the system's.
fn caption_buttons_for(os: &str, fullscreen: bool, maximized: bool) -> Vec<CaptionButton> {
    if os != "windows" || fullscreen {
        return Vec::new();
    }
    vec![
        CaptionButton::Minimize,
        if maximized {
            CaptionButton::Restore
        } else {
            CaptionButton::Maximize
        },
        CaptionButton::Close,
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptionButton {
    Minimize,
    Maximize,
    Restore,
    Close,
}

impl CaptionButton {
    fn id(self) -> &'static str {
        match self {
            Self::Minimize => "caption-minimize",
            Self::Maximize => "caption-maximize",
            Self::Restore => "caption-restore",
            Self::Close => "caption-close",
        }
    }

    fn icon(self) -> SbIcon {
        match self {
            Self::Minimize => SbIcon::WindowMinimize,
            Self::Maximize => SbIcon::WindowMaximize,
            Self::Restore => SbIcon::WindowRestore,
            Self::Close => SbIcon::WindowClose,
        }
    }

    /// The automation target. Maximize and Restore are one button in two states.
    fn target(self) -> &'static str {
        match self {
            Self::Minimize => "titlebar/min",
            Self::Maximize | Self::Restore => "titlebar/max",
            Self::Close => "titlebar/close",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Minimize => "Minimize",
            Self::Maximize => "Maximize",
            Self::Restore => "Restore",
            Self::Close => "Close",
        }
    }

    fn control_area(self) -> WindowControlArea {
        match self {
            Self::Minimize => WindowControlArea::Min,
            Self::Maximize | Self::Restore => WindowControlArea::Max,
            Self::Close => WindowControlArea::Close,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_buttons_follow_the_platform() {
        use CaptionButton::{Close, Maximize, Minimize, Restore};

        assert_eq!(
            caption_buttons_for("windows", false, false),
            [Minimize, Maximize, Close]
        );
        assert_eq!(
            caption_buttons_for("windows", false, true),
            [Minimize, Restore, Close]
        );
        assert_eq!(caption_buttons_for("windows", true, false), []);
        // macOS: the traffic lights are the system's.
        assert_eq!(caption_buttons_for("macos", false, false), []);
    }
}
