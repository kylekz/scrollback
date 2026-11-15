//! The owner's locations through the sync service (DESIGN §4.10–§4.12): adding each kind,
//! the same place twice, renaming, removing one that is syncing and adding it back, the
//! connection test's every outcome and WSL's distros. Every place but this machine is a
//! stand-in, or a home of the test's own read through a local `sh`; no real distro or host is
//! reached and neither `wsl.exe` nor `ssh` runs.

use std::sync::Arc;

use gpui::{Entity, TestAppContext};
use pretty_assertions::assert_eq;
use sb_core::catalogue::Shelf;
use sb_core::location::{Location, LocationKind};
use sb_core::transport::wsl::Distro;
use sb_core::transport::{Launcher, Probe, TransportError};
use sb_test_support::{Outline, ProjectsTree};

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
    sync.update(cx, |sync, cx| sync.rename(&id, "  Work distro ", cx));
    cx.run_until_parked();
    assert_eq!(locations(&sync, cx)[0].name, "Work distro");
    // Nothing is no name.
    sync.update(cx, |sync, cx| sync.rename(&id, "   ", cx));
    cx.run_until_parked();
    assert_eq!(locations(&sync, cx)[0].name, "Work distro");
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

    // Adding its place again brings it back, with everything archived from it.
    add(&sync, macbook(), "The Mac", true, cx);
    let back = locations(&sync, cx);
    assert_eq!(back.len(), 2);
    assert_eq!(
        (back[1].id.clone(), back[1].name.as_str()),
        (host.id, "The Mac")
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
