//! Move… behind Settings › Archive (PLAN §4.4, DESIGN §4.13): the app moves onto the copy and
//! the settings file says so; Stop, and refusals in words, leave the app where it was; the
//! owner's changes made while the services are closed are written to the archive they come
//! back over; and a start after a move cut short clears the partial copy away and says so.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{Entity, TestAppContext};
use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::{Archive, Cleared};
use sb_core::catalogue::Shelf;
use sb_core::location::LocationKind;

use super::settings::{Ended, MoveState, load};
use super::sync::Held;
use super::*;
use crate::test_support::{SESSIONS, archive_dir, init_in, init_library, machine};

fn settings(cx: &mut TestAppContext) -> Entity<SettingsService> {
    cx.update(|cx| cx.services().settings.clone())
}

/// Settles the services and whatever waits on a timer, as a switch does.
fn settle(cx: &mut TestAppContext) {
    for _ in 0..20 {
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
    }
}

fn sessions(cx: &mut TestAppContext) -> usize {
    cx.update(|cx| cx.services().library.read(cx).counts().all)
}

/// Where Move… stands, as a word, with what it says.
fn standing(cx: &mut TestAppContext) -> (&'static str, Option<String>) {
    settings(cx).read_with(cx, |settings, _| match settings.move_state() {
        None => ("none", None),
        Some(MoveState::Checking(_)) => ("checking", None),
        Some(MoveState::Refused { why, .. }) => ("refused", Some(why.clone())),
        Some(MoveState::Asking(_)) => ("asking", None),
        Some(MoveState::Running(_)) => ("running", None),
        Some(MoveState::Clearing(_)) => ("clearing", None),
        Some(MoveState::Ended(ended)) => (
            match ended {
                Ended::Stopped { .. } => "stopped",
                Ended::Failed { .. } => "failed",
                Ended::CutShort { .. } => "cut short",
            },
            Some(ended.words()),
        ),
    })
}

/// Looks at `folder` for a move, and says yes if asked.
fn move_to(folder: &Path, cx: &mut TestAppContext) {
    let settings = settings(cx);
    settings.update(cx, |settings, cx| {
        settings.check_move(folder.to_path_buf(), cx)
    });
    settle(cx);
    assert_eq!(standing(cx).0, "asking", "{:?}", standing(cx).1);
    settings.update(cx, |settings, cx| settings.confirm_move(cx));
    settle(cx);
}

/// Every file under `dir`, with its bytes, but the databases' shared memory.
fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else if !path.to_string_lossy().ends_with("-shm") {
                found.push((
                    path.strip_prefix(dir).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap(),
                ));
            }
        }
    }
    found.sort();
    found
}

#[gpui::test]
fn a_move_puts_the_app_on_the_copy_and_the_old_folder_is_said_and_never_written(
    cx: &mut TestAppContext,
) {
    init_library(cx);
    let from = archive_dir(cx);
    let data = from.parent().unwrap().to_path_buf();
    let elsewhere = tempfile::tempdir().unwrap();
    let to = elsewhere.path().join("archive");
    move_to(&to, cx);

    assert_eq!(standing(cx), ("none", None));
    cx.update(|cx| {
        let services = cx.services();
        assert_eq!(services.archive.read(cx).dir(), to);
        assert_eq!(crate::pages::Showing::now(cx).key(), "sessions");
        let settings = services.settings.read(cx);
        let moved = settings
            .moved_here(&to)
            .expect("Settings says where the old copy is");
        assert_eq!(moved.from, from);
        /* The copy was verified as soon as the app was on it. */
        let last = services.upkeep.read(cx).last().expect("a verify ran");
        assert_eq!((last.damaged_total, last.uncatalogued), (0, 0));
    });
    assert_eq!(sessions(cx), SESSIONS.len());
    let saved = load(&data).settings;
    assert_eq!(saved.archive, Some(to.clone()));
    assert_eq!(saved.moving, None);
    assert_eq!(saved.moved.map(|moved| moved.from), Some(from.clone()));

    /* The old folder isn't written again, whatever happens on the copy, and opens as it was. */
    let before = files(&from);
    cx.update(|cx| {
        let services = cx.services();
        let (library, sync) = (services.library.clone(), services.sync.clone());
        library.update(cx, |library, cx| library.show(Shelf::Starred, cx));
        sync.update(cx, |sync, cx| sync.set_stale_after_days(3, cx));
    });
    settle(cx);
    assert_eq!(files(&from), before);
    let old = Archive::open(&from).expect("the old archive is left closed, as it was");
    assert_eq!(old.catalogue().totals().unwrap().sessions, SESSIONS.len());
    drop(old);

    /* Dismissed, it is forgotten, there and in the file; the old folder stays. */
    settings(cx).update(cx, |settings, cx| settings.dismiss_moved(cx));
    settle(cx);
    assert_eq!(load(&data).settings.moved, None);
    assert!(from.join("archive.json").is_file());
}

