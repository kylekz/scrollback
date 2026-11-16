//! Trash and deleting for good (DESIGN §4.8), over the test library of
//! [`crate::test_support`]: restoring, into a folder that is gone too, its Undo, and the
//! confirmation that deletes for good, whose `Enter` never confirms.

use gpui::{
    Entity, KeyUpEvent, Keystroke, Modifiers, TestAppContext, VisualTestContext, point, px,
};
use pretty_assertions::assert_eq;
use sb_core::archive::ArchiveError;
use sb_core::catalogue::{Deletion, Filed, Session, SessionId, Shelf};
use sb_core::filing::FolderId;

use super::Showing;
use crate::services::library::Refusal;
use crate::services::{ActiveServices as _, LibraryService};
use crate::shell::Shell;
use crate::test_support::{SESSIONS, bounds, open_shell, write_with};

fn library(cx: &mut VisualTestContext) -> Entity<LibraryService> {
    cx.update(|_, cx| cx.services().library.clone())
}

fn show(shelf: Shelf, cx: &mut VisualTestContext) {
    library(cx).update(cx, |library, cx| library.show(shelf, cx));
    cx.run_until_parked();
}

/// Presses and releases `key`: a button with the keyboard is clicked on the release.
fn press(cx: &mut VisualTestContext, key: &'static str) {
    cx.simulate_keystrokes(key);
    cx.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse(key).unwrap(),
    });
    cx.run_until_parked();
}

