//! The library pages' interactions (DESIGN §4.1, §4.4–§4.7, §4.14): drag and drop, selection,
//! the session menu, the filters, the list view, Unsorted's paths and the paths editor, over
//! the test library of [`crate::test_support`]: three sessions, all in Unsorted at first.

use gpui::{
    Entity, Modifiers, MouseButton, Pixels, Point, TestAppContext, VisualTestContext, point, px,
};
use pretty_assertions::assert_eq;
use sb_core::catalogue::{Filed, SessionId, Shelf};
use sb_core::filing::FolderId;

use crate::menu;
use crate::services::{ActiveServices as _, Dragged, LibraryService, PageView, Verdict};
use crate::shell::Shell;
use crate::test_support::{SESSIONS, bounds, open_shell};

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
        view.model = Some("model-a".to_string());
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

    // A folder doesn't go into itself, nor into one inside it: no target shows.
    let a_row = centre("sidebar-folder-a", cx);
    drag_to(a_row, a_row + point(px(0.), px(2.)), cx);
    drag_to(a_row, a_row, cx);
    let refused = verdict(cx);
    assert!(!refused.as_ref().is_some_and(Verdict::takes), "{refused:?}");
    cx.simulate_keystrokes("escape");

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
