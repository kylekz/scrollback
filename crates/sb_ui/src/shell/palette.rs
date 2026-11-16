//! The search palette over the window (DESIGN §4.9, [`crate::search`]): `Ctrl K` or the title
//! bar's search field opens it over a library page, and `Ctrl K` again, `Esc` or a press on the
//! scrim closes it. It doesn't open over the locations sheet or a dialog.

use gpui::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable as _, Subscription, Window,
};

use super::Shell;
use crate::actions::shell::Search;
use crate::commands;
use crate::overlay::{self, Layer};
use crate::pages::Showing;
use crate::search::{Palette, PaletteEvent};
use crate::services::ActiveServices as _;
use crate::theme::SbTheme;

/// The palette open, where the keyboard was before it opened, and what the shell hears of it.
pub(super) struct Open {
    palette: Entity<Palette>,
    back: Option<FocusHandle>,
    _heard: Subscription,
}

impl Shell {
    /// Opens the palette over the page shown, or closes it (`Ctrl K`).
    pub(super) fn toggle_palette(
        &mut self,
        _: &Search,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette.is_some() {
            return self.close_palette(false, window, cx);
        }
        if self.locations.is_some() || !Showing::now(cx).is_library() {
            return;
        }
        // A narrow window's sidebar gives way to it, as to a press outside the sidebar.
        self.sidebar_open = false;
        let back = window.focused(cx);
        let shelf = cx.services().library.read(cx).shelf();
        let palette = cx.new(|cx| Palette::new(shelf, window, cx));
        let heard = cx.subscribe_in(&palette, window, |this, _, event, window, cx| match event {
            PaletteEvent::Close { to_main } => this.close_palette(*to_main, window, cx),
            PaletteEvent::Run(run) => {
                this.close_palette(true, window, cx);
                commands::run(*run, window, cx);
            }
        });
        self.palette = Some(Open {
            palette,
            back,
            _heard: heard,
        });
        cx.notify();
    }

    /**
     * Closes the palette. The keyboard goes back to where it was, or with `to_main` to the
     * main area: a session was opened, a page shown or a command is to run there.
     */
    pub(super) fn close_palette(
        &mut self,
        to_main: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(Open { back, .. }) = self.palette.take() {
            back.filter(|_| !to_main)
                .unwrap_or_else(|| self.main.focus_handle(cx))
                .focus(window, cx);
            cx.notify();
        }
    }

    /// The palette, while it is open.
    pub fn palette(&self) -> Option<&Entity<Palette>> {
        self.palette.as_ref().map(|open| &open.palette)
    }

    /// The palette over the scrim, while it is open.
    pub(super) fn palette_layer(
        &self,
        window: &Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let open = self.palette.as_ref()?;
        let shell = cx.entity().downgrade();
        Some(overlay::modal(
            open.palette.clone(),
            Layer::Sheet,
            window,
            theme,
            move |window, cx| {
                shell
                    .update(cx, |shell, cx| shell.close_palette(false, window, cx))
                    .ok();
            },
        ))
    }
}
