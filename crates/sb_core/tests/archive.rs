//! The archive on disk (PLAN §4, §12): opening and the lock, reading versions back, verify, and
//! what a start does with files it doesn't know.

mod common;

use std::fs;
use std::io::Read as _;
use std::path::PathBuf;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::{Archive, ArchiveError, Problem};
use sb_core::catalogue::Reason;
use sb_test_support::{Agent, DbAgent, FileAgent, edit_catalogue};

use common::{Bench, files_under};

/// A synced bench of one session, and the session's key.
fn synced() -> (Bench<FileAgent>, String) {
    let mut agent = FileAgent::new();
    let key = agent.add_session(20);
    let bench = Bench::new(agent);
    bench.sync();
    (bench, key)
}

/// A well-formed segment of bytes that were never archived: only the catalogue can tell.
fn other_segment() -> Vec<u8> {
    zstd::encode_all(&b"other bytes\n"[..], 3).unwrap()
}

#[test]
fn a_folder_without_an_archive_is_not_opened_as_one() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        Archive::open(dir.path()),
        Err(ArchiveError::NotAnArchive(_))
    ));
    /* Nothing was created in its place. */
    assert!(files_under(dir.path()).is_empty());

    let id = Archive::create(dir.path()).unwrap().id().to_string();
    assert!(matches!(
        Archive::create(dir.path()),
        Err(ArchiveError::AlreadyAnArchive(_))
    ));
    assert_eq!(Archive::open(dir.path()).unwrap().id(), id);
}

