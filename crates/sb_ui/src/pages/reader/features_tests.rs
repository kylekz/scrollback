//! The tool renderers ([`super::kinds`]), the facts margin ([`super::facts`]), Find in session
//! ([`super::find`]) and versions, in a window over sessions of `sb_test_support::reader`.

use gpui::{Entity, TestAppContext, VisualTestContext, px, size};
use pretty_assertions::assert_eq;
use sb_core::catalogue::Session;
use sb_core::transcript::Content;
use sb_test_support::reader;

use super::ReaderSnapshot;
use super::rows::{BodyPart, ProsePart, RowKind, rows};
use crate::services::ActiveServices as _;
use crate::services::reader::Load;
use crate::shell::Shell;
use crate::test_support::{bounds, open_shell, open_shell_sized, write_with};

/// Writes `write`'s sessions on this machine and syncs them.
fn synced(write: impl FnOnce(&mut sb_test_support::ProjectsTree), cx: &mut VisualTestContext) {
    write_with(write, cx);
    let sync = cx.update(|_, cx| cx.services().sync.clone());
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();
}

/// Draws the window until what lands meanwhile is drawn.
fn draw(cx: &mut VisualTestContext) {
    for _ in 0..4 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn session(title: &str, cx: &mut VisualTestContext) -> Session {
    cx.update(|_, cx| {
        cx.services()
            .library
            .read(cx)
            .sessions()
            .expect("All sessions is loaded")
            .sessions
            .iter()
            .find(|session| session.title.as_deref() == Some(title))
            .unwrap_or_else(|| panic!("{title} is synced"))
            .clone()
    })
}

fn open(title: &str, cx: &mut VisualTestContext) {
    let session = session(title, cx);
    let library = cx.update(|_, cx| cx.services().library.clone());
    library.update(cx, |library, cx| library.open(session, cx));
    cx.run_until_parked();
    draw(cx);
}

fn snapshot(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> ReaderSnapshot {
    cx.update(|_, cx| shell.read(cx).reader(cx))
}

/// The body rows of the open session's own calls, by tool name, with Full calls on.
fn bodies(cx: &mut VisualTestContext) -> Vec<(String, Vec<BodyPart>)> {
    cx.update(|_, cx| {
        let reader = cx.services().reader.read(cx);
        let open = reader.open_session().expect("a session is open");
        let mut calls: Vec<(String, Vec<BodyPart>)> = Vec::new();
        for row in rows(open, reader.full_calls(), None) {
            match row.kind {
                RowKind::Call { at, .. } if at.conversation == open.main().unwrap().item => {
                    calls.push((open.call(&at).unwrap().name.clone(), Vec::new()))
                }
                RowKind::Body { part, at, .. } if at.conversation == open.main().unwrap().item => {
                    if let Some(last) = calls.last_mut() {
                        last.1.push(part);
                    }
                }
                _ => {}
            }
        }
        calls
    })
}

#[gpui::test]
fn each_kind_of_tool_draws_its_own_body(cx: &mut TestAppContext) {
    let (_, cx) = open_shell(cx);
    synced(
        |tree| {
            reader::playlist(tree);
        },
        cx,
    );
    open("Downloads: paste a whole playlist", cx);
    cx.simulate_keystrokes("e");
    draw(cx);
    let bodies = bodies(cx);
    let of = |name: &str| -> Vec<BodyPart> {
        bodies
            .iter()
            .find(|(tool, _)| tool == name)
            .unwrap_or_else(|| panic!("a {name} call"))
            .1
            .clone()
    };
    /* A command: its header, its output, its footer. */
    let shell_body = of("Bash");
    assert_eq!(shell_body.first(), Some(&BodyPart::Head));
    assert_eq!(shell_body.last(), Some(&BodyPart::Footer));
    assert!(shell_body.contains(&BodyPart::Lines {
        part: Some(0),
        chunk: 0
    }));
    /* A read, a content search: their headers and lines. */
    assert_eq!(of("Read").first(), Some(&BodyPart::Head));
    assert_eq!(of("Grep").first(), Some(&BodyPart::Head));
    assert!(!of("Glob").contains(&BodyPart::Head));
    /* An edit's diff, and a write's, past its first six lines. */
    assert_eq!(of("Edit"), [BodyPart::Diff { chunk: 0 }]);
    assert_eq!(
        of("Write"),
        [
            BodyPart::Diff { chunk: 0 },
            BodyPart::DiffMore { all: false }
        ]
    );
    /* Todos, a web search's results, a web fetch's Markdown, MCP side by side. */
    assert_eq!(
        of("TodoWrite"),
        (0..4).map(BodyPart::Todo).collect::<Vec<_>>()
    );
    assert_eq!(of("WebSearch"), [BodyPart::Result(0), BodyPart::Result(1)]);
    assert!(matches!(
        of("WebFetch")[..],
        [BodyPart::Head, BodyPart::Fetched { piece: 0, chunk: 0 }, ..]
    ));
    assert_eq!(
        of("mcp__github__search_issues"),
        [BodyPart::Head, BodyPart::Pair { chunk: 0 }]
    );
    /* A tool of no kind scrollback knows: a header that says so, then as sent and answered. */
    assert_eq!(
        of("PlaylistProbe")[..2],
        [BodyPart::Head, BodyPart::InputHead]
    );
    /* Its row reads `unknown`, the tool's own name the target; a mapped tool, its label. */
    cx.update(|_, cx| {
        let reader = cx.services().reader.read(cx);
        let open = reader.open_session().unwrap();
        let main = open.main().unwrap();
        let shown = |name: &str| {
            let (_, call) = main.calls().find(|(_, call)| call.name == name).unwrap();
            (
                super::calls::label(call, &open.id.provider, cx),
                crate::services::reader::shape::target(call, None),
            )
        };
        assert_eq!(
            shown("PlaylistProbe"),
            ("unknown".to_string(), "PlaylistProbe".to_string())
        );
        assert_eq!(shown("Bash").0, "bash");
    });
    /* The diff is the edit's own, numbered, its words marked. */
    cx.update(|_, cx| {
        let reader = cx.services().reader.read(cx);
        let open = reader.open_session().unwrap();
        let main = open.main().unwrap();
        let (edit, _) = main.calls().find(|(_, call)| call.name == "Edit").unwrap();
        let at = crate::services::reader::BlockRef {
            conversation: main.item.clone(),
            block: edit,
        };
        let Some(Load::Loaded(body)) = open.body(&at) else {
            panic!("the edit's body is read");
        };
        let diff = body.diff.clone().unwrap().unwrap();
        assert_eq!(diff.counts(), (1, 1));
        assert!(diff.lines.iter().any(|line| !line.words.is_empty()));
    });
}

#[gpui::test]
fn find_searches_bodies_and_subagents_and_goes_to_each_match(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    synced(
        |tree| {
            reader::backfill(tree);
        },
        cx,
    );
    open("Throttle yt-dlp metadata backfill", cx);
    assert_eq!(snapshot(&shell, cx).open_calls, 0);
    cx.simulate_keystrokes("secondary-f");
    cx.simulate_input("health");
    cx.run_until_parked();
    draw(cx);
    let found = snapshot(&shell, cx).find.expect("Find has a query");
    assert_eq!(found.0, "health");
    let count = found.1.expect("the search landed");
    assert!(count.starts_with("1 of "), "{count}");
    /* Each next match is gone to, opening what holds it; past the last, the first again. */
    let total: usize = count.rsplit(' ').next().unwrap().parse().unwrap();
    let mut seen = Vec::new();
    for _ in 0..total {
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        draw(cx);
        seen.push(snapshot(&shell, cx).find.unwrap().2.unwrap());
    }
    assert_eq!(
        snapshot(&shell, cx).find.unwrap().1.unwrap(),
        format!("1 of {total}")
    );
    /* In a subagent's own calls, in a body, and opened. */
    assert!(seen.iter().any(|it| it.starts_with("agent-")), "{seen:?}");
    assert!(seen.iter().any(|it| it.ends_with("output/0")), "{seen:?}");
    assert!(snapshot(&shell, cx).open_calls > 0);
    cx.simulate_keystrokes("shift-enter");
    cx.run_until_parked();
    assert_eq!(
        snapshot(&shell, cx).find.unwrap().1.unwrap(),
        format!("{total} of {total}")
    );
    /* No match says so; Esc closes Find. */
    cx.simulate_keystrokes("secondary-f");
    cx.simulate_input("zzzz-not-anywhere");
    cx.run_until_parked();
    assert_eq!(
        snapshot(&shell, cx).find.unwrap().1.as_deref(),
        Some("No matches")
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(snapshot(&shell, cx).find, None);
}

#[gpui::test]
fn going_to_a_match_in_a_subagent_only_find_has_read_opens_that_subagent(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    synced(
        |tree| {
            reader::backfill(tree);
        },
        cx,
    );
    open("Throttle yt-dlp metadata backfill", cx);
    cx.simulate_keystrokes("secondary-f");
    cx.simulate_input("health");
    cx.run_until_parked();
    draw(cx);
    /* The subagent's matches are found by Find's own read of it; the reader hasn't read it. */
    let current = |cx: &mut VisualTestContext| snapshot(&shell, cx).find.unwrap().2.unwrap();
    for _ in 0..100 {
        if current(cx).starts_with("agent-") {
            break;
        }
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        draw(cx);
    }
    let current = current(cx);
    /* Its row name is the subagent's file name. */
    let (name, _) = current.split_once('/').expect("a match in a subagent");
    let of = |item: &str| item.ends_with(&format!("/{name}.jsonl"));
    /* Going there reads the subagent and opens its panel, so the match's block is drawn. */
    cx.update(|_, cx| {
        let reader = cx.services().reader.read(cx);
        let open = reader.open_session().expect("a session is open");
        assert!(
            open.subagents_read().any(|(item, _)| of(item)),
            "{name} is read"
        );
        let laid = rows(open, reader.full_calls(), None);
        assert!(
            laid.iter()
                .filter_map(|row| row.block())
                .any(|at| of(&at.conversation)),
            "{name}'s rows are laid out"
        );
    });
}

#[gpui::test]
fn the_reader_takes_the_keys_back_from_find_only_once_it_is_closed(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    synced(
        |tree| {
            reader::playlist(tree);
        },
        cx,
    );
    open("Downloads: paste a whole playlist", cx);
    cx.simulate_keystrokes("secondary-f");
    /* `e` and `j` are typed, not Full calls and the next session. */
    cx.simulate_input("ej");
    cx.run_until_parked();
    let calls = cx.update(|_, cx| cx.services().reader.read(cx).full_calls());
    assert!(!calls);
    assert_eq!(snapshot(&shell, cx).find.unwrap().0, "ej");
}

#[gpui::test]
fn a_find_that_matches_everything_stops_at_its_bound(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    synced(
        |tree| {
            reader::hostile(tree, 256 * 1024);
        },
        cx,
    );
    open("Reader fixture: hostile content", cx);
    cx.simulate_keystrokes("secondary-f");
    cx.simulate_input("e");
    cx.run_until_parked();
    draw(cx);
    let count = snapshot(&shell, cx).find.unwrap().1.unwrap();
    assert_eq!(count, "1 of 10,000+");
}

/// A session without calls has no Calls block in its facts; one with calls has.
#[gpui::test]
fn the_facts_count_calls_only_for_a_session_that_made_some(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    open(crate::test_support::SESSIONS[0].title, cx);
    assert!(snapshot(&shell, cx).facts);
    assert!(bounds(cx, "facts-calls").is_some());
    open(crate::test_support::SESSIONS[2].title, cx);
    assert!(snapshot(&shell, cx).facts);
    assert_eq!(bounds(cx, "facts-calls"), None);
}

#[gpui::test]
fn the_facts_show_beside_a_wide_column_and_over_a_narrow_one(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1100.), px(700.)), cx);
    synced(
        |tree| {
            reader::backfill(tree);
        },
        cx,
    );
    open("Throttle yt-dlp metadata backfill", cx);
    assert!(!snapshot(&shell, cx).facts);
    let toggle = bounds(cx, "facts-toggle").expect("the Facts toggle shows");
    cx.simulate_click(toggle.center(), gpui::Modifiers::default());
    draw(cx);
    assert!(snapshot(&shell, cx).facts);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!snapshot(&shell, cx).facts);
    /* `]` goes to the next turn. */
    cx.simulate_keystrokes("] ]");
    draw(cx);
    let (_, kind, text) = snapshot(&shell, cx)
        .focused
        .expect("the keyboard is on a row");
    assert_eq!(
        (kind, text.as_str()),
        ("prompt", "Run the tests and show me")
    );
}

