//! What the views' tests share: a main window with the shell in it.

use gpui::{
    AppContext as _, Bounds, Entity, Pixels, Size, TestAppContext, VisualTestContext, px, size,
};
use gpui_appearance::{MaterialSupport, Mode};
use gpui_component::Root;

use crate::shell::Shell;

/// A 1440x900 main window around a [`Shell`], on a platform that shows every material.
pub fn open_shell(cx: &mut TestAppContext) -> (Entity<Shell>, &mut VisualTestContext) {
    open_shell_sized(size(px(1440.), px(900.)), cx)
}

/// [`open_shell`] in a window of `window_size`.
pub fn open_shell_sized(
    window_size: Size<Pixels>,
    cx: &mut TestAppContext,
) -> (Entity<Shell>, &mut VisualTestContext) {
    cx.update(|cx| {
        crate::init(cx).expect("sb_ui initialises");
        crate::appearance::init(MaterialSupport::ALL, Mode::System, cx);
    });
    let mut shell = None;
    let window = cx.open_window(window_size, |window, cx| {
        let view = cx.new(|cx| Shell::new(window, cx));
        shell = Some(view.clone());
        Root::new(view, window, cx)
    });
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    cx.run_until_parked();
    (shell.expect("the window built a shell"), cx)
}

/// The bounds an element with `.debug_selector(|| name.into())` drew at, after a fresh frame.
pub fn bounds(cx: &mut VisualTestContext, name: &'static str) -> Option<Bounds<Pixels>> {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.debug_bounds(name)
}
