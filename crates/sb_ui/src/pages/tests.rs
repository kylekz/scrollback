use gpui::{Entity, Focusable as _, Modifiers, TestAppContext, VisualTestContext, point, px, size};
use pretty_assertions::assert_eq;
use sb_core::catalogue::Shelf;

use super::*;
use crate::services::LibraryService;
use crate::shell::SidebarPlacement;
use crate::test_support::{
    SESSIONS, bounds, init, machine, open_shell, open_shell_sized, open_window,
};

fn showing(cx: &mut VisualTestContext) -> Showing {
    cx.update(|_, cx| Showing::now(cx))
}

fn library(cx: &mut VisualTestContext) -> Entity<LibraryService> {
    cx.update(|_, cx| cx.services().library.clone())
}

/// The title of the selected session of a page of sessions.
fn selected(cx: &mut VisualTestContext) -> Option<String> {
    library(cx).read_with(cx, |library, _| {
        let page = library.sessions()?;
        page.sessions[library.selected_index()?].title.clone()
    })
}

fn shelf(cx: &mut VisualTestContext) -> Shelf {
    library(cx).read_with(cx, |library, _| library.shelf())
}

fn folder_names(cx: &mut VisualTestContext) -> Vec<String> {
    library(cx).read_with(cx, |library, _| {
        library
            .folders()
            .iter()
            .map(|folder| folder.name.clone())
            .collect()
    })
}

#[gpui::test]
fn the_first_run_shows_until_a_location_is_added(cx: &mut TestAppContext) {
    init(machine(), cx);
    let (_shell, cx) = open_window(size(px(1440.), px(900.)), cx);
    assert_eq!(showing(cx), Showing::FirstRun);
    // DESIGN §4.10: the title bar has the mark and no search, Sync all or Settings.
    assert!(bounds(cx, "titlebar-mark").is_some());
    assert_eq!(bounds(cx, "titlebar-search"), None);

    // Enter adds the checked location, which syncs, and the library takes the first run's place.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(showing(cx), Showing::Sessions);
    assert!(bounds(cx, "titlebar-search").is_some());
    library(cx).read_with(cx, |library, _| {
        assert_eq!(library.counts().all, SESSIONS.len())
    });
}

