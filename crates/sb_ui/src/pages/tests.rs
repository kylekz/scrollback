use gpui::{
    Entity, Focusable as _, KeyUpEvent, Keystroke, Modifiers, TestAppContext, VisualTestContext,
    point, px, size,
};
use pretty_assertions::assert_eq;
use sb_core::catalogue::Shelf;
use sb_test_support::{break_segment_commits, mend_segment_commits};

use super::*;
use crate::pages::sessions::LineMeasure;
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
    /* There are no folders: Trash is the last page's row, and the arrows go on to the
    location rows; up comes back. */
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(shelf(cx), Shelf::Trash);
    assert_eq!(showing(cx), Showing::Trash);
    cx.simulate_keystrokes("down");
    assert_eq!(shelf(cx), Shelf::Trash);
    cx.simulate_keystrokes("up up");
    cx.run_until_parked();
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
    cx.simulate_keystrokes("down down down down");
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

    /* F2 on the folder's page, with the sidebar hidden, shows it too; Esc leaves the name,
    and a second one hides the sidebar. */
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
    /* Typing replaces the selected name; Left moves the caret, so the next letter goes
    before the last. No arrow, at either end of the text, shows another page. */
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
    /* Left on the inner goes to the outer, and again closes it; back on the inner's page,
    its row is hidden. */
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
    /* The page's filter field, the title bar's search field, Sync all and Settings, the
    sidebar's rows and its two buttons, the main area. */
    assert_eq!(stops, 8);
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
    // The filter field, the first group's toggle, then its Create folder from this path.
    cx.simulate_keystrokes("tab tab");
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
    /* The button went with its group: the keyboard is back on the page, where the new
    folder's row took it for its name. */
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
fn an_arrow_from_a_session_in_a_closed_group_goes_on_from_where_it_is(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    library(cx).update(cx, |library, cx| library.show(Shelf::Unsorted, cx));
    cx.run_until_parked();
    let session = |group: usize, row: usize, cx: &mut VisualTestContext| {
        library(cx).read_with(cx, |library, _| {
            library.groups()[group].sessions[row].id.clone()
        })
    };
    let selected_id = |cx: &mut VisualTestContext| {
        library(cx).read_with(cx, |library, _| library.selected().cloned())
    };
    /* The second group's session, then that group closed: its toggle is the fourth Tab stop,
    after the filter field and the first group's toggle and button. */
    cx.simulate_keystrokes("down down down");
    assert_eq!(selected_id(cx), Some(session(1, 0, cx)));
    cx.simulate_keystrokes("tab tab tab tab");
    press(cx, "enter");
    cx.simulate_keystrokes("f6 f6");
    assert!(main_focused(&shell, cx));
    // Up goes to the session above where it is, the first group's last, not to the top.
    cx.simulate_keystrokes("up");
    assert_eq!(selected_id(cx), Some(session(0, 1, cx)));
}

