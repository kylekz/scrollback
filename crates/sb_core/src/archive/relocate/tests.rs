//! Moving the archive (PLAN §4.4): what is copied and checked, a crash at each step, a stop
//! and a failure, what the archive is given and loses meanwhile, and the refusals. The old
//! folder is compared byte for byte, times included, before and after.

use std::cell::RefCell;

use pretty_assertions::assert_eq;

use super::*;
use crate::catalogue::{Commit, SessionId, Target, Totals};
use crate::location::{LocationId, LocationKind};
use crate::provider::{ItemKind, ScannedItem};

/// Every file under a folder with its length, time and bytes, by its path relative to it.
type Tree = BTreeMap<PathBuf, (u64, Option<SystemTime>, Vec<u8>)>;

fn tree(dir: &Path) -> Tree {
    let mut found = Tree::new();
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else {
                let metadata = fs::metadata(&path).unwrap();
                found.insert(
                    path.strip_prefix(dir).unwrap().to_path_buf(),
                    (
                        metadata.len(),
                        metadata.modified().ok(),
                        fs::read(&path).unwrap(),
                    ),
                );
            }
        }
    }
    found
}

/// Stores `bytes` as the first version of the transcript `key` of the session `session`.
fn store(archive: &Archive, location: &LocationId, key: &str, session: &str, bytes: &[u8]) {
    let scanned = ScannedItem {
        key: key.to_string(),
        kind: ItemKind::Transcript,
        session: Some(session.to_string()),
        length: bytes.len() as u64,
        stamp: "1".to_string(),
        lines: false,
        export_version: None,
    };
    let mut writer = archive.range_writer(0);
    writer.write(bytes).unwrap();
    let commit = Commit {
        location,
        provider: "fixture",
        scanned: &scanned,
        target: Target::NewItem,
        number: 1,
        from: 0,
        length: bytes.len() as u64,
        hash: Hash::of(bytes),
    };
    let stored = archive
        .store(vec![(commit, writer.finish().unwrap())])
        .unwrap();
    assert!(stored.iter().all(Result::is_ok), "{stored:?}");
}

/**
 * An archive in `dir/archive` as a start leaves it, closed: three sessions' transcripts, a
 * file set aside in `orphans/`, today's backup of the catalogue, its search index, and a file
 * of the owner's beside them.
 */
fn archive_in(dir: &Path) -> (PathBuf, LocationId) {
    let path = dir.join("archive");
    let archive = Archive::create(&path).unwrap();
    let location = archive
        .catalogue()
        .add_location(LocationKind::Local, "This machine")
        .unwrap()
        .id;
    for n in 0..3 {
        let bytes = format!("{{\"session\": {n}}}\n").repeat(200 * (n + 1));
        store(
            &archive,
            &location,
            &format!("projects/{n}.jsonl"),
            &format!("s{n}"),
            bytes.as_bytes(),
        );
    }
    archive.search_index().unwrap();
    let orphan = path.join(layout::ORPHANS).join("files/set-aside.zst");
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, "set aside").unwrap();
    fs::write(path.join("notes.txt"), "the owner's").unwrap();
    drop(archive);
    /* A start: today's backup, after which opening it writes nothing. */
    drop(opened(&path));
    (path, location)
}

fn opened(path: &Path) -> Archive {
    let archive = Archive::open(path).unwrap();
    archive.wait_for_removals();
    archive
}

fn moved_into(archive: &Archive, target: &Path) -> Move {
    let plan = archive.plan_move(target).unwrap();
    archive.start_move(&plan).unwrap()
}

fn go(_: &MoveProgress) {}

/// A move of `archive` into `target` from start to end.
fn moved(archive: &Archive, target: &Path) -> Archive {
    let mut moving = moved_into(archive, target);
    moving.copy(archive, &CancelToken::new(), &mut go).unwrap();
    moving
        .finish(archive, &CancelToken::new(), &mut go, |_| Ok(()))
        .unwrap()
}

