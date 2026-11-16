//! The owner's locations through the sync service (DESIGN §4.10–§4.12): adding each kind,
//! the same place twice, renaming, removing one that is syncing and adding it back, the
//! connection test's every outcome and WSL's distros. Every place but this machine is a
//! stand-in, or a home of the test's own read through a local `sh`; no real distro or host is
//! reached and neither `wsl.exe` nor `ssh` runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{Entity, TestAppContext};
use pretty_assertions::assert_eq;
use sb_core::catalogue::{LastCheck, Shelf};
use sb_core::location::{Location, LocationKind};
use sb_core::transport::wsl::Distro;
use sb_core::transport::{Launcher, Probe, TransportError};
use sb_test_support::{Outline, ProjectsTree};

use super::location_tests::Asleep;
use super::sync::{TestFailure, TestState};
use super::*;
use crate::test_support::{SESSIONS, init_reaching, machine};

fn ubuntu() -> LocationKind {
    LocationKind::Wsl {
        distro: "Ubuntu-24.04".to_string(),
    }
}

fn macbook() -> LocationKind {
    LocationKind::Ssh {
        host: "user@macbook".to_string(),
    }
}

/// A distro's or a host's own sessions: one only it has.
fn elsewhere() -> ProjectsTree {
    let mut tree = ProjectsTree::new();
    tree.outlined(&Outline {
        project: "/home/user/code/server",
        title: "Written elsewhere",
        prompt: "Only the other place has this one.",
        model: "model-a",
        day: (2026, 11, 14),
        calls: 4,
    });
    tree
}

/// A probe whose shell answered with every tool.
fn answered() -> Probe {
    Probe {
        home: "/home/user".to_string(),
        clock_ns: 0,
        clock_ahead_ns: 0,
        missing: Vec::new(),
        whole_seconds: false,
        shell: "/bin/sh".to_string(),
        login_shell: Some("/bin/bash".to_string()),
        drive_mount: None,
        took: std::time::Duration::from_millis(1400),
    }
}

/// The installed distros the stand-in lists: two to offer and Docker's own.
fn distros() -> Vec<Distro> {
    let distro = |name: &str, running, utility| Distro {
        name: name.to_string(),
        running,
        default: name == "Ubuntu-24.04",
        utility,
    };
    vec![
        distro("Ubuntu-24.04", true, false),
        distro("Debian", false, false),
        distro("docker-desktop", true, true),
    ]
}

/**
 * The services over a new archive with [`machine`] as this machine, every other place read as
 * [`elsewhere`]'s home, a connection test that answers as `tester` does, and WSL with
 * [`distros`].
 */
fn init(
    tester: impl Fn(&Launcher) -> Result<Probe, TransportError> + Send + Sync + 'static,
    cx: &mut TestAppContext,
) -> Entity<SyncService> {
    // The closure keeps the tree, and with it its folder, for as long as the app.
    let other = elsewhere();
    init_reaching(
        tempfile::tempdir().expect("a scratch folder"),
        machine(),
        Reach {
            connect: Arc::new(move |_, _| Box::new(other.transport())),
            tester: Arc::new(move |launcher, _| tester(launcher)),
            distros: Arc::new(|_| Ok(Some(distros()))),
            launcher: Arc::new(LocationKind::launcher),
            development_home: None,
        },
        cx,
    );
    cx.update(|cx| cx.services().sync.clone())
}

fn locations(sync: &Entity<SyncService>, cx: &mut TestAppContext) -> Vec<Location> {
    sync.read_with(cx, |sync, _| sync.locations().unwrap_or_default().to_vec())
}

fn add(
    sync: &Entity<SyncService>,
    kind: LocationKind,
    name: &str,
    sync_now: bool,
    cx: &mut TestAppContext,
) {
    sync.update(cx, |sync, cx| {
        sync.add(kind, name.to_string(), sync_now, cx)
    });
    cx.run_until_parked();
}