#[test]
fn why_a_sync_failed_is_a_sentence_of_its_own() {
    assert_eq!(
        sentence("Agent: 1 item was not archived, because: Permission denied (os error 13)"),
        "Agent: 1 item was not archived, because: Permission denied (os error 13)."
    );
    assert_eq!(sentence("the disk is full."), "The disk is full.");
    assert_eq!(sentence(""), "");
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

/** Every key the reader binds (`Enter` last: it opens the next session from a page of them). */
const READER_KEYS: &str =
    "[ ] up down right left space secondary-c pageup pagedown home end e e s s enter";

#[gpui::test]
fn the_readers_keys_do_nothing_while_a_session_opens_or_after_it_failed(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let reader =
        |cx: &mut VisualTestContext| cx.update(|_, cx| cx.services().reader.read(cx).state().key());
    cx.simulate_keystrokes("right enter");
    assert_eq!(reader(cx), "open");
    /* `j` opens the next session, and the keys after it land before it is parsed. */
    cx.simulate_keystrokes(&format!("j {READER_KEYS}"));
    assert_eq!(reader(cx), "open");

    /* A catalogue that has lost its versions cannot say what a session is made of. */
    sb_test_support::edit_catalogue(&archive_dir(cx), "ALTER TABLE version RENAME TO lost");
    cx.simulate_keystrokes("k");
    assert_eq!(reader(cx), "failed");
    cx.simulate_keystrokes(READER_KEYS);
    assert_eq!(showing(cx), Showing::Session);
}

/**
 * The reader's keys and the library's share letters, each in its own context: in the reader
 * `S` stars the open session, `J` opens the next one shown and selects it on the page, and
 * the page's `Delete` and `Ctrl A` do nothing to it. A dialog over it has the keyboard. It
 * opens from the list view as from the cards.
 */
#[gpui::test]
fn the_readers_keys_act_on_the_open_session_and_the_pages_keys_dont_reach_it(
    cx: &mut TestAppContext,
) {
    let (shell, cx) = open_shell(cx);
    let open_title = |cx: &mut VisualTestContext| {
        library(cx).read_with(cx, |library, _| {
            library.open_session().and_then(|open| open.title.clone())
        })
    };
    let starred = |title: &str, cx: &mut VisualTestContext| {
        library(cx).read_with(cx, |library, _| {
            let page = library.sessions().unwrap();
            page.sessions
                .iter()
                .find(|session| session.title.as_deref() == Some(title))
                .unwrap()
                .starred
        })
    };
    /* A folder for a dialog to ask about, its name kept in its row. */
    library(cx).update(cx, |library, cx| {
        library.create_folder("work".to_string(), None, cx)
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let work = library(cx).read_with(cx, |library, _| library.folders()[0].id);
    library(cx).update(cx, |library, cx| library.show(Shelf::All, cx));
    cx.update(|window, cx| shell.read(cx).focus_handle(cx).focus(window, cx));
    cx.run_until_parked();

    cx.simulate_keystrokes("right enter");
    assert_eq!(showing(cx), Showing::Session);
    assert_eq!(open_title(cx).as_deref(), Some(SESSIONS[0].title));
    cx.simulate_keystrokes("s");
    cx.run_until_parked();
    assert!(starred(SESSIONS[0].title, cx));
    assert!(library(cx).read_with(cx, |library, _| library.open_session().unwrap().starred));

    cx.simulate_keystrokes("j");
    cx.run_until_parked();
    assert_eq!(open_title(cx).as_deref(), Some(SESSIONS[1].title));
    assert_eq!(selected(cx).as_deref(), Some(SESSIONS[1].title));

    cx.simulate_keystrokes("delete secondary-a");
    cx.run_until_parked();
    assert_eq!(showing(cx), Showing::Session);
    assert_eq!(open_title(cx).as_deref(), Some(SESSIONS[1].title));
    library(cx).read_with(cx, |library, _| {
        assert_eq!(
            library.counts().all,
            SESSIONS.len(),
            "nothing went to Trash"
        );
    });

    /* A dialog over the reader has the keyboard: the reader's keys don't reach it. */
    library(cx).update(cx, |library, cx| library.ask_delete(work, cx));
    cx.run_until_parked();
    assert!(bounds(cx, "delete-folder").is_some());
    cx.simulate_keystrokes("k s");
    cx.run_until_parked();
    assert_eq!(open_title(cx).as_deref(), Some(SESSIONS[1].title));
    assert!(!starred(SESSIONS[1].title, cx));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(library(cx).read_with(cx, |library, _| library.deleting().is_none()));
    cx.simulate_keystrokes("k");
    cx.run_until_parked();
    assert_eq!(open_title(cx).as_deref(), Some(SESSIONS[0].title));

    /* Back, to the list view, and open from its row. */
    cx.simulate_keystrokes("alt-left");
    let library = library(cx);
    library.update(cx, |library, cx| {
        let mut view = library.view();
        view.list = true;
        library.set_view(view, cx);
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("down enter");
    assert_eq!(showing(cx), Showing::Session);
    assert_eq!(open_title(cx).as_deref(), Some(SESSIONS[1].title));
}

/**
 * A location with a long name: its tag keeps to its cap on the cards (DESIGN §4.4), and the
 * counts keep their room, in a wide window and in a narrow one.
 */
fn a_long_location_name_leaves_the_counts(
    window_size: gpui::Size<gpui::Pixels>,
    cx: &mut TestAppContext,
) {
    let (_shell, cx) = open_shell_sized(window_size, cx);
    let sync = cx.update(|_, cx| cx.services().sync.clone());
    let here = sync.read_with(cx, |sync, _| sync.locations().unwrap()[0].id.clone());
    sync.update(cx, |sync, cx| {
        sync.rename(&here, "Distro for Long Running Builds and Experiments", cx)
            .unwrap();
    });
    cx.run_until_parked();
    let card = bounds(cx, "card-0").expect("the first card");
    let calls = bounds(cx, "card-calls-0").expect("its count shows");
    assert!(calls.right() <= card.right(), "{calls:?} inside {card:?}");
}

#[gpui::test]
fn a_long_location_name_leaves_the_counts_in_a_wide_window(cx: &mut TestAppContext) {
    a_long_location_name_leaves_the_counts(size(px(1440.), px(900.)), cx);
}

#[gpui::test]
fn a_long_location_name_leaves_the_counts_in_a_narrow_window(cx: &mut TestAppContext) {
    a_long_location_name_leaves_the_counts(size(px(960.), px(600.)), cx);
}

/**
 * A page whose oldest session is of another year, and a location whose tag reaches its cap:
 * the page's decision on counts measures the widest day with its year ("28 Dec 2025") and its
 * largest count, and reports what it measured (DESIGN §4.4).
 */
fn cards_of_a_page_spanning_two_years(window_width: f32, cx: &mut TestAppContext) -> LineMeasure {
    let (shell, cx) = open_shell_sized(size(px(window_width), px(900.)), cx);
    crate::test_support::write_session(
        &sb_test_support::Outline {
            project: "/home/user/notes",
            title: "Last year's notes",
            prompt: "Tidy last year's notes.",
            model: "model-a",
            day: (2025, 12, 3),
            calls: 412,
        },
        cx,
    );
    let sync = cx.update(|_, cx| cx.services().sync.clone());
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();
    let here = sync.read_with(cx, |sync, _| sync.locations().unwrap()[0].id.clone());
    sync.update(cx, |sync, cx| {
        sync.rename(&here, &"a".repeat(80), cx).unwrap();
    });
    cx.run_until_parked();
    bounds(cx, "card-0");
    let measure = cx.update(|_, cx| {
        let main = shell.read(cx).main().read(cx);
        main.sessions().read(cx).line_measure().clone()
    });
    assert_eq!(measure.day.0, "28 Dec 2025");
    assert_eq!(measure.tags[0].2, px(80.));
    measure
}

/**
 * The line such a card must hold: the mark (12), a model's least (12), the tag at its cap (80),
 * the day (72.6 in Geist Mono at 11px), the glyph, 4 and a three-digit count (35.8), and four
 * gaps of 6: 236.4. A card of 277 (1440 wide) and one of 276 (Windows at 1440 and a fractional
 * scale) both hold it, with their 4 spare, by 5.6 or more.
 */
#[gpui::test]
fn a_card_of_277_holds_a_capped_tag_a_day_with_its_year_and_the_count(cx: &mut TestAppContext) {
    let measure = cards_of_a_page_spanning_two_years(1440., cx);
    assert_eq!(measure.inner, px(247.));
    assert_eq!(
        (measure.calls.0.as_str(), measure.day.1.round()),
        ("412", px(73.))
    );
    assert!(measure.shows_calls);
}

#[gpui::test]
fn a_card_of_276_holds_them_too(cx: &mut TestAppContext) {
    let measure = cards_of_a_page_spanning_two_years(1436., cx);
    assert_eq!(measure.inner, px(246.));
    assert!(measure.shows_calls);
    assert!(!measure.tags_past_caps);
}

#[gpui::test]
fn the_arrows_row_goes_with_a_location_that_is_removed(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let sync = cx.update(|_, cx| cx.services().sync.clone());
    let distro = sb_core::location::LocationKind::Wsl {
        distro: "Ubuntu-24.04".to_string(),
    };
    sync.update(cx, |sync, cx| {
        sync.add(distro, "Ubuntu-24.04".to_string(), false, cx)
    });
    cx.run_until_parked();
    // Past Unsorted, the last page's row, to this machine's row.
    cx.simulate_keystrokes("f6 down down down");
    let here = sync.read_with(cx, |sync, _| sync.locations().unwrap()[0].id.clone());
    sync.update(cx, |sync, cx| sync.remove(&here, cx));
    cx.run_until_parked();
    // Its row is gone, and the arrows' with it: Enter doesn't open the next location.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(cx.update(|_, cx| shell.read(cx).locations_sheet().is_none()));
    assert!(main_focused(&shell, cx));
}