/// The archive in `path` verifies with nothing wrong, and holds `totals`.
fn assert_whole(path: &Path, totals: Totals) {
    let archive = opened(path);
    let report = archive.verify(&CancelToken::new()).unwrap();
    assert!(report.damaged.is_empty(), "{:?}", report.damaged);
    assert!(report.unread.is_empty(), "{:?}", report.unread);
    assert!(report.uncatalogued.is_empty(), "{:?}", report.uncatalogued);
    assert!(report.removing.is_empty(), "{:?}", report.removing);
    assert_eq!(archive.catalogue().totals().unwrap(), totals);
}

#[test]
fn a_move_copies_every_file_and_the_old_folder_stays_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let (from, _) = archive_in(dir.path());
    let before = tree(&from);
    let archive = opened(&from);
    let totals = archive.catalogue().totals().unwrap();
    let target = dir.path().join("elsewhere").join("archive");
    let plan = archive.plan_move(&target).unwrap();
    assert!(!target.exists(), "looking makes nothing");
    assert_eq!(plan.target, target);
    assert!(plan.files >= 6, "{plan:?}");
    assert!(plan.needed > plan.bytes);
    assert!(plan.free.is_some_and(|free| free >= plan.needed));

    let mut moving = archive.start_move(&plan).unwrap();
    let mut reported = Vec::new();
    moving
        .copy(&archive, &CancelToken::new(), &mut |progress| {
            reported.push(*progress);
        })
        .unwrap();
    let last = *reported.last().unwrap();
    assert_eq!((last.files_done, last.files), (plan.files, plan.files));
    assert_eq!(last.bytes_done, plan.bytes);
    let committed = RefCell::new(None);
    let moved = moving
        .finish(&archive, &CancelToken::new(), &mut go, |to| {
            *committed.borrow_mut() = Some(to.to_path_buf());
            Ok(())
        })
        .unwrap();
    assert_eq!(committed.into_inner(), Some(target.clone()));
    assert_eq!(moved.id(), archive.id());
    assert!(!target.join(MARKER).exists());
    drop((moved, archive));

    assert_eq!(tree(&from), before, "the old folder is as it was");
    assert_whole(&target, totals);
    let copied = tree(&target);
    for kept in ["notes.txt", "orphans/files/set-aside.zst", "archive.json"] {
        assert_eq!(
            copied[Path::new(kept)].2,
            before[Path::new(kept)].2,
            "{kept}"
        );
    }
    assert!(copied.keys().any(|path| path.starts_with("backups")));
}

/// A crash at any step leaves the old archive in use and as it was, with what the move made
/// removed by the next start; or, once the settings name the new folder, the new archive
/// whole and in use.
#[test]
fn a_crash_at_any_step_leaves_one_archive_whole_and_in_use() {
    let steps = [
        None,
        Some(Step::Copying),
        Some(Step::Copied),
        Some(Step::Catalogue),
        Some(Step::Caught),
        Some(Step::Placed),
        Some(Step::Checked),
        Some(Step::Unmarked),
        Some(Step::Committed),
    ];
    for step in steps {
        let dir = tempfile::tempdir().unwrap();
        let (from, _) = archive_in(dir.path());
        let before = tree(&from);
        let target = dir.path().join("moved");
        /* The app's settings: the archive in use, and the move, recorded before it starts. */
        let settings = RefCell::new((from.clone(), None::<(PathBuf, String)>));
        let archive = opened(&from);
        let totals = archive.catalogue().totals().unwrap();
        let plan = archive.plan_move(&target).unwrap();
        settings.borrow_mut().1 = Some((plan.target.clone(), plan.id.clone()));
        let mut moving = archive.start_move(&plan).unwrap();
        /* With no step, it is cut short as soon as it has started. */
        if let Some(step) = step {
            moving.crash_at(step);
            let crashed = moving
                .copy(&archive, &CancelToken::new(), &mut go)
                .and_then(|()| {
                    moving
                        .finish(&archive, &CancelToken::new(), &mut go, |to| {
                            *settings.borrow_mut() = (to.to_path_buf(), None);
                            Ok(())
                        })
                        .map(drop)
                });
            assert!(
                matches!(crashed, Err(MoveError::Crashed(at)) if at == step),
                "{step:?}: {crashed:?}"
            );
        }
        drop((moving, archive));

        /* What the next start does with what the settings say. */
        let (in_use, pending) = settings.into_inner();
        match pending {
            Some((target, id)) => {
                assert_eq!(in_use, from, "{step:?}");
                let cleared = clear_move(&target, &id);
                if step == Some(Step::Unmarked) {
                    /* A whole copy, no longer marked as the move's: left, and said. */
                    assert_eq!(cleared, Cleared::Left);
                    assert!(target.join(layout::MANIFEST).is_file());
                } else {
                    assert_eq!(cleared, Cleared::Removed, "{step:?}");
                    assert!(!target.exists(), "{step:?}: {:?}", tree(&target).keys());
                }
            }
            None => {
                assert_eq!(step, Some(Step::Committed));
                assert_eq!(in_use, target);
                assert_whole(&target, totals);
            }
        }
        assert_eq!(tree(&from), before, "{step:?}: the old folder is as it was");
    }
}

