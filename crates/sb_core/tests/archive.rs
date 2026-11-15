//! The archive on disk (PLAN §4, §12): opening and the lock, reading versions back, verify, and
//! what a start does with files it doesn't know.

mod common;

use std::fs;
use std::path::PathBuf;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::{Archive, ArchiveError, Problem};
use sb_core::catalogue::Reason;
use sb_test_support::{Agent, DbAgent, FileAgent};

use common::{Bench, files_under};

/// A synced bench of one session, and the session's key.
fn synced() -> (Bench<FileAgent>, String) {
    let mut agent = FileAgent::new();
    let key = agent.add_session(20);
    let bench = Bench::new(agent);
    bench.sync();
    (bench, key)
}

#[test]
fn a_folder_without_an_archive_is_not_opened_as_one() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        Archive::open(dir.path()),
        Err(ArchiveError::NotAnArchive(_))
    ));
    /* Nothing was created in its place. */
    assert_eq!(files_under(dir.path()), Vec::<PathBuf>::new());

    let id = Archive::create(dir.path()).unwrap().id().to_string();
    assert!(matches!(
        Archive::create(dir.path()),
        Err(ArchiveError::AlreadyAnArchive(_))
    ));
    assert_eq!(Archive::open(dir.path()).unwrap().id(), id);
}

#[test]
fn a_second_opener_is_refused_by_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let first = Archive::create(dir.path()).unwrap();
    assert!(matches!(
        Archive::open(dir.path()),
        Err(ArchiveError::Locked(_))
    ));
    drop(first);
    Archive::open(dir.path()).unwrap();
}

#[test]
fn an_archive_from_a_newer_build_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    drop(Archive::create(dir.path()).unwrap());
    let manifest = dir.path().join("archive.json");
    let newer = fs::read_to_string(&manifest)
        .unwrap()
        .replace("\"format\": 1", "\"format\": 2");
    fs::write(&manifest, newer).unwrap();
    assert!(matches!(
        Archive::open(dir.path()),
        Err(ArchiveError::NewerFormat { found: 2, .. })
    ));
}

