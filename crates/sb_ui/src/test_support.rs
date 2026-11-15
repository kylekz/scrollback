//! What the views' and the services' tests share: the services over a scratch archive, with a
//! generated agent tree as this machine (PLAN §12), and a main window with the shell in it.
//!
//! Background jobs run on gpui's deterministic scheduler, so `run_until_parked` carries a
//! sync, its index and the library's load through to the end.

use std::sync::Arc;

use gpui::{
    AppContext as _, Bounds, Entity, Global, Pixels, Size, TestAppContext, VisualTestContext, px,
    size,
};
use gpui_appearance::{MaterialSupport, Mode};
use gpui_component::Root;
use sb_core::CancelToken;
use sb_core::location::LocationKind;
use sb_core::transport::Transport;
use sb_test_support::{Outline, ProjectsTree};
use tempfile::TempDir;

use crate::services::{self, ActiveServices as _, ServicesConfig};
use crate::shell::Shell;

/// The scratch folders of a test's services, kept for as long as its app.
struct Scratch {
    archive: TempDir,
    machine: ProjectsTree,
}

impl Global for Scratch {}

/// The sessions [`machine`] holds: two of one project and one of another, the newest first.
pub const SESSIONS: [Outline; 3] = [
    Outline {
        project: "/home/user/code/app",
        title: "Fix the flaky upload test",
        prompt: "The upload test fails one run in five. Find the race and fix it.",
        model: "model-a",
        day: (2026, 11, 12),
        calls: 7,
    },
    Outline {
        project: "/home/user/code/app",
        title: "Add retries to the fetcher",
        prompt: "Retry a failed fetch three times with backoff before giving up.",
        model: "model-b",
        day: (2026, 11, 10),
        calls: 3,
    },
    Outline {
        project: "/home/user/notes",
        title: "Summarise this week's notes",
        prompt: "Read the notes from this week and write a summary for Friday.",
        model: "model-a",
        day: (2026, 10, 30),
        calls: 0,
    },
];

/// An agent's transcripts on this machine: [`SESSIONS`].
pub fn machine() -> ProjectsTree {
    let mut tree = ProjectsTree::new();
    for outline in &SESSIONS {
        tree.outlined(outline);
    }
    tree
}

/// Starts scrollback's UI and its services over a new archive, with `machine` as this
/// machine, and lets the archive open: the state of a first run.
pub fn init(machine: ProjectsTree, cx: &mut TestAppContext) {
    init_in(tempfile::tempdir().expect("a scratch folder"), machine, cx);
}

/// [`init`] with the archive in the `archive` folder of the scratch folder `archive`, which a
/// test may have put something in.
pub fn init_in(archive: TempDir, machine: ProjectsTree, cx: &mut TestAppContext) {
    init_with(
        archive,
        machine,
        |kind, _| panic!("this test reaches no {kind:?}"),
        cx,
    );
}

/**
 * [`init_in`], with the locations of the other kinds than this machine reached through
 * `elsewhere`: never the real distro or host a kind names, but a home of the test's own, read
 * through a local shell or a stand-in transport.
 */
pub fn init_with(
    archive: TempDir,
    machine: ProjectsTree,
    elsewhere: impl Fn(&LocationKind, &CancelToken) -> Box<dyn Transport> + Send + Sync + 'static,
    cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        crate::init(cx).expect("sb_ui initialises");
        crate::appearance::init(MaterialSupport::ALL, Mode::System, cx);
        let here = machine.transport();
        services::init(
            ServicesConfig {
                archive_dir: archive.path().join("archive"),
                providers: sb_core::provider::builtin(),
                connect: Arc::new(move |kind, cancel| match kind {
                    LocationKind::Local => Box::new(here.clone()),
                    other => elsewhere(other, cancel),
                }),
            },
            cx,
        );
        cx.set_global(Scratch { archive, machine });
    });
    cx.run_until_parked();
}

/// Has the agent on this machine write another session, for the next sync to find.
pub fn write_session(outline: &Outline, cx: &mut TestAppContext) {
    cx.update(|cx| cx.global_mut::<Scratch>().machine.outlined(outline));
}

/// The folder the services' archive is in.
pub fn archive_dir(cx: &mut TestAppContext) -> std::path::PathBuf {
    cx.update(|cx| cx.global::<Scratch>().archive.path().join("archive"))
}

/// [`init`] with [`machine`], then this machine added as a location and synced: a library
/// of three sessions, all in Unsorted.
pub fn init_library(cx: &mut TestAppContext) {
    init(machine(), cx);
    cx.update(|cx| {
        let sync = cx.services().sync.clone();
        sync.update(cx, |sync, cx| sync.add_local(cx));
    });
    cx.run_until_parked();
}

/// A 1440x900 main window around a [`Shell`] over [`init_library`]'s library, on a platform
/// that shows every material.
pub fn open_shell(cx: &mut TestAppContext) -> (Entity<Shell>, &mut VisualTestContext) {
    open_shell_sized(size(px(1440.), px(900.)), cx)
}

/// [`open_shell`] in a window of `window_size`.
pub fn open_shell_sized(
    window_size: Size<Pixels>,
    cx: &mut TestAppContext,
) -> (Entity<Shell>, &mut VisualTestContext) {
    init_library(cx);
    open_window(window_size, cx)
}

/// A main window around a [`Shell`] over the services already started.
pub fn open_window(
    window_size: Size<Pixels>,
    cx: &mut TestAppContext,
) -> (Entity<Shell>, &mut VisualTestContext) {
    let mut shell = None;
    let window = cx.open_window(window_size, |window, cx| {
        let view = cx.new(|cx| Shell::new(window, cx));
        shell = Some(view.clone());
        Root::new(view, window, cx)
    });
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    cx.run_until_parked();
    (shell.expect("the window built a shell"), cx)
}

/// The bounds an element with `.debug_selector(|| name.into())` drew at, after a fresh frame.
pub fn bounds(cx: &mut VisualTestContext, name: &'static str) -> Option<Bounds<Pixels>> {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.debug_bounds(name)
}