#[test]
fn a_stop_or_a_failure_removes_what_the_move_made_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let (from, _) = archive_in(dir.path());
    let archive = opened(&from);
    let totals = archive.catalogue().totals().unwrap();
    /* A folder there already, with what Finder leaves in it. */
    let target = dir.path().join("chosen");
    fs::create_dir(&target).unwrap();
    fs::write(target.join(".DS_Store"), "Finder's").unwrap();
    let names = || {
        fs::read_dir(&target)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    };

    let stop = CancelToken::new();
    let mut moving = moved_into(&archive, &target);
    let stopped = moving.copy(&archive, &stop, &mut |progress| {
        if progress.files_done == 2 {
            stop.cancel();
        }
    });
    assert!(matches!(stopped, Err(MoveError::Cancelled)), "{stopped:?}");
    assert_eq!(moving.abandon(), Cleared::Removed);
    assert_eq!(names(), [".DS_Store"]);

    /* Stopped while it finishes. */
    let stop = CancelToken::new();
    let mut moving = moved_into(&archive, &target);
    moving.copy(&archive, &CancelToken::new(), &mut go).unwrap();
    stop.cancel();
    let stopped = moving.finish(&archive, &stop, &mut go, |_| Ok(()));
    assert!(matches!(stopped, Err(MoveError::Cancelled)), "{stopped:?}");
    assert_eq!(moving.abandon(), Cleared::Removed);
    assert_eq!(names(), [".DS_Store"]);

    /* The settings can't be written: the move didn't happen. */
    let mut moving = moved_into(&archive, &target);
    moving.copy(&archive, &CancelToken::new(), &mut go).unwrap();
    let failed = moving.finish(&archive, &CancelToken::new(), &mut go, |_| {
        Err(io::Error::other("the disk is full"))
    });
    assert!(matches!(failed, Err(MoveError::Commit(_))), "{failed:?}");
    assert_eq!(moving.abandon(), Cleared::Removed);
    assert_eq!(names(), [".DS_Store"]);

    /* And it goes there in the end. */
    drop(moved(&archive, &target));
    drop(archive);
    assert_whole(&target, totals);
}

