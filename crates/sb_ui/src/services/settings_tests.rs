//! The services behind Settings (DESIGN §4.13): the app's settings file, the interval sync
//! keeps while the app is open, the archive's own stale threshold, moving onto another
//! archive, verify and fetching a damaged item again, and allowing a session deleted for good
//! to be archived again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use gpui::{Entity, TestAppContext};
use pretty_assertions::assert_eq;
use sb_core::archive::Archive;
use sb_core::catalogue::Shelf;
use sb_core::location::LocationKind;
use sb_test_support::Outline;

use super::settings::{Switch, load};
use super::sync::Refetch;
use super::upkeep::Fix;
use super::*;
use crate::test_support::{
    SESSIONS, archive_dir, init_in, init_library, machine, machine_file, write_session,
};

fn settings(cx: &mut TestAppContext) -> Entity<SettingsService> {
    cx.update(|cx| cx.services().settings.clone())
}

fn sessions(cx: &mut TestAppContext) -> usize {
    cx.update(|cx| cx.services().library.read(cx).counts().all)
}

/// A session the agent on this machine writes while the app is open.
fn written_later(title: &'static str, cx: &mut TestAppContext) {
    write_session(
        &Outline {
            project: "/home/user/notes",
            title,
            prompt: "Nobody asks for this sync.",
            model: "model-a",
            day: (2026, 11, 15),
            calls: 0,
        },
        cx,
    );
}

fn minutes(minutes: u64) -> Duration {
    Duration::from_secs(minutes * 60)
}

/// The scheduler, on gpui's fake clock: each interval counts from when it is chosen, and only
/// when asked there are no rounds at all.
#[gpui::test]
fn sync_every_follows_settings_and_only_when_asked_leaves_the_owner(cx: &mut TestAppContext) {
    init_library(cx);
    let settings = settings(cx);
    settings.update(cx, |settings, cx| {
        settings.set_sync_every(SyncEvery::Five, cx)
    });
    written_later("Written in the first five minutes", cx);
    cx.executor()
        .advance_clock(minutes(5) - Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(sessions(cx), 3);
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(sessions(cx), 4);

    /* Only when asked: no round, however long the app stays open. */
    settings.update(cx, |settings, cx| {
        settings.set_sync_every(SyncEvery::Asked, cx)
    });
    cx.update(|cx| assert_eq!(cx.services().sync.read(cx).next_round(), None));
    written_later("Written while only the owner syncs", cx);
    cx.executor().advance_clock(minutes(180));
    cx.run_until_parked();
    assert_eq!(sessions(cx), 4);

    /* An hour from when it is chosen, not from the last round. */
    settings.update(cx, |settings, cx| {
        settings.set_sync_every(SyncEvery::Sixty, cx)
    });
    cx.update(|cx| assert!(cx.services().sync.read(cx).next_round().is_some()));
    cx.executor().advance_clock(minutes(59));
    cx.run_until_parked();
    assert_eq!(sessions(cx), 4);
    cx.executor().advance_clock(minutes(1));
    cx.run_until_parked();
    assert_eq!(sessions(cx), 5);
}

/// A data dir whose settings say `json`, and whose archive has this machine as a location.
fn data_dir_with(json: &str) -> tempfile::TempDir {
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join(settings::SETTINGS_FILE), json).unwrap();
    let archive = Archive::create(&data.path().join(settings::ARCHIVE_FOLDER)).unwrap();
    archive
        .catalogue()
        .add_location(LocationKind::Local, "This machine")
        .unwrap();
    data
}

