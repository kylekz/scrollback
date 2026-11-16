//! The library pages' interactions (DESIGN §4.1, §4.4–§4.7, §4.14): drag and drop, selection,
//! the session menu, the filters, the list view, Unsorted's paths and the paths editor, over
//! the test library of [`crate::test_support`]: three sessions, all in Unsorted at first.

use gpui::{
    App, Entity, Focusable as _, Modifiers, MouseButton, Pixels, Point, TestAppContext,
    VisualTestContext, point, px, size,
};
use pretty_assertions::assert_eq;
use sb_core::catalogue::{Filed, SessionId, Shelf};
use sb_core::filing::FolderId;

use super::paths_editor::PathsEditor;
use crate::menu;
use crate::services::library::NEW_FOLDER;
use crate::services::{ActiveServices as _, Dragged, Leave, LibraryService, PageView, Verdict};
use crate::shell::{Shell, SidebarPlacement};
use crate::test_support::{SESSIONS, bounds, open_shell, open_shell_sized};

fn library(cx: &mut VisualTestContext) -> Entity<LibraryService> {
    cx.update(|_, cx| cx.services().library.clone())
}

/// Makes a folder, keeping the name it is given.
fn folder(name: &str, parent: Option<FolderId>, cx: &mut VisualTestContext) -> FolderId {
    let name = name.to_string();
    library(cx).update(cx, |library, cx| {
        library.create_folder(name.clone(), parent, cx)
    });
    cx.run_until_parked();
    // The new folder's row is renaming it: keep the name.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    library(cx).read_with(cx, |library, _| {
        library
            .folders()
            .iter()
            .rev()
            .find(|it| it.name == name)
            .map(|it| it.id)
            .expect("the folder is made")
    })
}

fn show(shelf: Shelf, cx: &mut VisualTestContext) {
    library(cx).update(cx, |library, cx| library.show(shelf, cx));
    cx.run_until_parked();
}

/// How the session titled `title` is filed, and whether it is in Trash.
fn filed(title: &str, cx: &mut VisualTestContext) -> Filed {
    let archive = cx.update(|_, cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    let sessions = archive.catalogue().sessions(Shelf::All).unwrap();
    sessions
        .into_iter()
        .find(|it| it.title.as_deref() == Some(title))
        .map(|it| it.filed)
        .expect("the session")
}

fn toast(cx: &mut VisualTestContext) -> Option<String> {
    library(cx).read_with(cx, |library, _| library.toast().map(|it| it.text.clone()))
}

fn undo(cx: &mut VisualTestContext) {
    library(cx).update(cx, LibraryService::undo);
    cx.run_until_parked();
}

fn centre(name: &'static str, cx: &mut VisualTestContext) -> Point<Pixels> {
    bounds(cx, name)
        .unwrap_or_else(|| panic!("{name} is drawn"))
        .center()
}

/// Presses at `from` and moves to `to` in steps, the button held.
fn drag_to(from: Point<Pixels>, to: Point<Pixels>, cx: &mut VisualTestContext) {
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    for step in 1..=8 {
        let t = step as f32 / 8.;
        let at = point(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
        cx.simulate_mouse_move(at, Some(MouseButton::Left), Modifiers::none());
    }
    cx.run_until_parked();
}

fn drag(from: &'static str, to: &'static str, cx: &mut VisualTestContext) -> Point<Pixels> {
    let (from, to) = (centre(from, cx), centre(to, cx));
    drag_to(from, to, cx);
    to
}

fn release(at: Point<Pixels>, cx: &mut VisualTestContext) {
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

fn verdict(cx: &mut VisualTestContext) -> Option<Verdict> {
    library(cx).read_with(cx, |library, _| library.drag()?.verdict(library))
}

/// `Ctrl`, or `Cmd` on macOS.
fn secondary(cx: &mut VisualTestContext) -> Modifiers {
    cx.update(|_, cx| Modifiers::secondary_key(cx))
}

fn menu_items(cx: &mut VisualTestContext) -> Option<(Vec<String>, Vec<String>)> {
    cx.update(|_, cx| menu::items(cx))
}

#[gpui::test]
fn a_card_dropped_on_a_folder_is_filed_there_by_hand_and_undo_puts_it_back(
    cx: &mut TestAppContext,
) {
    let (_shell, cx) = open_shell(cx);
    let work = folder("work", None, cx);
    show(Shelf::All, cx);
    let to = drag("card-0", "sidebar-folder-work", cx);
    // The ghost says where it goes and what that does (DESIGN §4.1).
    assert_eq!(
        verdict(cx),
        Some(Verdict::Takes {
            result: "work".to_string(),
            note: "files by hand".to_string()
        })
    );
    release(to, cx);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::ByHand(work));
    assert_eq!(toast(cx).as_deref(), Some("Filed in work by hand"));
    assert!(library(cx).read_with(cx, |library, _| library.drag().is_none()));

    // Dropped there again, it refuses, and says why; nothing changes.
    let to = drag("card-0", "sidebar-folder-work", cx);
    assert_eq!(
        verdict(cx),
        Some(Verdict::Refuses("already in work".to_string()))
    );
    release(to, cx);

    undo(cx);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::Unsorted);
}

#[gpui::test]
fn a_press_is_a_drag_after_4px_and_esc_or_letting_go_elsewhere_drops_nothing(
    cx: &mut TestAppContext,
) {
    let (_shell, cx) = open_shell(cx);
    folder("work", None, cx);
    show(Shelf::All, cx);
    // 3px: gpui has started its drag, but nothing shows and nothing would take it.
    let from = centre("card-0", cx);
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        from + point(px(3.), px(0.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.run_until_parked();
    library(cx).read_with(cx, |library, _| {
        let drag = library.drag().expect("a drag");
        assert!(!drag.moved);
    });
    release(from + point(px(3.), px(0.)), cx);
    assert!(library(cx).read_with(cx, |library, _| library.drag().is_none()));

    // Esc gives a drag up.
    drag("card-0", "sidebar-folder-work", cx);
    assert!(verdict(cx).is_some_and(|it| it.takes()));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(library(cx).read_with(cx, |library, _| library.drag().is_none()));

    // Let go over a card, which takes no drop: nothing is filed.
    let to = drag("card-0", "card-2", cx);
    assert_eq!(verdict(cx), None);
    release(to, cx);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::Unsorted);
}

#[gpui::test]
fn sessions_selected_together_move_to_trash_together_and_come_back_with_undo(
    cx: &mut TestAppContext,
) {
    let (_shell, cx) = open_shell(cx);
    let first = centre("card-0", cx);
    let second = centre("card-1", cx);
    cx.simulate_click(first, Modifiers::none());
    let ctrl = secondary(cx);
    cx.simulate_click(second, ctrl);
    let chosen = library(cx).read_with(cx, |library, _| library.selection().len());
    assert_eq!(chosen, 2);
    // A drag of one of them carries both: the ghost says "2 sessions".
    drag("card-1", "sidebar-trash", cx);
    library(cx).read_with(cx, |library, _| {
        let drag = library.drag().expect("a drag");
        assert!(matches!(&drag.what, Dragged::Sessions { ids, .. } if ids.len() == 2));
    });
    cx.simulate_keystrokes("escape");

    cx.simulate_keystrokes("delete");
    cx.run_until_parked();
    let counts = library(cx).read_with(cx, |library, _| {
        (library.counts().all, library.counts().trash)
    });
    assert_eq!(counts, (1, 2));
    assert_eq!(toast(cx).as_deref(), Some("2 sessions moved to Trash"));
    undo(cx);
    let counts = library(cx).read_with(cx, |library, _| {
        (library.counts().all, library.counts().trash)
    });
    assert_eq!(counts, (3, 0));

    // Shift selects the run between; Ctrl A everything shown.
    let third = centre("card-2", cx);
    cx.simulate_click(first, Modifiers::none());
    cx.simulate_click(third, Modifiers::shift());
    let chosen = library(cx).read_with(cx, |library, _| library.selection().len());
    assert_eq!(chosen, 3);
    cx.simulate_click(first, Modifiers::none());
    cx.simulate_keystrokes("secondary-a");
    let chosen = library(cx).read_with(cx, |library, _| library.selection().len());
    assert_eq!(chosen, 3);
}

#[gpui::test]
fn dropped_on_trash_a_session_leaves_the_page(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let to = drag("card-0", "sidebar-trash", cx);
    assert!(verdict(cx).is_some_and(|it| it.takes()));
    release(to, cx);
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.counts().trash),
        1
    );
    assert_eq!(toast(cx).as_deref(), Some("Moved to Trash"));
}

