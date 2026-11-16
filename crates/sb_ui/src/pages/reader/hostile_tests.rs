//! Find, the facts margin, History and the renderers against hostile and very large sessions:
//! the review's probes of phase 4b, kept as tests where they can assert and as measurements
//! (ignored) where they time.
//!
//! The measurements: `cargo test --release -p sb_ui --lib hostile_tests -- --ignored
//! --nocapture --test-threads=1`.

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{TestAppContext, VisualTestContext, px, size};
use pretty_assertions::assert_eq;
use sb_test_support::reader::{FABLE, Script};
use serde_json::json;

use super::ReaderSnapshot;
use super::facts::HistoryOf;
use crate::services::ActiveServices as _;
use crate::services::reader::find::SUBAGENTS;
use crate::services::reader::shape::{self, ansi, diff};
use crate::services::reader::text::Text;
use crate::services::reader::{Load, ReaderState};
use crate::test_support::{open_shell_sized, write_with};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a>(&mut self, pieces: &[&'a str]) -> &'a str {
        pieces[(self.next() % pieces.len() as u64) as usize]
    }

    fn string(&mut self, pieces: &[&str], most: u64) -> String {
        let n = self.next() % most;
        (0..n).map(|_| self.pick(pieces)).collect()
    }
}

/// Marks in bounds, on characters, in order and apart.
fn check_marks<M: std::fmt::Debug>(text: &str, marks: &[(Range<usize>, M)], what: &str) {
    let mut end = 0;
    for (range, mark) in marks {
        assert!(
            range.start <= range.end && range.end <= text.len(),
            "{what}: {range:?} {mark:?} out of {} in {text:?}",
            text.len()
        );
        assert!(
            text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
            "{what}: {range:?} not on chars in {text:?}"
        );
        assert!(
            range.start >= end,
            "{what}: overlapping {marks:?} in {text:?}"
        );
        end = range.end;
    }
}

const PIECES: &[&str] = &[
    "\u{1b}",
    "[",
    "]",
    "\r",
    "\n",
    "\r\n",
    "\t",
    "m",
    ";",
    "31",
    "38;5;9",
    "\u{7}",
    "\\",
    "é",
    "\u{202E}",
    "\u{85}",
    "a",
    "🎉",
    "P",
    "\u{1b}[",
    "\u{1b}]8;;http://x\u{1b}\\",
    "  12\t",
    "3→",
    "fn x()",
    "src/a.rs:4:",
    ":",
    "0",
    "\u{301}",
    "\"",
    "{",
    "}",
    "-",
    "e9",
    "true",
];

#[test]
fn shaped_text_and_its_marks_are_never_out_of_bounds() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..20_000 {
        let raw = rng.string(PIECES, 40);
        let (text, marks) = ansi::strip(&raw);
        check_marks(&text, &marks, "ansi");
        assert_eq!(ansi::strip_text(&raw), text);
        let finds: Vec<(Range<usize>, bool)> =
            sb_core::search::find_all(&text, rng.pick(&["a", "é", "e", "1", " "]), 100)
                .into_iter()
                .map(|range| (range, false))
                .collect();
        let shaped = Text::marked(text, marks);
        let count = shaped.line_count();
        for lines in [0..count, 0..1, count.saturating_sub(1)..count] {
            let (drawn, marks) = shaped.draw(lines.clone(), &shaped.marks);
            check_marks(&drawn, &marks, "drawn marks");
            let (drawn, found) = shaped.draw(lines, &finds);
            check_marks(&drawn, &found, "drawn finds");
        }
        let numbered = shape::numbered(&raw, Some('\t'), Some("rs"), &|| true);
        check_marks(&raw, &numbered, "numbered");
        check_marks(&raw, &shape::found(&raw, "a|1"), "found");
        let diff = diff::diff(&raw, &rng.string(PIECES, 40), (1, 1));
        for line in &diff.lines {
            assert!(diff.text.text.get(line.range.clone()).is_some());
            check_marks(
                &diff.text.text,
                &line
                    .words
                    .iter()
                    .map(|it| (it.clone(), ()))
                    .collect::<Vec<_>>(),
                "words",
            );
        }
    }
}