fn test_state(sync: &Entity<SyncService>, cx: &mut TestAppContext) -> Option<TestState> {
    sync.read_with(cx, |sync, _| sync.test().map(|test| test.state.clone()))
}

#[gpui::test]
fn a_first_run_lists_the_distros_and_looks_in_those_that_run(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    sync.read_with(cx, |sync, _| {
        assert!(sync.is_first_run());
        let distros = sync.distros().expect("the distros are listed");
        let offered: Vec<&str> = distros.offered().map(|it| it.name.as_str()).collect();
        // Docker's own distro is nobody's to add (PLAN §6.3).
        assert_eq!(offered, ["Ubuntu-24.04", "Debian"]);
        // Only the running one is looked in: listing never starts a distro.
        let surveyed: Vec<&String> = distros.surveys.keys().collect();
        assert_eq!(surveyed, ["Ubuntu-24.04"]);
        let found = &distros.surveys["Ubuntu-24.04"];
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].as_ref().map(|it| it.sessions), Ok(1));
    });
}

#[gpui::test]
fn several_places_are_added_at_once_and_synced(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    sync.update(cx, |sync, cx| {
        sync.add_local(cx);
        sync.add(ubuntu(), "Ubuntu-24.04".to_string(), true, cx);
        // A stopped distro is added as it is: its first sync starts it (PLAN §6.3).
        sync.add(
            LocationKind::Wsl {
                distro: "Debian".to_string(),
            },
            "Debian".to_string(),
            false,
            cx,
        );
    });
    cx.run_until_parked();
    // Added at once, they are in the order their writes ended.
    let mut added = locations(&sync, cx);
    let order = [sync::local_name(), "Ubuntu-24.04", "Debian"];
    added.sort_by_key(|it| order.iter().position(|name| *name == it.name));
    let names: Vec<&str> = added.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(names, order);
    sync.read_with(cx, |sync, _| {
        assert!(!sync.is_first_run());
        let states: Vec<LocationStatus> = added.iter().map(|it| sync.status(it)).collect();
        assert!(matches!(states[0], LocationStatus::Synced(_)));
        assert!(matches!(states[1], LocationStatus::Synced(_)));
        assert_eq!(states[2], LocationStatus::NeverSynced);
        // What each holds, and where the agent keeps it there.
        let details = sync.details();
        assert_eq!(details.sessions(&added[0].id), SESSIONS.len());
        assert_eq!(details.sessions(&added[1].id), 1);
        assert!(details.bytes(&added[1].id) > 0);
        assert_eq!(details.found(&added[1].id).len(), 1);
        assert!(details.found(&added[1].id)[0].root.is_some());
        assert_eq!(details.sessions(&added[2].id), 0);
    });
}

#[gpui::test]
fn a_place_that_is_a_location_already_is_not_added_again(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    add(&sync, ubuntu(), "Ubuntu-24.04", false, cx);
    // wsl.exe takes a distro's name in any case: this is the same distro.
    let again = LocationKind::Wsl {
        distro: "ubuntu-24.04".to_string(),
    };
    sync.read_with(cx, |sync, _| {
        let same = sync::refusal(&again, sync.locations().unwrap().iter());
        assert_eq!(same.map(|it| it.name.as_str()), Some("Ubuntu-24.04"));
    });
    add(&sync, again, "Another", false, cx);
    assert_eq!(locations(&sync, cx).len(), 1);
}