/// What the archive is given while its files are copied is in the copy, and what it lets go
/// of isn't: the finish copies what changed since, and the catalogue as it is then.
#[test]
fn what_the_archive_stores_and_removes_during_the_copy_is_carried() {
    let dir = tempfile::tempdir().unwrap();
    let (from, location) = archive_in(dir.path());
    /* Nothing set aside yet: its folder appears during the copy. */
    fs::remove_dir_all(from.join(layout::ORPHANS)).unwrap();
    let archive = opened(&from);
    let target = dir.path().join("moved");
    let mut moving = moved_into(&archive, &target);
    let mut meanwhile = false;
    moving
        .copy(&archive, &CancelToken::new(), &mut |progress| {
            if progress.files_done == 1 && !meanwhile {
                meanwhile = true;
                store(
                    &archive,
                    &location,
                    "projects/new.jsonl",
                    "new",
                    b"written meanwhile\n",
                );
                let deleted = SessionId {
                    location: location.clone(),
                    provider: "fixture".to_string(),
                    key: "s0".to_string(),
                };
                archive.catalogue().trash(&deleted).unwrap();
                let shown = archive.catalogue().deletion(&deleted).unwrap();
                archive.delete_for_good(&shown).unwrap();
                archive.wait_for_removals();
                let aside = from.join(layout::ORPHANS).join("files/later.zst");
                fs::create_dir_all(aside.parent().unwrap()).unwrap();
                fs::write(aside, "set aside later").unwrap();
            }
        })
        .unwrap();
    assert!(meanwhile);
    let totals = archive.catalogue().totals().unwrap();
    drop(
        moving
            .finish(&archive, &CancelToken::new(), &mut go, |_| Ok(()))
            .unwrap(),
    );
    drop(archive);
    assert_whole(&target, totals);
    assert_eq!(
        fs::read_to_string(target.join(layout::ORPHANS).join("files/later.zst")).unwrap(),
        "set aside later"
    );
}

/// A segment's copy that doesn't hold what the catalogue records fails the move; one the
/// archive itself holds no better is carried as it is, and a verify of either finds it.
#[test]
fn a_copy_must_hold_what_the_catalogue_records_unless_the_archive_doesnt_either() {
    let dir = tempfile::tempdir().unwrap();
    let (from, _) = archive_in(dir.path());
    let archive = opened(&from);
    let segment = recorded(&archive.catalogue().stored().unwrap())
        .into_keys()
        .next()
        .unwrap();
    let target = dir.path().join("moved");
    let mut moving = moved_into(&archive, &target);
    moving.copy(&archive, &CancelToken::new(), &mut go).unwrap();
    /* The copy is spoilt after it was checked, and the check forgotten. */
    fs::write(target.join(&segment), b"not a frame").unwrap();
    for copied in moving.copied.values_mut() {
        copied.checked = None;
    }
    let failed = moving.finish(&archive, &CancelToken::new(), &mut go, |_| Ok(()));
    assert!(
        matches!(&failed, Err(MoveError::Mismatch(path)) if path.ends_with(&segment)),
        "{failed:?}"
    );
    assert_eq!(moving.abandon(), Cleared::Removed);

    /* The archive's own is damaged: the copy is as it is, and verify says so of both. */
    fs::write(from.join(&segment), b"damaged before the move").unwrap();
    let moved = moved(&archive, &target);
    let found = |archive: &Archive| -> Vec<PathBuf> {
        let report = archive.verify(&CancelToken::new()).unwrap();
        report.damaged.iter().map(|damage| damage.file()).collect()
    };
    assert_eq!(found(&moved), found(&archive));
    assert_eq!(found(&moved), [segment]);
}

