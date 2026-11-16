//! The search palette (DESIGN §4.9) over the test library of [`crate::test_support`]: three
//! sessions, two started in one project and one in another, all in Unsorted.

use gpui::{Entity, Focusable as _, Modifiers, TestAppContext, VisualTestContext, point, px};
use pretty_assertions::assert_eq;
use sb_core::catalogue::Shelf;
use sb_core::search::{MatchKind, Query};
use sb_core::transcript::At;

use super::*;
use crate::services::{ActiveServices as _, LibraryService};
use crate::shell::Shell;
use crate::test_support::{SESSIONS, bounds, open_shell};

fn library(cx: &mut VisualTestContext) -> Entity<LibraryService> {
    cx.update(|_, cx| cx.services().library.clone())
}

fn service(cx: &mut VisualTestContext) -> Entity<SearchService> {
    cx.update(|_, cx| cx.services().search.clone())
}

fn shown(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Option<Entity<Palette>> {
    cx.update(|_, cx| shell.read(cx).palette().cloned())
}

/// Opens the palette with `Ctrl K`.
fn open_palette(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Entity<Palette> {
    cx.simulate_keystrokes("secondary-k");
    cx.run_until_parked();
    shown(shell, cx).expect("the palette is open")
}

/// Types `text` in the query field, and lets the search answer.
fn typed(text: &str, cx: &mut VisualTestContext) {
    cx.simulate_input(text);
    cx.run_until_parked();
}

fn chips(palette: &Entity<Palette>, cx: &mut VisualTestContext) -> Vec<String> {
    palette.read_with(cx, |palette, _| {
        palette
            .chips()
            .iter()
            .map(|chip| chip.label.clone())
            .collect()
    })
}

fn text(palette: &Entity<Palette>, cx: &mut VisualTestContext) -> String {
    palette.read_with(cx, |palette, cx| palette.text(cx))
}

/// The titles of the sessions the last search found.
fn found(cx: &mut VisualTestContext) -> Option<Vec<String>> {
    service(cx).read_with(cx, |search, _| match search.answer()? {
        (_, Answer::Found(results), _) => Some(
            results
                .sessions
                .iter()
                .filter_map(|found| found.session.title.clone())
                .collect(),
        ),
        _ => None,
    })
}

fn rows(palette: &Entity<Palette>, cx: &mut VisualTestContext) -> Vec<Row> {
    palette.read_with(cx, |palette, cx| palette.rows(cx))
}

fn highlight(palette: &Entity<Palette>, cx: &mut VisualTestContext) -> usize {
    palette.read_with(cx, |palette, _| palette.highlight())
}

fn main_focused(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| shell.read(cx).focus_handle(cx).is_focused(window))
}

#[gpui::test]
fn ctrl_k_the_field_esc_and_the_scrim_open_and_close_it(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let opened = open_palette(&shell, cx);
    assert!(bounds(cx, "palette").is_some());
    let focused = cx.update(|window, cx| opened.read(cx).input.focus_handle(cx).is_focused(window));
    assert!(focused, "its query field has the keyboard");
    // DESIGN §4.9: 840 wide, 64 from the window's top.
    let drawn = bounds(cx, "palette").unwrap();
    assert_eq!((drawn.size.width, drawn.origin.y), (px(840.), px(64.)));

    cx.simulate_keystrokes("secondary-k");
    assert!(shown(&shell, cx).is_none(), "Ctrl K again");
    assert!(main_focused(&shell, cx), "the keyboard goes back");

    open_palette(&shell, cx);
    cx.simulate_keystrokes("escape");
    assert!(shown(&shell, cx).is_none(), "Esc");

    let field = bounds(cx, "titlebar-search")
        .expect("the search field")
        .center();
    cx.simulate_click(field, Modifiers::none());
    cx.run_until_parked();
    assert!(shown(&shell, cx).is_some(), "the title bar's field");
    cx.simulate_click(point(px(200.), px(850.)), Modifiers::none());
    cx.run_until_parked();
    assert!(shown(&shell, cx).is_none(), "a press on the scrim");
}

#[gpui::test]
fn typing_searches_and_each_keystroke_supersedes_the_search_before(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let palette = open_palette(&shell, cx);
    typed("upload", cx);
    assert_eq!(found(cx), Some(vec![SESSIONS[0].title.to_string()]));
    assert_eq!(
        palette.read_with(cx, |palette, cx| palette.results(cx).map(counted)),
        Some("2 matches in 1 session".to_string()),
        "its title and its prompt"
    );

    // Two queries asked at once: only the second's answer lands, whatever the first finds.
    let query = |term: &str| Query {
        terms: vec![term.to_string()],
        ..Query::default()
    };
    service(cx).update(cx, |search, cx| {
        search.search(query("notes"), cx);
        search.search(query("backoff"), cx);
    });
    assert!(service(cx).read_with(cx, |search, _| search.is_searching()));
    cx.run_until_parked();
    assert_eq!(found(cx), Some(vec![SESSIONS[1].title.to_string()]));
    let asked = service(cx).read_with(cx, |search, _| search.answer().unwrap().0.terms.clone());
    assert_eq!(asked, ["backoff"]);
    assert!(service(cx).read_with(cx, |search, _| !search.is_searching()));

    // A letter or two asks for more; text no session holds finds nothing.
    palette.update_in(cx, |palette, window, cx| {
        palette.set_text("ok".into(), window, cx)
    });
    cx.run_until_parked();
    assert!(matches!(
        service(cx).read_with(cx, |search, _| search.answer().map(|it| it.1.clone())),
        Some(Answer::KeepTyping)
    ));
}

#[gpui::test]
fn tokens_turn_into_chips_tab_completes_and_backspace_takes_the_last_away(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let palette = open_palette(&shell, cx);
    assert_eq!(
        chips(&palette, cx),
        Vec::<String>::new(),
        "All sessions has no scope"
    );

    typed("in:unsorted ", cx);
    assert_eq!(chips(&palette, cx), ["In: Unsorted"]);
    assert_eq!(text(&palette, cx), "");

    // `Tab` adds the first completion of the token being typed.
    typed("model:model-", cx);
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    assert_eq!(chips(&palette, cx), ["In: Unsorted", "Model: model-a"]);
    assert_eq!(text(&palette, cx), "");
    typed("fix", cx);
    assert_eq!(found(cx), Some(vec![SESSIONS[0].title.to_string()]));

    // A key's start completes to the key.
    palette.update_in(cx, |palette, window, cx| {
        palette.set_text("bran".into(), window, cx)
    });
    cx.simulate_keystrokes("tab");
    assert_eq!(text(&palette, cx), "branch:");

    // `Backspace` in an empty query takes the last chip away, one at a time.
    palette.update_in(cx, |palette, window, cx| {
        palette.set_text(String::new(), window, cx)
    });
    cx.simulate_keystrokes("backspace");
    assert_eq!(chips(&palette, cx), ["In: Unsorted"]);
    cx.simulate_keystrokes("backspace");
    assert_eq!(chips(&palette, cx), Vec::<String>::new());
}

#[gpui::test]
fn the_scope_is_the_page_it_opened_over_and_search_everywhere_drops_it(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let id = library(cx).read_with(cx, |library, _| {
        library.sessions().unwrap().sessions[1].id.clone()
    });
    library(cx).update(cx, |library, cx| library.trash(vec![id], cx));
    cx.run_until_parked();
    library(cx).update(cx, |library, cx| library.show(Shelf::Unsorted, cx));
    cx.run_until_parked();
    let palette = open_palette(&shell, cx);
    assert_eq!(chips(&palette, cx), ["In: Unsorted"]);

    // In Trash only: nothing in Unsorted holds it.
    typed("backoff", cx);
    assert_eq!(found(cx), Some(vec![]));
    assert!(bounds(cx, "palette").is_some());
    let everywhere = palette.read_with(cx, |palette, cx| palette.rows(cx).len());
    assert_eq!(everywhere, 0);
    palette.update(cx, |palette, cx| palette.remove_chip(0, cx));
    cx.run_until_parked();
    assert_eq!(
        found(cx),
        Some(vec![]),
        "Trash is searched only with in:trash"
    );
    typed(" in:trash ", cx);
    assert_eq!(found(cx), Some(vec![SESSIONS[1].title.to_string()]));
}

#[gpui::test]
fn the_arrows_wrap_and_enter_opens_the_session_at_the_match(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let palette = open_palette(&shell, cx);
    typed("backoff", cx);
    assert_eq!(rows(&palette, cx), [Row::Session(0), Row::Match(0, 0)]);
    assert_eq!(highlight(&palette, cx), 0);
    cx.simulate_keystrokes("down");
    assert_eq!(highlight(&palette, cx), 1);
    cx.simulate_keystrokes("down");
    assert_eq!(highlight(&palette, cx), 0, "past the last, the first");
    cx.simulate_keystrokes("up");
    assert_eq!(highlight(&palette, cx), 1, "before the first, the last");

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(shown(&shell, cx).is_none());
    assert!(main_focused(&shell, cx));
    let (open, at) = library(cx).read_with(cx, |library, _| {
        (
            library.open_session().and_then(|it| it.title.clone()),
            library.read_at().cloned(),
        )
    });
    assert_eq!(open.as_deref(), Some(SESSIONS[1].title));
    let at = at.expect("it opens at the match");
    assert_eq!(at.kind, MatchKind::Prompt);
    assert!(matches!(at.at, Some(At::Block { turn: 0, .. })));
    assert!(!at.subagent);
    assert_eq!(at.find, "backoff");
    assert_eq!(at.flash.as_deref(), Some("backoff"));

    // Back on the page, the session and the search are the recent ones.
    cx.simulate_keystrokes("alt-left");
    let palette = open_palette(&shell, cx);
    assert_eq!(
        rows(&palette, cx),
        [Row::RecentSession(0), Row::RecentSearch(0)]
    );
    // Enter on a recent search searches it again.
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    assert_eq!(text(&palette, cx), "backoff");
    assert_eq!(found(cx), Some(vec![SESSIONS[1].title.to_string()]));
}

#[gpui::test]
fn ctrl_enter_shows_the_sessions_folder_page_with_it_selected(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    open_palette(&shell, cx);
    typed("notes", cx);
    cx.simulate_keystrokes("secondary-enter");
    cx.run_until_parked();
    assert!(shown(&shell, cx).is_none());
    let (shelf, selected, open) = library(cx).read_with(cx, |library, _| {
        (
            library.shelf(),
            library.selected().cloned(),
            library.open_session().is_some(),
        )
    });
    assert_eq!(shelf, Shelf::Unsorted);
    assert!(!open);
    let title = library(cx).read_with(cx, |library, _| {
        library
            .session(&selected.unwrap())
            .and_then(|it| it.title.clone())
    });
    assert_eq!(title.as_deref(), Some(SESSIONS[2].title));
}

#[gpui::test]
fn the_commands_group_heads_the_results_and_enter_runs_one(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let palette = open_palette(&shell, cx);
    typed("trash", cx);
    let rows = rows(&palette, cx);
    assert_eq!(rows[..2], [Row::Command(0), Row::Command(1)]);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(shown(&shell, cx).is_none());
    assert_eq!(
        library(cx).read_with(cx, |library, _| library.shelf()),
        Shelf::Trash,
        "Show Trash"
    );

    // A command with a key runs that key's action where the keyboard rests: the main area.
    open_palette(&shell, cx);
    typed("new folder", cx);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let folders = library(cx).read_with(cx, |library, _| library.folders().len());
    assert_eq!(folders, 1, "New folder");
}

#[gpui::test]
fn no_key_of_the_shell_acts_behind_the_palette(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let palette = open_palette(&shell, cx);
    for key in ["secondary-shift-n", "f2", "secondary-l", "alt-left"] {
        cx.simulate_keystrokes(key);
        cx.run_until_parked();
    }
    assert!(shown(&shell, cx).is_some());
    assert!(cx.update(|_, cx| shell.read(cx).locations_sheet().is_none()));
    assert_eq!(library(cx).read_with(cx, |it, _| it.folders().len()), 0);
    assert!(
        cx.update(|window, cx| palette.read(cx).input.focus_handle(cx).is_focused(window)),
        "the query field keeps the keyboard"
    );
}
