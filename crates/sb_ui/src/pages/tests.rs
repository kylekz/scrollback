use gpui::{
    Entity, Focusable as _, KeyUpEvent, Keystroke, Modifiers, TestAppContext, VisualTestContext,
    point, px, size,
};
use pretty_assertions::assert_eq;
use sb_core::catalogue::Shelf;
use sb_test_support::{break_segment_commits, mend_segment_commits};

use super::*;
use crate::services::LibraryService;
use crate::shell::{Shell, SidebarPlacement};
use crate::test_support::{
    SESSIONS, archive_dir, bounds, init, machine, open_shell, open_shell_sized, open_window,
};

/// Whether the keyboard is on something, and that something is drawn.
fn keyboard_is_somewhere(cx: &mut VisualTestContext) -> bool {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.update(|window, cx| window.focused(cx).is_some())
}

fn main_focused(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| shell.read(cx).focus_handle(cx).is_focused(window))
}

/// Presses and releases `key`: a button with the keyboard is clicked on the release.
fn press(cx: &mut VisualTestContext, key: &'static str) {
    cx.simulate_keystrokes(key);
    cx.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse(key).unwrap(),
    });
    cx.run_until_parked();
}

fn placement(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> SidebarPlacement {
    cx.update(|window, cx| {
        shell
            .read(cx)
            .sidebar_placement(window.viewport_size().width, cx)
    })
}

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
    let main_focused = |cx: &mut VisualTestContext| main_focused(&shell, cx);
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
fn a_folder_made_in_a_narrow_window_is_named_in_the_sidebar_over_the_page(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1000.), px(700.)), cx);
    assert_eq!(placement(&shell, cx), SidebarPlacement::Collapsed);
    cx.simulate_keystrokes("secondary-shift-n");
    cx.run_until_parked();
    // The row that is renamed has the keyboard, so the sidebar shows for it.
    assert_eq!(placement(&shell, cx), SidebarPlacement::Overlay);
    assert!(keyboard_is_somewhere(cx));
    assert!(bounds(cx, "sidebar").is_some());
    cx.simulate_input("work");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(folder_names(cx), ["work"]);
    // The keyboard is in the sidebar, which still shows: the arrows walk its rows.
    assert!(keyboard_is_somewhere(cx));
    assert_eq!(placement(&shell, cx), SidebarPlacement::Overlay);
    cx.simulate_keystrokes("down");
    assert_eq!(shelf(cx), Shelf::Starred);
    cx.simulate_keystrokes("escape");
    assert_eq!(placement(&shell, cx), SidebarPlacement::Collapsed);
    assert!(main_focused(&shell, cx));

    // F2 on the folder's page, with the sidebar hidden, shows it too; Esc leaves the name,
    // and a second one hides the sidebar.
    let work = library(cx).read_with(cx, |library, _| library.folders()[0].id);
    library(cx).update(cx, |library, cx| library.show(Shelf::Folder(work), cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    assert_eq!(placement(&shell, cx), SidebarPlacement::Overlay);
    cx.simulate_input("play");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(folder_names(cx), ["play"]);
    assert!(keyboard_is_somewhere(cx));
}

#[gpui::test]
fn f2_renames_the_shown_folder_wherever_the_keyboard_is(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    library(cx).update(cx, |library, cx| {
        library.create_folder("work".into(), None, cx)
    });
    cx.run_until_parked();
    // The new folder is being named: keep its name.
    cx.simulate_keystrokes("enter");
    let work = library(cx).read_with(cx, |library, _| library.folders()[0].id);
    library(cx).update(cx, |library, cx| library.show(Shelf::Folder(work), cx));
    cx.run_until_parked();
    cx.update(|window, cx| shell.read(cx).focus_handle(cx).focus(window, cx));
    assert!(main_focused(&shell, cx));
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    assert!(!main_focused(&shell, cx));
    cx.simulate_input("play");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(folder_names(cx), ["play"]);
}

#[gpui::test]
fn the_arrows_are_the_text_fields_while_a_folder_is_renamed(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    for name in ["one", "two"] {
        library(cx).update(cx, |library, cx| {
            library.create_folder(name.into(), None, cx)
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }
    let one = library(cx).read_with(cx, |library, _| library.folders()[0].id);
    library(cx).update(cx, |library, cx| library.show(Shelf::Folder(one), cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    // Typing replaces the selected name; Left moves the caret, so the next letter goes
    // before the last. No arrow, at either end of the text, shows another page.
    cx.simulate_input("abc");
    cx.simulate_keystrokes("left");
    cx.simulate_input("Z");
    cx.simulate_keystrokes("up down right right right left left left left left left");
    assert_eq!(shelf(cx), Shelf::Folder(one));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(folder_names(cx), ["abZc", "two"]);
    // A name longer than the catalogue takes is cut at what it takes.
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    cx.simulate_input(&"L".repeat(sb_core::catalogue::FOLDER_NAME_CHARS));
    cx.simulate_input("MORE");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        folder_names(cx)[0],
        "L".repeat(sb_core::catalogue::FOLDER_NAME_CHARS)
    );
}

#[gpui::test]
fn a_folder_hidden_in_a_closed_one_gives_the_arrows_the_row_that_hides_it(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    library(cx).update(cx, |library, cx| {
        library.create_folder("outer".into(), None, cx)
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    let outer = library(cx).read_with(cx, |library, _| library.folders()[0].id);
    library(cx).update(cx, |library, cx| {
        library.create_folder("inner".into(), Some(outer), cx)
    });
    cx.run_until_parked();
    // Naming the inner folder opened the outer one; keep the name, show the inner's page.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let inner = library(cx).read_with(cx, |library, _| library.folders()[1].id);
    library(cx).update(cx, |library, cx| library.show(Shelf::Folder(inner), cx));
    cx.run_until_parked();
    // Left on the inner goes to the outer, and again closes it; back on the inner's page,
    // its row is hidden.
    cx.simulate_keystrokes("left left");
    assert_eq!(shelf(cx), Shelf::Folder(outer));
    library(cx).update(cx, |library, cx| library.show(Shelf::Folder(inner), cx));
    cx.run_until_parked();
    // Right opens what hides it, and it is still the page shown.
    cx.simulate_keystrokes("right");
    assert_eq!(shelf(cx), Shelf::Folder(inner));
    cx.simulate_keystrokes("left left");
    library(cx).update(cx, |library, cx| library.show(Shelf::Folder(inner), cx));
    cx.run_until_parked();
    // Up, down and left each go to the row that hides it.
    for key in ["up", "down", "left"] {
        cx.simulate_keystrokes(key);
        assert_eq!(shelf(cx), Shelf::Folder(outer), "{key}");
        library(cx).update(cx, |library, cx| library.show(Shelf::Folder(inner), cx));
        cx.run_until_parked();
    }
}

#[gpui::test]
fn tab_walks_the_buttons_and_never_strands_the_keyboard(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    assert!(main_focused(&shell, cx));
    // Round the window and back to the main area, the keyboard always on something drawn.
    let mut stops = 0;
    loop {
        cx.simulate_keystrokes("tab");
        assert!(keyboard_is_somewhere(cx));
        stops += 1;
        assert!(stops < 40, "Tab never comes back to the main area");
        if main_focused(&shell, cx) {
            break;
        }
    }
    // The title bar's Sync all, the sidebar's rows and its two buttons, the main area.
    assert_eq!(stops, 5);
    // Shift Tab goes the other way: to the sidebar's buttons, where the page's keys still work.
    cx.simulate_keystrokes("shift-tab");
    assert!(!main_focused(&shell, cx));
    cx.simulate_keystrokes("f6");
    assert!(main_focused(&shell, cx));
    cx.simulate_keystrokes("right");
    assert_eq!(selected(cx).as_deref(), Some(SESSIONS[0].title));
}

#[gpui::test]
fn tab_reaches_a_groups_buttons_and_enter_presses_them(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    library(cx).update(cx, |library, cx| library.show(Shelf::Unsorted, cx));
    cx.run_until_parked();
    assert!(main_focused(&shell, cx));
    // The first group's toggle, then its Create folder from this path.
    cx.simulate_keystrokes("tab");
    press(cx, "enter");
    cx.simulate_keystrokes("down");
    let first = library(cx).read_with(cx, |library, _| {
        let selected = library.selected().cloned();
        (selected, library.groups()[1].sessions[0].id.clone())
    });
    assert_eq!(first.0, Some(first.1), "the first group is closed");
    press(cx, "space");
    cx.simulate_keystrokes("tab");
    press(cx, "enter");
    assert_eq!(folder_names(cx), ["app"]);
    // The button went with its group: the keyboard is back on the page, where the new
    // folder's row took it for its name.
    assert!(keyboard_is_somewhere(cx));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(folder_names(cx), ["app"]);
    assert!(keyboard_is_somewhere(cx));
}

#[gpui::test]
fn a_failed_sync_is_a_banner_with_the_reason_and_a_way_to_try_again(cx: &mut TestAppContext) {
    init(machine(), cx);
    break_segment_commits(&archive_dir(cx));
    let (shell, cx) = open_window(size(px(1440.), px(900.)), cx);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(showing(cx), Showing::Sessions);
    let banner = bounds(cx, "sync-failed").expect("the banner");
    // Across the page, above its header.
    assert_eq!(
        (banner.origin.x, banner.size.width),
        (px(248. + 24.), px(1440. - 248. - 48.))
    );
    assert!(banner.origin.y < bounds(cx, "page-header").expect("the header").origin.y);

    // Tab reaches Try again from the page, and Enter presses it.
    mend_segment_commits(&archive_dir(cx));
    assert!(main_focused(&shell, cx));
    cx.simulate_keystrokes("tab");
    press(cx, "enter");
    assert_eq!(bounds(cx, "sync-failed"), None);
    library(cx).read_with(cx, |library, _| {
        assert_eq!(library.counts().all, SESSIONS.len())
    });
    assert!(keyboard_is_somewhere(cx));
}

#[gpui::test]
fn picking_a_row_closes_a_narrow_windows_sidebar(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1000.), px(700.)), cx);
    let placement = |cx: &mut VisualTestContext| placement(&shell, cx);
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
