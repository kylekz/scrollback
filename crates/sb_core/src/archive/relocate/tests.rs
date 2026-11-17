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

/**
 * Every file under `dir` with its length, time and bytes, but a folder's own `lock`: a move
 * holds its target's while it copies, and on Windows that is a byte-range lock no other
 * handle can read through (error 33), where elsewhere the lock is advisory.
 */
fn tree(dir: &Path) -> Tree {
    let mut found = Tree::new();
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else if path == dir.join(layout::LOCK) {
                continue;
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
/// whole and in use, its marker taken away by the next start. No whole copy is ever left that
/// nothing uses.
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
        Some(Step::Committed),
    ];
    for step in steps {
        let dir = tempfile::tempdir().unwrap();
        let (from, _) = archive_in(dir.path());
        let before = tree(&from);
        let target = dir.path().join("moved");
        /* The app's settings: the archive in use, and the move, recorded before it starts. */
        let settings = RefCell::new((from.clone(), None::<(PathBuf, String, bool)>));
        let archive = opened(&from);
        let totals = archive.catalogue().totals().unwrap();
        let plan = archive.plan_move(&target).unwrap();
        settings.borrow_mut().1 = Some((plan.target.clone(), plan.id.clone(), plan.made));
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
            Some((target, id, made)) => {
                assert_eq!(in_use, from, "{step:?}");
                assert_eq!(clear_move(&target, &id, made), Cleared::Removed, "{step:?}");
                assert!(!target.exists(), "{step:?}: {:?}", tree(&target).keys());
            }
            None => {
                assert_eq!(step, Some(Step::Committed));
                assert_eq!(in_use, target);
                assert!(target.join(MARKER).is_file());
                forget_marker(&target);
                assert!(!target.join(MARKER).exists());
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

/// [`archive_in`]'s archive with `more` sessions besides its three, closed: enough files that
/// the copies run side by side.
fn archive_with(dir: &Path, more: usize) -> PathBuf {
    let (from, location) = archive_in(dir);
    let archive = opened(&from);
    for n in 3..3 + more {
        let bytes = format!("{{\"more\": {n}}}\n").repeat(40);
        store(
            &archive,
            &location,
            &format!("projects/{n}.jsonl"),
            &format!("s{n}"),
            bytes.as_bytes(),
        );
    }
    from
}

/// What is in the folder `dir`, by name.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A stop while several files are being copied stops every copy before `copy` returns: nothing
/// is said or written in the new folder after it, and what was made there is removed whole.
#[test]
fn a_stop_while_files_are_copied_side_by_side_stops_them_all() {
    let dir = tempfile::tempdir().unwrap();
    let from = archive_with(dir.path(), 60);
    let before = tree(&from);
    let archive = opened(&from);
    let target = dir.path().join("chosen");
    fs::create_dir(&target).unwrap();
    fs::write(target.join(".DS_Store"), "Finder's").unwrap();
    let stop = CancelToken::new();
    let mut moving = moved_into(&archive, &target);
    let mut told = 0;
    let stopped = moving.copy(&archive, &stop, &mut |progress| {
        told = progress.files_done;
        if progress.files_done == 10 {
            stop.cancel();
        }
    });
    assert!(matches!(stopped, Err(MoveError::Cancelled)), "{stopped:?}");
    assert_eq!(told, 10, "nothing is said once stopped");
    let left = tree(&target);
    assert!(left.len() < before.len(), "it stopped before the end");
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(tree(&target) == left, "nothing is written after it returns");
    assert_eq!(moving.abandon(), Cleared::Removed);
    assert_eq!(names_in(&target), [".DS_Store"]);
    drop(archive);
    assert!(tree(&from) == before, "the old folder is as it was");
}

/// Keeps a move from reading the file it is made with until it is dropped: on Windows another
/// program holds it, and elsewhere its permissions let nobody read it.
struct Kept {
    #[cfg(windows)]
    _held: File,
    #[cfg(unix)]
    path: PathBuf,
}

impl Kept {
    #[cfg(windows)]
    fn from(path: &Path) -> Self {
        use std::os::windows::fs::OpenOptionsExt as _;

        Self {
            _held: File::options().read(true).share_mode(0).open(path).unwrap(),
        }
    }

    #[cfg(unix)]
    fn from(path: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        Self {
            path: path.to_path_buf(),
        }
    }
}

#[cfg(unix)]
impl Drop for Kept {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o644)).unwrap();
    }
}

/**
 * A file that can't be read fails the copy, naming it, while the others are copied beside it:
 * every copy stops, and what was made is removed. On Windows another program holds it, which
 * the copy waits out for a moment first; elsewhere its permissions keep scrollback out.
 */