#[gpui::test]
fn a_stopped_move_leaves_the_app_where_it_was_and_removes_the_copy(cx: &mut TestAppContext) {
    init_library(cx);
    let from = archive_dir(cx);
    let elsewhere = tempfile::tempdir().unwrap();
    let to = elsewhere.path().join("archive");
    settings(cx).update(cx, |settings, _| settings.hold_move_after(1, Held::Stop));
    move_to(&to, cx);

    let (state, said) = standing(cx);
    assert_eq!(state, "stopped");
    assert!(
        said.as_deref()
            .is_some_and(|said| said.contains("was removed")),
        "{said:?}"
    );
    assert!(!to.exists());
    cx.update(|cx| assert_eq!(cx.services().archive.read(cx).dir(), from));
    let saved = load(from.parent().unwrap()).settings;
    assert_eq!(
        (saved.archive, saved.moving, saved.moved),
        (None, None, None)
    );
    assert_eq!(sessions(cx), SESSIONS.len());

    /* And it can go there after all. */
    settings(cx).update(cx, |settings, cx| settings.dismiss_move(cx));
    move_to(&to, cx);
    cx.update(|cx| assert_eq!(cx.services().archive.read(cx).dir(), to));
}

#[gpui::test]
fn a_folder_that_cant_take_the_archive_is_refused_in_words(cx: &mut TestAppContext) {
    init_library(cx);
    let from = archive_dir(cx);
    let documents = tempfile::tempdir().unwrap();
    std::fs::write(documents.path().join("notes.txt"), "mine").unwrap();
    let install = tempfile::tempdir().unwrap();
    settings(cx).update(cx, |settings, _| {
        settings.set_install_root(install.path().to_path_buf())
    });
    for (folder, says) in [
        (documents.path().to_path_buf(), "isn't empty"),
        (from.clone(), "the archive's own folder"),
        (from.join("files"), "inside the archive"),
        (install.path().join("archive"), "install folder"),
        (documents.path().join("notes.txt"), "a file, not a folder"),
    ] {
        settings(cx).update(cx, |settings, cx| settings.check_move(folder.clone(), cx));
        settle(cx);
        let (state, why) = standing(cx);
        assert_eq!(state, "refused", "{folder:?}");
        assert!(
            why.as_deref().is_some_and(|why| why.contains(says)),
            "{folder:?}: {why:?}"
        );
        settings(cx).update(cx, |settings, cx| settings.dismiss_move(cx));
    }
    cx.update(|cx| assert_eq!(cx.services().archive.read(cx).dir(), from));
    assert!(!install.path().join("archive").exists(), "nothing made");
}

/// The owner's changes made while the services are closed for a move wait, and are written to
/// the archive the services come back over: here the one left, as after a failed move.
#[gpui::test]
fn changes_made_while_a_move_finishes_are_written_after_it(cx: &mut TestAppContext) {
    init_library(cx);
    let ids: Vec<_> = cx.update(|cx| {
        let archive = cx.services().archive.read(cx).archive().cloned().unwrap();
        let sessions = archive.catalogue().sessions(Shelf::All).unwrap();
        sessions.into_iter().map(|session| session.id).collect()
    });
    let switched = cx.update(|cx| {
        let switching = switch(true, cx, |open| {
            (open.expect("the archive is open"), Err(()))
        });
        let library = cx.services().library.clone();
        library.update(cx, |library, cx| library.star(ids.clone(), true, cx));
        switching
    });
    settle(cx);
    assert_eq!(cx.foreground_executor.block_test(switched), Err(()));
    cx.update(|cx| {
        let library = cx.services().library.read(cx);
        assert_eq!(library.counts().starred, SESSIONS.len());
    });
}

