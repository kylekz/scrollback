//! What Settings › Archive asks of the core (DESIGN §4.13): the archive's own settings in the
//! catalogue's `kv`, the last verify kept there, the archive's size, fetching a damaged item
//! again from its location (PLAN §4.4), allowing a session deleted for good to be archived
//! again and undoing that (PLAN §4.5), and what a folder holds as the place for an archive.

mod common;

use std::fs;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::{
    Archive, ArchiveError, DamageKind, DamageRecord, Holds, Problem, VerifyProgress, id_in,
    open_or_make, what_is_in,
};
use sb_core::catalogue::{CatalogueError, Reason};
use sb_core::sync::{SyncError, refetch};
use sb_test_support::{Agent, FileAgent};

use common::{Bench, for_both_agents};

/// A bench with one session synced, and its transcript's key.
fn synced<A: Agent + Default>() -> (Bench<A>, String) {
    let mut agent = A::default();
    let key = agent.add_session(20);
    let bench = Bench::new(agent);
    bench.sync();
    (bench, key)
}

/// Overwrites the middle of the item `key`'s first segment.
fn damage<A: Agent>(bench: &Bench<A>, key: &str) {
    let segment = bench.segment_files_of(key).remove(0);
    let mut bytes = fs::read(&segment).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    fs::write(&segment, bytes).unwrap();
}

fn fetch_again<A: Agent>(bench: &Bench<A>, key: &str) -> Result<u32, SyncError> {
    refetch(
        bench.archive(),
        &bench.location,
        bench.agent.provider().id().as_str(),
        key,
        &bench.agent.transport(),
        &[bench.agent.provider()],
        &CancelToken::new(),
    )
}

#[test]
fn an_archive_setting_is_kept_in_the_catalogue_and_survives_a_restart() {
    let (mut bench, _) = synced::<FileAgent>();
    let catalogue = bench.archive().catalogue();
    assert_eq!(catalogue.setting("sync.stale_after_days").unwrap(), None);
    catalogue
        .set_setting("sync.stale_after_days", "14")
        .unwrap();
    catalogue
        .set_setting("sync.stale_after_days", "21")
        .unwrap();
    bench.reopen();
    assert_eq!(
        bench
            .archive()
            .catalogue()
            .setting("sync.stale_after_days")
            .unwrap()
            .as_deref(),
        Some("21")
    );
}

#[test]
fn a_verify_that_runs_to_its_end_is_kept_with_what_it_found() {
    let (mut bench, key) = synced::<FileAgent>();
    assert_eq!(bench.archive().catalogue().last_verify().unwrap(), None);
    damage(&bench, &key);

    let mut told = Vec::new();
    let report = bench
        .archive()
        .verify_reporting(&CancelToken::new(), &mut |progress| told.push(progress))
        .unwrap();
    let segments = report.segments;
    assert_eq!(
        told.first(),
        Some(&VerifyProgress {
            segments_done: 0,
            segments
        })
    );
    assert_eq!(
        told.last(),
        Some(&VerifyProgress {
            segments_done: segments,
            segments
        })
    );

    bench.reopen();
    let last = bench.archive().catalogue().last_verify().unwrap().unwrap();
    assert_eq!(
        (last.versions, last.segments, last.unread),
        (report.versions, segments, 0)
    );
    assert_eq!(last.damaged_total, 1);
    let kept: Vec<DamageRecord> = report.damaged.iter().map(DamageRecord::from).collect();
    assert_eq!(last.damaged, kept);
    let damage = &last.damaged[0];
    assert!(matches!(
        damage.kind,
        DamageKind::SegmentChanged | DamageKind::SegmentUnreadable
    ));
    /* The file the problem is in, as the archive lays it out (PLAN §4.1). */
    assert!(bench.dir().join(&damage.file).is_file(), "{}", damage.file);
    assert!(damage.file.starts_with("files/"));
    assert!(damage.file.ends_with("v0001/0000000000000000.zst"));
}