#[gpui::test]
fn the_arrows_move_between_cards_and_enter_s_and_alt_left_act_on_one(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    assert_eq!(showing(cx), Showing::Sessions);
    assert_eq!(selected(cx), None);
    // The first arrow selects the first card; then November's two, and October's one below.
    cx.simulate_keystrokes("right");
    assert_eq!(selected(cx).as_deref(), Some(SESSIONS[0].title));
    cx.simulate_keystrokes("right");
    assert_eq!(selected(cx).as_deref(), Some(SESSIONS[1].title));
    cx.simulate_keystrokes("down");
    assert_eq!(selected(cx).as_deref(), Some(SESSIONS[2].title));
    cx.simulate_keystrokes("up");
    assert_eq!(selected(cx).as_deref(), Some(SESSIONS[0].title));
    cx.simulate_keystrokes("left");
    assert_eq!(
        selected(cx).as_deref(),
        Some(SESSIONS[0].title),
        "the first"
    );

    cx.simulate_keystrokes("s");
    library(cx).read_with(cx, |library, _| {
        assert!(library.sessions().unwrap().sessions[0].starred);
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("enter");
    assert_eq!(showing(cx), Showing::Session);
    library(cx).read_with(cx, |library, _| {
        let open = library.open_session().expect("a session is open");
        assert_eq!(open.title.as_deref(), Some(SESSIONS[0].title));
    });
    cx.simulate_keystrokes("alt-left");
    assert_eq!(showing(cx), Showing::Sessions);
    assert_eq!(selected(cx).as_deref(), Some(SESSIONS[0].title));
}

#[gpui::test]
fn f6_moves_the_keyboard_to_the_sidebar_where_the_arrows_show_pages(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let main_focused = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| shell.read(cx).focus_handle(cx).is_focused(window))
    };
    assert!(main_focused(cx));
    cx.simulate_keystrokes("f6");
    assert!(!main_focused(cx));
    assert_eq!(shelf(cx), Shelf::All);
    cx.simulate_keystrokes("up");
    assert_eq!(shelf(cx), Shelf::All, "the first row");
    cx.simulate_keystrokes("down");
    assert_eq!(shelf(cx), Shelf::Starred);
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(shelf(cx), Shelf::Unsorted);
    assert_eq!(showing(cx), Showing::Unsorted);
    // Trash has no page yet, and there are no folders: Unsorted is the last row.
    cx.simulate_keystrokes("down");
    assert_eq!(shelf(cx), Shelf::Unsorted);

    // Enter hands the keyboard to the page: its sessions, top to bottom.
    cx.simulate_keystrokes("enter");
    assert!(main_focused(cx));
    let selected_id = |cx: &mut VisualTestContext| {
        library(cx).read_with(cx, |library, _| library.selected().cloned())
    };
    cx.simulate_keystrokes("down");
    let first = selected_id(cx).expect("a session of the first group");
    cx.simulate_keystrokes("down down");
    let third = selected_id(cx).expect("the second group's session");
    assert_ne!(first, third);
    cx.simulate_keystrokes("down");
    assert_eq!(selected_id(cx), Some(third), "the last");
    cx.simulate_keystrokes("enter");
    assert_eq!(showing(cx), Showing::Session);
    library(cx).read_with(cx, |library, _| {
        assert_eq!(
            library.open_session().and_then(|it| it.title.as_deref()),
            Some(SESSIONS[2].title)
        );
    });
    // Ctrl B moves the keyboard too while the sidebar is docked.
    cx.simulate_keystrokes("secondary-b");
    assert!(!main_focused(cx));
    cx.simulate_keystrokes("secondary-b");
    assert!(main_focused(cx));
}

#[gpui::test]
fn a_new_folder_is_named_in_its_row(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    cx.simulate_keystrokes("secondary-shift-n");
    cx.run_until_parked();
    assert_eq!(folder_names(cx), ["New folder"]);
    // The row's input has the keyboard, with the name selected: typing replaces it.
    cx.simulate_input("work");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(folder_names(cx), ["work"]);

    // The sidebar has the keyboard again: down to the folder, F2 renames it, Esc leaves it.
    cx.simulate_keystrokes("down down down");
    cx.run_until_parked();
    let work = library(cx).read_with(cx, |library, _| library.folders()[0].id);
    assert_eq!(shelf(cx), Shelf::Folder(work));
    cx.simulate_keystrokes("f2");
    cx.simulate_input("play");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(folder_names(cx), ["work"]);
    cx.simulate_keystrokes("f2");
    cx.simulate_input("play");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(folder_names(cx), ["play"]);
}

#[gpui::test]
fn picking_a_row_closes_a_narrow_windows_sidebar(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1000.), px(700.)), cx);
    let placement = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            shell
                .read(cx)
                .sidebar_placement(window.viewport_size().width, cx)
        })
    };
    cx.simulate_keystrokes("secondary-b");
    assert_eq!(placement(cx), SidebarPlacement::Overlay);
    // Starred's row: the second, 32 high under the sidebar's 10px padding and the 40px bar.
    cx.simulate_click(point(px(100.), px(100.)), Modifiers::none());
    assert_eq!(placement(cx), SidebarPlacement::Collapsed);
    assert_eq!(shelf(cx), Shelf::Starred);
    // F6 shows it to give it the keyboard, and hides it when the keyboard leaves.
    cx.simulate_keystrokes("f6");
    assert_eq!(placement(cx), SidebarPlacement::Overlay);
    cx.simulate_keystrokes("f6");
    assert_eq!(placement(cx), SidebarPlacement::Collapsed);
}
