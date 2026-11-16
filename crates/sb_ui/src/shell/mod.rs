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
//! an opened session, `Ctrl L` opens the locations sheet (DESIGN §5).
//!
//! **The locations sheet** ([`LocationsSheet`], DESIGN §4.11) opens over the scrim from a
//! sidebar location row, the Locations overline or `Ctrl L`, while a library page shows. It
//! keeps the keyboard while it is open, and gives it back to where it was when it closes. So
//! do the library's dialogs ([`dialogs`]).
//!
//! **Settings** (DESIGN §4.13) open in the main area from the title bar's gear or `Ctrl ,`,
//! while a library page shows; `Esc`, `Alt ←` and Back go back to the page they were opened
//! from. While a library dialog, or Settings' own confirmation, is open, `Esc` answers it and
//! the shell's other keys do nothing.
//!
//! **Another archive.** When the app moves onto another archive (Settings › Use a different
//! folder…, `crate::services::reopen`), the shell makes its sidebar and main area anew over
//! the new services, and closes the locations sheet and any dialog: the window is the same,
//! its contents are the other archive's.
//!
//! **While a drag is held** (`crate::drag`) no key but `Esc` does anything, so that nothing
//! changes under it: `Esc` gives the drag up. Resting on the sidebar's button in a narrow
//! window shows the sidebar over the main area until the drag ends, so that its folders can
//! be dropped on.
//!
//! **Closing the window, or quitting,** while the paths editor has changes to save asks first
//! (DESIGN §4.7), as leaving its page does; the window closes, or the app quits, once the owner
//! has answered.

mod dialogs;
pub mod title_bar;

#[cfg(test)]
mod tests;

use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Size, Styled as _,
    Subscription, Task, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions,
    deferred, div, point, prelude::FluentBuilder as _, px, size,
};
use gpui_appearance::Backing;
use gpui_component::Root;

use crate::actions::library::{Back, NewFolder, SyncAll};
use crate::actions::shell::{Dismiss, FocusOther, OpenLocations, OpenSettings, ToggleSidebar};
use crate::actions::sidebar::Rename;
use crate::automation::AutomationTargetExt as _;
use crate::keymap;
use crate::locations_sheet::{LocationsSheet, LocationsSheetEvent};
use crate::overlay::{self, Layer};
use crate::pages::{MainArea, Showing};
use crate::services::library::LibraryEvent;
use crate::services::{ActiveServices as _, DropTarget, Leave, Services};
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
    /// The locations sheet while it is open, with where the keyboard was before it.
    locations: Option<(
        Entity<LocationsSheet>,
        Option<FocusHandle>,
        [Subscription; 2],
    )>,
    /// The library's dialog while one is open, and where the keyboard rests in it.
    dialog: Option<dialogs::Dialog>,
    dialog_focus: FocusHandle,
    /// Showing the sidebar for a drag resting on its button, and whether it shows for one.
    springing: Option<Task<()>>,
    sprung: bool,
    /// What the shell hears from the services and the sidebar, made anew with them.
    attached: [Subscription; 6],
    _observers: [Subscription; 4],
}

/**
 * How long a drag rests on the sidebar's button before the sidebar shows (DESIGN §4.1's closed
 * folders open after as long).
 */
const SPRING: Duration = Duration::from_millis(500);

