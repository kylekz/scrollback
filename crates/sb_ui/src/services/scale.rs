//! What the library costs at scale (PLAN §14): an archive of 50,000 sessions and 1,000 folders
//! three levels deep, with a third of the sessions in Unsorted under thousands of paths. It
//! prints how long each background read takes, and how long the UI thread spends on what it
//! does itself: taking a load in, laying a page's rows out, and drawing frames while a page of
//! every session scrolls.
//!
//! ```text
//! cargo test --release -p sb_ui scale -- --ignored --nocapture
//! ```

use std::collections::HashSet;
use std::time::{Duration, Instant};

use gpui::{Modifiers, ScrollDelta, ScrollEvent, TestAppContext, point, px, size};
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::catalogue::Shelf;
use sb_core::filing::FolderId;
use sb_core::location::LocationKind;
use sb_core::sync::sync_location;
use sb_test_support::{Outline, ProjectsTree};

use super::library::load;
use super::*;
use crate::pages::sessions::{row_sizes, rows};
use crate::sidebar::tree;
use crate::test_support::{bounds, init_in, open_window};
use crate::theme::{Appearance, Material, SbTheme};

const SESSIONS: usize = 50_000;

/// How long `work` takes, in milliseconds, with what it returns.
fn timed<T>(work: impl FnOnce() -> T) -> (T, f64) {
    let start = Instant::now();
    let result = work();
    (result, start.elapsed().as_secs_f64() * 1000.)
}