#[gpui::test]
fn a_move_cut_short_is_cleared_away_at_the_next_start(cx: &mut TestAppContext) {
    let data = tempfile::tempdir().unwrap();
    let from = data.path().join(settings::ARCHIVE_FOLDER);
    let elsewhere = tempfile::tempdir().unwrap();
    let to = elsewhere.path().join("archive");
    /* A move that had copied every file when the app stopped. */
    {
        let archive = Archive::create(&from).unwrap();
        archive
            .catalogue()
            .add_location(LocationKind::Local, "This machine")
            .unwrap();
        let plan = archive.plan_move(&to).unwrap();
        let mut moving = archive.start_move(&plan).unwrap();
        moving
            .copy(&archive, &CancelToken::new(), &mut |_| {})
            .unwrap();
        drop(moving);
        std::fs::write(
            data.path().join(settings::SETTINGS_FILE),
            serde_json::json!({
                "v": 1,
                "move": {
                    "v": 1,
                    "id": plan.id,
                    "made": plan.made,
                    "from": from.display().to_string(),
                    "to": to.display().to_string(),
                    "started": 1_791_115_200_000_i64,
                },
            })
            .to_string(),
        )
        .unwrap();
    }
    init_in(data, machine(), cx);
    settle(cx);

    let (state, said) = standing(cx);
    assert_eq!(state, "cut short");
    assert!(
        said.as_deref()
            .is_some_and(|said| said.contains("didn't finish") && said.contains("was removed")),
        "{said:?}"
    );
    assert!(!to.exists(), "the partial copy is gone");
    cx.update(|cx| assert_eq!(cx.services().archive.read(cx).dir(), from));
    assert_eq!(load(from.parent().unwrap()).settings.moving, None);
    settings(cx).update(cx, |settings, cx| settings.dismiss_move(cx));
    assert_eq!(standing(cx), ("none", None));
}

/// A start whose recorded move's folder holds nothing of it leaves the folder as it is.
#[gpui::test]
fn a_folder_no_longer_the_moves_is_left_as_it_is(cx: &mut TestAppContext) {
    let data = tempfile::tempdir().unwrap();
    let from = data.path().join(settings::ARCHIVE_FOLDER);
    drop(Archive::create(&from).unwrap());
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("notes.txt"), "mine").unwrap();
    std::fs::write(
        data.path().join(settings::SETTINGS_FILE),
        serde_json::json!({
            "v": 1,
            "move": {
                "v": 1,
                "id": "a move",
                "made": false,
                "from": from.display().to_string(),
                "to": elsewhere.path().display().to_string(),
                "started": 1_791_115_200_000_i64,
            },
        })
        .to_string(),
    )
    .unwrap();
    init_in(data, machine(), cx);
    settle(cx);
    let cleared = settings(cx).read_with(cx, |settings, _| match settings.move_state() {
        Some(MoveState::Ended(Ended::CutShort { cleared, .. })) => Some(cleared.clone()),
        _ => None,
    });
    assert_eq!(cleared, Some(Cleared::Left));
    assert!(elsewhere.path().join("notes.txt").is_file());
}

/// While a move runs, a change that would be written to the archive it leaves is refused,
/// saying why: the locations, Stale after.
#[gpui::test]
fn changes_the_move_would_lose_are_refused_while_it_runs(cx: &mut TestAppContext) {
    init_library(cx);
    let elsewhere = tempfile::tempdir().unwrap();
    let to = elsewhere.path().join("archive");
    let settings = settings(cx);
    settings.update(cx, |settings, cx| settings.check_move(to.clone(), cx));
    settle(cx);
    let (sync, location) = cx.update(|cx| {
        let sync = cx.services().sync.clone();
        let location = sync.read(cx).locations().unwrap()[0].id.clone();
        (sync, location)
    });
    settings.update(cx, |settings, cx| settings.confirm_move(cx));
    sync.update(cx, |sync, cx| {
        sync.set_stale_after_days(3, cx);
        assert_eq!(sync.stale_after_days(), 7);
        assert_eq!(sync.refused(), Some(settings::WAIT_FOR_THE_MOVE));
        assert_eq!(
            sync.rename(&location, "Renamed", cx),
            Err(settings::WAIT_FOR_THE_MOVE.to_string())
        );
    });
    settle(cx);
    cx.update(|cx| assert_eq!(cx.services().archive.read(cx).dir(), to));
}

/// A marker left in the archive in use, by a move whose last steps a crash cut short once the
/// settings named its folder, is taken away at the start.
#[gpui::test]
fn a_marker_left_in_the_archive_in_use_is_taken_away_at_the_start(cx: &mut TestAppContext) {
    let data = tempfile::tempdir().unwrap();
    let archive = data.path().join(settings::ARCHIVE_FOLDER);
    drop(Archive::create(&archive).unwrap());
    std::fs::write(archive.join(sb_core::archive::MARKER), "{}").unwrap();
    init_in(data, machine(), cx);
    settle(cx);
    assert!(!archive.join(sb_core::archive::MARKER).exists());
    cx.update(|cx| assert!(cx.services().archive.read(cx).archive().is_some()));
}