#[test]
fn an_edit_is_numbered_from_its_own_first_line_and_hostile_numbers_never_overflow() {
    /* The agent says the first changed line is 12; `old_string` starts with a line it kept. */
    assert_eq!(
        shape::edit_start(Some((12, 12)), "a\nb\nc", "a\nB\nc"),
        (11, 11)
    );
    assert_eq!(shape::edit_start(Some((12, 14)), "b", "B"), (12, 14));
    assert_eq!(shape::edit_start(None, "a\nb", "a\nc"), (1, 1));
    assert_eq!(
        shape::edit_start(Some((u64::MAX, u64::MAX)), "a\nb", "a\nc"),
        (u32::MAX, u32::MAX)
    );
    assert_eq!(shape::edit_start(Some((1, 0)), "a\nb", "a\nc"), (1, 1));
    let numbered = diff::diff("a\nb\nc", "a\nB\nc", (11, 11));
    assert_eq!(
        numbered
            .lines
            .iter()
            .map(|line| (line.old, line.new))
            .collect::<Vec<_>>(),
        [
            (Some(11), Some(11)),
            (Some(12), None),
            (None, Some(12)),
            (Some(13), Some(13))
        ]
    );
    /* A read at u64::MAX: a target, never an overflow. */
    let call = sb_core::transcript::ToolCall {
        name: "Read".to_string(),
        kind: sb_core::transcript::ToolKind::Read,
        input: sb_core::transcript::ToolInput::Read {
            path: "/home/user/a.rs".to_string(),
            first_line: Some(u64::MAX),
            lines: Some(2),
        },
        raw_input: sb_core::transcript::Stored {
            item: "x".into(),
            offset: 0,
            length: 0,
            encoding: sb_core::transcript::Encoding::Plain,
        },
        result: None,
        subagents: Vec::new(),
    };
    assert!(shape::target(&call, None).ends_with(&u64::MAX.to_string()));
}

/* Windows over sessions written for each test. */

fn synced(write: impl FnOnce(&mut sb_test_support::ProjectsTree), cx: &mut VisualTestContext) {
    write_with(write, cx);
    let sync = cx.update(|_, cx| cx.services().sync.clone());
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();
}

fn draw(cx: &mut VisualTestContext) {
    for _ in 0..4 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

/// The fastest and the median of `n` frames.
fn frames(cx: &mut VisualTestContext, n: usize) -> (Duration, Duration) {
    let mut times: Vec<Duration> = (0..n)
        .map(|_| {
            let started = Instant::now();
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            started.elapsed()
        })
        .collect();
    times.sort();
    (times[0], times[times.len() / 2])
}

fn open(title: &str, cx: &mut VisualTestContext) {
    let session = cx.update(|_, cx| {
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
    });
    let library = cx.update(|_, cx| cx.services().library.clone());
    library.update(cx, |library, cx| library.open(session, cx));
    cx.run_until_parked();
    draw(cx);
}

fn snapshot(
    shell: &gpui::Entity<crate::shell::Shell>,
    cx: &mut VisualTestContext,
) -> ReaderSnapshot {
    cx.update(|_, cx| shell.read(cx).reader(cx))
}

fn find(
    query: &str,
    shell: &gpui::Entity<crate::shell::Shell>,
    cx: &mut VisualTestContext,
) -> Option<String> {
    cx.simulate_keystrokes("secondary-f");
    cx.simulate_input(query);
    cx.run_until_parked();
    snapshot(shell, cx).find.and_then(|find| find.1)
}

/// Code of `lines` lines, different for each `seed`.
fn code(seed: usize, lines: usize) -> String {
    (0..lines)
        .map(|n| format!("let v{n} = f({seed}, \"{}\");\n", n * 7 % 13))
        .collect()
}

/// A session of 300 file reads and 100 large edits, which a capped Find must not go through.
fn reads_and_edits(tree: &mut sb_test_support::ProjectsTree) {
    let mut session = tree.session("/home/user/code/fc", 0);
    {
        let mut s = Script::new(&mut session.transcript, "2026-11-13", "f");
        s.at("10:00:00").prompt("go e");
        for n in 0..300 {
            let read = s.call(
                FABLE,
                "Read",
                json!({ "file_path": format!("/home/user/code/fc/a{n}.rs") }),
            );
            let out: String = code(n, 500)
                .lines()
                .enumerate()
                .map(|(i, line)| format!("{:>6}\t{line}\n", i + 1))
                .collect();
            s.result(&read, json!(out), json!({ "file": { "numLines": 500 } }));
        }
        for n in 0..100 {
            let edit = s.call(
                FABLE,
                "Edit",
                json!({ "file_path": format!("/home/user/code/fc/b{n}.rs"), "old_string": code(n, 8_000), "new_string": code(n + 1, 8_000) }),
            );
            s.result(&edit, json!("ok"), json!({}));
        }
    }
    session.title("Find costs");
}

#[gpui::test]
fn a_find_that_matches_everywhere_stops_reading_at_its_bound(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1440.), px(900.)), cx);
    synced(reads_and_edits, cx);
    open("Find costs", cx);
    let started = Instant::now();
    assert_eq!(find("e", &shell, cx).as_deref(), Some("1 of 10,000+"));
    /* It read as far as its 10,000 matches: a few of the reads, none of the edits. */
    let reader = cx.update(|_, cx| cx.services().reader.clone());
    let (searched, capped) = cx.update(|_, cx| {
        let counted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let going = {
            let counted = counted.clone();
            move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
        };
        let findings = reader.read(cx).find_now("e", &going, cx);
        (
            counted.load(std::sync::atomic::Ordering::Relaxed),
            findings.capped,
        )
    });
    assert!(capped);
    assert!(
        searched < 100,
        "{searched} checks for a search capped in its first reads"
    );
    eprintln!("capped find: {:?}", started.elapsed());
}

