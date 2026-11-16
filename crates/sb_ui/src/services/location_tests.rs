//! The sync service over every kind of location (PLAN §5, §6): a WSL distro and an SSH host as
//! the catalogue holds them, each reached through a home of the test's own, never the distro
//! or the host it names. Those read through a shell need `sh`, and are left out on Windows.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{Entity, TestAppContext};
use pretty_assertions::assert_eq;
use sb_core::archive::Archive;
use sb_core::location::{Location, LocationKind};
use sb_core::transport::{
    Env, FileStat, Hashed, Listing, PrefixRequest, RangeFailure, RangeRequest, RangeSink,
    Transport, TransportError,
};
use sb_test_support::{Outline, ProjectsTree};
use tempfile::TempDir;

use super::*;
use crate::test_support::{SESSIONS, init, init_with, machine};

/// What the host over SSH says when it is asleep.
const ASLEEP: &str = "ssh: connect to host macbook port 22: Operation timed out";

/// A host that is asleep: every call says it can't be reached.
pub(super) struct Asleep;

impl Asleep {
    fn asleep<T>(&self) -> Result<T, TransportError> {
        Err(TransportError::Unreachable(ASLEEP.to_string()))
    }
}

impl Transport for Asleep {
    fn env(&self, _: &[&str]) -> Result<Env, TransportError> {
        self.asleep()
    }

    fn stat(&self, _: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        self.asleep()
    }

    fn list(&self, _: &str) -> Result<Listing, TransportError> {
        self.asleep()
    }