#[gpui::test]
fn the_session_menu_moves_a_session_and_files_it_automatically_again(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let work = folder("work", None, cx);
    show(Shelf::All, cx);
    // The keyboard is in the sidebar after naming the folder: F6 takes it to the page.
    cx.simulate_keystrokes("f6 right");
    cx.simulate_keystrokes("shift-f10");
    cx.run_until_parked();
    let (items, _) = menu_items(cx).expect("the session menu");
    assert_eq!(
        items,
        ["open", "star", "move", "copy-path", "history", "trash"]
    );
    // Move to folder ›: its first folder, work.
    cx.simulate_keystrokes("down down down right");
    cx.run_until_parked();
    let (_, submenu) = menu_items(cx).expect("the session menu");
    assert_eq!(submenu, ["folder/work", "new-folder"]);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(menu_items(cx), None);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::ByHand(work));

    // Filed by hand, its menu offers File automatically, which no path here claims.
    cx.simulate_keystrokes("shift-f10");
    cx.run_until_parked();
    let (items, _) = menu_items(cx).expect("the session menu");
    assert!(items.contains(&"file-automatically".to_string()));
    cx.simulate_keystrokes("down down down down down down enter");
    cx.run_until_parked();
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::Unsorted);
    assert_eq!(
        toast(cx).as_deref(),
        Some("In Unsorted: no folder's path claims it")
    );
    // Undo hands it back to the folder it was filed in by hand.
    undo(cx);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::ByHand(work));

    // M opens Move to folder alone; Esc closes it, and S stars.
    cx.simulate_keystrokes("m");
    cx.run_until_parked();
    let (items, _) = menu_items(cx).expect("Move to folder");
    assert_eq!(items, ["folder/work", "new-folder"]);
    cx.simulate_keystrokes("escape");
    assert_eq!(menu_items(cx), None);
}

#[gpui::test]
fn the_filter_field_narrows_the_page_and_each_page_keeps_its_own(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let shown = |shell: &Entity<Shell>, cx: &mut VisualTestContext| {
        cx.update(|_, cx| shell.read(cx).main().read(cx).sessions().read(cx).shown())
    };
    assert_eq!(shown(&shell, cx), 3);
    // A card is selected, so that S would star it if the key reached the page.
    let card = centre("card-0", cx);
    cx.simulate_click(card, Modifiers::none());
    let field = centre("filter-field", cx);
    cx.simulate_click(field, Modifiers::none());
    cx.simulate_keystrokes("f l a k y");
    cx.run_until_parked();
    assert_eq!(shown(&shell, cx), 1);
    // A single letter is the field's while it has the keyboard: S types, and stars nothing.
    cx.simulate_keystrokes("s");
    cx.run_until_parked();
    let view = library(cx).read_with(cx, |library, _| library.view());
    assert_eq!(view.filter, "flakys");
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.counts().starred),
        0
    );
    assert_eq!(shown(&shell, cx), 0);
    assert!(crate::test_support::bounds(cx, "filter-field").is_some());

    // Starred has its own view; All sessions keeps its filter.
    show(Shelf::Starred, cx);
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.view()),
        PageView::default()
    );
    show(Shelf::All, cx);
    let view = library(cx).read_with(cx, |library, _| library.view());
    assert_eq!(view.filter, "flakys");
    library(cx).update(cx, |library, cx| {
        let mut view = library.view();
        view.clear_filters();
        let provider = library.sessions().unwrap().sessions[0].id.provider.clone();
        view.model = Some((provider, "model-a".to_string()));
        library.set_view(view, cx);
    });
    cx.run_until_parked();
    assert_eq!(shown(&shell, cx), 2);
}