#[gpui::test]
fn find_searches_the_first_subagents_reads_each_once_and_says_so(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1440.), px(900.)), cx);
    let count = SUBAGENTS + 10;
    synced(
        |tree| {
            let mut session = tree.session("/home/user/code/fc", 0);
            let mut calls = Vec::new();
            {
                let mut s = Script::new(&mut session.transcript, "2026-11-13", "s");
                s.at("10:00:00").prompt("go");
                for n in 0..count {
                    calls.push(s.call(
                        FABLE,
                        "Agent",
                        json!({ "subagent_type": "general-purpose", "description": format!("Part {n}"), "prompt": "Look." }),
                    ));
                }
            }
            for (n, call) in calls.iter().enumerate() {
                let mut sub = session.subagent_of(call, "general-purpose", &format!("Part {n}"));
                let agent = sub.agent.clone();
                {
                    let mut s = Script::new(&mut sub, "2026-11-13", "p");
                    s.at("12:00:00").prompt("Look.");
                    s.reply(FABLE, &format!("reply of part {n}"));
                }
                let mut main = Script::new(&mut session.transcript, "2026-11-13", "q");
                main.at("12:30:00").result(
                    call,
                    json!("Done."),
                    json!({ "agentId": agent, "status": "completed" }),
                );
            }
            session.title("Many subagents");
        },
        cx,
    );
    open("Many subagents", cx);
    let said = find("zzzz", &shell, cx).unwrap();
    assert_eq!(said, "No matches");
    assert_eq!(
        find("reply of part", &shell, cx).unwrap(),
        format!("1 of {SUBAGENTS} · first {SUBAGENTS} subagents")
    );
    let kept = cx.update(|_, cx| {
        cx.services()
            .reader
            .read(cx)
            .open_session()
            .unwrap()
            .found_count()
    });
    assert_eq!(kept, SUBAGENTS);
}

#[gpui::test]
fn find_searches_again_when_the_session_is_read_again(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1440.), px(900.)), cx);
    let mut file = None;
    synced(
        |tree| {
            let key = sb_test_support::reader::backfill(tree);
            file = Some(tree.path(&format!("projects/{key}.jsonl")));
        },
        cx,
    );
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
    open("Throttle yt-dlp metadata backfill", cx);
    let before = find("health", &shell, cx).unwrap();
    let reader = cx.update(|_, cx| cx.services().reader.clone());
    let (key, v2) = cx.update(|_, cx| {
        let open = reader.read(cx).open_session().unwrap();
        (
            open.main_key().unwrap().clone(),
            open.history().unwrap().versions[1].0.id,
        )
    });
    reader.update(cx, |reader, cx| reader.view_version(Some((key, v2)), cx));
    cx.run_until_parked();
    draw(cx);
    /* v2 is the first turn alone: its own matches, not v1's. */
    let after = snapshot(&shell, cx).find.unwrap();
    assert_eq!(after.0, "health");
    assert!(after.1.is_some());
    assert_ne!(after.1.as_deref(), Some(before.as_str()));
}