#[gpui::test]
fn a_location_is_renamed_and_keeps_what_it_holds(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    add(&sync, ubuntu(), "Ubuntu-24.04", true, cx);
    let id = locations(&sync, cx)[0].id.clone();
    let renamed = sync.update(cx, |sync, cx| {
        sync.rename(&id, "  Work\u{202e} distro ", cx)
    });
    assert_eq!(renamed, Ok(()));
    cx.run_until_parked();
    assert_eq!(locations(&sync, cx)[0].name, "Work distro");
    // Nothing is no name, and is said so; the location keeps its name.
    let refused = sync.update(cx, |sync, cx| sync.rename(&id, " \u{200b} ", cx));
    assert_eq!(refused, Err("A location needs a name.".to_string()));
    cx.run_until_parked();
    assert_eq!(locations(&sync, cx)[0].name, "Work distro");
    // Another location's name is refused, whatever its case; a long one is cut.
    add(&sync, macbook(), "The Mac", false, cx);
    let refused = sync.update(cx, |sync, cx| sync.rename(&id, "the mac", cx));
    assert_eq!(
        refused,
        Err("The Mac is another location's name.".to_string())
    );
    let long = "x".repeat(400);
    sync.update(cx, |sync, cx| sync.rename(&id, &long, cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        locations(&sync, cx)[0].name.chars().count(),
        sb_core::location::NAME_CHARS
    );
    sync.update(cx, |sync, cx| sync.rename(&id, "Work distro", cx))
        .unwrap();
    cx.run_until_parked();
    let archive = cx.update(|cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    let kept = archive.catalogue().location(&id).unwrap().unwrap();
    assert_eq!(kept.name, "Work distro");
    sync.read_with(cx, |sync, _| assert_eq!(sync.details().sessions(&id), 1));
}

#[gpui::test]
fn removing_a_syncing_location_stops_it_and_keeps_its_sessions(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    add(&sync, LocationKind::Local, sync::local_name(), true, cx);
    let library = cx.update(|cx| cx.services().library.clone());
    assert_eq!(
        library.read_with(cx, |library, _| library.counts().all),
        SESSIONS.len()
    );
    add(&sync, macbook(), "user@macbook", true, cx);
    let host = locations(&sync, cx)[1].clone();
    // The owner removes it while a sync of it is under way.
    sync.update(cx, |sync, cx| {
        sync.sync(&host.id, cx);
        assert!(sync.is_syncing());
        sync.remove(&host.id, cx);
        assert!(sync.is_removing(&host.id));
    });
    cx.run_until_parked();

    sync.read_with(cx, |sync, _| {
        assert!(!sync.is_syncing());
        assert!(!sync.is_removing(&host.id));
        let names: Vec<&str> = sync
            .locations()
            .unwrap()
            .iter()
            .map(|it| it.name.as_str())
            .collect();
        assert_eq!(names, [sync::local_name()]);
        assert_eq!(sync.removed().len(), 1);
        // Its sessions still name it.
        assert_eq!(
            sync.location(&host.id).map(|it| it.name.as_str()),
            Some("user@macbook")
        );
    });
    // What its pass stored before it stopped stays in the archive, and is shown.
    let archive = cx.update(|cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    let catalogue = archive.catalogue();
    let removed = catalogue.location(&host.id).unwrap().unwrap();
    assert!(removed.removed.is_some());
    let held = catalogue.holdings().unwrap();
    assert!(held.iter().any(|it| it.location == host.id));

    // Syncing everything leaves it alone.
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();
    let after = catalogue.location(&host.id).unwrap().unwrap();
    assert_eq!(after.last_attempt, removed.last_attempt);

    // Adding its place again brings it back, under its name, with everything archived from it.
    add(&sync, macbook(), "The Mac", true, cx);
    let back = locations(&sync, cx);
    assert_eq!(back.len(), 2);
    assert_eq!(
        (back[1].id.clone(), back[1].name.as_str()),
        (host.id, "user@macbook")
    );
    sync.read_with(cx, |sync, _| assert!(sync.removed().is_empty()));
    let all = catalogue.sessions(Shelf::All).unwrap().len();
    assert_eq!(all, SESSIONS.len() + 1);
}

#[gpui::test]
fn removing_every_location_is_not_a_first_run(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    add(&sync, LocationKind::Local, sync::local_name(), true, cx);
    let id = locations(&sync, cx)[0].id.clone();
    sync.update(cx, |sync, cx| sync.remove(&id, cx));
    cx.run_until_parked();
    sync.read_with(cx, |sync, _| {
        assert_eq!(sync.locations().map(<[_]>::len), Some(0));
        assert!(!sync.is_first_run());
    });
    // The library is still the archive's.
    let library = cx.update(|cx| cx.services().library.clone());
    library.update(cx, |library, cx| library.show(Shelf::All, cx));
    cx.run_until_parked();
    assert_eq!(
        library.read_with(cx, |library, _| library.counts().all),
        SESSIONS.len()
    );
}

#[gpui::test]
fn a_host_that_answers_is_tested_and_what_is_there_is_found(cx: &mut TestAppContext) {
    let sync = init(
        |launcher| match launcher {
            Launcher::Ssh { host } if host == "user@macbook" => Ok(answered()),
            other => panic!("no {other:?} here"),
        },
        cx,
    );
    sync.update(cx, |sync, cx| sync.test_place(macbook(), cx));
    sync.read_with(cx, |sync, _| {
        let test = sync.test().expect("a test runs");
        assert!(matches!(test.state, TestState::Running(_)));
        // The command is the launcher's own (DESIGN §4.12).
        assert_eq!(
            test.command.as_deref(),
            Some(macbook().launcher().unwrap().command_line().as_str())
        );
    });
    cx.run_until_parked();
    let Some(TestState::Passed { probe, found }) = test_state(&sync, cx) else {
        panic!("the test passes");
    };
    assert_eq!(probe.map(|it| it.took.as_millis()), Some(1400));
    let found = found[0].as_ref().expect("the agent is found");
    assert_eq!(found.sessions, 1);
    assert!(found.bytes > 0);
    // Nothing was added by testing.
    assert!(locations(&sync, cx).is_empty());
}

#[gpui::test]
fn each_way_a_test_can_fail_is_told_apart(cx: &mut TestAppContext) {
    let sync = init(
        |launcher| {
            let Launcher::Ssh { host } = launcher else {
                panic!("only hosts here");
            };
            match host.as_str() {
                "asleep" => Err(TransportError::Unreachable("timed out".into())),
                "refuses" => Err(TransportError::Refused("Permission denied".into())),
                "changed" => Err(TransportError::HostKey(
                    "Host key verification failed.".into(),
                )),
                "bare" => Ok(Probe {
                    missing: vec!["sha256sum or shasum".to_string()],
                    ..answered()
                }),
                "no-ssh" => Err(TransportError::Launch {
                    program: "ssh".to_string(),
                    source: std::io::ErrorKind::NotFound.into(),
                }),
                _ => Err(TransportError::Failed("the login shell hung".into())),
            }
        },
        cx,
    );
    let mut outcomes = Vec::new();
    for host in ["asleep", "refuses", "changed", "bare", "no-ssh", "hangs"] {
        let kind = LocationKind::Ssh {
            host: host.to_string(),
        };
        sync.update(cx, |sync, cx| sync.test_place(kind, cx));
        cx.run_until_parked();
        let Some(TestState::Failed(failure)) = test_state(&sync, cx) else {
            panic!("{host} fails");
        };
        outcomes.push((failure.may_add_anyway(), failure));
    }
    assert_eq!(
        outcomes,
        [
            (true, TestFailure::Unreachable("timed out".to_string())),
            (true, TestFailure::Refused("Permission denied".to_string())),
            (
                false,
                TestFailure::HostKey("Host key verification failed.".to_string())
            ),
            (
                false,
                TestFailure::Missing(vec!["sha256sum or shasum".to_string()])
            ),
            (false, TestFailure::NotInstalled("ssh".to_string())),
            (true, TestFailure::Other("the login shell hung".to_string())),
        ]
    );
}

#[gpui::test]
fn a_cancelled_test_says_nothing(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    sync.update(cx, |sync, cx| {
        sync.test_place(macbook(), cx);
        sync.cancel_test();
    });
    cx.run_until_parked();
    assert!(test_state(&sync, cx).is_none());
    // A new test replaces the last, whose answer is dropped.
    sync.update(cx, |sync, cx| {
        sync.test_place(macbook(), cx);
        sync.test_place(ubuntu(), cx);
    });
    cx.run_until_parked();
    sync.read_with(cx, |sync, _| {
        let test = sync.test().unwrap();
        assert_eq!(test.kind, ubuntu());
        assert!(matches!(test.state, TestState::Passed { .. }));
    });
}

#[gpui::test]
fn wsl_that_fails_to_list_says_so_in_its_words(cx: &mut TestAppContext) {
    init_reaching(
        tempfile::tempdir().unwrap(),
        machine(),
        Reach {
            connect: Arc::new(|kind, _| panic!("no {kind:?} here")),
            tester: Arc::new(|launcher, _| panic!("no {launcher:?} here")),
            distros: Arc::new(|_| {
                Err(TransportError::Failed(
                    "The Windows Subsystem for Linux is not installed.".to_string(),
                ))
            }),
            launcher: Arc::new(LocationKind::launcher),
            development_home: None,
        },
        cx,
    );
    let sync = cx.update(|cx| cx.services().sync.clone());
    sync.read_with(cx, |sync, _| {
        let distros = sync.distros().expect("the distros were asked for");
        assert_eq!(
            distros.listed,
            Err("The Windows Subsystem for Linux is not installed.".to_string())
        );
        assert_eq!(distros.offered().count(), 0);
    });
}

#[cfg(unix)]
#[gpui::test]
fn a_host_read_through_a_local_shell_passes_its_test_and_syncs(cx: &mut TestAppContext) {
    use sb_core::transport::ShellTransport;
    use sb_test_support::loopback;

    let host = elsewhere();
    let (home, vars) = (host.home().to_path_buf(), host.vars().to_vec());
    let launcher = loopback("sh", &home, &vars);
    let shell = launcher.clone();
    init_reaching(
        tempfile::tempdir().unwrap(),
        machine(),
        Reach {
            connect: Arc::new(move |_, cancel| {
                Box::new(ShellTransport::new(shell.clone()).cancelled_by(cancel))
            }),
            // The real connection test, through a local shell in the host's place.
            tester: Arc::new(move |_, cancel| probe(&launcher, cancel)),
            distros: Arc::new(|_| Ok(None)),
            launcher: Arc::new(LocationKind::launcher),
            development_home: None,
        },
        cx,
    );
    let sync = cx.update(|cx| cx.services().sync.clone());
    sync.update(cx, |sync, cx| sync.test_place(macbook(), cx));
    cx.run_until_parked();
    let Some(TestState::Passed { probe, found }) = test_state(&sync, cx) else {
        panic!("the test passes: {:?}", test_state(&sync, cx));
    };
    let probe = probe.expect("the shell answered");
    assert!(probe.missing.is_empty(), "{probe:?}");
    assert_eq!(found[0].as_ref().map(|it| it.sessions), Ok(1));

    add(&sync, macbook(), "user@macbook", true, cx);
    let added = locations(&sync, cx);
    sync.read_with(cx, |sync, _| {
        assert!(matches!(sync.status(&added[0]), LocationStatus::Synced(_)));
        assert_eq!(sync.details().sessions(&added[0].id), 1);
    });
    drop(host);
}

#[gpui::test]
fn the_development_override_is_what_a_test_runs_and_shows(cx: &mut TestAppContext) {
    let other = elsewhere();
    init_reaching(
        tempfile::tempdir().unwrap(),
        machine(),
        Reach {
            connect: Arc::new(move |_, _| Box::new(other.transport())),
            tester: Arc::new(|launcher, _| match launcher {
                Launcher::Command { args, .. } if args.contains(&"HOME=/fixture".to_string()) => {
                    Ok(answered())
                }
                other => panic!("the override's launcher, not {other:?}"),
            }),
            distros: Arc::new(|_| Ok(None)),
            launcher: Arc::new(|kind| kind.launcher().map(|it| it.reading_home("/fixture"))),
            development_home: Some("/fixture".to_string()),
        },
        cx,
    );
    let sync = cx.update(|cx| cx.services().sync.clone());
    sync.update(cx, |sync, cx| sync.test_place(macbook(), cx));
    cx.run_until_parked();
    sync.read_with(cx, |sync, _| {
        assert_eq!(sync.development_home(), Some("/fixture"));
        let test = sync.test().unwrap();
        assert!(matches!(test.state, TestState::Passed { .. }));
        let command = test.command.as_deref().unwrap();
        assert!(command.ends_with("HOME=/fixture sh -s"), "{command}");
    });
}

#[gpui::test]
fn a_test_replaced_by_another_never_shows_its_answer_under_the_new_place(cx: &mut TestAppContext) {
    /* The first host's test fails, whatever its cancel says: a cancelled pass is dropped
    before it is answered, a failure only by the guard where the answer lands. */
    let sync = init(
        |launcher| match launcher {
            Launcher::Ssh { host } if host == "first" => {
                Err(TransportError::Unreachable("asleep".to_string()))
            }
            _ => Ok(answered()),
        },
        cx,
    );
    let (first, second) = (
        LocationKind::Ssh {
            host: "first".to_string(),
        },
        LocationKind::Ssh {
            host: "second".to_string(),
        },
    );
    sync.update(cx, |sync, cx| {
        sync.test_place(first, cx);
        sync.test_place(second.clone(), cx);
    });
    // Step by step: at no point does the first host's answer show as the second's.
    while cx.executor().tick() {
        sync.read_with(cx, |sync, _| {
            let test = sync.test().unwrap();
            assert_eq!(test.kind, second);
            assert!(
                !matches!(test.state, TestState::Failed(_)),
                "{:?}",
                test.state
            );
        });
    }
    cx.run_until_parked();
    assert!(matches!(
        test_state(&sync, cx),
        Some(TestState::Passed { .. })
    ));
}

#[gpui::test]
fn a_location_being_removed_is_not_synced(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    add(&sync, ubuntu(), "Ubuntu-24.04", false, cx);
    let id = locations(&sync, cx)[0].id.clone();
    sync.update(cx, |sync, cx| {
        sync.remove(&id, cx);
        sync.sync(&id, cx);
        assert!(!sync.is_syncing());
    });
    cx.run_until_parked();
    sync.read_with(cx, |sync, _| assert_eq!(sync.removed().len(), 1));
}

#[gpui::test]
fn a_place_known_here_is_refused_before_anything_is_written(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    add(&sync, ubuntu(), "Ubuntu-24.04", false, cx);
    // Known to the service: nothing runs, and nothing needs saying.
    add(&sync, ubuntu(), "Again", false, cx);
    sync.read_with(cx, |sync, _| assert_eq!(sync.refused(), None));
    assert_eq!(locations(&sync, cx).len(), 1);
}

#[gpui::test]
fn a_place_only_the_catalogue_knows_is_refused_by_the_write(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    // Another writer added it since the service read its locations.
    let archive = cx.update(|cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    archive
        .catalogue()
        .add_location(macbook(), "Behind its back")
        .unwrap();
    add(&sync, macbook(), "user@macbook", false, cx);
    sync.read_with(cx, |sync, _| {
        assert_eq!(
            sync.refused(),
            Some("That place is a location already: Behind its back.")
        );
    });
    assert_eq!(archive.catalogue().locations().unwrap().len(), 1);
}

#[gpui::test]
fn a_location_added_again_keeps_its_name_and_its_place_in_the_list(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    add(&sync, ubuntu(), "Work distro", false, cx);
    add(&sync, macbook(), "The Mac", false, cx);
    let id = locations(&sync, cx)[0].id.clone();
    sync.update(cx, |sync, cx| sync.remove(&id, cx));
    cx.run_until_parked();
    add(&sync, ubuntu(), "Ubuntu-24.04", false, cx);
    let names: Vec<String> = locations(&sync, cx).into_iter().map(|it| it.name).collect();
    assert_eq!(names, ["Work distro", "The Mac"]);
}

#[gpui::test]
fn a_rename_that_lands_after_a_removal_renames_the_removed_location(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    add(&sync, ubuntu(), "Ubuntu-24.04", false, cx);
    let id = locations(&sync, cx)[0].id.clone();
    sync.update(cx, |sync, cx| {
        sync.rename(&id, "Work distro", cx).unwrap();
        sync.remove(&id, cx);
    });
    cx.run_until_parked();
    sync.read_with(cx, |sync, _| {
        assert_eq!(sync.removed()[0].name, "Work distro");
    });
}

/**
 * The services with a distro of more sessions than one batch holds, so that a pass stopped
 * after its first batch leaves some; it is asleep while `asleep` is set.
 */
fn init_many(asleep: Arc<AtomicBool>, cx: &mut TestAppContext) -> Entity<SyncService> {
    let mut big = ProjectsTree::new();
    for at in 0..300 {
        big.outlined(&Outline {
            project: "/home/user/code/big",
            title: &format!("Session {at}"),
            prompt: "One of many.",
            model: "model-a",
            day: (2026, 11, 14),
            calls: 1,
        });
    }
    init_reaching(
        tempfile::tempdir().unwrap(),
        machine(),
        Reach {
            connect: Arc::new(move |_, _| {
                if asleep.load(Ordering::SeqCst) {
                    Box::new(Asleep)
                } else {
                    Box::new(big.transport())
                }
            }),
            tester: Arc::new(|_, _| Ok(answered())),
            distros: Arc::new(|_| Ok(None)),
            launcher: Arc::new(LocationKind::launcher),
            development_home: None,
        },
        cx,
    );
    cx.update(|cx| cx.services().sync.clone())
}

/// Adds the distro, its first sync stopped after one batch, and returns it.
fn add_stopped(sync: &Entity<SyncService>, cx: &mut TestAppContext) -> Location {
    sync.update(cx, |sync, _| sync.hold_after(1, super::sync::Held::Stop));
    add(sync, ubuntu(), "Ubuntu-24.04", true, cx);
    let distro = locations(sync, cx)[0].clone();
    let LocationStatus::Stopped { items_left, .. } =
        sync.read_with(cx, |sync, _| sync.status(&distro))
    else {
        panic!("the stopped pass says so");
    };
    assert!(items_left > 0);
    distro
}

#[gpui::test]
fn a_stopped_pass_says_what_it_left_until_one_completes(cx: &mut TestAppContext) {
    let sync = init_many(Arc::default(), cx);
    let distro = add_stopped(&sync, cx);
    sync.update(cx, |sync, cx| sync.sync(&distro.id, cx));
    cx.run_until_parked();
    let distro = locations(&sync, cx)[0].clone();
    assert!(matches!(
        sync.read_with(cx, |sync, _| sync.status(&distro)),
        LocationStatus::Synced(_)
    ));
}

#[gpui::test]
fn a_pass_after_a_stop_that_cant_reach_the_place_says_so(cx: &mut TestAppContext) {
    let asleep = Arc::new(AtomicBool::new(false));
    let sync = init_many(asleep.clone(), cx);
    let distro = add_stopped(&sync, cx);
    asleep.store(true, Ordering::SeqCst);
    sync.update(cx, |sync, cx| sync.sync(&distro.id, cx));
    cx.run_until_parked();
    let distro = locations(&sync, cx)[0].clone();
    assert!(matches!(
        sync.read_with(cx, |sync, _| sync.status(&distro)),
        LocationStatus::Unreachable { .. }
    ));
}

#[gpui::test]
fn a_stopped_location_removed_and_added_again_is_not_stopped(cx: &mut TestAppContext) {
    let asleep = Arc::new(AtomicBool::new(false));
    let sync = init_many(asleep.clone(), cx);
    let distro = add_stopped(&sync, cx);
    sync.update(cx, |sync, cx| sync.remove(&distro.id, cx));
    cx.run_until_parked();
    // Back without a sync: what the stopped pass stored is its last word, and no stop is.
    add(&sync, ubuntu(), "Ubuntu-24.04", false, cx);
    let distro = locations(&sync, cx)[0].clone();
    assert!(!matches!(
        sync.read_with(cx, |sync, _| sync.status(&distro)),
        LocationStatus::Stopped { .. }
    ));
}

fn host(name: &str) -> LocationKind {
    LocationKind::Ssh {
        host: name.to_string(),
    }
}

#[gpui::test]
fn a_location_added_again_whose_name_is_taken_needs_a_new_one(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    add(&sync, host("user@a"), "Work Mac", false, cx);
    let a = locations(&sync, cx)[0].id.clone();
    sync.update(cx, |sync, cx| sync.remove(&a, cx));
    cx.run_until_parked();
    add(&sync, host("user@b"), "Work Mac", false, cx);
    sync.read_with(cx, |sync, _| {
        let renamed = sync.restoring(&host("user@a")).map(|(_, renamed)| renamed);
        assert_eq!(renamed, Some(true));
    });

    // Its old name is another's now: that is refused, and it comes back under a new one.
    add(&sync, host("user@a"), "work mac", false, cx);
    sync.read_with(cx, |sync, _| {
        assert_eq!(sync.refused(), Some("Work Mac is another location's name."));
    });
    add(&sync, host("user@a"), "Old Mac", false, cx);
    let names: Vec<String> = locations(&sync, cx).into_iter().map(|it| it.name).collect();
    assert_eq!(names, ["Old Mac", "Work Mac"]);
}

#[gpui::test]
fn two_adds_under_way_cant_share_a_name(cx: &mut TestAppContext) {
    let sync = init(|_| Ok(answered()), cx);
    sync.update(cx, |sync, cx| {
        sync.add(host("user@a"), "Work Mac".to_string(), false, cx);
        sync.add(host("user@b"), "WORK MAC".to_string(), false, cx);
    });
    cx.run_until_parked();
    sync.read_with(cx, |sync, _| {
        assert_eq!(sync.refused(), Some("Work Mac is another location's name."));
    });
    assert_eq!(locations(&sync, cx).len(), 1);
}

fn last_check(sync: &Entity<SyncService>, cx: &mut TestAppContext) -> Option<LastCheck> {
    let id = locations(sync, cx)[0].id.clone();
    sync.read_with(cx, |sync, _| sync.details().last_check(&id).cloned())
}

#[gpui::test]
fn a_location_keeps_what_its_shell_said_to_its_last_test(cx: &mut TestAppContext) {
    let lacking = Arc::new(AtomicBool::new(false));
    let sync = init(
        {
            let lacking = lacking.clone();
            move |_| {
                Ok(Probe {
                    missing: if lacking.load(Ordering::SeqCst) {
                        vec!["find".to_string()]
                    } else {
                        Vec::new()
                    },
                    ..answered()
                })
            }
        },
        cx,
    );
    // The test it was added after is its first check.
    sync.update(cx, |sync, cx| sync.test_place(macbook(), cx));
    cx.run_until_parked();
    add(&sync, macbook(), "user@macbook", false, cx);
    let first = last_check(&sync, cx).expect("the test it was added after");
    assert_eq!(first.login_shell.as_deref(), Some("/bin/bash"));
    assert!(first.missing.is_empty());

    // A later test, tools missing and all, takes its place.
    lacking.store(true, Ordering::SeqCst);
    sync.update(cx, |sync, cx| sync.test_place(macbook(), cx));
    cx.run_until_parked();
    assert!(matches!(test_state(&sync, cx), Some(TestState::Failed(_))));
    assert_eq!(
        last_check(&sync, cx).map(|it| it.missing),
        Some(vec!["find".to_string()])
    );
}