#[gpui::test]
fn the_list_view_moves_with_the_arrows_and_opens_with_enter(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    library(cx).update(cx, |library, cx| {
        library.set_view(
            PageView {
                list: true,
                ..PageView::default()
            },
            cx,
        );
    });
    cx.run_until_parked();
    assert!(bounds(cx, "list-row-0").is_some());
    assert_eq!(bounds(cx, "card-0"), None);
    cx.simulate_keystrokes("down down right");
    let selected = library(cx).read_with(cx, |library, _| library.selected_index());
    assert_eq!(selected, Some(1));
    cx.simulate_keystrokes("enter");
    let open = library(cx).read_with(cx, |library, _| {
        library.open_session().and_then(|it| it.title.clone())
    });
    assert_eq!(open.as_deref(), Some(SESSIONS[1].title));
    // A click on a row with Ctrl adds it to the selection, as on a card.
    cx.simulate_keystrokes("alt-left");
    cx.run_until_parked();
    let row = centre("list-row-0", cx);
    cx.simulate_click(row, Modifiers::none());
    let (row, ctrl) = (centre("list-row-2", cx), secondary(cx));
    cx.simulate_click(row, ctrl);
    let chosen = library(cx).read_with(cx, |library, _| library.selection().len());
    assert_eq!(chosen, 2);
}

#[gpui::test]
fn a_groups_path_dragged_onto_a_folder_attaches_and_undo_detaches_it(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    folder("work", None, cx);
    show(Shelf::Unsorted, cx);
    let to = drag("unsorted-chip-0", "sidebar-folder-work", cx);
    assert_eq!(
        verdict(cx),
        Some(Verdict::Takes {
            result: "attach to work".to_string(),
            note: "files 2 sessions".to_string()
        })
    );
    release(to, cx);
    assert_eq!(
        toast(cx).as_deref(),
        Some("Attached to work · 2 sessions filed")
    );
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.counts().unsorted),
        1
    );
    undo(cx);
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.counts().unsorted),
        3
    );
}

#[gpui::test]
fn a_row_of_unsorted_dropped_on_a_folder_is_filed_there_and_the_menu_attaches_a_path(
    cx: &mut TestAppContext,
) {
    let (_shell, cx) = open_shell(cx);
    let work = folder("work", None, cx);
    show(Shelf::Unsorted, cx);
    let to = drag("unsorted-row-0-0", "sidebar-folder-work", cx);
    assert!(verdict(cx).is_some_and(|it| it.takes()));
    release(to, cx);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::ByHand(work));

    // Attach to folder ▾: the group's path, for its location, to work, with a toast.
    let attach = centre("unsorted-attach-0", cx);
    cx.simulate_click(attach, Modifiers::none());
    cx.run_until_parked();
    let (items, _) = menu_items(cx).expect("Attach to folder's menu");
    assert_eq!(items, ["folder/work"]);
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    assert_eq!(
        toast(cx).as_deref(),
        Some("Attached to work · 1 session filed")
    );
    assert_eq!(filed(SESSIONS[1].title, cx), Filed::ByRule(work));
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.counts().unsorted),
        1
    );
}

#[gpui::test]
fn a_folder_dropped_on_another_nests_and_between_two_reorders(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let a = folder("a", None, cx);
    let b = folder("b", None, cx);
    let parent = |id, cx: &mut VisualTestContext| {
        library(cx).read_with(cx, |library, _| library.folder(id).and_then(|it| it.parent))
    };
    let to = drag("sidebar-folder-b", "sidebar-folder-a", cx);
    assert_eq!(
        verdict(cx),
        Some(Verdict::Takes {
            result: "a".to_string(),
            note: "nests it".to_string()
        })
    );
    release(to, cx);
    assert_eq!(parent(b, cx), Some(a));
    assert_eq!(bounds(cx, "sidebar-folder-b"), None, "a is closed");

    // A drag resting on a closed folder for 500 ms opens it.
    drag("card-0", "sidebar-folder-a", cx);
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(500));
    cx.run_until_parked();
    assert!(bounds(cx, "sidebar-folder-b").is_some(), "a opened");
    cx.simulate_keystrokes("escape");

    // A folder doesn't go into itself, nor into one inside it: the ghost says why.
    let a_row = centre("sidebar-folder-a", cx);
    drag_to(a_row, a_row + point(px(0.), px(6.)), cx);
    assert_eq!(
        verdict(cx),
        Some(Verdict::Refuses("can't go inside itself".to_string()))
    );
    cx.simulate_keystrokes("escape");
    let to = drag("sidebar-folder-a", "sidebar-folder-b", cx);
    assert_eq!(
        verdict(cx),
        Some(Verdict::Refuses("can't go inside itself".to_string()))
    );
    release(to, cx);
    assert_eq!(parent(a, cx), None);

    // Above the first row, at the top: b moves out of a, before it.
    let top = bounds(cx, "sidebar-folder-a").unwrap();
    let from = centre("sidebar-folder-b", cx);
    let to = point(top.center().x, top.top() + px(2.));
    drag_to(from, to, cx);
    assert!(verdict(cx).is_some_and(|it| it.takes()));
    release(to, cx);
    let order = library(cx).read_with(cx, |library, _| {
        let folders = library.folders();
        folders
            .iter()
            .filter(|it| it.parent.is_none())
            .map(|it| it.name.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(order, ["b", "a"]);
}

#[gpui::test]
fn the_paths_editor_refuses_previews_saves_and_asks_before_leaving(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    show(Shelf::Unsorted, cx);
    let group = library(cx).read_with(cx, |library, _| {
        crate::services::group_key(&library.groups()[0])
    });
    library(cx).update(cx, |library, cx| library.create_folder_from(&group, cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    let app = library(cx).read_with(cx, |library, _| library.folders()[0].id);
    library(cx).update(cx, |library, cx| library.edit_paths(app, cx));
    cx.run_until_parked();
    let editor = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            shell
                .read(cx)
                .main()
                .read(cx)
                .sessions()
                .read(cx)
                .editor()
                .cloned()
        })
    };
    let open = editor(cx).expect("the paths editor");
    assert!(
        cx.update(|window, cx| open.read(cx).is_typing(window, cx)),
        "the add row has the keyboard"
    );
    assert!(!open.read_with(cx, |editor, _| editor.is_short()));

    // A path this folder has already is refused: Enter adds nothing.
    cx.simulate_input("/home/user/code/app/");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(open.read_with(cx, |editor, _| editor.added().count()), 0);

    // Another is a change to save, and the footer says what saving does.
    cx.simulate_keystrokes("secondary-a backspace");
    cx.simulate_input("/home/user/notes");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        open.read_with(cx, |editor, _| editor
            .added()
            .map(str::to_string)
            .collect::<Vec<_>>()),
        ["/home/user/notes"]
    );

    // Leaving with it asks; staying keeps it.
    show(Shelf::Starred, cx);
    assert!(library(cx).read_with(cx, |library, _| library.asked().is_some()));
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.shelf()),
        Shelf::Folder(app)
    );
    cx.simulate_keystrokes("escape");
    assert!(library(cx).read_with(cx, |library, _| library.asked().is_none()));

    // Ctrl Enter in the field saves: the session at that path is filed here.
    cx.update(|window, cx| open.update(cx, |editor, cx| editor.focus_input(window, cx)));
    cx.simulate_keystrokes("secondary-enter");
    cx.run_until_parked();
    assert!(editor(cx).is_none());
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.counts().unsorted),
        0
    );
    assert_eq!(toast(cx).as_deref(), Some("Paths of app saved"));
    undo(cx);
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.counts().unsorted),
        1
    );
    // Nothing waits now: leaving doesn't ask.
    show(Shelf::Starred, cx);
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.shelf()),
        Shelf::Starred
    );
}