#[gpui::test]
fn an_older_version_is_read_for_a_look_and_made_the_reading_copy(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let mut file = None;
    synced(
        |tree| {
            let key = reader::backfill(tree);
            file = Some(tree.path(&format!("projects/{key}.jsonl")));
        },
        cx,
    );
    /* The transcript rewritten shorter: a version held back. */
    let file = file.unwrap();
    let whole = std::fs::read_to_string(&file).unwrap();
    let first: String = whole
        .lines()
        .take(3)
        .map(|line| format!("{line}\n"))
        .collect();
    std::fs::write(&file, first).unwrap();
    let sync = cx.update(|_, cx| cx.services().sync.clone());
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();
    let held = session("Throttle yt-dlp metadata backfill", cx);
    assert!(held.held_back);
    open("Throttle yt-dlp metadata backfill", cx);
    let history = cx.update(|_, cx| {
        cx.services()
            .reader
            .read(cx)
            .open_session()
            .unwrap()
            .history()
            .cloned()
            .unwrap()
    });
    assert_eq!(history.versions.len(), 2);
    let (v1, v2) = (history.versions[0].0.id, history.versions[1].0.id);
    assert_eq!(history.reading, v1);
    /* v2, read for a look: the banner, and the facts say so. */
    let key = cx.update(|_, cx| {
        cx.services()
            .reader
            .read(cx)
            .open_session()
            .unwrap()
            .main_key()
            .cloned()
            .unwrap()
    });
    let service = cx.update(|_, cx| cx.services().reader.clone());
    service.update(cx, |reader, cx| reader.view_version(Some((key, v2)), cx));
    cx.run_until_parked();
    draw(cx);
    assert_eq!(snapshot(&shell, cx).viewing, Some(2));
    let prompts = cx.update(|_, cx| {
        let reader = cx.services().reader.read(cx);
        let main = reader.open_session().unwrap().main().unwrap().clone();
        main.blocks()
            .iter()
            .filter(|block| matches!(block.content, Content::Prompt { .. }))
            .count()
    });
    assert_eq!(prompts, 1);
    /* Made the reading copy: read as such, and the card is held back no more. */
    service.update(cx, |reader, cx| {
        reader.make_reading_copy(history.item, v2, cx)
    });
    cx.run_until_parked();
    draw(cx);
    assert_eq!(snapshot(&shell, cx).viewing, None);
    let reading = cx.update(|_, cx| {
        cx.services()
            .reader
            .read(cx)
            .open_session()
            .unwrap()
            .history()
            .unwrap()
            .reading
    });
    assert_eq!(reading, v2);
    assert!(!session("Throttle yt-dlp metadata backfill", cx).held_back);
}

