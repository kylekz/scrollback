use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext, point};
use gpui_appearance::{Appearance, Mode, Preferences};
use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::{bounds, open_shell, open_shell_sized};

fn placement(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> SidebarPlacement {
    cx.update(|window, cx| {
        shell
            .read(cx)
            .sidebar_placement(window.viewport_size().width, cx)
    })
}

#[gpui::test]
fn the_regions_sit_where_design_puts_them(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    assert_eq!(placement(&shell, cx), SidebarPlacement::Docked);
    // DESIGN §1.4, §2: title bar 40, sidebar 248, the main area takes the rest.
    let title_bar = bounds(cx, "titlebar").expect("the title bar");
    assert_eq!(title_bar.origin, point(px(0.), px(0.)));
    assert_eq!(title_bar.size, size(px(1440.), px(40.)));
    let sidebar = bounds(cx, "sidebar").expect("the sidebar");
    assert_eq!(sidebar.origin, point(px(0.), px(40.)));
    assert_eq!(sidebar.size, size(px(248.), px(860.)));
    let main = bounds(cx, "main").expect("the main area");
    assert_eq!(main.origin, point(px(248.), px(40.)));
    assert_eq!(main.size, size(px(1192.), px(860.)));
    // The search field is 460 wide, centred on the window.
    let search = bounds(cx, "titlebar-search").expect("the search field");
    assert_eq!((search.origin.x, search.size.width), (px(490.), px(460.)));
    assert_eq!(search.size.height, px(30.));
}

#[gpui::test]
fn the_mark_leaves_room_for_the_traffic_lights_only_while_they_show(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let mark_left =
        |cx: &mut VisualTestContext| bounds(cx, "titlebar-mark").map(|mark| mark.origin.x);
    let windowed = if cfg!(target_os = "macos") {
        px(84.)
    } else {
        px(12.)
    };
    assert_eq!(mark_left(cx), Some(windowed));
    // macOS hides the traffic lights in fullscreen.
    cx.update(|window, _| window.toggle_fullscreen());
    assert_eq!(mark_left(cx), Some(px(12.)));
}

#[gpui::test]
fn the_shell_has_the_keyboard(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    cx.update(|window, cx| {
        assert!(shell.read(cx).focus_handle(cx).is_focused(window));
    });
    // Its keys reach it from the main area, where the keyboard rests.
    cx.simulate_resize(size(px(1000.), px(700.)));
    cx.simulate_keystrokes("secondary-b");
    assert_eq!(placement(&shell, cx), SidebarPlacement::Overlay);
}

#[gpui::test]
fn a_narrow_window_collapses_the_sidebar_and_ctrl_b_shows_it_over_the_main_area(
    cx: &mut TestAppContext,
) {
    let (shell, cx) = open_shell_sized(size(px(1000.), px(700.)), cx);
    assert_eq!(placement(&shell, cx), SidebarPlacement::Collapsed);
    assert_eq!(bounds(cx, "sidebar"), None);
    assert_eq!(
        bounds(cx, "main").map(|b| (b.origin.x, b.size.width)),
        Some((px(0.), px(1000.))),
        "the main area takes the whole width"
    );

    cx.simulate_keystrokes("secondary-b");
    assert_eq!(placement(&shell, cx), SidebarPlacement::Overlay);
    let sidebar = bounds(cx, "sidebar").expect("the sidebar over the main area");
    assert_eq!(sidebar.origin, point(px(0.), px(40.)));
    assert_eq!(sidebar.size, size(px(248.), px(660.)));
    assert_eq!(
        bounds(cx, "main").map(|b| b.size.width),
        Some(px(1000.)),
        "the main area keeps its width under the sidebar"
    );

    // Esc closes it, and so does a click outside it; a click on it doesn't.
    cx.simulate_keystrokes("escape");
    assert_eq!(placement(&shell, cx), SidebarPlacement::Collapsed);
    cx.simulate_keystrokes("secondary-b");
    cx.simulate_click(point(px(100.), px(300.)), Modifiers::none());
    assert_eq!(placement(&shell, cx), SidebarPlacement::Overlay);
    cx.simulate_mouse_down(
        point(px(600.), px(300.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_up(
        point(px(600.), px(300.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    assert_eq!(placement(&shell, cx), SidebarPlacement::Collapsed);
}

#[gpui::test]
fn a_window_grown_wide_docks_the_sidebar_and_forgets_the_overlay(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1000.), px(700.)), cx);
    cx.simulate_keystrokes("secondary-b");
    assert_eq!(placement(&shell, cx), SidebarPlacement::Overlay);
    cx.simulate_resize(size(px(1040.), px(700.)));
    assert_eq!(bounds(cx, "sidebar").map(|b| b.size.width), Some(px(248.)));
    assert_eq!(placement(&shell, cx), SidebarPlacement::Docked);
    // Ctrl B leaves a docked sidebar alone.
    cx.simulate_keystrokes("secondary-b");
    assert_eq!(placement(&shell, cx), SidebarPlacement::Docked);
    cx.simulate_resize(size(px(1039.), px(700.)));
    assert_eq!(bounds(cx, "sidebar"), None);
    assert_eq!(placement(&shell, cx), SidebarPlacement::Collapsed);
}

#[gpui::test]
fn a_material_change_reaches_the_window(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    shell.read_with(cx, |shell, _| {
        assert_eq!(
            shell.applied_material(),
            crate::appearance::default_material()
        )
    });
    for material in [
        Material::Acrylic,
        Material::Opaque,
        Material::Vibrancy,
        Material::Mica,
    ] {
        cx.update(|_, cx| {
            gpui_appearance::set_preferences(
                Preferences {
                    mode: Mode::Light,
                    material,
                },
                cx,
            )
        });
        cx.run_until_parked();
        shell.read_with(cx, |shell, _| {
            assert_eq!(shell.applied_material(), material)
        });
        cx.update(|_, cx| {
            assert_eq!(cx.sb_theme().material, material);
            assert_eq!(cx.sb_theme().appearance, Appearance::Light);
        });
    }
}

#[gpui::test]
fn the_window_opens_at_its_size_with_the_material(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let options = cx.update(|_, cx| window_options(size(px(1440.), px(900.)), cx));
    assert_eq!(
        options.window_background,
        crate::appearance::default_material().window_background(Backing::Solid)
    );
    // DESIGN §2.
    assert_eq!(options.window_min_size, Some(size(px(960.), px(600.))));
    let Some(WindowBounds::Windowed(bounds)) = options.window_bounds else {
        panic!("a windowed window");
    };
    assert_eq!(bounds.size, size(px(1440.), px(900.)));
}