#[gpui::test]
fn an_empty_folder_offers_its_paths_and_attaches_a_suggestion_at_once(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let notes = folder("notes", None, cx);
    show(Shelf::Folder(notes), cx);
    let editor = cx.update(|_, cx| {
        shell
            .read(cx)
            .main()
            .read(cx)
            .sessions()
            .read(cx)
            .editor()
            .cloned()
    });
    assert!(
        editor
            .expect("the short editor")
            .read_with(cx, |editor, _| editor.is_short())
    );
    // Suggested from Unsorted: the path whose last component is the folder's name.
    let attach = centre("paths-attach-0", cx);
    cx.simulate_click(attach, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        toast(cx).as_deref(),
        Some("Attached to notes · 1 session filed")
    );
    assert_eq!(filed(SESSIONS[2].title, cx), Filed::ByRule(notes));
}

#[gpui::test]
fn a_folders_path_chip_dropped_on_another_folder_moves_the_path_there(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    show(Shelf::Unsorted, cx);
    let group = library(cx).read_with(cx, |library, _| {
        crate::services::group_key(&library.groups()[0])
    });
    library(cx).update(cx, |library, cx| library.create_folder_from(&group, cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    let app = library(cx).read_with(cx, |library, _| library.folders()[0].id);
    let work = folder("work", None, cx);
    show(Shelf::Folder(app), cx);
    // Onto its own folder: refused, and the ghost says why.
    let to = drag("path-chip-0", "sidebar-folder-app", cx);
    assert_eq!(
        verdict(cx),
        Some(Verdict::Refuses("already attached to app".to_string()))
    );
    release(to, cx);
    let to = drag("path-chip-0", "sidebar-folder-work", cx);
    assert_eq!(
        verdict(cx),
        Some(Verdict::Takes {
            result: "move to work".to_string(),
            note: "files 2 sessions".to_string()
        })
    );
    release(to, cx);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::ByRule(work));
    assert_eq!(
        toast(cx).as_deref(),
        Some("Moved /home/user/code/app to work")
    );
    undo(cx);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::ByRule(app));
}

#[gpui::test]
fn deleting_a_folder_asks_first(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let work = folder("work", None, cx);
    library(cx).update(cx, |library, cx| library.ask_delete(work, cx));
    cx.run_until_parked();
    assert!(bounds(cx, "delete-folder").is_some());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.folders().len()),
        1
    );
    library(cx).update(cx, |library, cx| {
        library.ask_delete(work, cx);
        library.answer_delete(true, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.folders().len()),
        0
    );
}

#[gpui::test]
fn a_toast_goes_after_five_seconds(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let id: SessionId = library(cx).read_with(cx, |library, _| {
        library.sessions().unwrap().sessions[0].id.clone()
    });
    library(cx).update(cx, |library, cx| library.trash(vec![id], cx));
    cx.run_until_parked();
    assert!(toast(cx).is_some());
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(5));
    cx.run_until_parked();
    assert_eq!(toast(cx), None);
}

/// The test library's sessions, the newest first.
fn session_ids(cx: &mut VisualTestContext) -> Vec<SessionId> {
    library(cx).read_with(cx, |library, _| {
        (library.sessions().unwrap().sessions.iter())
            .map(|it| it.id.clone())
            .collect()
    })
}

fn trashed(cx: &mut VisualTestContext) -> usize {
    library(cx).read_with(cx, |library, _| library.counts().trash)
}

fn main_has_keyboard(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| shell.read(cx).main().focus_handle(cx).is_focused(window))
}

#[gpui::test]
fn the_delete_folder_dialog_keeps_the_page_keys_from_the_page_and_gives_the_keyboard_back(
    cx: &mut TestAppContext,
) {
    let (shell, cx) = open_shell(cx);
    let work = folder("work", None, cx);
    show(Shelf::All, cx);
    let card = centre("card-0", cx);
    cx.simulate_click(card, Modifiers::none());
    library(cx).update(cx, |library, cx| library.ask_delete(work, cx));
    cx.run_until_parked();
    assert!(!main_has_keyboard(&shell, cx), "the dialog has it");

    // Delete, Enter and S are the page's keys: behind the dialog they do nothing.
    cx.simulate_keystrokes("delete enter s");
    cx.run_until_parked();
    let (starred, open) = library(cx).read_with(cx, |library, _| {
        (library.counts().starred, library.open_session().is_some())
    });
    assert_eq!((trashed(cx), starred, open), (0, 0, false));
    assert!(bounds(cx, "delete-folder").is_some());

    // Esc answers; the keyboard is back on the page, where Delete acts again.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(bounds(cx, "delete-folder"), None);
    assert!(main_has_keyboard(&shell, cx));
    cx.simulate_keystrokes("delete");
    cx.run_until_parked();
    assert_eq!(trashed(cx), 1);
}