#[gpui::test]
fn every_launch_syncs_even_when_only_the_owner_asks(cx: &mut TestAppContext) {
    init_in(
        data_dir_with(r#"{"v": 1, "sync_every": "asked"}"#),
        machine(),
        cx,
    );
    assert_eq!(sessions(cx), SESSIONS.len());
    cx.update(|cx| {
        let sync = cx.services().sync.read(cx);
        assert_eq!((sync.every(), sync.next_round()), (SyncEvery::Asked, None));
    });
}

#[gpui::test]
fn settings_are_kept_beside_the_archive_in_the_data_dir(cx: &mut TestAppContext) {
    init_in(
        data_dir_with(r#"{"v": 1, "mode": "dark", "kept": "as it was"}"#),
        machine(),
        cx,
    );
    let data = archive_dir(cx).parent().unwrap().to_path_buf();
    let settings = settings(cx);
    cx.update(|cx| {
        /* Read at the start, and what they say about appearance is in place. */
        assert_eq!(
            gpui_appearance::state(cx).preferences().mode,
            crate::appearance::Mode::Dark
        );
    });
    settings.update(cx, |settings, cx| {
        settings.set_mode(crate::appearance::Mode::Light, cx);
        settings.set_material(crate::theme::Material::Opaque, cx);
        settings.set_sync_every(SyncEvery::Thirty, cx);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(
            gpui_appearance::state(cx).preferences().mode,
            crate::appearance::Mode::Light
        );
    });
    let read = load(&data);
    assert_eq!(read.unsaveable, None);
    assert_eq!(read.settings.mode, crate::appearance::Mode::Light);
    assert_eq!(read.settings.material, crate::theme::Material::Opaque);
    assert_eq!(read.settings.sync_every, SyncEvery::Thirty);
    let text = std::fs::read_to_string(data.join(settings::SETTINGS_FILE)).unwrap();
    assert!(text.contains(r#""kept": "as it was""#), "{text}");
    /* The archive holds nothing of the app's settings. */
    assert!(!archive_dir(cx).join(settings::SETTINGS_FILE).exists());
}

#[gpui::test]
fn stale_after_is_the_archives_and_kept_within_its_range(cx: &mut TestAppContext) {
    init_library(cx);
    let sync = cx.update(|cx| cx.services().sync.clone());
    let kept = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let archive = cx.services().archive.read(cx).archive().cloned().unwrap();
            archive
                .catalogue()
                .setting("sync.stale_after_days")
                .unwrap()
        })
    };
    sync.read_with(cx, |sync, _| assert_eq!(sync.stale_after_days(), 7));
    assert_eq!(kept(cx), None);
    for (asked, taken) in [(14, 14), (0, 1), (365, 90)] {
        sync.update(cx, |sync, cx| sync.set_stale_after_days(asked, cx));
        cx.run_until_parked();
        sync.read_with(cx, |sync, _| assert_eq!(sync.stale_after_days(), taken));
        assert_eq!(kept(cx), Some(taken.to_string()));
    }
}

/// Every file under `dir` with its length, its modification time and its bytes.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (u64, SystemTime, Vec<u8>)> {
    let mut found = BTreeMap::new();
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else {
                let metadata = std::fs::metadata(&path).unwrap();
                found.insert(
                    path.strip_prefix(dir).unwrap().to_path_buf(),
                    (
                        metadata.len(),
                        metadata.modified().unwrap(),
                        std::fs::read(&path).unwrap(),
                    ),
                );
            }
        }
    }
    found
}

/// Settles the services and whatever waits on a timer, as a switch does.
fn settle(cx: &mut TestAppContext) {
    for _ in 0..10 {
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
    }
}

fn open_archive(cx: &mut TestAppContext) -> Weak<Archive> {
    cx.update(|cx| {
        Arc::downgrade(
            cx.services()
                .archive
                .read(cx)
                .archive()
                .expect("the archive is open"),
        )
    })
}