#[test]
fn a_start_empties_tmp_and_keeps_what_the_catalogue_records() {
    let (mut bench, key) = synced();
    let recorded = bench.segment_files();
    let leftover = bench.dir().join("tmp/leftover.zst");
    fs::write(&leftover, "half a segment").unwrap();

    bench.reopen();
    assert!(!leftover.exists());
    assert_eq!(bench.segment_files(), recorded);
    assert_eq!(bench.orphans(), Vec::<PathBuf>::new());
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_start_sets_aside_what_the_catalogue_does_not_record() {
    let (mut bench, key) = synced();
    let segment = bench.segment_files().remove(0);
    let version_dir = segment.parent().unwrap();
    let stray_segment = version_dir.join("0000000000999999.zst");
    fs::write(&stray_segment, "not a recorded segment").unwrap();
    let stray_item = bench
        .dir()
        .join("files")
        .join(bench.location.as_str())
        .join(bench.agent.provider().id())
        .join("ff/ff000000000000000000000000000000");
    fs::create_dir_all(stray_item.join("v0001")).unwrap();
    fs::write(stray_item.join("source.json"), "{}").unwrap();
    fs::write(stray_item.join("v0001/0000000000000000.zst"), "stray").unwrap();

    bench.reopen();
    assert!(!stray_segment.exists() && !stray_item.exists());
    /* Set aside with their bytes, under the paths they had. */
    let orphans = bench.dir().join("orphans");
    let moved = |path: &PathBuf| orphans.join(path.strip_prefix(bench.dir()).unwrap());
    assert_eq!(
        fs::read(moved(&stray_segment)).unwrap(),
        b"not a recorded segment"
    );
    assert_eq!(
        fs::read(moved(&stray_item).join("v0001/0000000000000000.zst")).unwrap(),
        b"stray"
    );
    assert_eq!(bench.orphans().len(), 3);
    bench.assert_mirrors(&[key]);
}

#[test]
fn verify_finds_a_missing_segment() {
    let (bench, key) = synced();
    fs::remove_file(bench.segment_files().remove(0)).unwrap();

    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    assert_eq!((report.versions, report.segments), (1, 1));
    assert_eq!(report.damaged.len(), 1);
    let damage = &report.damaged[0];
    assert_eq!((damage.key.as_str(), damage.version), (key.as_str(), 1));
    assert_eq!(damage.problems, [Problem::SegmentMissing { offset: 0 }]);
    assert!(bench.versions(&key)[0].damaged);
}

#[test]
fn verify_finds_a_corrupted_segment() {
    let (bench, key) = synced();
    let segment = bench.segment_files().remove(0);
    let mut bytes = fs::read(&segment).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    fs::write(&segment, bytes).unwrap();

    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    assert_eq!(report.damaged.len(), 1);
    assert!(
        matches!(
            report.damaged[0].problems[..],
            [
                Problem::SegmentUnreadable { offset: 0, .. }
                    | Problem::SegmentChanged { offset: 0 }
            ]
        ),
        "{:?}",
        report.damaged[0].problems
    );
    assert!(bench.versions(&key)[0].damaged);
}

#[test]
fn verify_finds_a_segment_replaced_by_other_bytes() {
    let (bench, key) = synced();
    let segment = bench.segment_files().remove(0);
    /* A well-formed segment of the wrong content: only the catalogue's hash can tell. */
    fs::write(
        &segment,
        zstd::encode_all(&b"other bytes\n"[..], 3).unwrap(),
    )
    .unwrap();

    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    assert_eq!(
        report.damaged[0].problems,
        [Problem::SegmentChanged { offset: 0 }]
    );
    assert!(bench.versions(&key)[0].damaged);
}

#[test]
fn a_damaged_item_still_at_its_source_is_fetched_again_as_a_new_version() {
    let (bench, key) = synced();
    fs::remove_file(bench.segment_files().remove(0)).unwrap();
    assert_eq!(
        bench
            .archive()
            .verify(&CancelToken::new())
            .unwrap()
            .damaged
            .len(),
        1
    );

    let report = bench.sync();
    assert_eq!((report.new_versions, report.held_back), (1, 0));
    let versions = bench.versions(&key);
    assert_eq!(versions[1].reason, Reason::Refetched);
    assert!(bench.read(&versions[1]) == bench.agent.content(&key));
    /* Verify repairs nothing: the first version is still reported, the new one is whole. */
    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    assert_eq!(report.damaged.len(), 1);
    assert_eq!(report.damaged[0].version, 1);
    assert_eq!(bench.sync().unchanged, 1);
}

#[test]
fn a_version_found_damaged_while_it_is_extended_is_not_extended() {
    let mut agent = DbAgent::new();
    let key = agent.add_session(20);
    let mut bench = Bench::new(agent);
    bench.sync();
    fs::write(
        bench.segment_files().remove(0),
        zstd::encode_all(&b"other bytes\n"[..], 3).unwrap(),
    )
    .unwrap();
    bench.agent.append(&key, 5);

    /* The pass that would append reads the version back first and finds it changed. */
    let mut passes = bench.try_sync(&CancelToken::new(), &mut |_| {}).unwrap();
    let report = passes.remove(0);
    assert_eq!((report.appended, report.failed.len()), (0, 1));
    assert_eq!(report.failed[0].key, key);
    assert!(bench.versions(&key)[0].damaged);
    assert_eq!(bench.segment_files().len(), 1);

    /* The next pass takes the whole source as a new version. */
    assert_eq!(bench.sync().new_versions, 1);
    let versions = bench.versions(&key);
    assert_eq!(versions[1].reason, Reason::Refetched);
    assert!(bench.read(&versions[1]) == bench.agent.content(&key));
}

#[test]
fn verify_passes_once_a_damaged_segment_is_put_back() {
    let (bench, key) = synced();
    let segment = bench.segment_files().remove(0);
    let bytes = fs::read(&segment).unwrap();
    fs::remove_file(&segment).unwrap();
    bench.archive().verify(&CancelToken::new()).unwrap();
    assert!(bench.versions(&key)[0].damaged);

    fs::write(&segment, bytes).unwrap();
    bench.assert_mirrors(std::slice::from_ref(&key));
    assert!(!bench.versions(&key)[0].damaged);
}

#[test]
fn verify_can_be_cancelled() {
    let (bench, _) = synced();
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(matches!(
        bench.archive().verify(&cancel),
        Err(ArchiveError::Cancelled)
    ));
}