/// Opens the paths editor of a folder made from Unsorted's first group, `app`, with a path
/// added and not saved.
fn editing_app(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Entity<PathsEditor> {
    show(Shelf::Unsorted, cx);
    let group = library(cx).read_with(cx, |library, _| {
        crate::services::group_key(&library.groups()[0])
    });
    library(cx).update(cx, |library, cx| library.create_folder_from(&group, cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    let app = library(cx).read_with(cx, |library, _| library.folders()[0].id);
    library(cx).update(cx, |library, cx| library.edit_paths(app, cx));
    cx.run_until_parked();
    let editor = cx
        .update(|_, cx| {
            shell
                .read(cx)
                .main()
                .read(cx)
                .sessions()
                .read(cx)
                .editor()
                .cloned()
        })
        .expect("the paths editor");
    cx.simulate_input("/home/user/notes");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    editor
}

#[gpui::test]
fn the_question_about_unsaved_paths_has_the_keyboard_until_it_is_answered(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let editor = editing_app(&shell, cx);
    let typing =
        |cx: &mut VisualTestContext| cx.update(|window, cx| editor.read(cx).is_typing(window, cx));
    assert!(typing(cx));
    show(Shelf::Starred, cx);
    assert!(library(cx).read_with(cx, |library, _| library.asked().is_some()));
    assert!(!typing(cx), "the question has the keyboard");

    // Typing goes nowhere behind it: the editor's changes stay as they were.
    cx.simulate_input("/home/user/elsewhere");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(editor.read_with(cx, |editor, _| editor.added().count()), 1);

    // Cancel: the question goes and the keyboard is back in the add row.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(library(cx).read_with(cx, |library, _| library.asked().is_none()));
    assert!(typing(cx));
}

#[gpui::test]
fn closing_the_window_with_unsaved_paths_asks_first(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let editor = editing_app(&shell, cx);
    assert!(!cx.simulate_close(), "the window stays to ask");
    assert!(library(cx).read_with(cx, |library, _| matches!(
        library.asked(),
        Some(Leave::Close)
    )));
    // Cancel keeps the changes, and the next close asks again.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!cx.simulate_close());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    // With nothing to save it closes at once.
    editor.update(cx, |editor, cx| editor.cancel(cx));
    cx.run_until_parked();
    assert!(cx.simulate_close());
}

#[gpui::test]
fn the_keys_act_only_on_the_sessions_the_page_shows(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let ids = session_ids(cx);
    let (first, second) = (centre("card-0", cx), centre("card-1", cx));
    cx.simulate_click(first, Modifiers::none());
    let ctrl = secondary(cx);
    cx.simulate_click(second, ctrl);
    // The filter hides both: the selection goes with them, and Delete and S do nothing.
    library(cx).update(cx, |library, cx| {
        library.set_view(
            PageView {
                filter: "summarise".to_string(),
                ..PageView::default()
            },
            cx,
        );
    });
    cx.run_until_parked();
    assert!(library(cx).read_with(cx, |library, _| library.selection().is_empty()));
    cx.simulate_keystrokes("delete s");
    cx.run_until_parked();
    assert_eq!(trashed(cx), 0);
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.counts().starred),
        0
    );

    /* A session moved to Trash leaves the selection at once; the one after it is selected,
    and the arrows go on from there. */
    library(cx).update(cx, |library, cx| library.set_view(PageView::default(), cx));
    cx.run_until_parked();
    let first = centre("card-0", cx);
    cx.simulate_click(first, Modifiers::none());
    cx.simulate_keystrokes("delete");
    cx.run_until_parked();
    assert_eq!(trashed(cx), 1);
    let selected = library(cx).read_with(cx, |library, _| library.selected().cloned());
    assert_eq!(selected.as_ref(), Some(&ids[1]));
    cx.simulate_keystrokes("right");
    let selected = library(cx).read_with(cx, |library, _| library.selected().cloned());
    assert_eq!(selected.as_ref(), Some(&ids[2]));
}

#[gpui::test]
fn undo_puts_back_what_was_before_each_change_however_quickly_they_came(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let work = folder("work", None, cx);
    let other = folder("other", None, cx);
    show(Shelf::All, cx);
    let id = session_ids(cx)[0].clone();
    // Two changes made before the first is written.
    library(cx).update(cx, |library, cx| {
        library.file_by_hand(vec![id.clone()], work, cx);
        library.file_by_hand(vec![id.clone()], other, cx);
    });
    cx.run_until_parked();
    assert_eq!(toast(cx).as_deref(), Some("Filed in other by hand"));
    undo(cx);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::ByHand(work));
}

#[gpui::test]
fn undo_after_the_world_moved_on_puts_back_what_it_can_and_says_what_it_couldnt(
    cx: &mut TestAppContext,
) {
    let (_shell, cx) = open_shell(cx);
    let work = folder("work", None, cx);
    let other = folder("other", None, cx);
    show(Shelf::All, cx);
    let ids = session_ids(cx);
    let archive = cx.update(|_, cx| cx.services().archive.read(cx).archive().cloned().unwrap());

    /* The folder one of the sessions was in is deleted before Undo: that one goes where the
    rules put it, the other back where it was, and the toast says so. */
    library(cx).update(cx, |library, cx| {
        library.file_by_hand(vec![ids[0].clone()], work, cx);
    });
    cx.run_until_parked();
    library(cx).update(cx, |library, cx| {
        library.file_by_hand(vec![ids[0].clone(), ids[1].clone()], other, cx);
    });
    cx.run_until_parked();
    archive.catalogue().delete_folder(work).unwrap();
    library(cx).update(cx, LibraryService::reload);
    cx.run_until_parked();
    undo(cx);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::Unsorted);
    assert_eq!(filed(SESSIONS[1].title, cx), Filed::Unsorted);
    assert_eq!(
        toast(cx).as_deref(),
        Some("Undone, but 1 session was filed by its path, as its folder is gone")
    );

    /* A path attached, then moved to another folder before Undo: the Undo is refused, and
    the banner says why in words. */
    let app = ("/home/user/code/app".to_string(), None);
    library(cx).update(cx, |library, cx| library.attach(other, app.clone(), 2, cx));
    cx.run_until_parked();
    archive
        .catalogue()
        .create_folder("elsewhere", None)
        .and_then(|elsewhere| {
            archive
                .catalogue()
                .move_path(other, (&app.0, None), elsewhere)
        })
        .unwrap();
    library(cx).update(cx, LibraryService::reload);
    cx.run_until_parked();
    undo(cx);
    let failure =
        library(cx).read_with(cx, |library, _| library.write_failure().map(str::to_string));
    assert_eq!(
        failure.as_deref(),
        Some(
            "The change of paths couldn't be saved: /home/user/code/app isn't attached to \
             that folder any more."
        )
    );

    // A toast whose Undo names a folder that is gone goes with it.
    let notes = ("/home/user/notes".to_string(), None);
    library(cx).update(cx, |library, cx| library.attach(other, notes, 1, cx));
    cx.run_until_parked();
    assert!(toast(cx).is_some());
    library(cx).update(cx, |library, cx| {
        library.ask_delete(other, cx);
        library.answer_delete(true, cx);
    });
    cx.run_until_parked();
    assert_eq!(toast(cx), None);
}

