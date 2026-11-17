//! Moving a synced archive into another folder (PLAN §4.4), for both kinds of agent: the copy
//! verifies and holds what the archive does, a sync that runs while its files are copied is
//! carried, and the old folder is as it was. The steps one by one, a crash at each, a stop and
//! the refusals are `sb_core::archive::relocate`'s own tests.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::{Archive, MoveProgress};
use sb_test_support::Agent;

use common::{Bench, for_both_agents};

for_both_agents!(
    a_synced_archive_moves_whole_and_the_old_folder_stays_as_it_was,
    a_sync_while_the_files_are_copied_is_carried,
);

/**
 * Every file under `dir` with its length, time and bytes, by its path relative to `dir`, but
 * the databases' shared memory, which every reader of an open database writes its read marks
 * in, and which holds nothing that is kept, and the archive's `lock`, which a move never copies
 * and which the open archive holds: on Windows a byte-range lock that no other handle can read
 * through (error 33), where elsewhere the lock is advisory.
 */
fn tree(dir: &Path) -> BTreeMap<PathBuf, (u64, Option<SystemTime>, Vec<u8>)> {
    common::files_under(dir)
        .into_iter()
        .filter(|path| !path.to_string_lossy().ends_with("-shm"))
        .filter(|path| path.strip_prefix(dir).ok() != Some(Path::new("lock")))
        .map(|path| {
            let metadata = fs::metadata(&path).unwrap();
            let bytes = fs::read(&path).unwrap();
            (
                path.strip_prefix(dir).unwrap().to_path_buf(),
                (metadata.len(), metadata.modified().ok(), bytes),
            )
        })
        .collect()
}

/// Moves the bench's archive into `target`, running `meanwhile` once the first file is
/// copied; the archive moved to, open.
fn moved<A: Agent>(bench: &Bench<A>, target: &Path, meanwhile: impl FnMut()) -> Archive {
    let archive = bench.archive();
    let plan = archive.plan_move(target).unwrap();
    let mut moving = archive.start_move(&plan).unwrap();
    let mut once = Some(meanwhile);
    moving
        .copy(
            archive,
            &CancelToken::new(),
            &mut |progress: &MoveProgress| {
                if progress.files_done == 1
                    && let Some(mut meanwhile) = once.take()
                {
                    meanwhile();
                }
            },
        )
        .unwrap();
    let moved = moving
        .finish(archive, &CancelToken::new(), &mut |_| {}, |_| Ok(()))
        .unwrap();
    moved.wait_for_removals();
    moved
}

/// `moved` verifies, and holds what the bench's archive does; its index makes its search text
/// again, which isn't copied.
fn assert_same<A: Agent>(bench: &Bench<A>, moved: &Archive) {
    let report = moved.verify(&CancelToken::new()).unwrap();
    assert!(report.damaged.is_empty(), "{:?}", report.damaged);
    assert!(report.unread.is_empty(), "{:?}", report.unread);
    assert!(report.uncatalogued.is_empty(), "{:?}", report.uncatalogued);
    assert!(report.removing.is_empty(), "{:?}", report.removing);
    let (ours, theirs) = (moved.catalogue(), bench.archive().catalogue());
    assert_eq!(ours.totals().unwrap(), theirs.totals().unwrap());
    assert_eq!(ours.locations().unwrap(), theirs.locations().unwrap());
    assert_eq!(
        moved.unsearchable().unwrap(),
        ours.totals().unwrap().sessions
    );
    sb_core::sync::index(
        moved,
        None,
        &[bench.agent.provider()],
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(moved.unsearchable().unwrap(), 0);
}

fn a_synced_archive_moves_whole_and_the_old_folder_stays_as_it_was<A: Agent + Default>() {
    let mut agent = A::default();
    for records in [5, 20, 40] {
        agent.add_session(records);
    }
    let bench = Bench::new(agent);
    bench.sync();
    let before = tree(bench.dir());
    let elsewhere = tempfile::tempdir().unwrap();
    let moved = moved(&bench, &elsewhere.path().join("archive"), || {});
    assert_same(&bench, &moved);
    assert_eq!(tree(bench.dir()), before);
}

fn a_sync_while_the_files_are_copied_is_carried<A: Agent + Default>() {
    let mut agent = A::default();
    let grown = agent.add_session(10);
    agent.add_session(20);
    let mut bench = Bench::new(agent);
    bench.sync();
    /* What the next pass finds: an append and a new session. */
    bench.agent.append(&grown, 15);
    let added = bench.agent.add_session(30);
    let elsewhere = tempfile::tempdir().unwrap();
    let moved = moved(&bench, &elsewhere.path().join("archive"), || {
        bench.sync();
    });
    assert_same(&bench, &moved);
    bench.assert_mirrors(&[grown, added]);
}