#[gpui::test]
fn another_folder_reopens_on_its_archive_and_nothing_writes_to_the_one_left(
    cx: &mut TestAppContext,
) {
    init_library(cx);
    let (left, left_archive) = (archive_dir(cx), open_archive(cx));
    let elsewhere = tempfile::tempdir().unwrap();
    let new = elsewhere.path().join("archive");
    let settings = settings(cx);

    /* An empty folder gets a new archive, and the app is moved onto it. */
    settings.update(cx, |settings, cx| settings.use_folder(new.clone(), cx));
    settle(cx);
    cx.update(|cx| {
        let services = cx.services();
        assert_eq!(services.settings.read(cx).switch(), None);
        assert_eq!(
            services.archive.read(cx).dir(),
            std::path::absolute(&new).unwrap()
        );
        assert_eq!(crate::pages::Showing::now(cx).key(), "first-run");
    });
    assert!(new.join("archive.json").is_file());
    assert_eq!(
        load(left.parent().unwrap()).settings.archive,
        Some(new.clone())
    );
    /* The archive left is closed: nothing holds it, and it opens again elsewhere. */
    assert!(
        left_archive.upgrade().is_none(),
        "the archive left is still open"
    );
    drop(Archive::open(&left).expect("its lock was let go"));

    /* Whatever happens on the new archive, the one left stays as it was. */
    let before = snapshot(&left);
    cx.update(|cx| {
        let sync = cx.services().sync.clone();
        sync.update(cx, |sync, cx| sync.add_local(cx));
    });
    settle(cx);
    assert_eq!(sessions(cx), SESSIONS.len());
    cx.update(|cx| {
        let services = cx.services();
        let (library, upkeep, sync) = (
            services.library.clone(),
            services.upkeep.clone(),
            services.sync.clone(),
        );
        library.update(cx, |library, cx| library.show(Shelf::Starred, cx));
        upkeep.update(cx, |upkeep, cx| upkeep.verify(cx));
        sync.update(cx, |sync, cx| sync.set_stale_after_days(3, cx));
    });
    settle(cx);
    assert_eq!(snapshot(&left), before);

    /* And the way back is the same: the archive there opens as it was. */
    settings.update(cx, |settings, cx| settings.use_folder(left.clone(), cx));
    settle(cx);
    cx.update(|cx| {
        let services = cx.services();
        assert_eq!(services.archive.read(cx).dir(), left);
        assert_eq!(services.sync.read(cx).stale_after_days(), 7);
    });
    assert_eq!(sessions(cx), SESSIONS.len());
    /* The default folder is not written down as a choice. */
    assert_eq!(load(left.parent().unwrap()).settings.archive, None);
}

#[gpui::test]
fn a_folder_that_holds_something_else_is_refused_and_the_app_stays(cx: &mut TestAppContext) {
    init_library(cx);
    let current = archive_dir(cx);
    let documents = tempfile::tempdir().unwrap();
    std::fs::write(documents.path().join("notes.txt"), "mine").unwrap();
    let settings = settings(cx);
    for folder in [
        documents.path().to_path_buf(),
        current.clone(),
        current.join("files"),
    ] {
        settings.update(cx, |settings, cx| settings.use_folder(folder.clone(), cx));
        settle(cx);
        let switch = settings.read_with(cx, |settings, _| settings.switch().cloned());
        assert!(
            matches!(switch, Some(Switch::Refused { .. })),
            "{folder:?}: {switch:?}"
        );
        cx.update(|cx| assert_eq!(cx.services().archive.read(cx).dir(), current));
    }
    assert_eq!(
        std::fs::read_dir(documents.path()).unwrap().count(),
        1,
        "nothing was made beside what the folder held"
    );
    assert_eq!(sessions(cx), SESSIONS.len());
    assert_eq!(load(current.parent().unwrap()).settings.archive, None);
}

/// Damages the first segment of the item `key` of this machine in the archive.
fn damage(key: &str, cx: &mut TestAppContext) {
    let folder = &sb_core::hash::Hash::of(key.as_bytes()).to_hex()[..32];
    let mut segments: Vec<PathBuf> = Vec::new();
    let mut folders = vec![archive_dir(cx).join("files")];
    while let Some(dir) = folders.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else if path.extension().is_some_and(|it| it == "zst")
                && path.iter().any(|part| part == folder)
            {
                segments.push(path);
            }
        }
    }
    segments.sort();
    let mut bytes = std::fs::read(&segments[0]).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(&segments[0], bytes).unwrap();
}