/// Builds the archive in `dir` from `machine`, which it fills. Returns a top-level folder.
fn build(dir: &std::path::Path, machine: &mut ProjectsTree) -> FolderId {
    let archive = Archive::create(dir).unwrap();
    let catalogue = archive.catalogue();
    let location = catalogue
        .add_location(LocationKind::Local, "This machine")
        .unwrap();
    // The folders first: attaching a path files every session there is.
    let mut paths: [Vec<String>; 3] = Default::default();
    let mut first = None;
    for top in 0..10 {
        let path = format!("/home/user/p{top}");
        let folder = catalogue.create_folder(&format!("p{top}"), None).unwrap();
        catalogue.attach_path(folder, &path, None).unwrap();
        first.get_or_insert(folder);
        for mid in 0..9 {
            let path = format!("{path}/q{mid}");
            let folder = catalogue
                .create_folder(&format!("q{mid}"), Some(folder))
                .unwrap();
            catalogue.attach_path(folder, &path, None).unwrap();
            for leaf in 0..10 {
                let path = format!("{path}/r{leaf}");
                let folder = catalogue
                    .create_folder(&format!("r{leaf}"), Some(folder))
                    .unwrap();
                catalogue.attach_path(folder, &path, None).unwrap();
                paths[2].push(path);
            }
            paths[1].push(path);
        }
        paths[0].push(path);
    }
    for at in 0..SESSIONS {
        let project = match at % 20 {
            0..9 => paths[2][at % 900].clone(),
            9..12 => paths[1][at % 90].clone(),
            12..14 => paths[0][at % 10].clone(),
            _ => format!("/elsewhere/u{}", at % 5000),
        };
        let month = at % 24;
        machine.outlined(&Outline {
            project: &project,
            title: &format!("Session {at} of a generated library"),
            prompt: "Start from the failing case, keep the change small and say what you left out.",
            model: "model-a",
            day: (
                2025 + month as i32 / 12,
                month as u32 % 12 + 1,
                at as u32 % 28 + 1,
            ),
            calls: at as u32 % 40,
        });
    }
    let passes = sync_location(
        &archive,
        &location.id,
        &machine.transport(),
        &sb_core::provider::builtin(),
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap();
    for pass in passes {
        pass.outcome.unwrap();
        pass.index.unwrap();
    }
    first.unwrap()
}

#[gpui::test]
#[ignore = "builds an archive of 50,000 sessions; run it by hand to measure"]
fn what_the_library_costs_at_fifty_thousand_sessions(cx: &mut TestAppContext) {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("archive");
    let mut machine = ProjectsTree::new();
    let (folder, built) = timed(|| build(&dir, &mut machine));
    eprintln!(
        "built {SESSIONS} sessions in 1,000 folders in {:.0} s",
        built / 1000.
    );

    // The background reads, each as one load of a page.
    let archive = Archive::open(&dir).unwrap();
    for (name, shelf) in [
        ("All sessions", Shelf::All),
        ("Starred", Shelf::Starred),
        ("Unsorted", Shelf::Unsorted),
        ("a top-level folder", Shelf::Folder(folder)),
    ] {
        let (loaded, ms) = timed(|| load(&archive, shelf).unwrap());
        let listed = match &loaded.page {
            Page::Sessions(page) => format!("{} sessions", page.sessions.len()),
            Page::Unsorted(groups) => format!("{} groups", groups.len()),
        };
        eprintln!("background: load {name} ({listed}): {ms:.1} ms");
    }
    let all = load(&archive, Shelf::All).unwrap();
    let catalogue = archive.catalogue();
    let (_, ms) = timed(|| catalogue.library_counts().unwrap());
    eprintln!("background: the sidebar's counts alone: {ms:.1} ms");
    let (folders, ms) = timed(|| catalogue.folders().unwrap());
    eprintln!("background: the folders alone: {ms:.1} ms");

    // What the UI thread does with a load: the rows of a page and of the tree.
    let theme = SbTheme::new(Appearance::Dark, Material::Opaque);
    let Page::Sessions(page) = &all.page else {
        panic!("All sessions is a page of sessions");
    };
    let (laid, ms) = timed(|| {
        let rows = rows(&page.months, 4);
        (row_sizes(&rows, &theme), rows)
    });
    eprintln!("UI thread: lay {} rows out: {ms:.2} ms", laid.1.len());
    let open: HashSet<FolderId> = folders.iter().map(|folder| folder.id).collect();
    let (shown, ms) = timed(|| tree::rows(&folders, &open));
    eprintln!("UI thread: the tree's {} rows: {ms:.2} ms", shown.len());
    drop(archive);

    // The app over that archive: frames of All sessions, still and scrolling.
    init_in(scratch, machine, cx);
    let (_shell, cx) = open_window(size(px(1440.), px(900.)), cx);
    let library = cx.update(|_, cx| cx.services().library.clone());
    library.read_with(cx, |library, _| assert_eq!(library.counts().all, SESSIONS));
    let frame = |cx: &mut gpui::VisualTestContext| {
        timed(|| {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        })
        .1
    };
    assert!(bounds(cx, "card-0").is_some(), "the first card shows");
    let mut frames: Vec<f64> = (0..200)
        .map(|_| {
            cx.simulate_event(ScrollEvent {
                position: point(px(800.), px(500.)),
                delta: ScrollDelta::Pixels(point(px(0.), px(-900.))),
                modifiers: Modifiers::none(),
                ..Default::default()
            });
            frame(cx)
        })
        .collect();
    assert_eq!(bounds(cx, "card-0"), None, "the page scrolled");
    frames.sort_by(f64::total_cmp);
    eprintln!(
        "UI thread: a frame of All sessions while scrolling a screen a frame: median {:.2} ms, \
         worst {:.2} ms",
        frames[frames.len() / 2],
        frames[frames.len() - 1]
    );

    // Showing another page: what the call costs before the load comes back, and the frame
    // that takes the load in.
    for (name, shelf) in [("Unsorted", Shelf::Unsorted), ("All sessions", Shelf::All)] {
        let (_, ms) = timed(|| library.update(cx, |library, cx| library.show(shelf, cx)));
        eprintln!("UI thread: asking for {name}: {ms:.3} ms");
        cx.run_until_parked();
        eprintln!("UI thread: a frame of {name}: {:.2} ms", frame(cx));
    }
    // A star, shown at once.
    let middle = library.read_with(cx, |library, _| {
        library.sessions().unwrap().sessions[SESSIONS / 2]
            .id
            .clone()
    });
    let (_, ms) = timed(|| {
        library.update(cx, |library, cx| {
            library.toggle_star(&middle, SESSIONS / 2, cx)
        });
    });
    eprintln!("UI thread: starring a session in the middle of the page: {ms:.3} ms");
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
}