#[gpui::test]
fn another_version_read_draws_its_own_text(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    let mut file = None;
    synced(
        |tree| {
            let key = reader::backfill(tree);
            file = Some(tree.path(&format!("projects/{key}.jsonl")));
        },
        cx,
    );
    /* The first prompt said otherwise, as long: a new version, the reading copy. */
    let file = file.unwrap();
    let whole = std::fs::read_to_string(&file).unwrap();
    std::fs::write(
        &file,
        whole.replacen("hammers YouTube", "batters YouTube", 1),
    )
    .unwrap();
    let sync = cx.update(|_, cx| cx.services().sync.clone());
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();
    open("Throttle yt-dlp metadata backfill", cx);
    let view = cx.update(|_, cx| shell.read(cx).reader_view(cx));
    /* The first prompt's text as its Markdown state draws it. */
    let drawn = |cx: &mut VisualTestContext| -> String {
        cx.update(|_, cx| {
            let view = view.read(cx);
            let (state, _) = view
                .states
                .iter()
                .find(|(piece, _)| piece.at.block.0 == 0 && piece.piece == 0)
                .map(|(_, state)| state)
                .expect("the first prompt is drawn");
            state.read(cx).rendered_text().as_str().to_string()
        })
    };
    assert!(drawn(cx).contains("batters YouTube"), "{}", drawn(cx));
    let (key, first) = cx.update(|_, cx| {
        let reader = cx.services().reader.read(cx);
        let open = reader.open_session().unwrap();
        (
            open.main_key().cloned().unwrap(),
            open.history().unwrap().versions[0].0.id,
        )
    });
    let service = cx.update(|_, cx| cx.services().reader.clone());
    service.update(cx, |reader, cx| reader.view_version(Some((key, first)), cx));
    cx.run_until_parked();
    draw(cx);
    assert_eq!(snapshot(&shell, cx).viewing, Some(1));
    assert!(drawn(cx).contains("hammers YouTube"), "{}", drawn(cx));
}

