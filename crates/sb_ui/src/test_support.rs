//! What the views' and the services' tests share: the services over a scratch archive, with a
//! generated agent tree as this machine (PLAN §12), and a main window with the shell in it.
//!
//! Background jobs run on gpui's deterministic scheduler, so `run_until_parked` carries a
//! sync, its index and the library's load through to the end.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AppContext as _, Bounds, Entity, Global, Pixels, Size, TestAppContext, VisualTestContext, px,
    size,
};
use gpui_appearance::MaterialSupport;
use gpui_component::Root;
use sb_core::CancelToken;
use sb_core::location::LocationKind;
use sb_core::transport::{Probe, Transport};
use sb_test_support::{Outline, ProjectsTree};
use tempfile::TempDir;

use crate::services::{self, ActiveServices as _, Reach, ServicesConfig};
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
 * through a local shell or a stand-in transport. No connection test runs, and this machine
 * has no WSL.
 */
pub fn init_with(
    archive: TempDir,
    machine: ProjectsTree,
    elsewhere: impl Fn(&LocationKind, &CancelToken) -> Box<dyn Transport> + Send + Sync + 'static,
    cx: &mut TestAppContext,
) {
    init_reaching(
        archive,
        machine,
        Reach {
            connect: Arc::new(elsewhere),
            tester: Arc::new(|launcher, _| panic!("this test tests no {launcher:?}")),
            distros: Arc::new(|_| Ok(None)),
            launcher: Arc::new(LocationKind::launcher),
            development_home: None,
        },
        cx,
    );
}

/// [`init_with`], with the connection test and WSL's distros the test's stand-ins too:
/// `elsewhere` reaches every place but this machine. The scratch folder `archive` is the data
/// dir: the archive is its `archive` folder, and the app's settings are beside it.
pub fn init_reaching(
    archive: TempDir,
    machine: ProjectsTree,
    elsewhere: Reach,
    cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        crate::init(cx).expect("sb_ui initialises");
        let settings = services::settings::load(archive.path());
        crate::appearance::init(
            MaterialSupport::ALL,
            settings.settings.mode,
            settings.settings.material,
            cx,
        );
        let here = machine.transport();
        let connect = elsewhere.connect;
        services::init(
            ServicesConfig {
                settings,
                mode_override: None,
                providers: sb_core::provider::builtin(),
                reach: Reach {
                    connect: Arc::new(move |kind, cancel| match kind {
                        LocationKind::Local => Box::new(here.clone()),
                        other => connect(other, cancel),
                    }),
                    tester: elsewhere.tester,
                    distros: elsewhere.distros,
                    launcher: elsewhere.launcher,
                    development_home: elsewhere.development_home,
                },
            },
            cx,
        );
        cx.set_global(Scratch { archive, machine });
    });
    cx.run_until_parked();
}

/// What a shell that answered with every tool says to the connection test.
pub fn answered() -> Probe {
    Probe {
        home: "/home/user".to_string(),
        clock_ns: 0,
        clock_ahead_ns: 0,
        missing: Vec::new(),
        whole_seconds: false,
        shell: "/bin/sh".to_string(),
        login_shell: None,
        drive_mount: None,
        took: Duration::from_millis(1),
    }
}

/**
 * [`init`] where every other place is [`machine`]'s tree again and every connection test
 * passes ([`answered`]); WSL has no distros. Nothing real is reached.
 */
pub fn init_answering(cx: &mut TestAppContext) {
    init_reaching(
        tempfile::tempdir().expect("a scratch folder"),
        machine(),
        Reach {
            connect: Arc::new(|_, _| Box::new(machine().transport())),
            tester: Arc::new(|_, _| Ok(answered())),
            distros: Arc::new(|_| Ok(None)),
            launcher: Arc::new(LocationKind::launcher),
            development_home: None,
        },
        cx,
    );
}

/// Has the agent on this machine write another session, for the next sync to find.
pub fn write_session(outline: &Outline, cx: &mut TestAppContext) {
    cx.update(|cx| cx.global_mut::<Scratch>().machine.outlined(outline));
}

/// The file of this machine's agent that the item `key` is.
pub fn machine_path(key: &str, cx: &mut TestAppContext) -> std::path::PathBuf {
    cx.update(|cx| cx.global::<Scratch>().machine.path(key))
}

/// The key of the transcript of [`SESSIONS`]' session `at`, once it is archived.
pub fn machine_file(at: usize, cx: &mut TestAppContext) -> String {
    cx.update(|cx| {
        let archive = cx.services().archive.read(cx).archive().cloned();
        let catalogue = archive.as_ref().expect("the archive is open").catalogue();
        let session = catalogue
            .sessions(sb_core::catalogue::Shelf::All)
            .unwrap()
            .into_iter()
            .find(|session| session.title.as_deref() == Some(SESSIONS[at].title))
            .expect("the session is archived");
        catalogue
            .items(&session.id.location, &session.id.provider)
            .unwrap()
            .into_iter()
            .find(|item| {
                item.session.as_deref() == Some(session.id.key.as_str())
                    && item.kind == sb_core::provider::ItemKind::Transcript
            })
            .expect("the session has a transcript")
            .key
    })
}

/// Has the agent on this machine write what `write` writes, for the next sync to find.
pub fn write_with(write: impl FnOnce(&mut ProjectsTree), cx: &mut TestAppContext) {
    cx.update(|cx| write(&mut cx.global_mut::<Scratch>().machine));
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

/**
 * The bounds an element with `.debug_selector(|| name.into())` drew at, after a fresh frame.
 * gpui records selectors only in a build with debug assertions, so in a release build this is
 * always `None`, and the tests that use it fail there: they are run unoptimised, as
 * `script/check` runs them.
 */
pub fn bounds(cx: &mut VisualTestContext, name: &'static str) -> Option<Bounds<Pixels>> {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.debug_bounds(name)
}
