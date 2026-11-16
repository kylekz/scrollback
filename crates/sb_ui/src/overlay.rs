//! Sheets and dialogs over the scrim (DESIGN §2 "Overlays", §3): the scrim covers everything
//! below the title bar, the overlay sits on `s2` with `shadow-pop`, and a press on the scrim or
//! `Esc` closes it. Whichever view opens an overlay draws it with [`modal`], so a dialog opened
//! from a sheet, or from the first run, sits above what opened it.
//!
//! An overlay keeps the keyboard inside it while it is open (`gpui_component`'s focus trap,
//! which `Tab` and `Shift Tab` keep to), and whoever opened it gives the keyboard back to where
//! it was when it closes.

use gpui::{
    AnyElement, App, Div, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    Pixels, Size, Styled as _, Window, anchored, deferred, div, point, px,
};

use crate::theme::SbTheme;

/// How an overlay is stacked: a dialog over a sheet over the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Sheet = 1,
    Dialog = 2,
    /// A confirmation over a dialog or a sheet.
    Confirm = 3,
}

/// The room an overlay has in a window of `viewport`: below the title bar, inside the
/// overlay margin on every side.
pub fn room(viewport: Size<Pixels>, theme: &SbTheme) -> Size<Pixels> {
    let margin = theme.sizes.overlay_margin;
    Size {
        width: (viewport.width - margin * 2.).max(px(0.)),
        height: (viewport.height - theme.sizes.title_bar_height - margin * 2.).max(px(0.)),
    }
}

/// The surface of a sheet or a dialog (`.sheet`, `.dialog`: `s2`, a `line` border, radius 12,
/// `shadow-pop`). Presses on it stay with it.
pub fn surface(theme: &SbTheme) -> Div {
    let c = &theme.colors;
    div()
        .flex()
        .flex_col()
        .overflow_hidden()
        .rounded(theme.radii.sheet)
        .border_1()
        .border_color(c.line)
        .bg(c.s2)
        .shadow(theme.shadows.pop.clone())
        .text_color(c.fg)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/**
 * `content` over the scrim on `layer`, centred across the window and the overlay margin below
 * the title bar. A press on the scrim calls `dismiss`.
 */
pub fn modal(
    content: impl IntoElement,
    layer: Layer,
    window: &Window,
    theme: &SbTheme,
    dismiss: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    let viewport = window.viewport_size();
    let below = theme.sizes.title_bar_height;
    deferred(
        anchored().position(point(px(0.), below)).child(
            div()
                .id(("scrim", layer as usize))
                .w(viewport.width)
                .h(viewport.height - below)
                .bg(theme.colors.scrim)
                .occlude()
                .on_mouse_down(MouseButton::Left, move |_, window, cx| dismiss(window, cx))
                .flex()
                .justify_center()
                .items_start()
                .pt(theme.sizes.overlay_margin)
                .child(content),
        ),
    )
    .with_priority(layer as usize)
    .into_any_element()
}