#[gpui::test]
fn a_session_opened_at_a_search_match_fills_find_at_that_match(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    synced(
        |tree| {
            reader::backfill(tree);
        },
        cx,
    );
    cx.simulate_keystrokes("secondary-k");
    cx.run_until_parked();
    cx.simulate_input("progress");
    cx.run_until_parked();
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    draw(cx);
    let snapshot = snapshot(&shell, cx);
    let flashed = snapshot.flash.expect("the match flashes").0;
    let (query, count, current) = snapshot.find.expect("Find is filled");
    assert_eq!(query, "progress");
    assert!(count.is_some());
    /* The current match is the one flashed, not the first. */
    assert!(current.unwrap().starts_with(&flashed), "{flashed}");
}

/// The facts margin's column, inside its 16 padding at the window's right edge.
fn facts_column(window_width: f32) -> (f32, f32) {
    (
        window_width - super::facts::FACTS_WIDTH + 16.,
        window_width - 16.,
    )
}

/**
 * Each of the facts' values is one line inside the margin's column (DESIGN §4.2.8), however
 * long: When is two, and each model one. Its bounds, by debug selector.
 */
fn assert_facts_on_their_lines(window_width: f32, cx: &mut VisualTestContext) {
    let (left, right) = facts_column(window_width);
    let line = |name: &'static str, lines: f32, cx: &mut VisualTestContext| {
        let at = bounds(cx, name).unwrap_or_else(|| panic!("{name} is drawn"));
        let (x, width, height) = (
            f32::from(at.origin.x),
            f32::from(at.size.width),
            f32::from(at.size.height),
        );
        assert!(
            x >= left - 1. && x + width <= right + 1.,
            "{name} from {x} to {} is inside {left}..{right}",
            x + width
        );
        assert!(
            (height - 16. * lines).abs() < 1.,
            "{name} is {lines} lines of 16, not {height} high"
        );
        at
    };
    line("facts-path", 1., cx);
    line("facts-branch", 1., cx);
    line("facts-day", 1., cx);
    line("facts-time", 1., cx);
    line("facts-tokens", 1., cx);
    line("facts-models", 4., cx);
}

