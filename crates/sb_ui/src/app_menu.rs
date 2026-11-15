//! The macOS menu bar (DESIGN §5, "macOS"): scrollback's app menu (Services, Hide scrollback,
//! Hide Others, Show All, Quit scrollback) and a Window menu (Minimize). A Mac app without them
//! has no Quit and no `⌘Q`; their keys are [`crate::keymap::MACOS_SHORTCUTS`], which gpui shows
//! beside each item. Windows has no menu bar: scrollback draws its own title bar there
//! (DESIGN §2), and this does nothing.

use gpui::{App, Menu, MenuItem, SystemMenuType};

use crate::actions::app;

/// Installs the menu bar and the app actions behind it, on macOS.
pub fn init(cx: &mut App) {
    if !cfg!(target_os = "macos") {
        return;
    }
    cx.on_action(|_: &app::Quit, cx| cx.quit());
    cx.on_action(|_: &app::Hide, cx| cx.hide());
    cx.on_action(|_: &app::HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &app::ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &app::Minimize, cx| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| window.minimize_window());
        }
    });
    cx.set_menus(menus());
}

/// The menus, left to right.
pub fn menus() -> Vec<Menu> {
    vec![
        menu(
            "scrollback",
            vec![
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide scrollback", app::Hide),
                MenuItem::action("Hide Others", app::HideOthers),
                MenuItem::action("Show All", app::ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit scrollback", app::Quit),
            ],
        ),
        menu("Window", vec![MenuItem::action("Minimize", app::Minimize)]),
    ]
}

fn menu(name: &'static str, items: Vec<MenuItem>) -> Menu {
    Menu {
        name: name.into(),
        items,
        disabled: false,
    }
}
