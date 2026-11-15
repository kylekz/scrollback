//! The root view (PLAN §11 `shell`, DESIGN §2): the title bar, then the sidebar and the main
//! area side by side, over the window material (DESIGN §3).
//!
//! The shell keeps its window in step with [`crate::appearance`]: it tracks it with
//! `gpui_appearance`, which applies the resolved material and light/dark appearance to the
//! window and reports the OS's light/dark changes; and it repaints whenever
//! [`crate::theme::install`] puts a new [`SbTheme`] in place.
//!
//! **Narrow windows** (DESIGN §2). Below 1040px the sidebar leaves the layout
//! ([`SidebarPlacement::Collapsed`]) and a button appears left of the mark. The button and
//! `Ctrl B` show the sidebar over the main area ([`SidebarPlacement::Overlay`]). The button or
//! `Ctrl B` again, `Esc`, the window growing back, or a press of any mouse button outside it
//! closes it: on the main area or on the title bar, except on the title bar's own buttons (the
//! sidebar's, which toggles, and Windows' caption buttons, which are the OS's).
//!
//! **Keyboard.** It rests on the main area ([`MainArea`]'s focus handle), inside the shell's key
//! context, so the shell's keys and the shown page's work at once. `F6` moves it between the
//! sidebar and the main area, and so does `Ctrl B` while the sidebar is docked (DESIGN §4.1);
//! `Enter` in the sidebar hands it to the page. `Tab` walks the title bar's buttons, the
//! sidebar and its buttons, then the main area and the page's buttons, in that order
//! (DESIGN §4.1), and `Enter` or `Space` presses the button that has the keyboard. The main
//! area is the window's `Main` landmark, which is what assistive technology announces while
//! nothing in it has the keyboard.
//!
//! **The keyboard is always on something drawn.** When what had it leaves the window (a
//! button of a group that was filed, a sidebar that was hidden), it goes to the nearest thing
//! around it that is still there, or to the main area. A folder renamed while the sidebar is
//! hidden shows the sidebar over the main area for its row.
//!
//! The library's own keys work wherever the keyboard is: `Ctrl Shift N` makes a folder, `F2`
//! renames the folder whose page is shown, `F5` syncs every location, `Alt ←` goes back from
//! an opened session (DESIGN §5).

pub mod title_bar;

#[cfg(test)]
mod tests;

use anyhow::{Context as _, Result};
use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Size, Styled as _,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions, deferred,
    div, point, prelude::FluentBuilder as _, px, size,
};
use gpui_appearance::Backing;
use gpui_component::Root;

use crate::actions::library::{Back, NewFolder, SyncAll};
use crate::actions::shell::{Dismiss, FocusOther, ToggleSidebar};
use crate::actions::sidebar::Rename;
use crate::automation::AutomationTargetExt as _;
use crate::keymap;
use crate::pages::{MainArea, Showing};
use crate::services::ActiveServices as _;
use crate::sidebar::{Sidebar, SidebarEvent};
use crate::theme::{ActiveTheme as _, Material, SbTheme, StyledText as _};

/// Where the sidebar is (module docs, "Narrow windows").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarPlacement {
    /// Beside the main area.
    Docked,
    /// Hidden: the window is too narrow to dock it.
    Collapsed,
    /// Over the main area of a narrow window.
    Overlay,
}

impl SidebarPlacement {
    /// `"docked"`, `"collapsed"` or `"overlay"`, as automation's `state` spells it.
    pub fn key(self) -> &'static str {
        match self {
            Self::Docked => "docked",
            Self::Collapsed => "collapsed",
            Self::Overlay => "overlay",
        }
    }
}

/// The root view of the main window.
pub struct Shell {
    /// Keeps the window's material and light/dark in step (module docs).
    appearance: gpui_appearance::WindowTracker,
    sidebar: Entity<Sidebar>,
    /// Where the keyboard rests (module docs).
    main: Entity<MainArea>,
    /// Whether a narrow window shows the sidebar over its main area.
    sidebar_open: bool,
    _observers: [Subscription; 6],
}