/**
 * The facts of a session whose values are as long as they come keep each value on its line:
 * a 200-character path and a long branch are cut, not wrapped; each shows whole on hover and
 * copies whole on a click.
 */
#[gpui::test]
fn the_facts_cut_long_values_and_show_and_copy_them_whole(cx: &mut TestAppContext) {
    assert_eq!(reader::LONG_PATH.len(), 200);
    let (shell, cx) = open_shell(cx);
    cx.update(|_, cx| crate::system::install(crate::system::SystemShell::scripted(), cx));
    synced(
        |tree| {
            reader::facts(tree);
        },
        cx,
    );
    open("Reader fixture: facts as long as they come", cx);
    assert_facts_on_their_lines(1440., cx);
    /* The path is far wider than the column, so it was cut to fit. */
    let path = bounds(cx, "facts-path").unwrap();
    let whole = cx.update(|window, cx| {
        let theme = crate::theme::ActiveTheme::sb_theme(cx).clone();
        crate::components::text_width(reader::LONG_PATH, &theme.text.path_small, window)
    });
    assert!(
        whole > path.size.width * 3.,
        "{whole:?} against {:?}",
        path.size.width
    );
    /* Each is cut in its middle: it keeps its start and its end. */
    let view = cx.update(|_, cx| shell.read(cx).reader_view(cx));
    let drawn: Vec<String> = cx.update(|_, cx| {
        view.read(cx)
            .facts_cut
            .iter()
            .map(|layout| layout.text())
            .collect()
    });
    assert_eq!(drawn.len(), 2);
    for (drawn, value) in drawn.iter().zip([reader::LONG_PATH, reader::LONG_BRANCH]) {
        let (start, end) = drawn
            .split_once('\u{2026}')
            .unwrap_or_else(|| panic!("{drawn:?} is cut"));
        assert!(
            start.len() > 4 && value.starts_with(start),
            "{drawn:?} keeps the start of {value:?}"
        );
        assert!(
            end.len() > 4 && value.ends_with(end),
            "{drawn:?} keeps the end of {value:?}"
        );
    }
    for (name, whole, value) in [
        ("facts-path", "facts-path-whole", reader::LONG_PATH),
        ("facts-branch", "facts-branch-whole", reader::LONG_BRANCH),
    ] {
        let at = bounds(cx, name).unwrap();
        cx.simulate_mouse_move(at.center(), None, gpui::Modifiers::default());
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(2));
        draw(cx);
        assert!(bounds(cx, whole).is_some(), "{name} shows whole on hover");
        cx.simulate_click(at.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        let copied = cx.update(|_, cx| crate::system::requests(cx).last().cloned());
        assert_eq!(
            copied,
            Some(crate::system::SystemRequest::Copy(value.to_string()))
        );
    }
}