#[test]
fn a_folder_that_cant_take_the_archive_is_refused_before_anything_is_made() {
    let dir = tempfile::tempdir().unwrap();
    let (from, _) = archive_in(dir.path());
    let archive = opened(&from);
    let another = dir.path().join("another");
    drop(Archive::create(&another).unwrap());
    let full = dir.path().join("full");
    fs::create_dir(&full).unwrap();
    fs::write(full.join("notes.txt"), "mine").unwrap();
    let file = dir.path().join("file.txt");
    fs::write(&file, "mine").unwrap();
    let refused = |target: &Path| match archive.plan_move(target) {
        Err(MoveError::Refused(refusal)) => refusal,
        other => panic!("{target:?}: {other:?}"),
    };
    assert!(matches!(refused(&from), Refusal::Same));
    assert!(matches!(
        refused(&from.join("files").join("..").join(".")),
        Refusal::Same
    ));
    assert!(matches!(refused(&from.join("files")), Refusal::Inside));
    assert!(matches!(
        refused(&from.join("files").join("new")),
        Refusal::Inside
    ));
    assert!(matches!(refused(dir.path()), Refusal::Holds));
    assert!(matches!(refused(&another), Refusal::AnArchive));
    let inside_another = another.join("files").join("new");
    assert!(
        matches!(refused(&inside_another), Refusal::Nested(other) if other == resolved(&another))
    );
    assert!(matches!(refused(&full), Refusal::NotEmpty));
    assert!(matches!(refused(&file), Refusal::NotAFolder));
    assert!(!inside_another.exists() && !from.join("files").join("new").exists());

    /* A volume without room: what it has and what the move needs. */
    assert!(matches!(
        room(dir.path(), Some(10), 11),
        Err(Refusal::NoRoom {
            free: 10,
            needed: 11,
            ..
        })
    ));
    assert!(room(dir.path(), Some(11), 11).is_ok());
    assert!(
        room(dir.path(), None, 11).is_ok(),
        "not known isn't refused"
    );

    /* Something put there after it was looked at, or another scrollback holding it. */
    let late = dir.path().join("late");
    let plan = archive.plan_move(&late).unwrap();
    fs::create_dir(&late).unwrap();
    fs::write(late.join("notes.txt"), "mine").unwrap();
    assert!(matches!(
        archive.start_move(&plan),
        Err(MoveError::Refused(Refusal::NotEmpty))
    ));
    assert_eq!(
        fs::read_dir(&late).unwrap().count(),
        1,
        "nothing left there"
    );
    /* Another scrollback making an archive there has its lock, which is something. */
    let held = dir.path().join("held");
    let plan = archive.plan_move(&held).unwrap();
    fs::create_dir(&held).unwrap();
    let _held = lock(&held).unwrap();
    assert!(matches!(
        archive.start_move(&plan),
        Err(MoveError::Refused(Refusal::NotEmpty))
    ));
}

#[cfg(unix)]
#[test]
fn a_folder_that_cant_be_written_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let (from, _) = archive_in(dir.path());
    let archive = opened(&from);
    let closed = dir.path().join("closed");
    fs::create_dir(&closed).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o500)).unwrap();
    let refused = archive.plan_move(&closed.join("archive"));
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(refused, Err(MoveError::Refused(Refusal::Unwritable(_)))),
        "{refused:?}"
    );
}

/// A start after a crash clears a partial copy only from a folder marked as that move's that
/// no scrollback has open; what isn't the move's stays, and what can't be removed is said.
#[test]
fn only_a_folder_marked_as_the_move_is_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let (from, _) = archive_in(dir.path());
    let archive = opened(&from);
    let target = dir.path().join("moved");
    let plan = archive.plan_move(&target).unwrap();
    let mut moving = archive.start_move(&plan).unwrap();
    moving.copy(&archive, &CancelToken::new(), &mut go).unwrap();
    drop(moving);
    fs::write(target.join("added by hand.txt"), "mine").unwrap();
    let copied = tree(&target);

    assert_eq!(clear_move(&target, "another move"), Cleared::Left);
    assert_eq!(
        clear_move(&dir.path().join("nowhere"), &plan.id),
        Cleared::Left
    );
    let held = lock(&target).unwrap();
    assert_eq!(clear_move(&target, &plan.id), Cleared::Left);
    drop(held);
    assert_eq!(tree(&target), copied);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let stuck = target.join(layout::ORPHANS).join(layout::FILES);
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o500)).unwrap();
        let cleared = clear_move(&target, &plan.id);
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(&cleared, Cleared::Partly(left) if left.iter().any(|left| left.starts_with(layout::ORPHANS))),
            "{cleared:?}"
        );
    }
    #[cfg(not(unix))]
    assert_eq!(clear_move(&target, &plan.id), Cleared::Removed);
    assert!(
        target.join("added by hand.txt").is_file(),
        "what isn't the move's stays"
    );
    assert!(!target.join(layout::FILES).exists());

    /* A folder the move made goes with what it holds. */
    let again = dir.path().join("again");
    let plan = archive.plan_move(&again).unwrap();
    drop(archive.start_move(&plan).unwrap());
    assert_eq!(clear_move(&again, &plan.id), Cleared::Removed);
    assert!(!again.exists());
}