#[test]
fn a_last_verify_kept_by_another_version_reads_as_none_and_a_stop_at_the_end_keeps_nothing() {
    let (bench, _) = synced::<FileAgent>();
    let catalogue = bench.archive().catalogue();
    bench.archive().verify(&CancelToken::new()).unwrap();
    let kept = catalogue.setting("verify.last").unwrap().unwrap();
    assert!(kept.contains(r#""v":1"#), "{kept}");
    catalogue
        .set_setting("verify.last", &kept.replace(r#""v":1"#, r#""v":2"#))
        .unwrap();
    assert_eq!(catalogue.last_verify().unwrap(), None);

    /* Stopped as it read its last version: nothing kept. */
    let cancel = CancelToken::new();
    let stopped = bench.archive().verify_reporting(&cancel, &mut |progress| {
        if progress.segments_done == progress.segments && progress.segments > 0 {
            cancel.cancel();
        }
    });
    assert!(matches!(stopped, Err(ArchiveError::Cancelled)));
    assert_eq!(catalogue.last_verify().unwrap(), None);
}

#[test]
fn a_cancelled_verify_leaves_the_last_one_as_it_was() {
    let (bench, key) = synced::<FileAgent>();
    bench.archive().verify(&CancelToken::new()).unwrap();
    let before = bench.archive().catalogue().last_verify().unwrap();
    damage(&bench, &key);
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(matches!(
        bench.archive().verify(&cancel),
        Err(ArchiveError::Cancelled)
    ));
    assert_eq!(bench.archive().catalogue().last_verify().unwrap(), before);
}

#[test]
fn a_missing_segment_names_its_file_and_a_version_problem_its_folder() {
    let (bench, key) = synced::<FileAgent>();
    let segment = bench.segment_files_of(&key).remove(0);
    fs::remove_file(&segment).unwrap();
    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    let damage = &report.damaged[0];
    assert_eq!(damage.problems, [Problem::SegmentMissing { offset: 0 }]);
    assert_eq!(bench.dir().join(damage.file()), segment);
    let whole = sb_core::archive::Damage {
        problems: vec![Problem::VersionChanged],
        ..damage.clone()
    };
    assert_eq!(bench.dir().join(whole.file()), segment.parent().unwrap());
}

fn a_damaged_item_is_fetched_again_as_a_new_version<A: Agent + Default>() {
    let (bench, key) = synced::<A>();
    let original = bench.agent.content(&key);
    damage(&bench, &key);
    bench.archive().verify(&CancelToken::new()).unwrap();
    assert!(bench.versions(&key)[0].damaged);

    assert_eq!(fetch_again(&bench, &key).unwrap(), 2);
    let versions = bench.versions(&key);
    assert_eq!(versions.len(), 2);
    /* The damaged one is kept, and marked; the new one is the source's bytes. */
    assert!(versions[0].damaged);
    assert_eq!(
        (versions[1].reason, versions[1].damaged),
        (Reason::Refetched, false)
    );
    assert_eq!(bench.read(&versions[1]), original);
    /* Its session reads the new version. */
    assert_eq!(bench.session(&key).reading_version, Some(2));
    /* Nothing is left to fetch again. */
    assert!(matches!(
        fetch_again(&bench, &key),
        Err(SyncError::NotDamaged(_))
    ));
}

fn an_item_its_location_no_longer_has_is_not_fetched_again<A: Agent + Default>() {
    let (bench, key) = synced::<A>();
    damage(&bench, &key);
    bench.archive().verify(&CancelToken::new()).unwrap();
    let mut bench = bench;
    bench.agent.delete(&key);
    assert!(matches!(
        fetch_again(&bench, &key),
        Err(SyncError::NotAtLocation(_))
    ));
    assert_eq!(bench.versions(&key).len(), 1);
}

for_both_agents!(
    a_damaged_item_is_fetched_again_as_a_new_version,
    an_item_its_location_no_longer_has_is_not_fetched_again,
);

#[test]
fn an_item_that_is_not_damaged_is_not_fetched_again() {
    let (bench, key) = synced::<FileAgent>();
    assert!(matches!(
        fetch_again(&bench, &key),
        Err(SyncError::NotDamaged(_))
    ));
    assert!(matches!(
        fetch_again(&bench, "no such item"),
        Err(SyncError::NotDamaged(_))
    ));
    assert_eq!(bench.versions(&key).len(), 1);
}

#[test]
fn nothing_is_fetched_again_while_its_location_syncs() {
    let (bench, key) = synced::<FileAgent>();
    damage(&bench, &key);
    bench.archive().verify(&CancelToken::new()).unwrap();
    let mut refused = None;
    bench
        .try_sync(&CancelToken::new(), &mut |_| {
            refused.get_or_insert_with(|| fetch_again(&bench, &key));
        })
        .unwrap();
    assert!(matches!(refused, Some(Err(SyncError::Busy(_)))));
}

#[test]
fn allowing_a_session_again_can_be_undone_until_a_sync_archives_it() {
    let mut agent = FileAgent::default();
    let keys = [agent.add_session(3), agent.add_session(3)];
    let bench = Bench::new(agent);
    bench.sync();
    let id = bench.session_id(&keys[0]);
    let catalogue = bench.archive().catalogue();
    catalogue.trash(&id).unwrap();
    bench.delete_for_good(&id).unwrap();
    let tombstones = catalogue.tombstones().unwrap();

    /* Allowed, then undone: the tombstone is as it was, and the next sync still skips it. */
    let allowed = catalogue.remove_tombstone(&id).unwrap();
    assert_eq!(allowed.tombstone, tombstones[0]);
    assert_eq!(catalogue.tombstones().unwrap(), []);
    bench.archive().restore_tombstone(&allowed).unwrap();
    assert_eq!(catalogue.tombstones().unwrap(), tombstones);
    let synced = bench.sync();
    assert_eq!((synced.deleted, synced.new_items), (1, 0));

    /* Allowed, then synced: not put back while the sync runs, which archives the session
    again; after it, the undo is refused. */
    let allowed = catalogue.remove_tombstone(&id).unwrap();
    let mut refused = None;
    bench
        .try_sync(&CancelToken::new(), &mut |_| {
            refused.get_or_insert_with(|| bench.archive().restore_tombstone(&allowed));
        })
        .unwrap();
    assert!(matches!(refused, Some(Err(ArchiveError::Syncing(_)))));
    assert!(matches!(
        bench.archive().restore_tombstone(&allowed),
        Err(ArchiveError::Catalogue(CatalogueError::ArchivedAgain(_)))
    ));
    assert_eq!(catalogue.tombstones().unwrap(), []);
    bench.assert_mirrors(&keys);
}

#[test]
fn the_archive_measures_what_it_holds() {
    let mut agent = FileAgent::default();
    let keys = [agent.add_session(20), agent.add_session(5)];
    let mut bench = Bench::new(agent);
    bench.sync();
    bench.agent.rewrite(&keys[0]);
    bench.sync();

    let stats = bench.archive().stats().unwrap();
    assert_eq!(
        (
            stats.totals.sessions,
            stats.totals.items,
            stats.totals.versions
        ),
        (2, 2, 3)
    );
    let original: u64 = keys
        .iter()
        .flat_map(|key| bench.versions(key))
        .map(|version| version.length)
        .sum();
    assert_eq!(stats.totals.bytes, original);
    let stored: u64 = bench
        .segment_files()
        .iter()
        .map(|file| fs::metadata(file).unwrap().len())
        .sum();
    assert!(stats.stored >= stored, "{} < {stored}", stats.stored);
    assert!(stats.databases > 0);
    assert!(stats.on_disk >= stats.stored + stats.databases + stats.backups_bytes);
}

#[test]
fn a_folder_opens_its_archive_gets_a_new_one_when_empty_and_refuses_anything_else() {
    let root = tempfile::tempdir().unwrap();

    /* Not there yet, or empty but for what a file manager leaves: a new archive. */
    let missing = root.path().join("missing");
    assert_eq!(what_is_in(&missing).unwrap(), Holds::Nothing);
    let made = open_or_make(&missing).unwrap();
    let id = made.id().to_string();
    drop(made);
    let shown = root.path().join("shown");
    fs::create_dir(&shown).unwrap();
    fs::write(shown.join(".DS_Store"), b"x").unwrap();
    fs::write(shown.join("desktop.ini"), b"x").unwrap();
    fs::write(shown.join("._notes.txt"), b"x").unwrap();
    assert_eq!(what_is_in(&shown).unwrap(), Holds::Nothing);
    drop(open_or_make(&shown).unwrap());

    /* An archive opens as itself, and says who it is without being opened. */
    assert_eq!(what_is_in(&missing).unwrap(), Holds::Archive);
    assert_eq!(id_in(&missing).unwrap().as_deref(), Some(id.as_str()));
    assert_eq!(id_in(root.path()).unwrap(), None);
    assert_eq!(open_or_make(&missing).unwrap().id(), id);

    /* No archive is made inside another, its tmp/ included, nor around one. */
    for inside in [missing.join("tmp"), missing.join("files").join("new")] {
        assert_eq!(
            what_is_in(&inside).unwrap(),
            Holds::InsideAnArchive(missing.clone())
        );
        assert!(matches!(
            open_or_make(&inside),
            Err(ArchiveError::Nested { .. })
        ));
        assert!(matches!(
            Archive::create(&inside),
            Err(ArchiveError::Nested { .. })
        ));
    }
    let around = root.path().join("around");
    fs::create_dir(&around).unwrap();
    drop(Archive::create(&around.join("inner")).unwrap());
    assert!(matches!(
        Archive::create(&around),
        Err(ArchiveError::Nested { .. })
    ));
    assert!(!around.join("archive.json").exists());

    /* Anything else is refused, and nothing is made in it. */
    let documents = root.path().join("documents");
    fs::create_dir(&documents).unwrap();
    fs::write(documents.join("notes.txt"), b"mine").unwrap();
    assert_eq!(what_is_in(&documents).unwrap(), Holds::Other);
    assert!(matches!(
        open_or_make(&documents),
        Err(ArchiveError::NotAnArchive(_))
    ));
    let mut left: Vec<_> = fs::read_dir(&documents)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    left.sort();
    assert_eq!(left, ["notes.txt"]);
    assert!(Archive::open(&documents).is_err());
}