impl Shell {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let main = cx.new(MainArea::new);
        main.focus_handle(cx).focus(window, cx);
        let sidebar = cx.new(|cx| Sidebar::new(window, cx));
        let services = cx.services();
        let (archive, sync, library) = (
            services.archive.clone(),
            services.sync.clone(),
            services.library.clone(),
        );
        Self {
            // gpui opened the window light or dark as the OS is; the theme may say otherwise.
            appearance: gpui_appearance::track_window(Backing::Solid, window, cx),
            sidebar_open: false,
            _observers: [
                cx.observe_global_in::<SbTheme>(window, |_, _, cx| cx.notify()),
                // The title bar follows the archive, the locations and the passes.
                cx.observe(&archive, |_, _, cx| cx.notify()),
                cx.observe(&sync, |_, _, cx| cx.notify()),
                cx.observe(&library, |_, _, cx| cx.notify()),
                cx.subscribe_in(&sidebar, window, |this, _, event, window, cx| {
                    match event {
                        // Picking a row closes a narrow window's overlay (DESIGN §2), and
                        // the keyboard goes to the page with it.
                        SidebarEvent::Picked => {
                            if std::mem::take(&mut this.sidebar_open) {
                                this.main.focus_handle(cx).focus(window, cx);
                            }
                        }
                        SidebarEvent::Open => {
                            this.sidebar_open = false;
                            this.main.focus_handle(cx).focus(window, cx);
                        }
                        // The row that is renamed has the keyboard, so it must show.
                        SidebarEvent::Renaming => {
                            this.sidebar_open = this
                                .sidebar_placement(window.viewport_size().width, cx)
                                != SidebarPlacement::Docked;
                        }
                    }
                    cx.notify();
                }),
                // What had the keyboard is gone from the window (module docs).
                cx.on_focus_lost(window, |this, window, cx| {
                    window
                        .focus_lost_restore_target(cx)
                        .unwrap_or_else(|| this.main.focus_handle(cx))
                        .focus(window, cx);
                }),
            ],
            sidebar,
            main,
        }
    }

    /// The material applied to the window.
    pub fn applied_material(&self) -> Material {
        self.appearance.applied().material
    }

    /// Where the sidebar is in a window `window_width` wide.
    pub fn sidebar_placement(&self, window_width: gpui::Pixels, cx: &App) -> SidebarPlacement {
        if window_width >= cx.sb_theme().sizes.sidebar_collapse_width {
            SidebarPlacement::Docked
        } else if self.sidebar_open {
            SidebarPlacement::Overlay
        } else {
            SidebarPlacement::Collapsed
        }
    }

    /// What the main area shows.
    pub fn showing(&self, cx: &App) -> Showing {
        Showing::now(cx)
    }

    /// Shows or hides a narrow window's sidebar, the keyboard going with it. A docked
    /// sidebar always shows, so there the keyboard alone moves.
    fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_placement(window.viewport_size().width, cx) == SidebarPlacement::Docked {
            return self.focus_other(&FocusOther, window, cx);
        }
        self.sidebar_open = !self.sidebar_open;
        if self.sidebar_open {
            self.sidebar.focus_handle(cx).focus(window, cx);
        } else {
            self.main.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// Moves the keyboard between the sidebar and the main area. A narrow window shows its
    /// sidebar to give it the keyboard, and hides it again when the keyboard leaves.
    fn focus_other(&mut self, _: &FocusOther, window: &mut Window, cx: &mut Context<Self>) {
        let sidebar = self.sidebar.focus_handle(cx);
        if sidebar.contains_focused(window, cx) {
            self.sidebar_open = false;
            self.main.focus_handle(cx).focus(window, cx);
        } else {
            self.sidebar_open = self.sidebar_placement(window.viewport_size().width, cx)
                != SidebarPlacement::Docked;
            sidebar.focus(window, cx);
        }
        cx.notify();
    }

    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_open {
            self.sidebar_open = false;
            self.main.focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    fn new_folder(&mut self, _: &NewFolder, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.new_folder(false, cx));
    }

    fn rename(&mut self, _: &Rename, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.rename_shown(window, cx));
    }

    pub(super) fn sync_all(&mut self, _: &SyncAll, _: &mut Window, cx: &mut Context<Self>) {
        let sync = cx.services().sync.clone();
        sync.update(cx, |sync, cx| sync.sync_all(cx));
    }

    fn back(&mut self, _: &Back, _: &mut Window, cx: &mut Context<Self>) {
        let library = cx.services().library.clone();
        library.update(cx, |library, cx| library.close(cx));
    }

    /// The main area (DESIGN §2): where the pages and the reader go. Always solid, since
    /// reading never sits on a material (DESIGN §3).
    fn render_main(&self, theme: &SbTheme) -> AnyElement {
        div()
            .id("main")
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.colors.bg_app)
            .debug_selector(|| "main".into())
            .child(self.main.clone())
            .automation_target("shell/main")
    }

    /// The sidebar on `background`: `bg-side` docked, its solid value as an overlay
    /// (DESIGN §2).
    fn render_sidebar(&self, background: gpui::Hsla, theme: &SbTheme) -> AnyElement {
        div()
            .id("sidebar")
            .flex_none()
            .w(theme.sizes.sidebar_width)
            .h_full()
            .bg(background)
            .border_r_1()
            .border_color(theme.colors.line_subtle)
            .debug_selector(|| "sidebar".into())
            .child(self.sidebar.clone())
            .automation_target("shell/sidebar")
    }

    /// The sidebar over the main area of a narrow window, and under it a layer that takes the
    /// press outside which closes it. The layer ends at the title bar and clips the sidebar's
    /// shadow there, so the bar is neither covered nor darkened.
    fn render_sidebar_overlay(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        deferred(
            div()
                .absolute()
                .top(theme.sizes.title_bar_height)
                .bottom_0()
                .left_0()
                .right_0()
                .overflow_hidden()
                .occlude()
                .on_any_mouse_down(
                    cx.listener(|this, _, window, cx| this.dismiss(&Dismiss, window, cx)),
                )
                .child(
                    div()
                        .h_full()
                        .w(theme.sizes.sidebar_width)
                        .shadow(theme.shadows.pop.clone())
                        // A press on the sidebar itself is not a press outside it.
                        .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                        .child(self.render_sidebar(theme.colors.bg_side_overlay, theme)),
                ),
        )
        .into_any_element()
    }
}

