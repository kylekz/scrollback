//! Opening a session at a search match ([`super::place`]), from the palette, in a session whose
//! subagents nest ([`sb_test_support::with_subagents`]).

use std::path::Path;
use std::time::Duration;

use gpui::{Entity, TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use sb_test_support::{WithSubagents, with_subagents};

use super::ReaderSnapshot;
use crate::services::ActiveServices as _;
use crate::shell::Shell;
use crate::test_support::{open_shell, write_with};

/// The library of [`open_shell`], and the session of [`with_subagents`] synced into it with
/// `turns` turns between its first and its last.
fn open_with_subagents(
    cx: &mut TestAppContext,
) -> (Entity<Shell>, &mut VisualTestContext, WithSubagents) {
    let (shell, cx) = open_shell(cx);
    let mut made = None;
    write_with(
        |tree| made = Some(with_subagents(tree, "/home/user/code/app", "Widgets", 40)),
        cx,
    );
    let sync = cx.update(|_, cx| cx.services().sync.clone());
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();
    (shell, cx, made.expect("the session was written"))
}

/// Draws the window until what lands meanwhile (prose prepared, Markdown parsed) is drawn.
fn draw(cx: &mut VisualTestContext) {
    for _ in 0..3 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

/// Searches for `term` in the palette and opens the session at its first match.
fn open_at(term: &str, shell: &Entity<Shell>, cx: &mut VisualTestContext) -> ReaderSnapshot {
    cx.simulate_keystrokes("secondary-k");
    cx.run_until_parked();
    cx.simulate_input(term);
    cx.run_until_parked();
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    draw(cx);
    snapshot(shell, cx)
}

fn snapshot(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> ReaderSnapshot {
    cx.update(|_, cx| shell.read(cx).reader(cx))
}

/// The name the reader gives a subagent's conversation: its item's file stem.
fn stem(key: &str) -> &str {
    Path::new(key)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("a subagent's key names a file")
}

/// Where the flash is: its block's conversation, its text, and whether it was drawn.
fn flashed(snapshot: &ReaderSnapshot) -> Option<(String, String, bool)> {
    snapshot.flash.as_ref().map(|(block, text, drawn)| {
        let conversation = block.rsplit_once('/').map_or(block.as_str(), |(it, _)| it);
        (conversation.to_string(), text.clone(), *drawn)
    })
}

/// The kind and the text of the row the keyboard is on.
fn focused(snapshot: &ReaderSnapshot) -> (&'static str, String) {
    let (_, kind, text) = snapshot.focused.clone().expect("the keyboard is on a row");
    (kind, text)
}

#[gpui::test]
fn a_prompt_match_scrolls_to_its_turn_and_flashes_for_a_second(cx: &mut TestAppContext) {
    let (shell, cx, _) = open_with_subagents(cx);
    let reader = open_at("zanzibar", &shell, cx);
    assert_eq!(focused(&reader).0, "prompt");
    assert!(focused(&reader).1.contains("zanzibar"), "{reader:?}");
    assert!(reader.top > 0, "it scrolled down to the last turn");
    assert_eq!(
        flashed(&reader),
        Some(("main".to_string(), "zanzibar".to_string(), true))
    );
    let read_at = reader.read_at.expect("the reader took the request");
    assert_eq!(read_at.find, "zanzibar", "kept for Find in session");
    let library = cx.update(|_, cx| cx.services().library.clone());
    assert!(
        library.read_with(cx, |it, _| it.read_at().is_none()),
        "taken once"
    );

    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert_eq!(snapshot(&shell, cx).flash, None, "a second, then it goes");
}

#[gpui::test]
fn a_reply_match_puts_the_keyboard_on_the_reply(cx: &mut TestAppContext) {
    let (shell, cx, _) = open_with_subagents(cx);
    let reader = open_at("quokka", &shell, cx);
    assert_eq!(focused(&reader).0, "reply");
    assert!(focused(&reader).1.contains("quokka"));
    assert_eq!(
        flashed(&reader),
        Some(("main".to_string(), "quokka".to_string(), true))
    );
}

#[gpui::test]
fn a_match_in_a_nested_subagent_opens_each_subagent_on_the_way(cx: &mut TestAppContext) {
    let (shell, cx, made) = open_with_subagents(cx);
    let reader = open_at("gizmotron", &shell, cx);
    let place = reader.read_at.clone().and_then(|it| it.subagent);
    assert_eq!(place.map(|it| it.item), Some(made.nested.clone()));
    /* The session's call, and the subagent's call that started the nested one. */
    assert_eq!(reader.open_calls, 2, "{reader:?}");
    assert_eq!(focused(&reader).0, "reply");
    assert!(focused(&reader).1.contains("gizmotron"));
    assert_eq!(
        flashed(&reader),
        Some((
            stem(&made.nested).to_string(),
            "gizmotron".to_string(),
            true
        ))
    );

    /* The outer subagent's own text opens it alone. */
    let reader = open_at("frobnicator", &shell, cx);
    let place = reader.read_at.clone().and_then(|it| it.subagent);
    assert_eq!(place.map(|it| it.item), Some(made.subagent.clone()));
    assert_eq!(
        flashed(&reader),
        Some((
            stem(&made.subagent).to_string(),
            "frobnicator".to_string(),
            true
        ))
    );
}

#[gpui::test]
fn a_match_in_a_subagent_no_call_started_opens_its_panel(cx: &mut TestAppContext) {
    let (shell, cx, made) = open_with_subagents(cx);
    let reader = open_at("quuxifier", &shell, cx);
    let read_at = reader.read_at.clone().expect("the reader took the request");
    assert_eq!(read_at.at, Some(sb_core::transcript::At::Unattached(0)));
    assert_eq!(focused(&reader).0, "reply");
    assert_eq!(
        flashed(&reader),
        Some((stem(&made.lone).to_string(), "quuxifier".to_string(), true))
    );
}

#[gpui::test]
fn the_same_match_opened_again_moves_the_reader_again(cx: &mut TestAppContext) {
    let (shell, cx, _) = open_with_subagents(cx);
    let first = open_at("zanzibar", &shell, cx);
    cx.simulate_keystrokes("home");
    draw(cx);
    let moved = snapshot(&shell, cx);
    assert_eq!(moved.top, 0, "Home went back to the first row");
    assert_eq!(focused(&moved).0, "prompt");
    assert!(!focused(&moved).1.contains("zanzibar"));

    let again = open_at("zanzibar", &shell, cx);
    assert!(focused(&again).1.contains("zanzibar"));
    assert_eq!(again.top, first.top);
    assert_eq!(
        again.read_at.as_ref().map(|it| it.request),
        first.read_at.map(|it| it.request + 1)
    );
    assert_eq!(flashed(&again).map(|it| it.2), Some(true));
}