/// Over the column of a narrow window, the facts keep their values on their lines too.
#[gpui::test]
fn the_facts_over_a_narrow_column_keep_their_values_on_their_lines(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(960.), px(600.)), cx);
    synced(
        |tree| {
            reader::facts(tree);
        },
        cx,
    );
    open("Reader fixture: facts as long as they come", cx);
    let toggle = bounds(cx, "facts-toggle").expect("the Facts toggle shows");
    cx.simulate_click(toggle.center(), gpui::Modifiers::default());
    draw(cx);
    assert!(snapshot(&shell, cx).facts);
    assert_facts_on_their_lines(960., cx);
}

/**
 * A memory file the agent loaded on its way is one quiet row, closed even with Full calls on;
 * opened, its text follows as Markdown, through the prose rows (DESIGN §4.2.11).
 */
#[gpui::test]
fn a_file_loaded_into_the_context_opens_to_its_text(cx: &mut TestAppContext) {
    let (_, cx) = open_shell(cx);
    synced(
        |tree| {
            reader::facts(tree);
        },
        cx,
    );
    open("Reader fixture: facts as long as they come", cx);
    cx.simulate_keystrokes("e");
    draw(cx);
    let laid = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            let reader = cx.services().reader.read(cx);
            rows(reader.open_session().unwrap(), reader.full_calls(), None)
        })
    };
    let files: Vec<_> = laid(cx)
        .into_iter()
        .filter_map(|row| match row.kind {
            RowKind::ContextFile { at, open } => Some((at, open)),
            _ => None,
        })
        .collect();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|(_, open)| !open), "Full calls opens none");
    let loaded: Vec<(String, bool)> = cx.update(|_, cx| {
        let reader = cx.services().reader.read(cx);
        let open = reader.open_session().unwrap();
        files
            .iter()
            .map(|(at, _)| match &open.block(at).unwrap().content {
                Content::ContextFile { path, differs, .. } => (path.clone(), *differs),
                other => panic!("{other:?}"),
            })
            .collect()
    });
    assert_eq!(
        loaded,
        [
            (reader::MEMORY_FILES[0].to_string(), false),
            (reader::MEMORY_FILES[1].to_string(), true)
        ]
    );
    /* No record of the session is one scrollback doesn't know. */
    assert!(
        !laid(cx)
            .iter()
            .any(|row| matches!(row.kind, RowKind::Unrecognised { .. }))
    );
    let first = files[0].0.clone();
    let service = cx.update(|_, cx| cx.services().reader.clone());
    service.update(cx, |reader, cx| {
        reader.toggle(
            crate::services::reader::Toggle::ContextFile(first.clone()),
            cx,
        )
    });
    cx.run_until_parked();
    draw(cx);
    let prose: Vec<ProsePart> = laid(cx)
        .into_iter()
        .filter_map(|row| match row.kind {
            RowKind::Prose { at, part, .. } if at == first => Some(part),
            _ => None,
        })
        .collect();
    assert!(
        prose.contains(&ProsePart::Markdown(0)),
        "its text is Markdown: {prose:?}"
    );
}

/// How long Find takes over the reader's budget session (PLAN §14), in release:
/// `cargo test --release -p sb_ui find_over_the_budget_session -- --ignored --nocapture`.
#[gpui::test]
#[ignore = "a measurement"]
fn find_over_the_budget_session(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    synced(
        |tree| {
            reader::large(tree, reader::Large::BUDGET);
        },
        cx,
    );
    open("A long session to measure the reader", cx);
    for query in ["cargo test --package part_9", "e"] {
        let started = std::time::Instant::now();
        cx.simulate_keystrokes("secondary-f");
        cx.simulate_input(query);
        cx.run_until_parked();
        let count = snapshot(&shell, cx).find.unwrap().1;
        eprintln!("{query}: {count:?} in {:?}", started.elapsed());
    }
}
