//! The agent writing while a pass reads (PLAN §4.2, §5). Whatever it does and whenever, each
//! state of a stored version is bytes the source held at one instant, and what can't be shown
//! to be that is not stored but left for the next pass.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::time::SystemTime;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::catalogue::Reason;
use sb_core::provider::ProviderError;
use sb_core::sync::{SyncError, WAITING_HOURS};
use sb_test_support::{Agent, FileAgent, edit_catalogue};

use common::{Bench, Meddle, When, files_under};

/// A synced bench of one session, and the session's key.
fn synced() -> (Bench<FileAgent>, String) {
    let mut agent = FileAgent::new();
    let key = agent.add_session(6);
    let bench = Bench::new(agent);
    bench.sync();
    (bench, key)
}

/// `content` with one of its first bytes changed: other bytes of the same length.
fn altered(content: &[u8]) -> Vec<u8> {
    let mut altered = content.to_vec();
    altered[12] ^= 1;
    altered
}

/// The keys of what the last pass could not archive, as the catalogue keeps them.
fn waiting(bench: &Bench<FileAgent>) -> Vec<String> {
    bench
        .archive()
        .catalogue()
        .waiting(&bench.location, bench.agent.provider().id().as_str())
        .unwrap()
        .into_iter()
        .map(|waiting| waiting.key)
        .collect()
}

#[test]
fn a_file_rewritten_before_its_new_bytes_are_fetched_is_not_extended() {
    /* Once leaving its trace in the file's times, once not: a write that keeps the size and
    puts the modification time back. */
    for traceless in [false, true] {
        let (mut bench, key) = synced();
        let archived = bench.agent.content(&key);
        bench.agent.append(&key, 5);
        let rewritten = altered(&bench.agent.content(&key));

        /* The scan saw a file that had grown, which the pass takes for an append. */
        let report = bench
            .pass_with(bench.meddling(Meddle::at(When::AfterScan, || {
                if traceless {
                    bench.agent.overwrite_unseen(&key, &rewritten);
                } else {
                    bench.agent.overwrite(&key, &rewritten);
                }
            })))
            .unwrap();
        /* Its start is no longer what is archived: it is stored whole, as what it is. */
        assert_eq!(
            (report.appended, report.new_versions, report.deferred.len()),
            (0, 1, 0)
        );
        let versions = bench.versions(&key);
        assert_eq!(versions[1].reason, Reason::Rewritten);
        assert!(bench.read(&versions[0]) == archived);
        assert_eq!(bench.segments(&versions[0]).len(), 1);
        assert!(bench.read(&versions[1]) == rewritten);
        assert!(files_under(&bench.dir().join("tmp")).is_empty());
        bench.assert_verifies();
    }
}