#[test]
fn a_file_that_cant_be_read_fails_the_copies_side_by_side_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let from = archive_with(dir.path(), 60);
    let before = tree(&from);
    let archive = opened(&from);
    let segments: Vec<PathBuf> = recorded(&archive.catalogue().stored().unwrap())
        .into_keys()
        .collect();
    let held = from.join(&segments[segments.len() / 2]);
    let target = dir.path().join("moved");
    let mut moving = moved_into(&archive, &target);
    let kept = Kept::from(&held);
    let failed = moving.copy(&archive, &CancelToken::new(), &mut go);
    drop(kept);
    assert!(
        matches!(&failed, Err(MoveError::Held(path) | MoveError::Io { path, .. }) if *path == held),
        "{failed:?}"
    );
    assert_eq!(moving.abandon(), Cleared::Removed);
    assert!(!target.exists());
    drop(archive);
    assert!(tree(&from) == before, "the old folder is as it was");
}

/// A move's progress weighs files and their bytes together: a few large files copied first
/// don't fill the bar while thousands of small ones wait.
#[test]
fn progress_weighs_the_files_and_their_bytes_together() {
    let at = |files_done, files, bytes_done, bytes| {
        MoveProgress {
            files_done,
            files,
            bytes_done,
            bytes,
            ..MoveProgress::default()
        }
        .fraction()
    };
    assert_eq!(at(0, 0, 0, 0), 0.);
    assert_eq!(at(10, 10, 5_000, 5_000), 1.);
    /* The catalogue's backups first, 12 of 18 MB, then 393 of 21,263 small segments. */
    let early = at(393, 21_263, 12_000_000, 18_000_000);
    assert!(early < 0.03, "{early}");
    /* Files of a size: the bar follows them, by count or by bytes. */
    assert!((at(500, 1_000, 500_000, 1_000_000) - 0.5).abs() < 1e-9);
    /* A large transcript among small ones moves the bar further than one of them does. */
    assert!(at(1, 100, 50_000_000, 50_100_000) > 0.5);
    assert!(at(1, 100, 1_000, 50_100_000) < 0.01);
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

    assert_eq!(clear_move(&target, "another move", false), Cleared::Left);
    assert_eq!(
        clear_move(&dir.path().join("nowhere"), &plan.id, true),
        Cleared::Nothing
    );
    let held = lock(&target).unwrap();
    assert_eq!(clear_move(&target, &plan.id, true), Cleared::Left);
    drop(held);
    assert_eq!(tree(&target), copied);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let stuck = target.join(layout::ORPHANS).join(layout::FILES);
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o500)).unwrap();
        let cleared = clear_move(&target, &plan.id, true);
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(&cleared, Cleared::Partly(left) if left.iter().any(|left| left.starts_with(layout::ORPHANS))),
            "{cleared:?}"
        );
    }
    #[cfg(not(unix))]
    assert_eq!(clear_move(&target, &plan.id, true), Cleared::Removed);
    assert!(
        target.join("added by hand.txt").is_file(),
        "what isn't the move's stays"
    );
    assert!(!target.join(layout::FILES).exists());

    /* A folder the move made goes with what it holds. */
    let again = dir.path().join("again");
    let plan = archive.plan_move(&again).unwrap();
    drop(archive.start_move(&plan).unwrap());
    assert_eq!(clear_move(&again, &plan.id, true), Cleared::Removed);
    assert!(!again.exists());
}

/// The files an operating system leaves in a folder are each folder's own: one in the archive
/// and one in the folder moved to don't meet, the move keeps the folder's own, and a stopped
/// move leaves it there.
#[test]
fn system_files_on_both_sides_stay_each_sides_own() {
    let dir = tempfile::tempdir().unwrap();
    let (from, _) = archive_in(dir.path());
    let names = [".DS_Store", "Thumbs.db", "desktop.ini", "._archive.json"];
    for name in names {
        fs::write(from.join(name), "the archive's").unwrap();
    }
    let archive = opened(&from);
    let target = dir.path().join("chosen");
    fs::create_dir(&target).unwrap();
    for name in names {
        fs::write(target.join(name), "the folder's").unwrap();
    }
    let mut moving = moved_into(&archive, &target);
    moving.copy(&archive, &CancelToken::new(), &mut go).unwrap();
    assert_eq!(moving.abandon(), Cleared::Removed);
    for name in names {
        assert_eq!(
            fs::read_to_string(target.join(name)).unwrap(),
            "the folder's"
        );
    }
    let totals = archive.catalogue().totals().unwrap();
    drop(moved(&archive, &target));
    drop(archive);
    assert_whole(&target, totals);
    for name in names {
        assert_eq!(
            fs::read_to_string(target.join(name)).unwrap(),
            "the folder's"
        );
        assert_eq!(
            fs::read_to_string(from.join(name)).unwrap(),
            "the archive's"
        );
    }
}