#[test]
fn an_archive_named_by_a_relative_path_opens_again() {
    /* A folder under the working directory, named from there. */
    let here = std::env::current_dir().unwrap();
    let dir = tempfile::tempdir_in(&here).unwrap();
    let relative = dir.path().strip_prefix(&here).unwrap().join("archive");
    assert!(relative.is_relative());

    let created = Archive::create(&relative).unwrap();
    assert!(created.dir().is_absolute());
    let id = created.id().to_string();
    drop(created);
    assert_eq!(Archive::open(&relative).unwrap().id(), id);
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
fn an_archive_that_lost_its_catalogue_is_not_opened_with_an_empty_one() {
    let (mut bench, key) = synced();
    let files = files_under(&bench.dir().join("files"));
    bench.close();
    let database = bench.dir().join("scrollback.db");
    let kept = fs::read(&database).unwrap();
    fs::remove_file(&database).unwrap();

    match Archive::open(bench.dir()) {
        Err(ArchiveError::CatalogueMissing { newest_backup, .. }) => {
            assert_eq!(newest_backup, None);
        }
        other => panic!("{other:?}"),
    }
    /* Nothing was created, and nothing archived was moved. */
    assert!(!database.exists());
    assert_eq!(files_under(&bench.dir().join("files")), files);
    assert!(bench.orphans().is_empty());

    fs::write(&database, kept).unwrap();
    bench.reopen();
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_catalogue_that_is_not_the_archives_is_not_opened() {
    let (mut bench, key) = synced();
    let files = files_under(&bench.dir().join("files"));
    bench.close();
    let database = bench.dir().join("scrollback.db");
    let kept = fs::read(&database).unwrap();
    /* The catalogue of another archive, in the place of this one's. */
    let other = tempfile::tempdir().unwrap();
    drop(Archive::create(other.path()).unwrap());
    fs::copy(other.path().join("scrollback.db"), &database).unwrap();

    assert!(matches!(
        Archive::open(bench.dir()),
        Err(ArchiveError::AnothersCatalogue {
            catalogue: Some(_),
            ..
        })
    ));
    /* Nothing archived was moved, and nothing was set aside. */
    assert_eq!(files_under(&bench.dir().join("files")), files);
    assert!(bench.orphans().is_empty());

    fs::write(&database, kept).unwrap();
    bench.reopen();
    bench.assert_mirrors(&[key]);
}

#[test]
fn an_archive_is_not_created_over_what_is_left_of_one() {
    let (mut bench, _) = synced();
    let before = files_under(&bench.dir().join("files"));
    bench.close();
    fs::remove_file(bench.dir().join("archive.json")).unwrap();
    assert!(matches!(
        Archive::create(bench.dir()),
        Err(ArchiveError::HoldsAnArchivesFiles(_))
    ));

    /* Nor over a catalogue alone that has been used. */
    fs::rename(bench.dir().join("files"), bench.dir().join("files.away")).unwrap();
    assert!(matches!(
        Archive::create(bench.dir()),
        Err(ArchiveError::HoldsAnArchivesFiles(_))
    ));
    fs::rename(bench.dir().join("files.away"), bench.dir().join("files")).unwrap();

    /* Nor over files alone. */
    for file in ["scrollback.db", "scrollback.db-wal", "scrollback.db-shm"] {
        let _ = fs::remove_file(bench.dir().join(file));
    }
    assert!(matches!(
        Archive::create(bench.dir()),
        Err(ArchiveError::HoldsAnArchivesFiles(_))
    ));
    assert!(!bench.dir().join("scrollback.db").exists());
    assert!(!bench.dir().join("archive.json").exists());
    assert_eq!(files_under(&bench.dir().join("files")), before);
}

#[test]
fn a_create_that_was_cut_short_can_be_done_again() {
    let dir = tempfile::tempdir().unwrap();
    let id = Archive::create(dir.path()).unwrap().id().to_string();
    /* As a create that stopped before its last step leaves the folder. */
    fs::remove_file(dir.path().join("archive.json")).unwrap();

    let again = Archive::create(dir.path()).unwrap();
    assert_eq!(again.id(), id);
    drop(again);
    assert_eq!(Archive::open(dir.path()).unwrap().id(), id);
}

#[test]
fn an_archive_whose_state_cannot_be_written_still_opens() {
    let (mut bench, key) = synced();
    bench.close();
    /* As after a sync that died storing; and nothing can take the file's place, as when the
    disk is full. */
    let state = bench.dir().join("state.json");
    fs::remove_file(&state).unwrap();
    fs::create_dir(&state).unwrap();
    fs::write(state.join("in the way"), "").unwrap();

    bench.reopen();
    let version = bench.versions(&key).remove(0);
    assert!(bench.read(&version) == bench.agent.content(&key));
    bench.assert_verifies();
}

#[test]
fn a_catalogue_that_records_nothing_does_not_get_the_files_set_aside() {
    let (mut bench, _) = synced();
    let files = files_under(&bench.dir().join("files"));
    bench.close();
    /* This archive's catalogue as it was before its first sync, as an early backup has it,
    and a start that looks. */
    edit_catalogue(
        bench.dir(),
        "DELETE FROM segment; DELETE FROM version; DELETE FROM item;",
    );

    bench.reopen();
    assert_eq!(files_under(&bench.dir().join("files")), files);
    assert!(bench.orphans().is_empty());
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
    assert!(bench.orphans().is_empty());
    bench.assert_mirrors(&[key]);
}

/// Puts things the catalogue doesn't record under the bench's `files/`, of every sort, and
/// returns each as the path verify reports it by, relative to the archive, with the bytes of
/// a file in it.
fn strays(bench: &Bench<FileAgent>) -> Vec<(PathBuf, PathBuf, &'static str)> {
    let dir = bench.dir();
    let segment = bench.segment_files().remove(0);
    let version = segment.parent().unwrap().strip_prefix(dir).unwrap();
    let item = version.parent().unwrap();
    let provider = item.parent().unwrap().parent().unwrap();
    let strays = vec![
        /* A segment-like file the catalogue has no row for. */
        (version.join("0000000000999999.zst"), PathBuf::new(), "a"),
        /* A recorded segment and a recorded version under names the layout doesn't give. */
        (version.join("0.zst"), PathBuf::new(), "b"),
        (item.join("v1"), PathBuf::from("0000000000000000.zst"), "c"),
        /* Files and folders where the layout has none. */
        (item.join("stray.txt"), PathBuf::new(), "d"),
        (provider.join("stray"), PathBuf::from("deep/file"), "e"),
        (PathBuf::from("files/stray.txt"), PathBuf::new(), "f"),
        /* An item the catalogue doesn't have, the only one in its folder. */
        (
            provider.join("ff"),
            PathBuf::from("ff000000000000000000000000000000/v0001/0000000000000000.zst"),
            "g",
        ),
    ];
    for (reported, inside, bytes) in &strays {
        let mut file = dir.join(reported);
        file.extend(inside);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, bytes).unwrap();
    }
    strays
}

/// Asserts the bench's strays were set aside with their bytes, under the paths they had.
fn assert_set_aside(bench: &Bench<FileAgent>, strays: &[(PathBuf, PathBuf, &'static str)]) {
    for (reported, inside, bytes) in strays {
        assert!(!bench.dir().join(reported).exists(), "{reported:?}");
        let mut moved = bench.dir().join("orphans").join(reported);
        moved.extend(inside);
        assert_eq!(fs::read_to_string(moved).unwrap(), *bytes, "{reported:?}");
    }
    assert_eq!(bench.orphans().len(), strays.len());
}

#[test]
fn a_start_after_a_sync_that_did_not_end_sets_aside_what_the_catalogue_does_not_record() {
    let (mut bench, key) = synced();
    let strays = strays(&bench);
    /* As a sync that was storing when the app died leaves it. */
    let state = bench.dir().join("state.json");
    let syncing = fs::read_to_string(&state)
        .unwrap()
        .replace("\"syncing\":false", "\"syncing\":true");
    fs::write(&state, syncing).unwrap();

    bench.reopen();
    assert_set_aside(&bench, &strays);
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_start_with_no_word_of_the_last_one_sets_aside_what_the_catalogue_does_not_record() {
    let (mut bench, key) = synced();
    let strays = strays(&bench);
    fs::remove_file(bench.dir().join("state.json")).unwrap();

    bench.reopen();
    assert_set_aside(&bench, &strays);
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_start_after_a_sync_that_ended_well_does_not_look_and_verify_reports_instead() {
    let (mut bench, key) = synced();
    let strays = strays(&bench);

    bench.reopen();
    assert!(
        strays
            .iter()
            .all(|(path, ..)| bench.dir().join(path).exists())
    );
    assert!(bench.orphans().is_empty());
    let mut uncatalogued = bench
        .archive()
        .verify(&CancelToken::new())
        .unwrap()
        .uncatalogued;
    uncatalogued.sort();
    let mut expected: Vec<_> = strays.iter().map(|(path, ..)| path.clone()).collect();
    expected.sort();
    assert_eq!(uncatalogued, expected);

    /* What verify found is a reason for the next start to look. */
    bench.reopen();
    assert_set_aside(&bench, &strays);
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_catalogue_restored_from_before_the_last_sync_costs_no_segment() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(20);
    let mut bench = Bench::new(agent);
    bench.sync();
    bench.close();
    let database = bench.dir().join("scrollback.db");
    let backup = fs::read(&database).unwrap();
    bench.reopen();
    bench.agent.append(&key, 5);
    bench.sync();
    let newer = bench.segment_files();
    let stored: Vec<_> = newer.iter().map(|file| fs::read(file).unwrap()).collect();
    assert_eq!(newer.len(), 2);

    /* The catalogue goes back to its backup, which knows one segment. */
    bench.close();
    fs::write(&database, backup).unwrap();
    bench.reopen();
    assert_eq!(bench.segment_files().len(), 1);
    let orphans = bench.orphans();
    assert_eq!(orphans.len(), 1);
    assert!(stored.contains(&fs::read(&orphans[0]).unwrap()));

    /* And the source still has what that segment held. */
    assert_eq!(bench.sync().appended, 1);
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_version_whose_segments_are_more_than_it_records_is_not_extended() {
    let (mut bench, key) = two_segments();
    let version = bench.versions(&key).remove(0);
    let segments = bench.segments(&version);
    let files = bench.segment_files();
    let stored: Vec<_> = files.iter().map(|file| fs::read(file).unwrap()).collect();

    /* The version as it was after its first segment, with the second segment's row still
    there: the next pass would mean to store at an offset where a recorded segment is. */
    edit_catalogue(
        bench.dir(),
        &format!(
            "UPDATE version SET length = {}, hash = '{}' WHERE id = {};
             UPDATE item SET seen_stamp = 'earlier';",
            segments[0].length,
            segments[0].hash.to_hex(),
            version.id
        ),
    );
    let report = bench.pass_with(bench.agent.provider()).unwrap();
    assert_eq!((report.appended, report.failed.len()), (0, 1));
    assert!(bench.versions(&key)[0].damaged);
    /* Both segments are where they were, byte for byte, and nothing was set aside. */
    assert_eq!(bench.segment_files(), files);
    for (file, stored) in files.iter().zip(&stored) {
        assert!(&fs::read(file).unwrap() == stored);
    }
    assert!(bench.orphans().is_empty());
    assert!(files_under(&bench.dir().join("tmp")).is_empty());

    /* The source is then fetched whole, as a new version. */
    bench.agent.append(&key, 1);
    assert_eq!(bench.sync().new_versions, 1);
}

#[test]
fn reading_a_version_fails_on_bytes_that_were_not_archived() {
    let (bench, key) = synced();
    let version = bench.versions(&key).remove(0);
    fs::write(bench.segment_files().remove(0), other_segment()).unwrap();

    let mut content = Vec::new();
    let error = bench
        .archive()
        .read_version(version.id)
        .unwrap()
        .read_to_end(&mut content)
        .unwrap_err();
    assert_eq!(
        Problem::of(&error),
        Some(&Problem::SegmentChanged { offset: 0 })
    );
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
fn verify_finds_a_truncated_segment() {
    let (bench, key) = synced();
    let segment = bench.segment_files().remove(0);
    let bytes = fs::read(&segment).unwrap();
    fs::write(&segment, &bytes[..bytes.len() / 2]).unwrap();

    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    assert!(
        matches!(
            report.damaged[0].problems[..],
            [Problem::SegmentUnreadable { offset: 0, .. }]
        ),
        "{:?}",
        report.damaged[0].problems
    );
    assert!(report.unread.is_empty());
    assert!(bench.versions(&key)[0].damaged);
}

#[test]
fn verify_finds_a_segment_replaced_by_other_bytes() {
    let (bench, key) = synced();
    fs::write(bench.segment_files().remove(0), other_segment()).unwrap();

    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    assert_eq!(
        report.damaged[0].problems,
        [Problem::SegmentChanged { offset: 0 }]
    );
    assert!(bench.versions(&key)[0].damaged);
}

/// A bench whose one session was synced twice, so its version has two segments.
fn two_segments() -> (Bench<FileAgent>, String) {
    let mut agent = FileAgent::new();
    let key = agent.add_session(20);
    let mut bench = Bench::new(agent);
    bench.sync();
    bench.agent.append(&key, 5);
    bench.sync();
    (bench, key)
}

#[test]
fn verify_finds_every_problem_of_a_version() {
    let (bench, key) = two_segments();
    for file in bench.segment_files() {
        fs::remove_file(file).unwrap();
    }
    let second = bench.segments(&bench.versions(&key)[0])[1].offset;

    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    assert_eq!(
        report.damaged[0].problems,
        [
            Problem::SegmentMissing { offset: 0 },
            Problem::SegmentMissing { offset: second }
        ]
    );
}

#[test]
fn verify_finds_segments_that_do_not_follow_one_another() {
    let (bench, key) = two_segments();
    let second = bench.segments(&bench.versions(&key)[0])[1].offset;
    /* The catalogue loses the row of the first segment: nothing holds the bytes before the
    second. */
    edit_catalogue(bench.dir(), "DELETE FROM segment WHERE offset = 0");

    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    assert_eq!(
        report.damaged[0].problems,
        [Problem::Gap { offset: second }]
    );
    assert!(bench.versions(&key)[0].damaged);
}

#[test]
fn verify_finds_segments_that_are_not_the_version_together() {
    for edit in [
        "UPDATE version SET length = length + 1",
        "UPDATE version SET hash = (SELECT hash FROM segment WHERE offset > 0)",
    ] {
        let (bench, key) = two_segments();
        edit_catalogue(bench.dir(), edit);

        let report = bench.archive().verify(&CancelToken::new()).unwrap();
        assert_eq!(report.damaged.len(), 1, "{edit}");
        assert_eq!(report.damaged[0].problems, [Problem::VersionChanged]);
        assert!(bench.versions(&key)[0].damaged);
    }
}

#[cfg(unix)]
#[test]
fn a_segment_that_cannot_be_read_now_is_reported_and_not_marked_damaged() {
    use std::os::unix::fs::PermissionsExt as _;

    let (bench, key) = synced();
    let segment = bench.segment_files().remove(0);
    fs::set_permissions(&segment, fs::Permissions::from_mode(0o000)).unwrap();
    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    fs::set_permissions(&segment, fs::Permissions::from_mode(0o644)).unwrap();

    assert!(report.damaged.is_empty());
    assert_eq!(report.unread.len(), 1);
    assert_eq!(report.unread[0].key, key);
    assert!(!bench.versions(&key)[0].damaged);
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
    fs::write(bench.segment_files().remove(0), other_segment()).unwrap();
    bench.agent.append(&key, 5);

    /* The pass that would append reads the version back first and finds it changed. */
    let report = bench.pass_with(bench.agent.provider()).unwrap();
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

#[cfg(unix)]
#[test]
fn a_version_that_cannot_be_read_while_it_is_extended_is_left_alone() {
    use std::os::unix::fs::PermissionsExt as _;

    let mut agent = FileAgent::new();
    let key = agent.add_session(20);
    let mut bench = Bench::new(agent);
    bench.sync();
    let segment = bench.segment_files().remove(0);
    fs::set_permissions(&segment, fs::Permissions::from_mode(0o000)).unwrap();
    bench.agent.append(&key, 5);

    let report = bench.pass_with(bench.agent.provider()).unwrap();
    fs::set_permissions(&segment, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!((report.appended, report.failed.len()), (0, 1));
    /* Nothing says its bytes are wrong: it is not fetched again whole, only tried again. */
    assert!(!bench.versions(&key)[0].damaged);
    assert_eq!(bench.sync().appended, 1);
    assert_eq!(bench.versions(&key).len(), 1);
    bench.assert_mirrors(&[key]);
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