#[gpui::test]
fn a_hostile_edit_start_opens_and_draws(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1440.), px(900.)), cx);
    synced(
        |tree| {
            let mut session = tree.session("/home/user/code/fc", 0);
            {
                let mut s = Script::new(&mut session.transcript, "2026-11-13", "h");
                s.at("10:00:00").prompt("go");
                let edit = s.call(
                    FABLE,
                    "Edit",
                    json!({ "file_path": "/home/user/code/fc/a.rs", "old_string": "a\nb", "new_string": "a\nc" }),
                );
                s.result(
                    &edit,
                    json!("ok"),
                    json!({ "structuredPatch": [{ "oldStart": u64::MAX, "newStart": u64::MAX, "lines": [" a", "-b", "+c"] }] }),
                );
                let read = s.call(
                    FABLE,
                    "Read",
                    json!({ "file_path": "/home/user/code/fc/a.rs", "offset": u64::MAX, "limit": u64::MAX }),
                );
                s.result(
                    &read,
                    json!("x"),
                    json!({ "file": { "numLines": u64::MAX, "totalLines": u64::MAX } }),
                );
            }
            session.title("Hostile numbers");
        },
        cx,
    );
    open("Hostile numbers", cx);
    cx.simulate_keystrokes("e");
    draw(cx);
    let open =
        cx.update(|_, cx| matches!(cx.services().reader.read(cx).state(), ReaderState::Open(_)));
    assert!(open);
    assert_eq!(snapshot(&shell, cx).open_calls, 2);
}

/* Measurements. */

#[gpui::test]
#[ignore = "a measurement"]
fn measure_find_per_keystroke(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1440.), px(900.)), cx);
    synced(reads_and_edits, cx);
    open("Find costs", cx);
    for query in ["zzzz-nothing", "e", "zzzz-nothing2"] {
        let started = Instant::now();
        let said = find(query, &shell, cx);
        eprintln!("{query}: {said:?} in {:?}", started.elapsed());
    }
}

#[gpui::test]
#[ignore = "a measurement"]
fn measure_find_over_large_mcp_json(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1440.), px(900.)), cx);
    let depth = 100;
    let n = (1024 * 1024 - 2 * depth - 40) / 2;
    let nested: serde_json::Value = serde_json::from_str(&format!(
        "{}{}0{}",
        "[".repeat(depth),
        "0,".repeat(n),
        "]".repeat(depth)
    ))
    .unwrap();
    synced(
        |tree| {
            let mut session = tree.session("/home/user/code/fc", 0);
            {
                let mut s = Script::new(&mut session.transcript, "2026-11-13", "m");
                s.at("10:00:00").prompt("go");
                for _ in 0..8 {
                    let call = s.call(FABLE, "mcp__srv__tool", json!({ "q": nested }));
                    s.result(&call, json!("ok"), json!({}));
                }
            }
            session.title("MCP json");
        },
        cx,
    );
    open("MCP json", cx);
    eprintln!("rss before find: {} MB", rss_mb());
    for query in ["zzzz", "zzzzq"] {
        let started = Instant::now();
        let said = find(query, &shell, cx);
        eprintln!(
            "{query}: {said:?} in {:?}, rss {} MB",
            started.elapsed(),
            rss_mb()
        );
    }
}

fn rss_mb() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
        / 1024
}

#[gpui::test]
#[ignore = "a measurement"]
fn measure_facts_with_forty_thousand_turns(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1440.), px(900.)), cx);
    synced(
        |tree| {
            let mut session = tree.session("/home/user/code/fc", 0);
            {
                let mut s = Script::new(&mut session.transcript, "2026-11-13", "t");
                for turn in 0..40_000 {
                    s.at(&format!(
                        "{:02}:{:02}:{:02}",
                        8 + turn / 3600 % 12,
                        turn / 60 % 60,
                        turn % 60
                    ))
                    .prompt(&format!(
                        "{}Prompt number {turn}",
                        "\n".repeat(if turn % 100 == 0 { 20_000 } else { 0 })
                    ));
                    s.reply(FABLE, "ok");
                }
            }
            session.title("Forty thousand turns");
        },
        cx,
    );
    open("Forty thousand turns", cx);
    eprintln!("40k turns, wide: {:?}", frames(cx, 8));
    cx.simulate_resize(size(px(1100.), px(900.)));
    draw(cx);
    eprintln!("40k turns, narrow: {:?}", frames(cx, 8));
    cx.simulate_resize(size(px(1440.), px(900.)));
    draw(cx);
    let said = find("Prompt", &shell, cx);
    eprintln!(
        "Find Prompt: {said:?}; wide, Find active: {:?}",
        frames(cx, 8)
    );
}