#[test]
fn new_bytes_that_changed_under_the_pass_are_not_taken_for_a_rewrite() {
    let (mut bench, key) = synced();
    let archived = bench.agent.content(&key);
    bench.agent.append(&key, 5);
    /* Between the fetch and the proof one of the new bytes changes. What is archived is still
    how the source starts. */
    let mut later = bench.agent.content(&key);
    let changed = later.len() - 10;
    later[changed] ^= 1;

    let report = bench
        .pass_with(bench.meddling(Meddle::at(When::BeforeProof, || {
            bench.agent.overwrite(&key, &later);
        })))
        .unwrap();
    assert_eq!(
        (report.appended, report.new_versions, report.deferred.len()),
        (0, 0, 1)
    );
    assert!(report.failed.is_empty());
    let versions = bench.versions(&key);
    assert_eq!(versions.len(), 1);
    assert!(bench.read(&versions[0]) == archived);
    assert!(files_under(&bench.dir().join("tmp")).is_empty());

    assert_eq!(bench.sync().appended, 1);
    assert_eq!(bench.versions(&key).len(), 1);
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_file_appended_to_while_the_pass_runs_is_stored_as_far_as_the_pass_saw_it() {
    let (mut bench, key) = synced();
    bench.agent.append(&key, 5);
    let scanned = bench.agent.content(&key);

    /* The agent goes on writing: the file is not as it was scanned, yet its start still is
    what the pass fetched, which is what the pass proves before it stores it. */
    let report = bench
        .pass_with(bench.meddling(Meddle::at(When::AfterScan, || {
            bench
                .agent
                .append_raw(&key, "{\"type\":\"assistant\"}\n{\"type\":\"us");
        })))
        .unwrap();
    assert_eq!((report.appended, report.deferred.len()), (1, 0));
    let version = bench.versions(&key).remove(0);
    assert!(bench.read(&version) == scanned);
    bench.assert_verifies();

    bench.agent.append_raw(&key, "er\"}\n");
    assert_eq!(bench.sync().appended, 1);
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_file_rewritten_while_it_is_read_is_not_stored() {
    /* Once leaving its trace in the file's times, once not. */
    for traceless in [false, true] {
        let mut agent = FileAgent::new();
        let session = agent.add_session(2);
        let mut bench = Bench::new(agent);
        bench.sync();
        /* A new file, large enough to be read in several pieces. */
        let key = bench.agent.session(&session).tool_result(700_000);
        let before = bench.agent.content(&key);
        let after = vec![b'b'; before.len()];

        /* After the first piece was read the agent rewrites the file: what arrives is the
        start of one state and the rest of another. */
        let report = bench
            .pass_with(bench.meddling(Meddle::at(When::InsideARange, || {
                if traceless {
                    bench.agent.overwrite_unseen(&key, &after);
                } else {
                    bench.agent.overwrite(&key, &after);
                }
            })))
            .unwrap();
        assert_eq!((report.new_items, report.deferred.len()), (0, 1));
        assert_eq!(report.deferred[0].key, key);
        assert!(report.failed.is_empty());
        /* Nothing of it is in the archive, and the catalogue knows it is waiting. */
        assert_eq!(bench.items().len(), 1);
        assert!(files_under(&bench.dir().join("tmp")).is_empty());
        assert_eq!(waiting(&bench), std::slice::from_ref(&key));
        /* A file that changed under the pass is not a failure of the location's sync. */
        assert_eq!(bench.location().last_error, None);

        assert_eq!(bench.sync().new_items, 1);
        assert!(waiting(&bench).is_empty());
        bench.assert_mirrors(&[key]);
    }
}

#[test]
fn an_item_that_keeps_changing_has_been_waiting_since_the_first_pass_that_missed_it() {
    let mut agent = FileAgent::new();
    let session = agent.add_session(2);
    let mut bench = Bench::new(agent);
    bench.sync();
    let key = bench.agent.session(&session).tool_result(700_000);
    let since = |bench: &Bench<FileAgent>| {
        bench
            .archive()
            .catalogue()
            .waiting(&bench.location, bench.agent.provider().id().as_str())
            .unwrap()[0]
            .since
    };
    let mut first = None;
    for round in 0..3_u8 {
        let report = bench
            .pass_with(bench.meddling(Meddle::at(When::InsideARange, || {
                bench.agent.overwrite(&key, &vec![b'a' + round; 700_000]);
            })))
            .unwrap();
        assert_eq!(report.deferred.len(), 1);
        assert_eq!(*first.get_or_insert(since(&bench)), since(&bench));
    }
}

#[test]
fn an_item_that_has_waited_too_long_counts_against_the_sync() {
    let mut agent = FileAgent::new();
    let session = agent.add_session(2);
    let mut bench = Bench::new(agent);
    bench.sync();
    let key = bench.agent.session(&session).tool_result(700_000);
    /* A pass that the file changes under, and the same pass that many hours later. */
    let missed = |round: u8| {
        bench
            .pass_with(bench.meddling(Meddle::at(When::InsideARange, || {
                bench.agent.overwrite(&key, &vec![b'a' + round; 700_000]);
            })))
            .unwrap()
    };
    let hours_pass = |hours: i64| {
        edit_catalogue(
            bench.dir(),
            &format!(
                "UPDATE waiting SET since = since - {}",
                hours * 60 * 60 * 1000
            ),
        );
    };
    let report = missed(0);
    assert_eq!((report.deferred.len(), report.failed.len()), (1, 0));
    assert_eq!(bench.location().last_error, None);

    /* Short of the bound it is still only left for the next pass. */
    hours_pass(WAITING_HOURS - 1);
    let report = missed(1);
    assert_eq!((report.deferred.len(), report.failed.len()), (1, 0));
    assert_eq!(bench.location().last_error, None);

    /* Past it, the sync has not archived everything. */
    hours_pass(2);
    let report = missed(2);
    assert_eq!((report.deferred.len(), report.failed.len()), (0, 1));
    assert_eq!(report.failed[0].key, key);
    assert!(bench.location().last_error.is_some());

    /* Until it does. */
    assert_eq!(bench.sync().new_items, 1);
    assert_eq!(bench.location().last_error, None);
    assert!(waiting(&bench).is_empty());
}

#[test]
fn a_whole_file_that_grew_after_the_scan_waits_for_the_next_pass() {
    let mut agent = FileAgent::new();
    let session = agent.add_session(2);
    let key = agent.session(&session).tool_result(1000);
    let bench = Bench::new(agent);

    /* It is not made of lines, so its first bytes alone are not a state to keep. */
    let report = bench
        .pass_with(bench.meddling(Meddle::at(When::AfterScan, || {
            bench.agent.append_raw(&key, " and more output");
        })))
        .unwrap();
    assert_eq!((report.new_items, report.deferred.len()), (1, 1));
    assert_eq!(bench.items().len(), 1);

    assert_eq!(bench.sync().new_items, 1);
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_file_that_shrank_or_went_after_the_scan_waits_for_the_next_pass() {
    let mut agent = FileAgent::new();
    let (shrinks, goes) = (agent.add_session(6), agent.add_session(6));
    let mut bench = Bench::new(agent);
    bench.sync();
    bench.agent.append(&shrinks, 2);
    bench.agent.append(&goes, 2);

    let report = bench
        .pass_with(bench.meddling(Meddle::at(When::AfterScan, || {
            bench.agent.overwrite(&shrinks, b"{}\n");
            fs::remove_file(bench.agent.tree().path(&goes)).unwrap();
        })))
        .unwrap();
    assert_eq!(
        (report.deferred.len(), report.appended, report.new_versions),
        (2, 0, 0)
    );
    /* Neither is a failure: the location's sync succeeded. */
    assert!(report.failed.is_empty());
    assert_eq!(bench.location().last_error, None);
    assert!(files_under(&bench.dir().join("tmp")).is_empty());

    let report = bench.sync();
    assert_eq!((report.new_versions, report.gone), (1, 1));
    assert_eq!(bench.versions(&shrinks)[1].reason, Reason::Shrank);
    bench.assert_mirrors(&[shrinks]);
}

#[test]
fn an_unfinished_last_line_is_not_taken_from_a_file_that_is_changing_after_all() {
    let (bench, key) = synced();
    bench.agent.append_raw(&key, r#"{"type":"user","mess"#);
    bench.sync();
    let stored = bench.versions(&key)[0].length;

    /* The file is as the last pass left it, so this pass means to take the rest as it is;
    then the agent writes again. */
    let report = bench
        .pass_with(bench.meddling(Meddle::at(When::AfterScan, || {
            bench.agent.append_raw(&key, r#"age":"it goes on"#);
        })))
        .unwrap();
    assert_eq!((report.appended, report.deferred.len()), (0, 1));
    assert_eq!(bench.versions(&key)[0].length, stored);

    bench.agent.append_raw(&key, "\"}\n");
    assert_eq!(bench.sync().appended, 1);
    bench.assert_mirrors(&[key]);
}

/// A bench of three sessions that were never synced.
fn unsynced() -> (Bench<FileAgent>, Vec<String>) {
    let mut agent = FileAgent::new();
    let keys = (3..6).map(|records| agent.add_session(records)).collect();
    (Bench::new(agent), keys)
}

#[test]
fn a_pass_cancelled_with_a_batch_fetched_and_not_proven_leaves_nothing_behind() {
    let (bench, keys) = unsynced();
    let cancel = CancelToken::new();
    let stopped = bench.try_sync_with(
        bench.meddling(Meddle::at(When::BeforeProof, || cancel.cancel())),
        &cancel,
        &mut |_| {},
    );
    /* The provider is asked for its proofs and the pass stops before it stores anything. */
    assert!(matches!(stopped, Err(SyncError::Cancelled)), "{stopped:?}");
    assert!(files_under(&bench.dir().join("tmp")).is_empty());
    assert!(bench.items().is_empty() && bench.segment_files().is_empty());

    assert_eq!(bench.sync().new_items, 3);
    bench.assert_mirrors(&keys);
}

#[test]
fn a_pass_whose_proofs_fail_leaves_nothing_behind() {
    let (bench, keys) = unsynced();
    let failed = bench.pass_with(bench.meddling(Meddle::NoProofs));
    assert!(
        matches!(failed, Err(SyncError::Provider(ProviderError::Halted))),
        "{failed:?}"
    );
    assert!(files_under(&bench.dir().join("tmp")).is_empty());
    assert!(bench.items().is_empty() && bench.segment_files().is_empty());
    assert!(bench.location().last_error.is_some());

    assert_eq!(bench.sync().new_items, 3);
    bench.assert_mirrors(&keys);
}

#[test]
fn a_range_the_provider_gives_no_proof_for_is_not_stored() {
    let (bench, keys) = unsynced();
    let report = bench
        .pass_with(bench.meddling(Meddle::FewerProofs))
        .unwrap();
    assert_eq!((report.new_items, report.deferred.len()), (2, 1));
    assert_eq!(bench.items().len(), 2);
    assert!(files_under(&bench.dir().join("tmp")).is_empty());

    assert_eq!(bench.sync().new_items, 1);
    bench.assert_mirrors(&keys);
}

#[test]
fn a_sync_leaves_the_source_as_it_was() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(30);
    let session = agent.session(&key);
    session.subagent(5);
    session.tool_result(300_000);
    let mut bench = Bench::new(agent);
    let source = |bench: &Bench<FileAgent>| -> BTreeMap<_, (Vec<u8>, SystemTime)> {
        files_under(bench.agent.tree().home())
            .into_iter()
            .map(|file| {
                let modified = fs::metadata(&file).unwrap().modified().unwrap();
                (file.clone(), (fs::read(&file).unwrap(), modified))
            })
            .collect()
    };

    let before = source(&bench);
    bench.sync();
    assert!(source(&bench) == before);

    /* And so does a pass that appends, checks a touched file, and verifies. */
    bench.agent.append(&key, 3);
    bench.agent.touch(&key);
    let before = source(&bench);
    bench.sync();
    bench.assert_verifies();
    assert!(source(&bench) == before);
    assert_eq!(before.len(), 4);
}