/// A removal under way when the finish starts, waiting on a segment's store, is waited for
/// before the finish keeps segments from being stored: the two never wait on each other.
#[test]
fn a_removal_under_way_when_the_finish_starts_is_waited_for() {
    let dir = tempfile::tempdir().unwrap();
    let (from, location) = archive_in(dir.path());
    let archive = std::sync::Arc::new(opened(&from));
    for n in 3..30 {
        let bytes = format!("{{\"more\": {n}}}\n").repeat(50);
        store(
            &archive,
            &location,
            &format!("projects/{n}.jsonl"),
            &format!("s{n}"),
            bytes.as_bytes(),
        );
    }
    let target = dir.path().join("moved");
    let mut moving = moved_into(&archive, &target);
    moving.copy(&archive, &CancelToken::new(), &mut go).unwrap();
    /* A store holds `placing`, so the removal a delete starts waits on it. */
    let storing = archive
        .placing
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    for n in 3..30 {
        let id = SessionId {
            location: location.clone(),
            provider: "fixture".to_string(),
            key: format!("s{n}"),
        };
        archive.catalogue().trash(&id).unwrap();
        let shown = archive.catalogue().deletion(&id).unwrap();
        archive.delete_for_good(&shown).unwrap();
    }
    let (sent, finished) = std::sync::mpsc::channel();
    let finishing = {
        let archive = archive.clone();
        std::thread::spawn(move || {
            let moved = moving.finish(&archive, &CancelToken::new(), &mut go, |_| Ok(()));
            sent.send(moved.map(drop).map_err(|error| error.to_string()))
                .unwrap();
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(200));
    drop(storing);
    let finished = finished
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the finish and the removal waited on each other");
    finishing.join().unwrap();
    assert_eq!(finished, Ok(()));
    let totals = archive.catalogue().totals().unwrap();
    drop(archive);
    assert_whole(&target, totals);
}

/// A move cut short before its marker was written left its lock and an empty `tmp/` at most:
/// the next start takes those away, and the folder too when the move made it, and says that
/// nothing had been copied.
#[test]
fn a_move_cut_short_before_its_marker_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    for made in [true, false] {
        let target = dir.path().join(format!("made {made}"));
        fs::create_dir(&target).unwrap();
        if !made {
            fs::write(target.join(".DS_Store"), "Finder's").unwrap();
        }
        drop(lock(&target).unwrap());
        fs::create_dir(target.join(layout::TMP)).unwrap();
        assert_eq!(clear_move(&target, "a move", made), Cleared::Nothing);
        assert_eq!(target.exists(), !made);
        if !made {
            assert_eq!(
                fs::read_dir(&target).unwrap().count(),
                1,
                "Finder's file stays"
            );
        }
    }
}

/// What a move needs counts the search index the new archive makes again.
#[test]
fn the_room_a_move_needs_counts_the_search_index() {
    let dir = tempfile::tempdir().unwrap();
    let (from, _) = archive_in(dir.path());
    let archive = opened(&from);
    let target = dir.path().join("moved");
    let before = archive.plan_move(&target).unwrap().needed;
    fs::write(
        from.join(format!("{SEARCH_DATABASE}-wal")),
        vec![0; 1 << 20],
    )
    .unwrap();
    assert!(archive.plan_move(&target).unwrap().needed >= before + (1 << 20));
}

/// Which cloud service keeps a folder in step, from its folders' names; a local folder is not
/// on the network.
#[test]
fn a_synced_folder_is_named_and_a_local_one_is_not_remote() {
    let cases = [
        (
            "/Users/user/Library/CloudStorage/OneDrive-Personal/scrollback",
            Some("OneDrive"),
        ),
        (
            "/Users/user/Library/CloudStorage/Dropbox/scrollback",
            Some("Dropbox"),
        ),
        ("/Users/user/Dropbox (Personal)/scrollback", Some("Dropbox")),
        (
            "/Users/user/Library/Mobile Documents/com~apple~CloudDocs/scrollback",
            Some("iCloud Drive"),
        ),
        (
            "/Users/user/Library/CloudStorage/GoogleDrive-user/My Drive/scrollback",
            Some("Google Drive"),
        ),
        ("/Users/user/code/scrollback", None),
        ("/Volumes/Backup/scrollback", None),
    ];
    for (path, service) in cases {
        assert_eq!(synced_by(Path::new(path)), service, "{path}");
    }
    let dir = tempfile::tempdir().unwrap();
    assert!(!on_network(&dir.path().join("not there yet")));
}