impl Focusable for Shell {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.main.focus_handle(cx)
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let placement = self.sidebar_placement(window.viewport_size().width, cx);
        // A window grown wide enough to dock the sidebar has no overlay to come back to.
        if placement == SidebarPlacement::Docked {
            self.sidebar_open = false;
        }
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.keep_name_left_behind(window, cx));
        // A sidebar that isn't shown can't keep the keyboard.
        if placement == SidebarPlacement::Collapsed
            && self.sidebar.focus_handle(cx).contains_focused(window, cx)
        {
            self.main.focus_handle(cx).focus(window, cx);
        }
        div()
            .id("shell")
            .key_context(keymap::SHELL)
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::focus_other))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::new_folder))
            .on_action(cx.listener(Self::rename))
            .on_action(cx.listener(Self::sync_all))
            .on_action(cx.listener(Self::back))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(c.window_background)
            .text_color(c.fg)
            .text_spec(&theme.text.body)
            // First, so its capture listener runs before any other.
            .child(crate::automation::pointer_guard())
            .child(self.render_title_bar(placement, window, &theme, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .when(placement == SidebarPlacement::Docked, |this| {
                        this.child(self.render_sidebar(c.bg_side, &theme))
                    })
                    .child(self.render_main(&theme)),
            )
            .when(placement == SidebarPlacement::Overlay, |this| {
                this.child(self.render_sidebar_overlay(&theme, cx))
            })
    }
}

/// Options for the main window: centred at exactly `window_size` (logical px), DESIGN §2's
/// minimum size, a custom title bar, and the installed theme's material.
pub fn window_options(window_size: Size<gpui::Pixels>, cx: &App) -> WindowOptions {
    let sizes = &cx.sb_theme().sizes;
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            // Centred, on whole pixels: from a half pixel the platform rounds the window's edges
            // outwards and it opens a pixel larger than asked.
            origin: Bounds::centered(None, window_size, cx)
                .origin
                .map(|coordinate| coordinate.floor()),
            size: window_size,
        })),
        titlebar: Some(TitlebarOptions {
            title: Some("scrollback".into()),
            // Hides the system title bar; the shell draws its own (`title_bar`).
            appears_transparent: true,
            // macOS: centred on the 40px title bar.
            traffic_light_position: Some(point(px(12.), px(12.))),
        }),
        window_min_size: Some(size(sizes.window_min_width, sizes.window_min_height)),
        window_background: gpui_appearance::window_background(Backing::Solid, cx),
        app_id: Some("scrollback".into()),
        ..Default::default()
    }
}

/// The main window and its root view.
pub struct MainWindow {
    pub handle: WindowHandle<Root>,
    pub shell: Entity<Shell>,
}

/// Opens the main window, gpui-component's `Root` around a [`Shell`], through
/// [`crate::automation::open_window`] like every scrollback window. Call after
/// [`crate::appearance::init`], so the window opens in the resolved theme and material.
pub fn open_main_window(window_size: Size<gpui::Pixels>, cx: &mut App) -> Result<MainWindow> {
    let options = window_options(window_size, cx);
    let mut shell = None;
    let handle = crate::automation::open_window(cx, options, |window, cx| {
        let view = cx.new(|cx| Shell::new(window, cx));
        shell = Some(view.clone());
        cx.new(|cx| Root::new(view, window, cx))
    })
    .context("opening the main window")?;
    let shell = shell.context("the main window built no shell")?;
    Ok(MainWindow { handle, shell })
}