    fn hash_prefixes(
        &self,
        _: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError> {
        self.asleep()
    }

    fn read_ranges(&self, _: &[RangeRequest], _: &mut dyn RangeSink) -> Result<(), TransportError> {
        self.asleep()
    }

    fn read_small(&self, _: &str) -> Result<Option<Vec<u8>>, TransportError> {
        self.asleep()
    }
}

/// A scratch folder whose archive holds the locations `kinds`, as if the owner had added them
/// in an earlier run.
fn archive_with(kinds: &[(LocationKind, &str)]) -> TempDir {
    let scratch = tempfile::tempdir().expect("a scratch folder");
    let archive = Archive::create(&scratch.path().join("archive")).expect("an archive");
    for (kind, name) in kinds {
        archive
            .catalogue()
            .add_location(kind.clone(), name)
            .expect("the location is added");
    }
    scratch
}

fn sync_service(cx: &mut TestAppContext) -> Entity<SyncService> {
    cx.update(|cx| cx.services().sync.clone())
}

fn locations(cx: &mut TestAppContext) -> Vec<(Location, LocationStatus)> {
    sync_service(cx).read_with(cx, |sync, _| {
        sync.locations()
            .expect("the locations are read")
            .iter()
            .map(|location| (location.clone(), sync.status(location)))
            .collect()
    })
}

/// The titles the library shows, in its order. Asked only by the tests that sync through a
/// shell on this machine, which are Unix's.
#[cfg(unix)]
fn titles(cx: &mut TestAppContext) -> Vec<String> {
    cx.update(|cx| {
        let library = cx.services().library.read(cx);
        library
            .sessions()
            .expect("a page of sessions")
            .sessions
            .iter()
            .filter_map(|session| session.title.clone())
            .collect()
    })
}

#[cfg(unix)]
#[gpui::test]
fn a_distro_is_synced_through_its_shell_beside_this_machine(cx: &mut TestAppContext) {
    use sb_core::transport::ShellTransport;
    use sb_test_support::loopback;

    let mut distro = ProjectsTree::new();
    distro.outlined(&Outline {
        project: "/home/user/code/server",
        title: "Written in the distro",
        prompt: "Only the distro has this one.",
        model: "model-a",
        day: (2026, 11, 14),
        calls: 4,
    });
    let (home, vars) = (distro.home().to_path_buf(), distro.vars().to_vec());
    let scratch = archive_with(&[
        (LocationKind::Local, "This machine"),
        (
            LocationKind::Wsl {
                distro: "Ubuntu-24.04".to_string(),
            },
            "Ubuntu-24.04",
        ),
    ]);
    init_with(
        scratch,
        machine(),
        move |kind, cancel| match kind {
            LocationKind::Wsl { .. } => {
                Box::new(ShellTransport::new(loopback("sh", &home, &vars)).cancelled_by(cancel))
            }
            other => panic!("no {other:?} here"),
        },
        cx,
    );
    // The start synced both, each through its own transport.
    let synced = locations(cx);
    assert_eq!(synced.len(), 2);
    for (location, status) in &synced {
        assert!(
            matches!(status, LocationStatus::Synced(_)),
            "{}: {status:?}",
            location.name
        );
    }
    let mut expected: Vec<&str> = SESSIONS.iter().map(|session| session.title).collect();
    expected.insert(0, "Written in the distro");
    assert_eq!(titles(cx), expected);
    cx.update(|cx| {
        let library = cx.services().library.read(cx);
        assert_eq!(library.sessions().unwrap().stats.locations, 2);
    });
}

#[gpui::test]
fn a_host_that_cant_be_reached_is_unreachable_and_no_failure(cx: &mut TestAppContext) {
    let scratch = archive_with(&[
        (LocationKind::Local, "This machine"),
        (
            LocationKind::Ssh {
                host: "user@macbook".to_string(),
            },
            "user@macbook",
        ),
    ]);
    init_with(scratch, machine(), |_, _| Box::new(Asleep), cx);
    // Added in the same instant, they are in no order of their own.
    let mut synced = locations(cx);
    synced.sort_by_key(|(location, _)| location.kind != LocationKind::Local);
    let (host, status) = &synced[1];
    let LocationStatus::Unreachable { since } = *status else {
        panic!("the host is unreachable: {status:?}");
    };
    assert_eq!(host.unreachable.as_ref().map(|it| it.since), Some(since));
    assert!(
        host.unreachable.as_ref().unwrap().reason.contains(ASLEEP),
        "{host:?}"
    );
    // Not an error: nothing on record says it failed, and this machine synced as ever.
    assert_eq!(host.last_error, None);
    assert!(matches!(synced[0].1, LocationStatus::Synced(_)));
    cx.update(|cx| {
        assert!(cx.services().is_settled(cx));
        assert_eq!(cx.services().library.read(cx).counts().all, SESSIONS.len());
    });

    // Still asleep at the next sync: since when stays the first time it was missed.
    sync_service(cx).update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();
    let again = locations(cx);
    let host = again.iter().find(|(location, _)| location.id == host.id);
    assert_eq!(
        host.map(|(_, status)| status),
        Some(&LocationStatus::Unreachable { since }),
        "since the first sync that missed it"
    );
}

#[cfg(unix)]
#[gpui::test]
fn a_host_whose_shell_fails_has_failed_in_its_own_words(cx: &mut TestAppContext) {
    use sb_core::transport::{Launcher, ShellTransport};

    let scratch = archive_with(&[(
        LocationKind::Ssh {
            host: "user@macbook".to_string(),
        },
        "user@macbook",
    )]);
    init_with(
        scratch,
        ProjectsTree::new(),
        |_, cancel| {
            // A launcher that ends before the shell answers, saying why.
            let launcher = Launcher::Command {
                program: "sh".into(),
                args: vec![
                    "-c".to_string(),
                    "echo 'the shell there is missing' >&2; exit 3".to_string(),
                ],
            };
            Box::new(ShellTransport::new(launcher).cancelled_by(cancel))
        },
        cx,
    );
    let (host, status) = locations(cx).remove(0);
    let LocationStatus::Failed(why) = status else {
        panic!("the host failed: {status:?}");
    };
    assert!(why.contains("the shell there is missing"), "{why}");
    assert_eq!(host.unreachable, None);
}

#[gpui::test]
fn a_sync_asked_for_while_its_location_is_indexed_waits_for_the_index(cx: &mut TestAppContext) {
    // More sessions than a pass stores in one batch, so that one stops between two.
    let mut machine = ProjectsTree::new();
    for at in 0..300 {
        machine.outlined(&Outline {
            project: "/home/user/code/app",
            title: &format!("Session {at}"),
            prompt: "One of many.",
            model: "model-a",
            day: (2026, 11, 1 + at % 28),
            calls: 1,
        });
    }
    init(machine, cx);
    let sync = sync_service(cx);
    // When the stopped pass's index starts, a sync of the same location is asked for.
    let (asked, overlapped) = (Rc::new(RefCell::new(false)), Rc::new(RefCell::new(false)));
    let _watching = cx.update(|cx| {
        let (asked, overlapped) = (asked.clone(), overlapped.clone());
        cx.observe(&sync, move |sync, cx| {
            let Some(id) = sync
                .read(cx)
                .locations()
                .and_then(|it| it.first())
                .map(|it| it.id.clone())
            else {
                return;
            };
            let indexing = sync.read(cx).indexing(Some(&id)).is_some();
            if indexing && sync.read(cx).is_syncing() {
                *overlapped.borrow_mut() = true;
            }
            if indexing && !asked.replace(true) {
                sync.update(cx, |sync, cx| sync.sync(&id, cx));
            }
        })
    });
    sync.update(cx, |sync, cx| {
        sync.hold_after(1, Held::Stop);
        sync.add_local(cx);
    });
    cx.run_until_parked();
    assert!(*asked.borrow(), "the stopped pass's location was indexed");
    assert!(
        !*overlapped.borrow(),
        "a pass ran beside its location's index"
    );
    // The sync asked for ran once the index was over, and carried on to the end.
    let (_, status) = locations(cx).remove(0);
    assert!(matches!(status, LocationStatus::Synced(_)), "{status:?}");
    cx.update(|cx| {
        assert!(cx.services().is_settled(cx));
        assert_eq!(cx.services().library.read(cx).counts().all, 300);
    });
}

#[gpui::test]
fn stopping_every_sync_drops_one_waiting_for_its_locations_index(cx: &mut TestAppContext) {
    // More sessions than a pass stores in one batch, so that one stops between two.
    let mut machine = ProjectsTree::new();
    for at in 0..300 {
        machine.outlined(&Outline {
            project: "/home/user/code/app",
            title: &format!("Session {at}"),
            prompt: "One of many.",
            model: "model-a",
            day: (2026, 11, 1 + at % 28),
            calls: 1,
        });
    }
    init(machine, cx);
    let sync = sync_service(cx);
    // When the stopped pass's index starts, a sync of the same location is asked for, then
    // every sync is stopped.
    let asked = Rc::new(RefCell::new(false));
    let _watching = cx.update(|cx| {
        let asked = asked.clone();
        cx.observe(&sync, move |sync, cx| {
            let Some(id) = sync
                .read(cx)
                .locations()
                .and_then(|it| it.first())
                .map(|it| it.id.clone())
            else {
                return;
            };
            if sync.read(cx).indexing(Some(&id)).is_some() && !asked.replace(true) {
                sync.update(cx, |sync, cx| {
                    sync.sync(&id, cx);
                    sync.stop_all();
                });
            }
        })
    });
    sync.update(cx, |sync, cx| {
        sync.hold_after(1, Held::Stop);
        sync.add_local(cx);
    });
    cx.run_until_parked();
    assert!(*asked.borrow(), "the stopped pass's location was indexed");
    // The sync that waited was dropped: the location never synced in full.
    let (_, status) = locations(cx).remove(0);
    assert!(!matches!(status, LocationStatus::Synced(_)), "{status:?}");
    cx.update(|cx| {
        assert!(cx.services().is_settled(cx));
        assert!(cx.services().library.read(cx).counts().all < 300);
    });
}