#[gpui::test]
fn a_click_finds_its_session_on_the_page_as_it_is_when_the_click_lands(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let ids = session_ids(cx);
    /* The first two leave the page, as another change would: the third is first now, and a
    click on it as it was drawn, third, finds it there. */
    let archive = cx.update(|_, cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    archive.catalogue().trash_all(&ids[..2]).unwrap();
    library(cx).update(cx, LibraryService::reload);
    cx.run_until_parked();
    let page = cx.update(|_, cx| shell.read(cx).main().read(cx).sessions().clone());
    let click = |id: &SessionId, hint: usize, cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            page.update(cx, |page, cx| {
                page.click((id, hint), (Modifiers::none(), 1), window, cx);
            });
        });
    };
    click(&ids[2], 2, cx);
    let selected = library(cx).read_with(cx, |library, _| {
        (library.selected().cloned(), library.selected_index())
    });
    assert_eq!(selected, (Some(ids[2].clone()), Some(0)));
    // One that is gone selects nothing else.
    click(&ids[0], 0, cx);
    let selected = library(cx).read_with(cx, |library, _| library.selected().cloned());
    assert_eq!(selected, Some(ids[2].clone()));
}

#[gpui::test]
fn shift_runs_from_the_last_plain_or_ctrl_click_and_ctrl_shift_adds_the_run(
    cx: &mut TestAppContext,
) {
    let (_shell, cx) = open_shell(cx);
    let cards = [
        centre("card-0", cx),
        centre("card-1", cx),
        centre("card-2", cx),
    ];
    let chosen = |cx: &mut VisualTestContext| {
        library(cx).read_with(cx, |library, _| library.selection().len())
    };
    cx.simulate_click(cards[0], Modifiers::none());
    cx.simulate_click(cards[2], Modifiers::shift());
    assert_eq!(chosen(cx), 3);
    // The run starts where the plain click was, not at the last Shift-click.
    cx.simulate_click(cards[1], Modifiers::shift());
    assert_eq!(chosen(cx), 2);

    let ctrl = secondary(cx);
    cx.simulate_click(cards[2], Modifiers::none());
    cx.simulate_click(cards[0], ctrl);
    assert_eq!(chosen(cx), 2);
    cx.simulate_click(
        cards[1],
        Modifiers {
            shift: true,
            ..ctrl
        },
    );
    assert_eq!(chosen(cx), 3);
}

#[gpui::test]
fn a_press_that_never_becomes_a_drag_is_a_click(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let ids = session_ids(cx);
    let wiggle = |at: Point<Pixels>, modifiers: Modifiers, cx: &mut VisualTestContext| {
        cx.simulate_mouse_down(at, MouseButton::Left, modifiers);
        let moved = at + point(px(3.), px(0.));
        cx.simulate_mouse_move(moved, Some(MouseButton::Left), modifiers);
        cx.simulate_mouse_up(moved, MouseButton::Left, modifiers);
        cx.run_until_parked();
    };
    // gpui starts its drag past 2px; the drag never moved 4px, so it was a click.
    let second = centre("card-1", cx);
    wiggle(second, Modifiers::none(), cx);
    let selected = library(cx).read_with(cx, |library, _| library.selected().cloned());
    assert_eq!(selected, Some(ids[1].clone()));
    assert!(library(cx).read_with(cx, |library, _| library.drag().is_none()));
    // With Ctrl it adds to the selection, as a click with Ctrl does.
    let third = centre("card-2", cx);
    let ctrl = secondary(cx);
    wiggle(third, ctrl, cx);
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.selection().len()),
        2
    );
}

#[gpui::test]
fn a_press_that_jumps_to_a_folder_drops_there(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let work = folder("work", None, cx);
    show(Shelf::All, cx);
    let (from, to) = (centre("card-0", cx), centre("sidebar-folder-work", cx));
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::none());
    release(to, cx);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::ByHand(work));
}

#[gpui::test]
fn a_key_pressed_mid_drag_does_nothing_and_the_release_still_drops(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let work = folder("work", None, cx);
    show(Shelf::All, cx);
    let to = drag("card-0", "sidebar-folder-work", cx);
    // Delete would move the dragged session to Trash: no key but Esc acts mid-drag.
    cx.simulate_keystrokes("delete s");
    cx.run_until_parked();
    assert_eq!(trashed(cx), 0);
    assert!(library(cx).read_with(cx, |library, _| library.drag().is_some()));
    release(to, cx);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::ByHand(work));
    assert!(library(cx).read_with(cx, |library, _| library.drag().is_none()));
}

#[gpui::test]
fn an_empty_folders_page_takes_sessions_but_not_folders(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let notes = folder("notes", None, cx);
    folder("work", None, cx);
    show(Shelf::Folder(notes), cx);
    drag("sidebar-folder-work", "page-empty", cx);
    assert_eq!(verdict(cx), None);
    cx.simulate_keystrokes("escape");
}

#[gpui::test]
fn a_drag_resting_on_a_narrow_windows_sidebar_button_shows_the_sidebar_until_it_ends(
    cx: &mut TestAppContext,
) {
    let (shell, cx) = open_shell_sized(size(px(960.), px(600.)), cx);
    let work = folder("work", None, cx);
    show(Shelf::All, cx);
    let placement = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| shell.read(cx).sidebar_placement(px(960.), cx))
    };
    // Naming the folder showed the sidebar; Esc hides it again.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(placement(cx), SidebarPlacement::Collapsed);
    let button = drag("card-0", "titlebar-sidebar", cx);
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(500));
    cx.run_until_parked();
    assert_eq!(placement(cx), SidebarPlacement::Overlay);
    let row = centre("sidebar-folder-work", cx);
    for step in 1..=8 {
        let t = step as f32 / 8.;
        let at = point(
            button.x + (row.x - button.x) * t,
            button.y + (row.y - button.y) * t,
        );
        cx.simulate_mouse_move(at, Some(MouseButton::Left), Modifiers::none());
    }
    release(row, cx);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::ByHand(work));
    assert_eq!(placement(cx), SidebarPlacement::Collapsed);
}

