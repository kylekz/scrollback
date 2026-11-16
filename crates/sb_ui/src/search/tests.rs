//! The search palette (DESIGN §4.9) over the test library of [`crate::test_support`]: three
//! sessions, two started in one project and one in another, all in Unsorted.

use std::time::Duration;

use gpui::{Entity, Focusable as _, Modifiers, TestAppContext, VisualTestContext, point, px};
use pretty_assertions::assert_eq;
use sb_core::catalogue::Shelf;
use sb_core::search::{MatchKind, Query};
use sb_core::transcript::At;
use serde_json::json;

use super::*;
use crate::services::{ActiveServices as _, LibraryService};
use crate::shell::Shell;
use crate::test_support::{SESSIONS, archive_dir, bounds, open_shell, write_with};

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

fn set_text(palette: &Entity<Palette>, text: &str, cx: &mut VisualTestContext) {
    let text = text.to_string();
    palette.update_in(cx, |palette, window, cx| palette.set_text(text, window, cx));
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

/// The last answer.
fn answer(cx: &mut VisualTestContext) -> Option<Answer> {
    service(cx).read_with(cx, |search, _| search.answer().map(|it| it.answer.clone()))
}

/// The titles of the sessions the last search found.
fn found(cx: &mut VisualTestContext) -> Option<Vec<String>> {
    match answer(cx)? {
        Answer::Found(results) => Some(
            results
                .sessions
                .iter()
                .filter_map(|found| found.session.title.clone())
                .collect(),
        ),
        _ => None,
    }
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

fn recent(cx: &mut VisualTestContext) -> Vec<String> {
    service(cx).read_with(cx, |search, _| {
        (search.recent_sessions())
            .filter_map(|it| it.title.clone())
            .collect()
    })
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
        palette.read_with(cx, |palette, cx| palette.view(cx).results().map(counted)),
        Some("2 matches in 1 session".to_string()),
        "its title and its prompt"
    );

    // Two queries asked at once: only the second's answer lands, whatever the first finds.
    let query = |term: &str| Query {
        terms: vec![term.to_string()],
        ..Query::default()
    };
    service(cx).update(cx, |search, cx| {
        search.search(query("notes"), "notes".into(), "notes".into(), cx);
        search.search(query("backoff"), "backoff".into(), "backoff".into(), cx);
    });
    assert!(service(cx).read_with(cx, |search, _| search.is_searching()));
    cx.run_until_parked();
    assert_eq!(found(cx), Some(vec![SESSIONS[1].title.to_string()]));
    let asked = service(cx).read_with(cx, |search, _| {
        let answered = search.answer().unwrap();
        (answered.query.terms.clone(), answered.text.clone())
    });
    assert_eq!(asked, (vec!["backoff".to_string()], "backoff".to_string()));
    assert!(service(cx).read_with(cx, |search, _| !search.is_searching()));

    // A letter or two asks for more.
    set_text(&palette, "ok", cx);
    assert_eq!(answer(cx), Some(Answer::KeepTyping));
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
    set_text(&palette, "bran", cx);
    cx.simulate_keystrokes("tab");
    assert_eq!(text(&palette, cx), "branch:");

    // `Backspace` in an empty query takes the last chip away, one at a time.
    set_text(&palette, "", cx);
    cx.simulate_keystrokes("backspace");
    assert_eq!(chips(&palette, cx), ["In: Unsorted"]);
    cx.simulate_keystrokes("backspace");
    assert_eq!(chips(&palette, cx), Vec::<String>::new());
}

#[gpui::test]
fn a_token_being_typed_filters_nothing_and_one_that_names_nothing_holds_the_results(
    cx: &mut TestAppContext,
) {
    let (shell, cx) = open_shell(cx);
    let palette = open_palette(&shell, cx);
    typed("the from:nowh", cx);
    let asked = service(cx).read_with(cx, |search, _| search.answer().unwrap().query.clone());
    assert_eq!(
        (asked.terms, asked.filters.location),
        (vec!["the".to_string()], None),
        "the token being typed is no filter yet"
    );
    assert_eq!(found(cx).map(|it| it.len()), Some(3));

    // Finished, it names nothing: the palette says so, and shows and searches nothing.
    typed("ere ", cx);
    assert_eq!(
        palette.read_with(cx, |palette, cx| palette.unresolved(cx)),
        Some("from:nowhere".to_string())
    );
    assert_eq!(rows(&palette, cx), []);
    assert_eq!(answer(cx), None);
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

    // In Trash only: nothing in Unsorted holds it, and Search everywhere drops the scope.
    typed("backoff", cx);
    assert_eq!(found(cx), Some(vec![]));
    assert_eq!(rows(&palette, cx), []);
    let search_everywhere = bounds(cx, "palette-everywhere").expect("Search everywhere");
    cx.simulate_click(search_everywhere.center(), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(chips(&palette, cx), Vec::<String>::new());
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
    typed("\"three times\" backoff", cx);
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
    assert_eq!(at.subagent, None);
    assert_eq!(at.find, "three times backoff", "the terms, without quotes");
    assert_eq!(at.flash.as_deref(), Some("three times"));
    let first = at.request;

    // The reader takes it once; asking again is a new request.
    let taken = library(cx).update(cx, |library, _| library.take_read_at());
    assert_eq!(taken.map(|it| it.request), Some(first));
    assert!(library(cx).read_with(cx, |it, _| it.read_at().is_none()));

    // Back on the page, the session and the search are the recent ones.
    cx.simulate_keystrokes("alt-left");
    let palette = open_palette(&shell, cx);
    assert_eq!(
        rows(&palette, cx),
        [Row::RecentSession(0), Row::RecentSearch(0)]
    );
    // Enter on a recent search searches it again, and the same match opens again, anew.
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    assert_eq!(text(&palette, cx), "\"three times\" backoff");
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    let again = library(cx).read_with(cx, |it, _| it.read_at().map(|it| it.request));
    assert_eq!(again, Some(first + 1));
}

#[gpui::test]
fn a_match_in_a_subagent_says_where_in_it_and_an_unattached_one_its_place(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    write_with(
        |tree| {
            let mut session = tree.session("/home/user/code/app", 0);
            session.transcript.prompt("Ask the widget expert.");
            let call = session.transcript.call(
                "Agent",
                json!({ "subagent_type": "expert", "description": "Ask", "prompt": "Explain." }),
            );
            let mut subagent = session.subagent_of(&call, "expert", "Ask");
            subagent.prompt("Explain widgets.");
            subagent.reply("A widget holds a frobnicator.");
            session.transcript.result_with(
                &call,
                json!("Explained."),
                false,
                json!({ "agentId": subagent.agent, "status": "completed" }),
            );
            let mut lone = session.subagent_without_meta();
            lone.prompt("Nobody asked.");
            lone.reply("The quuxifier hums.");
            session.transcript.title("Widgets");
        },
        cx,
    );
    let sync = cx.update(|_, cx| cx.services().sync.clone());
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();

    let open_at = |term: &str, cx: &mut VisualTestContext| {
        open_palette(&shell, cx);
        typed(term, cx);
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        let at = library(cx).read_with(cx, |it, _| it.read_at().cloned());
        cx.simulate_keystrokes("alt-left");
        at.expect("it opens at the match")
    };
    let inside = open_at("frobnicator", cx);
    assert!(matches!(inside.at, Some(At::Block { turn: 0, .. })));
    let place = inside.subagent.expect("the text is the subagent's");
    assert!(place.item.contains("agent-"), "{}", place.item);
    assert_eq!(inside.flash.as_deref(), Some("frobnicator"));

    let lone = open_at("quuxifier", cx);
    assert_eq!(lone.at, Some(At::Unattached(0)));
    assert!(lone.subagent.is_some());
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

    // On Trash's page, moving to Trash is no command.
    let palette = open_palette(&shell, cx);
    typed("trash", cx);
    assert_eq!(
        palette.read_with(cx, |palette, cx| palette.command_names(cx)),
        ["Show Trash"]
    );
    cx.simulate_keystrokes("escape");

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

#[gpui::test]
fn the_index_being_made_and_a_search_that_cant_run_are_said(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let dir = archive_dir(cx);
    sb_test_support::edit_catalogue(&dir, "UPDATE session SET searchable = 0");
    let palette = open_palette(&shell, cx);
    assert_eq!(service(cx).read_with(cx, |it, _| it.unsearchable()), 3);
    assert!(bounds(cx, "palette-indexing").is_some(), "Still indexing");

    typed("upload", cx);
    service(cx).update(cx, |search, cx| {
        search.answered(
            Answered {
                query: Query {
                    terms: vec!["upload".to_string()],
                    ..Query::default()
                },
                text: "upload".to_string(),
                find: "upload".to_string(),
                answer: Answer::Unavailable("the index is held by another program".to_string()),
                took: Duration::ZERO,
            },
            cx,
        );
    });
    cx.run_until_parked();
    assert!(bounds(cx, "palette-said").is_some());
    assert_eq!(rows(&palette, cx), []);
}

#[gpui::test]
fn the_recent_sessions_leave_once_in_trash_or_deleted_for_good(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let ids: Vec<_> = library(cx).read_with(cx, |library, _| {
        (library.sessions().unwrap().sessions.iter())
            .map(|it| it.id.clone())
            .collect()
    });
    for at in [0, 1] {
        library(cx).update(cx, |library, cx| library.open_session_at(&ids[at], at, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("alt-left");
    }
    assert_eq!(recent(cx), [SESSIONS[1].title, SESSIONS[0].title]);

    // Moved to Trash: it leaves the list when the palette opens again.
    library(cx).update(cx, |library, cx| library.trash(vec![ids[0].clone()], cx));
    cx.run_until_parked();
    open_palette(&shell, cx);
    assert_eq!(recent(cx), [SESSIONS[1].title]);
    cx.simulate_keystrokes("escape");

    // Deleted for good: it leaves the list at once.
    library(cx).update(cx, |library, cx| library.trash(vec![ids[1].clone()], cx));
    cx.run_until_parked();
    let session = cx.update(|_, cx| {
        let archive = cx.services().archive.read(cx).archive().cloned().unwrap();
        archive.catalogue().session(&ids[1]).unwrap().unwrap()
    });
    library(cx).update(cx, |library, cx| library.ask_delete_for_good(session, cx));
    cx.run_until_parked();
    library(cx).update(cx, |library, cx| library.answer_delete_for_good(true, cx));
    cx.run_until_parked();
    assert_eq!(recent(cx), Vec::<String>::new());
}