#[gpui::test]
fn verify_finds_damage_and_an_item_still_at_its_location_is_fetched_again(cx: &mut TestAppContext) {
    init_library(cx);
    let upkeep = cx.update(|cx| cx.services().upkeep.clone());
    let (kept, gone) = (machine_file(0, cx), machine_file(2, cx));
    damage(&kept, cx);
    damage(&gone, cx);
    /* One of them is no longer on this machine, and its next sync says so. */
    std::fs::remove_file(crate::test_support::machine_path(&gone, cx)).unwrap();
    cx.update(|cx| {
        let sync = cx.services().sync.clone();
        sync.update(cx, |sync, cx| sync.sync_all(cx));
    });
    settle(cx);

    upkeep.update(cx, |upkeep, cx| upkeep.verify(cx));
    cx.update(|cx| assert!(cx.services().upkeep.read(cx).verifying().is_some()));
    settle(cx);
    let problems = upkeep.read_with(cx, |upkeep, _| upkeep.problems().to_vec());
    assert_eq!(
        upkeep.read_with(cx, |upkeep, _| upkeep.last().unwrap().damaged.len()),
        2
    );
    let fix = |key: &str, problems: &[upkeep::Problem]| {
        problems
            .iter()
            .find(|problem| problem.damage.key == key)
            .map(|problem| {
                (
                    problem.fix,
                    problem.session.as_ref().and_then(|it| it.title.clone()),
                )
            })
            .unwrap()
    };
    assert_eq!(
        fix(&kept, &problems),
        (Fix::FetchAgain, Some(SESSIONS[0].title.to_string()))
    );
    assert_eq!(fix(&gone, &problems).0, Fix::Damaged);

    let damage = problems
        .iter()
        .find(|problem| problem.damage.key == kept)
        .unwrap()
        .damage
        .clone();
    let sync = cx.update(|cx| cx.services().sync.clone());
    sync.update(cx, |sync, cx| sync.refetch(&damage, cx));
    sync.read_with(cx, |sync, _| {
        assert_eq!(sync.refetch_of(&damage), Some(&Refetch::Running))
    });
    settle(cx);
    sync.read_with(cx, |sync, _| {
        assert_eq!(sync.refetch_of(&damage), Some(&Refetch::Done(2)))
    });
    /* Settings shows it fetched once it reads the archive again. */
    upkeep.update(cx, |upkeep, cx| upkeep.refresh(cx));
    settle(cx);
    let problems = upkeep.read_with(cx, |upkeep, _| upkeep.problems().to_vec());
    assert_eq!(fix(&kept, &problems).0, Fix::Fetched(2));
}

#[gpui::test]
fn a_session_allowed_again_can_be_undone_from_settings(cx: &mut TestAppContext) {
    init_library(cx);
    let archive = cx.update(|cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    let id = archive.catalogue().sessions(Shelf::All).unwrap()[0]
        .id
        .clone();
    archive.catalogue().trash(&id).unwrap();
    let shown = archive.catalogue().deletion(&id).unwrap();
    archive.delete_for_good(&shown).unwrap();
    let upkeep = cx.update(|cx| cx.services().upkeep.clone());
    upkeep.update(cx, |upkeep, cx| upkeep.shown(cx));
    settle(cx);
    let allowed = |cx: &mut TestAppContext| {
        upkeep.read_with(cx, |upkeep, _| {
            upkeep
                .deleted()
                .iter()
                .map(|deleted| deleted.allowed.is_some())
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(allowed(cx), [false]);

    upkeep.update(cx, |upkeep, cx| upkeep.allow(&id, cx));
    settle(cx);
    assert_eq!(allowed(cx), [true]);
    assert_eq!(archive.catalogue().tombstones().unwrap(), []);
    upkeep.update(cx, |upkeep, cx| upkeep.undo_allow(&id, cx));
    settle(cx);
    assert_eq!(allowed(cx), [false]);
    assert_eq!(archive.catalogue().tombstones().unwrap().len(), 1);

    /* Allowed and synced: archived again, and the undo is refused, saying why. */
    upkeep.update(cx, |upkeep, cx| upkeep.allow(&id, cx));
    settle(cx);
    cx.update(|cx| {
        let sync = cx.services().sync.clone();
        sync.update(cx, |sync, cx| sync.sync_all(cx));
    });
    settle(cx);
    assert_eq!(sessions(cx), SESSIONS.len());
    upkeep.update(cx, |upkeep, cx| upkeep.undo_allow(&id, cx));
    settle(cx);
    upkeep.read_with(cx, |upkeep, _| {
        assert!(upkeep.deleted()[0].refused.is_some());
    });
    /* Shown again, Settings lists none: the session is archived. */
    upkeep.update(cx, |upkeep, cx| upkeep.shown(cx));
    settle(cx);
    assert_eq!(allowed(cx), Vec::<bool>::new());
}
