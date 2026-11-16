//! Trash and deleting for good (DESIGN §4.8), over the test library of
//! [`crate::test_support`]: restoring, into a folder that is gone too, its Undo, and the
//! confirmation that deletes for good, whose `Enter` never confirms.

use gpui::{Entity, KeyUpEvent, Keystroke, TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use sb_core::catalogue::{Filed, Session, SessionId, Shelf};
use sb_core::filing::FolderId;

use super::Showing;
use crate::services::{ActiveServices as _, LibraryService};
use crate::shell::Shell;
use crate::test_support::{SESSIONS, bounds, open_shell};

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

    // Undo moves it to Trash again.
    library(cx).update(cx, LibraryService::undo);
    cx.run_until_parked();
    assert_eq!(in_trash(cx), [SESSIONS[0].title]);
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