impl Shell {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (main, sidebar, attached) = Self::attach(window, cx);
        // Closing the window asks first while the paths editor has changes to save.
        window.on_window_should_close(cx, |_, cx| {
            let library = cx.services().library.clone();
            let holding = library.read(cx).held().is_some();
            if holding {
                library.update(cx, |library, cx| library.ask_before(Leave::Close, cx));
            }
            !holding
        });
        Self {
            // gpui opened the window light or dark as the OS is; the theme may say otherwise.
            appearance: gpui_appearance::track_window(Backing::Solid, window, cx),
            sidebar_open: false,
            locations: None,
            dialog: None,
            dialog_focus: cx.focus_handle(),
            springing: None,
            sprung: false,
            attached,
            _observers: [
                cx.observe_global_in::<SbTheme>(window, |_, _, cx| cx.notify()),
                // No key but `Esc` acts while a drag is held (module docs).
                cx.intercept_keystrokes(|event, _, cx| {
                    if event.keystroke.key != "escape"
                        && cx.services().library.read(cx).drag().is_some()
                    {
                        cx.stop_propagation();
                    }
                }),
                // What had the keyboard is gone from the window (module docs).
                cx.on_focus_lost(window, |this, window, cx| {
                    window
                        .focus_lost_restore_target(cx)
                        .unwrap_or_else(|| this.main.focus_handle(cx))
                        .focus(window, cx);
                }),
                // Another archive's services (module docs).
                cx.observe_global_in::<Services>(window, |this, window, cx| {
                    this.locations = None;
                    this.dialog = None;
                    this.sidebar_open = false;
                    this.springing = None;
                    this.sprung = false;
                    (this.main, this.sidebar, this.attached) = Self::attach(window, cx);
                    cx.notify();
                }),
            ],
            sidebar,
            main,
        }
    }

    /// The main area and the sidebar over the services there are now, the keyboard on the
    /// main area, and what the shell hears from them.
    fn attach(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<MainArea>, Entity<Sidebar>, [Subscription; 6]) {
        let main = cx.new(|cx| MainArea::new(window, cx));
        main.focus_handle(cx).focus(window, cx);
        let sidebar = cx.new(|cx| Sidebar::new(window, cx));
        let services = cx.services();
        let (archive, sync, library, settings) = (
            services.archive.clone(),
            services.sync.clone(),
            services.library.clone(),
            services.settings.clone(),
        );
        let attached = [
            // The title bar follows the archive, the locations, the passes and Settings.
            cx.observe(&archive, |_, _, cx| cx.notify()),
            cx.observe(&sync, |_, _, cx| cx.notify()),
            cx.observe(&settings, |_, _, cx| cx.notify()),
            cx.observe(&library, |this, _, cx| {
                this.follow_drag(cx);
                cx.notify();
            }),
            cx.subscribe_in(&library, window, |_, _, event, window, cx| {
                if let LibraryEvent::Leave(leave) = event {
                    match leave {
                        Leave::Close => window.remove_window(),
                        Leave::Quit => cx.quit(),
                        Leave::Show(_) | Leave::Open(_) | Leave::Edit(_) => {}
                    }
                }
            }),
            cx.subscribe_in(&sidebar, window, |this, _, event, window, cx| {
                match event {
                    /* Picking a row closes a narrow window's overlay (DESIGN §2), and the
                    keyboard goes to the page with it. */
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
                    SidebarEvent::Locations(selected) => {
                        this.sidebar_open = false;
                        this.open_locations(selected.clone(), window, cx);
                    }
                }
                cx.notify();
            }),
        ];
        (main, sidebar, attached)
    }

    #[cfg(test)]
    pub(crate) fn sidebar(&self) -> &Entity<Sidebar> {
        &self.sidebar
    }

    /// The main area.
    pub fn main(&self) -> &Entity<MainArea> {
        &self.main
    }

    /// The section picked in Settings' list (DESIGN §4.13), whether or not they show.
    pub fn settings_section(&self, cx: &App) -> crate::pages::settings::Section {
        self.main.read(cx).settings_page().read(cx).current()
    }

    /// Whether a dialog is open over the window: the library's, or Settings' confirmation.
    fn asking(&self, cx: &App) -> bool {
        self.dialog.is_some() || self.main.read(cx).settings_page().read(cx).is_confirming()
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
        let library = cx.services().library.clone();
        let (dragging, deleting, asked) = {
            let library = library.read(cx);
            (
                library.drag().is_some(),
                library.deleting().is_some(),
                library.asked().is_some(),
            )
        };
        if dragging {
            crate::drag::end(window, cx);
        } else if deleting {
            library.update(cx, |library, cx| library.answer_delete(false, cx));
        } else if asked {
            library.update(cx, |library, cx| library.answer(false, cx));
        } else if self.sidebar_open {
            self.sidebar_open = false;
            self.main.focus_handle(cx).focus(window, cx);
            cx.notify();
        } else if Showing::now(cx) == Showing::Settings {
            self.main.update(cx, MainArea::leave_settings);
        }
    }

    /// Shows Settings in the main area, while a library page shows (module docs).
    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !Showing::now(cx).is_library() {
            return;
        }
        self.sidebar_open = false;
        self.main
            .update(cx, |main, cx| main.open_settings(window, cx));
        self.main.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /**
     * Shows a narrow window's sidebar once a drag has rested on its button for [`SPRING`], and
     * hides it again when the drag ends (module docs).
     */
    fn follow_drag(&mut self, cx: &mut Context<Self>) {
        let drag = cx.services().library.read(cx).drag();
        let resting = drag.is_some_and(|drag| drag.moved && drag.over == Some(DropTarget::Sidebar));
        if drag.is_none() && std::mem::take(&mut self.sprung) {
            self.sidebar_open = false;
        }
        if !resting {
            self.springing = None;
        } else if self.springing.is_none() && !self.sidebar_open {
            self.springing = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SPRING).await;
                this.update(cx, |this, cx| {
                    this.springing = None;
                    if cx.services().library.read(cx).drag().and_then(|it| it.over)
                        == Some(DropTarget::Sidebar)
                    {
                        this.sidebar_open = true;
                        this.sprung = true;
                        cx.notify();
                    }
                })
                .ok();
            }));
        }
    }

    /// Opens the locations sheet with `selected` selected, while a library page shows.
    pub fn open_locations(
        &mut self,
        selected: Option<sb_core::location::LocationId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.locations.is_some() || !Showing::now(cx).is_library() {
            return;
        }
        let back = window.focused(cx);
        let sheet = cx.new(|cx| LocationsSheet::new(selected, window, cx));
        let heard = [
            cx.subscribe_in(&sheet, window, |this, _, event, window, cx| match event {
                LocationsSheetEvent::Close => this.close_locations(window, cx),
            }),
            // The shell draws the sheet's dialogs, so it redraws with the sheet.
            cx.observe(&sheet, |_, _, cx| cx.notify()),
        ];
        self.locations = Some((sheet, back, heard));
        cx.notify();
    }

    /// Closes the locations sheet, and gives the keyboard back to where it was.
    fn close_locations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((sheet, back, _)) = self.locations.take() {
            sheet.update(cx, |sheet, cx| sheet.closing(window, cx));
            back.unwrap_or_else(|| self.main.focus_handle(cx))
                .focus(window, cx);
            cx.notify();
        }
    }

    /// The locations sheet, while it is open.
    pub fn locations_sheet(&self) -> Option<&Entity<LocationsSheet>> {
        self.locations.as_ref().map(|(sheet, ..)| sheet)
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
        if Showing::now(cx) == Showing::Settings {
            let settings = cx.services().settings.clone();
            return settings.update(cx, |settings, cx| settings.show(false, cx));
        }
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
        self.follow_dialogs(window, cx);
        // A sidebar that isn't shown can't keep the keyboard.
        if placement == SidebarPlacement::Collapsed
            && self.sidebar.focus_handle(cx).contains_focused(window, cx)
        {
            self.main.focus_handle(cx).focus(window, cx);
        }
        div()
            .id("shell")
            .key_context(keymap::SHELL)
            .on_action(cx.listener(Self::dismiss))
            /* The shell's other keys are for the window under a dialog: while one is open they
            do nothing, so that nothing changes behind it nor takes the keyboard from it. */
            .when(!self.asking(cx), |this| {
                this.on_action(cx.listener(Self::toggle_sidebar))
                    .on_action(cx.listener(Self::focus_other))
                    .on_action(cx.listener(Self::new_folder))
                    .on_action(cx.listener(Self::rename))
                    .on_action(cx.listener(Self::sync_all))
                    .on_action(cx.listener(Self::back))
                    .on_action(cx.listener(|this, _: &OpenLocations, window, cx| {
                        this.open_locations(None, window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                        this.open_settings(window, cx);
                    }))
            })
            // A drag is measured from its press, and ends wherever it is let go (`crate::drag`).
            .capture_any_mouse_down(|event, _, cx| crate::drag::pressed(event.position, cx))
            .on_drag_move::<crate::drag::Drag>(|event, _, cx| {
                crate::drag::moved(event.event.position, cx);
            })
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
            .child(crate::drag::layer())
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
            .children(self.locations.as_ref().map(|(sheet, ..)| {
                let shell = cx.entity().downgrade();
                overlay::modal(
                    sheet.clone(),
                    Layer::Sheet,
                    window,
                    &theme,
                    move |window, cx| {
                        shell
                            .update(cx, |shell, cx| shell.close_locations(window, cx))
                            .ok();
                    },
                )
            }))
            // A dialog over the sheet, on its own layer above it (DESIGN §3).
            .children(
                self.locations
                    .as_ref()
                    .map(|(sheet, ..)| sheet.clone())
                    .and_then(|sheet| sheet.update(cx, |sheet, cx| sheet.dialog_layer(window, cx))),
            )
            .children(self.dialog(window, &theme, cx))
            .children(crate::menu::layer(window, cx))
    }
}

/// Options for the main window: centred at exactly `window_size` (logical px), DESIGN §2's
/// minimum size, a custom title bar, and the installed theme's material.
pub fn window_options(window_size: Size<gpui::Pixels>, cx: &App) -> WindowOptions {
    let sizes = &cx.sb_theme().sizes;
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            /* Centred, on whole pixels: from a half pixel the platform rounds the window's edges
            outwards and it opens a pixel larger than asked. */
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