/// Every session the archive holds, in Trash or not.
fn sessions(cx: &mut VisualTestContext) -> Vec<Session> {
    let archive = cx.update(|_, cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    let catalogue = archive.catalogue();
    let mut all = catalogue.sessions(Shelf::All).unwrap();
    all.extend(catalogue.sessions(Shelf::Trash).unwrap());
    all
}

fn session(title: &str, cx: &mut VisualTestContext) -> Option<Session> {
    sessions(cx)
        .into_iter()
        .find(|it| it.title.as_deref() == Some(title))
}

fn id(title: &str, cx: &mut VisualTestContext) -> SessionId {
    session(title, cx).expect("the session").id
}

/// Makes a folder named `name` and files the session titled `title` in it by hand.
fn filed_in(name: &str, title: &str, cx: &mut VisualTestContext) -> FolderId {
    let archive = cx.update(|_, cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    let folder = archive.catalogue().create_folder(name, None).unwrap();
    archive
        .catalogue()
        .file_by_hand(&id(title, cx), folder)
        .unwrap();
    library(cx).update(cx, |library, cx| library.reload(cx));
    cx.run_until_parked();
    folder
}

/// Moves the session titled `title` to Trash, and shows Trash.
fn trash(title: &str, cx: &mut VisualTestContext) {
    let id = id(title, cx);
    library(cx).update(cx, |library, cx| library.trash(vec![id], cx));
    cx.run_until_parked();
    show(Shelf::Trash, cx);
}

fn toast(cx: &mut VisualTestContext) -> Option<(String, bool)> {
    library(cx).read_with(cx, |library, _| {
        library.toast().map(|it| (it.text.clone(), it.can_undo()))
    })
}

fn in_trash(cx: &mut VisualTestContext) -> Vec<String> {
    library(cx).read_with(cx, |library, _| {
        let mut titles: Vec<String> = (library.sessions().unwrap().sessions.iter())
            .filter_map(|it| it.title.clone())
            .collect();
        titles.sort();
        titles
    })
}

fn click(name: &'static str, cx: &mut VisualTestContext) {
    let at = bounds(cx, name).unwrap_or_else(|| panic!("{name} is drawn"));
    cx.simulate_click(at.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

#[gpui::test]
fn trash_is_a_page_and_restore_puts_a_session_back_with_an_undo(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let kept = filed_in("Kept", SESSIONS[0].title, cx);
    trash(SESSIONS[0].title, cx);
    assert_eq!(cx.update(|_, cx| Showing::now(cx)), Showing::Trash);
    assert_eq!(in_trash(cx), [SESSIONS[0].title]);
    assert!(bounds(cx, "trash-table").is_some());

    click("trash-restore-0", cx);
    assert_eq!(
        toast(cx),
        Some(("Restored to Kept".to_string(), true)),
        "the toast says where it went"
    );
    assert_eq!(
        session(SESSIONS[0].title, cx).unwrap().filed,
        Filed::ByHand(kept)
    );
    assert_eq!(in_trash(cx), Vec::<String>::new());
    assert!(bounds(cx, "trash-table").is_none(), "Trash is empty");

    // Undo moves it to Trash again, and its own toast has no Undo.
    library(cx).update(cx, LibraryService::undo);
    cx.run_until_parked();
    assert_eq!(in_trash(cx), [SESSIONS[0].title]);
    assert_eq!(toast(cx), Some(("Undone".to_string(), false)));
}

#[gpui::test]
fn a_session_whose_folder_is_gone_is_restored_where_the_rules_file_it(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let gone = filed_in("Gone", SESSIONS[0].title, cx);
    trash(SESSIONS[0].title, cx);
    // Its folder goes while it is in Trash, and a path claims where it started.
    let archive = cx.update(|_, cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    archive.catalogue().delete_folder(gone).unwrap();
    let app = archive.catalogue().create_folder("app", None).unwrap();
    archive
        .catalogue()
        .attach_path(app, SESSIONS[0].project, None)
        .unwrap();
    library(cx).update(cx, |library, cx| library.reload(cx));
    cx.run_until_parked();

    click("trash-restore-0", cx);
    assert_eq!(
        session(SESSIONS[0].title, cx).unwrap().filed,
        Filed::ByRule(app)
    );
    let (text, undo) = toast(cx).expect("a toast");
    assert!(
        text.starts_with("Restored") && text.contains("app"),
        "{text}"
    );
    assert!(undo);
}

#[gpui::test]
fn several_selected_are_restored_together(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let ids: Vec<SessionId> = SESSIONS.iter().map(|it| id(it.title, cx)).collect();
    library(cx).update(cx, |library, cx| library.trash(ids, cx));
    cx.run_until_parked();
    show(Shelf::Trash, cx);
    assert_eq!(in_trash(cx).len(), 3);
    cx.simulate_keystrokes("down secondary-a");
    assert_eq!(library(cx).read_with(cx, |it, _| it.selection().len()), 3);
    click("trash-restore-1", cx);
    assert_eq!(in_trash(cx), Vec::<String>::new());
    assert_eq!(
        toast(cx).map(|it| it.0),
        Some("3 sessions restored".to_string())
    );
}

#[gpui::test]
fn no_key_deletes_anything_on_trash_s_page(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    trash(SESSIONS[1].title, cx);
    cx.simulate_keystrokes("down");
    assert!(library(cx).read_with(cx, |it, _| it.selected().is_some()));
    for key in ["delete", "backspace", "cmd-backspace"] {
        cx.simulate_keystrokes(key);
        cx.run_until_parked();
    }
    assert_eq!(in_trash(cx), [SESSIONS[1].title]);
    assert!(library(cx).read_with(cx, |it, _| it.for_good().is_none()));
    // `Enter` opens it, and Back comes back to Trash.
    cx.simulate_keystrokes("enter");
    assert_eq!(cx.update(|_, cx| Showing::now(cx)), Showing::Session);
    cx.simulate_keystrokes("alt-left");
    assert_eq!(cx.update(|_, cx| Showing::now(cx)), Showing::Trash);
}

fn cancel_focused(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| {
        shell
            .read(cx)
            .for_good_cancel()
            .is_some_and(|cancel| cancel.is_focused(window))
    })
}

#[gpui::test]
fn deleting_for_good_asks_with_cancel_first_and_enter_never_confirms(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    trash(SESSIONS[2].title, cx);
    click("trash-delete-0", cx);
    let asked = library(cx).read_with(cx, |it, _| it.for_good().cloned());
    let deletion = asked
        .and_then(|it| it.deletion)
        .expect("what it removes is read");
    assert_eq!((deletion.files, deletion.versions), (1, 1));
    assert!(deletion.at_location);
    assert!(cancel_focused(&shell, cx), "Cancel has the keyboard first");
    assert!(bounds(cx, "for-good").is_some());

    // `Enter` presses Cancel, which has the keyboard: nothing is deleted.
    press(cx, "enter");
    assert!(library(cx).read_with(cx, |it, _| it.for_good().is_none()));
    assert_eq!(in_trash(cx), [SESSIONS[2].title]);

    // `Esc` and the scrim cancel too.
    click("trash-delete-0", cx);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(library(cx).read_with(cx, |it, _| it.for_good().is_none()));
    assert_eq!(in_trash(cx), [SESSIONS[2].title]);

    // `Tab` reaches Delete for good, and `Enter` presses it then.
    click("trash-delete-0", cx);
    press(cx, "tab");
    assert!(!cancel_focused(&shell, cx));
    press(cx, "enter");
    cx.run_until_parked();
    assert_eq!(session(SESSIONS[2].title, cx), None, "deleted for good");
    assert_eq!(in_trash(cx), Vec::<String>::new());
    let (text, undo) = toast(cx).expect("a toast");
    assert!(
        text.starts_with(&format!("Deleted “{}” for good", SESSIONS[2].title)),
        "{text}"
    );
    assert!(!undo, "deleting for good can't be undone");
    assert_eq!(library(cx).read_with(cx, |it, _| it.counts().trash), 0);
}

/// What deleting the session asked about would remove, once read.
fn asked(cx: &mut VisualTestContext) -> Option<(String, Option<Deletion>, Option<Refusal>)> {
    library(cx).read_with(cx, |it, _| {
        it.for_good().map(|asked| {
            (
                asked.session.title.clone().unwrap_or_default(),
                asked.deletion.clone(),
                asked.refused,
            )
        })
    })
}

fn ask(title: &str, cx: &mut VisualTestContext) {
    let session = session(title, cx).expect("the session");
    library(cx).update(cx, |library, cx| library.ask_delete_for_good(session, cx));
}

#[gpui::test]
fn confirming_waits_for_what_it_removes_and_the_scrim_cancels(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    trash(SESSIONS[0].title, cx);
    // Asked, and confirmed before what it removes is read: nothing happens yet.
    ask(SESSIONS[0].title, cx);
    library(cx).update(cx, |library, cx| library.answer_delete_for_good(true, cx));
    assert!(asked(cx).is_some_and(|(_, deletion, _)| deletion.is_none()));
    cx.run_until_parked();
    assert!(asked(cx).is_some_and(|(_, deletion, _)| deletion.is_some()));
    assert_eq!(in_trash(cx), [SESSIONS[0].title]);

    // A press on the scrim cancels.
    cx.simulate_click(point(px(200.), px(850.)), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(asked(cx), None);
    assert_eq!(in_trash(cx), [SESSIONS[0].title]);
}

#[gpui::test]
fn a_session_changed_since_it_was_shown_asks_again_with_its_numbers_now(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    trash(SESSIONS[0].title, cx);
    ask(SESSIONS[0].title, cx);
    cx.run_until_parked();
    let shown = asked(cx)
        .and_then(|(_, deletion, _)| deletion)
        .expect("read");

    // The agent writes on in it, and a sync archives that while the confirmation is open.
    let item = cx.update(|_, cx| {
        let archive = cx.services().archive.read(cx).archive().cloned().unwrap();
        let id = &shown.session;
        let items = archive
            .catalogue()
            .items(&id.location, &id.provider)
            .unwrap();
        let main = archive.catalogue().session(id).unwrap().unwrap().main_item;
        items
            .into_iter()
            .find(|it| Some(it.id) == main)
            .unwrap()
            .key
    });
    write_with(
        |tree| {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(tree.path(&item))
                .unwrap();
            file.write_all(b"{\"type\":\"mode\"}\n").unwrap();
        },
        cx,
    );
    let sync = cx.update(|_, cx| cx.services().sync.clone());
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();

    library(cx).update(cx, |library, cx| library.answer_delete_for_good(true, cx));
    cx.run_until_parked();
    let (title, now, refused) = asked(cx).expect("asked again");
    assert_eq!(title, SESSIONS[0].title);
    assert!(
        now.expect("read again").bytes > shown.bytes,
        "its numbers now"
    );
    assert_eq!(refused, Some(Refusal::Changed));
    assert!(
        cx.update(|window, cx| {
            shell
                .read(cx)
                .for_good_cancel()
                .is_some_and(|cancel| cancel.is_focused(window))
        }),
        "Cancel has the keyboard again"
    );
    assert_eq!(in_trash(cx), [SESSIONS[0].title], "nothing was deleted");
}

#[gpui::test]
fn a_refusal_while_its_location_syncs_shows_again_or_says_so_over_another(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let ids: Vec<SessionId> = SESSIONS.iter().map(|it| id(it.title, cx)).collect();
    library(cx).update(cx, |library, cx| library.trash(ids, cx));
    cx.run_until_parked();
    show(Shelf::Trash, cx);
    ask(SESSIONS[0].title, cx);
    cx.run_until_parked();
    let deletion = asked(cx).and_then(|(_, deletion, _)| deletion).unwrap();
    let syncing = || ArchiveError::Syncing(deletion.session.location.clone());

    // Its own confirmation shows again, with why.
    let first = session(SESSIONS[0].title, cx).unwrap();
    library(cx).update(cx, |library, cx| {
        library.answer_delete_for_good(false, cx);
        library.refused_for_good(deletion.clone(), first.clone(), syncing(), cx);
    });
    cx.run_until_parked();
    let (title, _, refused) = asked(cx).expect("asked again");
    assert_eq!(title, SESSIONS[0].title);
    assert_eq!(refused, Some(Refusal::Syncing));
    // Once no sync runs, it forgets why and reads what it removes again.
    let sync = cx.update(|_, cx| cx.services().sync.clone());
    sync.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    let (_, read, refused) = asked(cx).expect("still asked");
    assert_eq!((read.is_some(), refused), (true, None));

    // Over another session's confirmation it never takes that one's place: a toast says it.
    library(cx).update(cx, |library, cx| library.answer_delete_for_good(false, cx));
    ask(SESSIONS[1].title, cx);
    cx.run_until_parked();
    library(cx).update(cx, |library, cx| {
        library.refused_for_good(deletion.clone(), first, syncing(), cx);
    });
    cx.run_until_parked();
    assert_eq!(
        asked(cx).map(|it| it.0),
        Some(SESSIONS[1].title.to_string())
    );
    let (text, undo) = toast(cx).expect("a toast");
    assert!(
        text.starts_with(&format!("“{}” wasn't deleted", SESSIONS[0].title)),
        "{text}"
    );
    assert!(!undo);
}

#[gpui::test]
fn a_session_restored_before_its_delete_is_written_is_said_in_words(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    trash(SESSIONS[0].title, cx);
    ask(SESSIONS[0].title, cx);
    cx.run_until_parked();
    // Restored first, in the queue the delete then waits in.
    let id = id(SESSIONS[0].title, cx);
    library(cx).update(cx, |library, cx| {
        library.restore(vec![id], cx);
        library.answer_delete_for_good(true, cx);
    });
    cx.run_until_parked();
    assert!(session(SESSIONS[0].title, cx).is_some(), "not deleted");
    assert_eq!(
        toast(cx).map(|it| it.0),
        Some(format!(
            "“{}” was restored, so it wasn't deleted",
            SESSIONS[0].title
        ))
    );
}

#[gpui::test]
fn a_rows_buttons_neither_select_nor_open_it_and_its_reader_highlights_trash(
    cx: &mut TestAppContext,
) {
    let (_shell, cx) = open_shell(cx);
    let ids: Vec<SessionId> = SESSIONS[..2].iter().map(|it| id(it.title, cx)).collect();
    library(cx).update(cx, |library, cx| library.trash(ids, cx));
    cx.run_until_parked();
    show(Shelf::Trash, cx);
    cx.simulate_keystrokes("down down");
    let selected = library(cx).read_with(cx, |it, _| it.selected().cloned());
    assert!(selected.is_some());
    // Delete for good… on the other row: the selection stays, and nothing opens.
    click("trash-delete-0", cx);
    assert_eq!(
        library(cx).read_with(cx, |it, _| it.selected().cloned()),
        selected
    );
    library(cx).update(cx, |library, cx| library.answer_delete_for_good(false, cx));
    cx.run_until_parked();
    let at = bounds(cx, "trash-restore-0").expect("Restore").center();
    cx.simulate_event(gpui::MouseDownEvent {
        position: at,
        button: gpui::MouseButton::Left,
        modifiers: Modifiers::none(),
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(gpui::MouseUpEvent {
        position: at,
        button: gpui::MouseButton::Left,
        modifiers: Modifiers::none(),
        click_count: 2,
    });
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| Showing::now(cx)),
        Showing::Trash,
        "a double click opens nothing"
    );

    // A session read from Trash highlights Trash in the sidebar, not its old folder.
    let id = selected.unwrap();
    library(cx).update(cx, |library, cx| library.open_session_at(&id, 0, cx));
    assert_eq!(cx.update(|_, cx| Showing::now(cx)), Showing::Session);
    assert_eq!(
        library(cx).read_with(cx, |it, _| it.highlighted()),
        Shelf::Trash
    );
}

#[gpui::test]
fn tab_and_shift_tab_go_round_cancel_and_delete_for_good(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    trash(SESSIONS[0].title, cx);
    click("trash-delete-0", cx);
    let on_cancel = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            shell
                .read(cx)
                .for_good_cancel()
                .is_some_and(|cancel| cancel.is_focused(window))
        })
    };
    assert!(on_cancel(cx));
    for (key, cancel) in [
        ("tab", false),
        ("tab", true),
        ("tab", false),
        ("shift-tab", true),
        ("shift-tab", false),
        ("shift-tab", true),
    ] {
        cx.simulate_keystrokes(key);
        cx.run_until_parked();
        assert_eq!(on_cancel(cx), cancel, "after {key}");
        assert!(asked(cx).is_some(), "the dialog keeps the keyboard");
    }
}

#[gpui::test]
fn a_long_title_is_cut_in_the_middle_in_its_toast(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    trash(SESSIONS[0].title, cx);
    let mut long = session(SESSIONS[0].title, cx).unwrap();
    long.title = Some(format!("start {} end", "of a very long title ".repeat(12)));
    library(cx).update(cx, |library, cx| library.ask_delete_for_good(long, cx));
    cx.run_until_parked();
    library(cx).update(cx, |library, cx| library.answer_delete_for_good(true, cx));
    cx.run_until_parked();
    let (text, _) = toast(cx).expect("a toast");
    assert!(
        text.starts_with("Deleted “start")
            && text.contains('\u{2026}')
            && text.contains("end” for good"),
        "{text}"
    );
    assert!(text.chars().count() < 150, "{text}");
}

#[gpui::test]
fn delete_folder_goes_round_its_buttons_with_tab_too(cx: &mut TestAppContext) {
    let (_shell, cx) = open_shell(cx);
    let folder = filed_in("Kept", SESSIONS[0].title, cx);
    let ask = |cx: &mut VisualTestContext| {
        library(cx).update(cx, |library, cx| library.ask_delete(folder, cx));
        cx.run_until_parked();
    };
    let kept =
        |cx: &mut VisualTestContext| library(cx).read_with(cx, |it, _| it.folder(folder).is_some());
    // Tab reaches Cancel first, then Delete folder, then Cancel again.
    ask(cx);
    press(cx, "tab");
    press(cx, "enter");
    assert_eq!(
        library(cx).read_with(cx, |it, _| it.deleting()),
        None,
        "Cancel"
    );
    assert!(kept(cx));
    ask(cx);
    for key in ["tab", "tab", "tab", "shift-tab"] {
        press(cx, key);
    }
    press(cx, "enter");
    assert!(!kept(cx), "Delete folder");
}