#[gpui::test]
#[ignore = "a measurement"]
fn measure_history_of_thousands_of_versions(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1440.), px(900.)), cx);
    synced(
        |tree| {
            sb_test_support::reader::playlist(tree);
        },
        cx,
    );
    open("Downloads: paste a whole playlist", cx);
    eprintln!("no popover: {:?}", frames(cx, 6));
    let view = cx.update(|_, cx| shell.read(cx).reader_view(cx));
    for count in [100usize, 1_000, 5_000] {
        let history = sb_core::catalogue::History {
            item: 1,
            versions: (0..count)
                .map(|n| {
                    (
                        sb_core::catalogue::Version {
                            id: n as i64 + 1,
                            number: n as u32 + 1,
                            reason: sb_core::catalogue::Reason::Rewritten,
                            started_at: 1_700_000_000_000,
                            length: 1000,
                            hash: sb_core::hash::Hash::of(b""),
                            held_back: (n % 3 == 0).then_some(sb_core::catalogue::Held::Shorter),
                            damaged: false,
                            export_version: None,
                        },
                        n % 5,
                    )
                })
                .collect(),
            reading: 2,
            chosen: false,
        };
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.history = Some(HistoryOf {
                    key: "projects/x/y.jsonl".into(),
                    history: std::sync::Arc::new(history),
                    main: true,
                });
                cx.notify();
            })
        });
        eprintln!("History of {count} versions: {:?}", frames(cx, 6));
    }
}

#[gpui::test]
#[ignore = "a measurement"]
fn measure_hostile_bodies(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell_sized(size(px(1440.), px(900.)), cx);
    synced(
        |tree| {
            let mut session = tree.session("/home/user/code/fc", 0);
            {
                let mut s = Script::new(&mut session.transcript, "2026-11-13", "b");
                s.at("10:00:00").prompt("go");
                let edit = s.call(
                    FABLE,
                    "Edit",
                    json!({ "file_path": "/home/user/code/fc/a.rs", "old_string": code(1, 9_000), "new_string": code(2, 9_000) }),
                );
                s.result(&edit, json!("ok"), json!({ "structuredPatch": [] }));
                let fetch = s.call(
                    FABLE,
                    "WebFetch",
                    json!({ "url": "https://example.com/", "prompt": "read it" }),
                );
                let plain: String = (0..4_000)
                    .map(|n| format!("word{n} *a [b <c "))
                    .collect::<String>()
                    .repeat(4);
                s.result(&fetch, json!(plain), json!({}));
                let long = s.call(
                    FABLE,
                    "Edit",
                    json!({ "file_path": "/home/user/code/fc/b.rs", "old_string": "x".repeat(200_000), "new_string": "y".repeat(200_000) }),
                );
                s.result(&long, json!("ok"), json!({}));
            }
            session.title("Hostile bodies");
        },
        cx,
    );
    open("Hostile bodies", cx);
    eprintln!("collapsed: {:?}", frames(cx, 6));
    cx.simulate_keystrokes("e");
    draw(cx);
    eprintln!("Full calls: {:?}", frames(cx, 6));
    let calls = cx.update(|_, cx| {
        let reader = cx.services().reader.read(cx);
        let open = reader.open_session().unwrap();
        let main = open.main().unwrap();
        main.calls()
            .map(|(block, _)| crate::services::reader::BlockRef {
                conversation: main.item.clone(),
                block,
            })
            .collect::<Vec<_>>()
    });
    let reader = cx.update(|_, cx| cx.services().reader.clone());
    for at in &calls {
        reader.update(cx, |reader, cx| {
            reader.toggle(
                crate::services::reader::Toggle::AllLines(at.clone(), None),
                cx,
            )
        });
    }
    draw(cx);
    eprintln!("Full calls, Show all: {:?}", frames(cx, 6));
    let said = find("v", &shell, cx);
    draw(cx);
    eprintln!("Find v {said:?}: {:?}", frames(cx, 6));
    let loaded = cx.update(|_, cx| {
        let reader = cx.services().reader.read(cx);
        let open = reader.open_session().unwrap();
        calls
            .iter()
            .filter(|at| matches!(open.body(at), Some(Load::Loaded(_))))
            .count()
    });
    eprintln!("{loaded} bodies loaded of {}", calls.len());
}