#[gpui::test]
fn a_new_folder_takes_a_free_name_and_a_rename_to_a_siblings_name_is_refused(
    cx: &mut TestAppContext,
) {
    let (_shell, cx) = open_shell(cx);
    folder("work", None, cx);
    for _ in 0..2 {
        library(cx).update(cx, |library, cx| {
            library.create_folder(NEW_FOLDER.to_string(), None, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }
    let names = |cx: &mut VisualTestContext| {
        library(cx).read_with(cx, |library, _| {
            (library.folders().iter())
                .map(|it| it.name.clone())
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(names(cx), ["work", "New folder", "New folder 2"]);
    /* F2 renames the folder shown; "WORK" is work's name in another case: Enter leaves the
    field open with what was typed, and a name of its own is then kept. */
    let second = library(cx).read_with(cx, |library, _| library.folders()[2].id);
    show(Shelf::Folder(second), cx);
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    cx.simulate_input("WORK");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(names(cx), ["work", "New folder", "New folder 2"]);
    cx.simulate_input(" 2");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(names(cx), ["work", "New folder", "WORK 2"]);
    // Esc on a refused name keeps the folder's own.
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    cx.simulate_input("new FOLDER");
    cx.simulate_keystrokes("enter escape");
    cx.run_until_parked();
    assert_eq!(names(cx), ["work", "New folder", "WORK 2"]);
}

#[gpui::test]
fn shift_f10_in_the_filter_field_opens_no_menu(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let card = centre("card-0", cx);
    cx.simulate_click(card, Modifiers::none());
    let field = centre("filter-field", cx);
    cx.simulate_click(field, Modifiers::none());
    cx.simulate_keystrokes("shift-f10");
    cx.run_until_parked();
    assert_eq!(menu_items(cx), None);
}

#[gpui::test]
fn a_toast_stays_while_the_pointer_rests_on_it(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let id = session_ids(cx)[0].clone();
    library(cx).update(cx, |library, cx| library.trash(vec![id], cx));
    cx.run_until_parked();
    let over = centre("toast", cx);
    cx.simulate_mouse_move(over, None, Modifiers::none());
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(6));
    cx.run_until_parked();
    assert!(toast(cx).is_some());
    cx.simulate_mouse_move(point(px(10.), px(400.)), None, Modifiers::none());
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(5));
    cx.run_until_parked();
    assert_eq!(toast(cx), None);
}

#[gpui::test]
fn a_path_taken_off_and_added_again_is_no_change(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let work = folder("work", None, cx);
    let app = ("/home/user/code/app".to_string(), None);
    library(cx).update(cx, |library, cx| library.attach(work, app, 2, cx));
    cx.run_until_parked();
    library(cx).update(cx, |library, cx| library.edit_paths(work, cx));
    cx.run_until_parked();
    let editor = cx
        .update(|_, cx| {
            shell
                .read(cx)
                .main()
                .read(cx)
                .sessions()
                .read(cx)
                .editor()
                .cloned()
        })
        .expect("the paths editor");
    let remove = centre("paths-remove-0", cx);
    cx.simulate_click(remove, Modifiers::none());
    assert!(editor.read_with(cx, |editor, _| editor.is_pending()));
    cx.update(|window, cx| editor.update(cx, |editor, cx| editor.focus_input(window, cx)));
    cx.simulate_input("/home/user/code/app/");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(!editor.read_with(cx, |editor, _| editor.is_pending()));
    assert_eq!(editor.read_with(cx, |editor, _| editor.added().count()), 0);
}

#[gpui::test]
fn ctrl_click_takes_a_session_out_of_the_selection_and_the_keys_follow(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let ids = session_ids(cx);
    let cards = [
        centre("card-0", cx),
        centre("card-1", cx),
        centre("card-2", cx),
    ];
    let ctrl = secondary(cx);
    let state = |cx: &mut VisualTestContext| {
        library(cx).read_with(cx, |library, _| {
            let selection = library.selection();
            let shown: Vec<bool> = ids.iter().map(|id| library.is_selected(id)).collect();
            (selection.len(), library.selected().cloned(), shown)
        })
    };
    // Three, then Ctrl on the last: two are left, the first (where the run starts) acted on.
    cx.simulate_click(cards[0], Modifiers::none());
    cx.simulate_click(cards[1], ctrl);
    cx.simulate_click(cards[2], ctrl);
    cx.simulate_click(cards[2], ctrl);
    assert_eq!(
        state(cx),
        (2, Some(ids[0].clone()), vec![true, true, false])
    );
    // Two, then Ctrl on the first: the other alone is selected.
    cx.simulate_click(cards[0], ctrl);
    assert_eq!(
        state(cx),
        (1, Some(ids[1].clone()), vec![false, true, false])
    );
    // Delete trashes what shows as selected.
    cx.simulate_keystrokes("delete");
    cx.run_until_parked();
    assert_eq!(trashed(cx), 1);
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::Unsorted);
    let archive = cx.update(|_, cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    let in_trash = archive.catalogue().sessions(Shelf::Trash).unwrap();
    assert_eq!(in_trash[0].id, ids[1]);
}

#[gpui::test]
fn the_shells_keys_do_nothing_behind_a_dialog(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let work = folder("work", None, cx);
    show(Shelf::Folder(work), cx);
    show(Shelf::All, cx);
    let card = centre("card-0", cx);
    cx.simulate_click(card, Modifiers::none());
    library(cx).update(cx, |library, cx| library.ask_delete(work, cx));
    cx.run_until_parked();
    // F6 twice, Ctrl B, Ctrl Shift N, F2, Ctrl L and Alt ←, then the page's Delete.
    cx.simulate_keystrokes("f6 f6 secondary-b secondary-shift-n f2 secondary-l alt-left delete");
    cx.run_until_parked();
    let folders = library(cx).read_with(cx, |library, _| library.folders().len());
    assert_eq!((folders, trashed(cx)), (1, 0));
    assert!(cx.update(|_, cx| shell.read(cx).locations_sheet().is_none()));
    assert!(bounds(cx, "delete-folder").is_some());
    assert!(!main_has_keyboard(&shell, cx));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(main_has_keyboard(&shell, cx));
}

#[gpui::test]
fn new_folders_made_before_the_folders_are_read_again_each_take_a_free_name(
    cx: &mut TestAppContext,
) {
    let (_shell, cx) = open_shell(cx);
    library(cx).update(cx, |library, cx| {
        library.create_folder(NEW_FOLDER.to_string(), None, cx);
        library.create_folder(NEW_FOLDER.to_string(), None, cx);
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    let names = library(cx).read_with(cx, |library, _| {
        (library.folders().iter())
            .map(|it| it.name.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(names, ["New folder", "New folder 2"]);
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.write_failure().map(str::to_string)),
        None
    );
}

#[gpui::test]
fn a_folder_doesnt_go_where_a_folder_has_its_name_and_a_delete_renames_what_would_clash(
    cx: &mut TestAppContext,
) {
    let (_shell, cx) = open_shell(cx);
    folder("Work", None, cx);
    let x = folder("x", None, cx);
    folder("work", Some(x), cx);
    // x is open, its work shown: Work onto x is refused, and the ghost says why.
    let to = drag("sidebar-folder-Work", "sidebar-folder-x", cx);
    assert_eq!(
        verdict(cx),
        Some(Verdict::Refuses(
            "a folder named Work is in x already".to_string()
        ))
    );
    release(to, cx);
    // Deleting x moves its work up beside Work: it is work 2 now, and the toast says so.
    library(cx).update(cx, |library, cx| {
        library.ask_delete(x, cx);
        library.answer_delete(true, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        toast(cx).as_deref(),
        Some("Folder deleted; its subfolder work is work 2 now")
    );
    let names = library(cx).read_with(cx, |library, _| {
        (library.folders().iter())
            .map(|it| it.name.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(names, ["Work", "work 2"]);
}

#[gpui::test]
fn an_undo_quiets_the_toasts_of_changes_made_before_it(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let work = folder("work", None, cx);
    let other = folder("other", None, cx);
    show(Shelf::All, cx);
    let id = session_ids(cx)[0].clone();
    library(cx).update(cx, |library, cx| {
        library.file_by_hand(vec![id.clone()], work, cx)
    });
    cx.run_until_parked();
    // Filed in other, and before that is written, the first toast's Undo.
    library(cx).update(cx, |library, cx| {
        library.file_by_hand(vec![id.clone()], other, cx);
        library.undo(cx);
    });
    cx.run_until_parked();
    // The Undo has the last word: no "Filed in other", only that it is undone.
    assert_eq!(toast(cx).as_deref(), Some("Undone"));
    assert_eq!(filed(SESSIONS[0].title, cx), Filed::Unsorted);
}

#[gpui::test]
fn saving_from_the_close_question_saves_the_paths_and_closes(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    editing_app(&shell, cx);
    let archive = cx.update(|_, cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    assert!(!cx.simulate_close());
    let page = cx.update(|_, cx| shell.read(cx).main().read(cx).sessions().clone());
    cx.update(|window, cx| page.update(cx, |page, cx| page.answer(Some(true), window, cx)));
    cx.run_until_parked();
    let notes = archive
        .catalogue()
        .sessions(Shelf::All)
        .unwrap()
        .into_iter()
        .find(|it| it.title.as_deref() == Some(SESSIONS[2].title))
        .map(|it| it.filed);
    assert!(matches!(notes, Some(Filed::ByRule(_))), "{notes:?}");
}

#[gpui::test]
fn a_quit_that_asks_nothing_saves_the_paths_editors_changes(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    editing_app(&shell, cx);
    let archive = cx.update(|_, cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    // From the Dock, or at logout: gpui runs the quit hooks without asking anything.
    let app: &mut TestAppContext = cx;
    app.update(App::shutdown);
    let notes = archive
        .catalogue()
        .sessions(Shelf::All)
        .unwrap()
        .into_iter()
        .find(|it| it.title.as_deref() == Some(SESSIONS[2].title))
        .map(|it| it.filed);
    assert!(matches!(notes, Some(Filed::ByRule(_))), "{notes:?}");
}

/**
 * Restart to update with unsaved paths asks about them first, as Quit does: Cancel keeps
 * everything and hands nothing over; going on hands the update over.
 */
#[gpui::test]
fn restarting_to_update_with_unsaved_paths_asks_first(cx: &mut TestAppContext) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::services::updates::{FIRST_CHECK_AFTER, Real, Updater};

    let (shell, cx) = open_shell(cx);
    let _editor = editing_app(&shell, cx);
    let restarts = Arc::new(AtomicUsize::new(0));
    let counted = restarts.clone();
    let updates = cx.update(|_, cx| cx.services().updates.clone());
    updates.update(cx, |updates, cx| {
        updates.set_updater(
            Updater::Real(Real {
                check: Arc::new(|| Box::pin(async { Ok(Some("0.2.0".to_string())) })),
                restart: Box::new(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
                others: Box::new(|| 0),
                then: |_| {},
            }),
            cx,
        )
    });
    cx.executor().advance_clock(FIRST_CHECK_AFTER);
    cx.run_until_parked();

    updates.update(cx, |updates, cx| updates.restart(cx));
    cx.run_until_parked();
    assert!(library(cx).read_with(cx, |library, _| matches!(
        library.asked(),
        Some(Leave::Restart)
    )));
    assert_eq!(
        restarts.load(Ordering::SeqCst),
        0,
        "nothing before the answer"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(library(cx).read_with(cx, |library, _| library.asked().is_none()));
    assert_eq!(
        restarts.load(Ordering::SeqCst),
        0,
        "Cancel hands nothing over"
    );

    updates.update(cx, |updates, cx| updates.restart(cx));
    cx.run_until_parked();
    library(cx).update(cx, |library, cx| library.answer(true, cx));
    cx.run_until_parked();
    assert_eq!(restarts.load(Ordering::SeqCst), 1);
    updates.read_with(cx, |updates, _| assert!(updates.is_restarted()));
}
