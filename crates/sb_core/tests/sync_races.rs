//! The agent writing while a pass reads (PLAN §4.2, §5). Whatever it does and whenever, each
//! state of a stored version is bytes the source held at one instant, and what can't be shown
//! to be that is not stored but left for the next pass.

mod common;

use std::fs;

use pretty_assertions::assert_eq;
use sb_core::catalogue::Reason;
use sb_test_support::{Agent, FileAgent};

use common::{Bench, Meddle, When};

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

#[test]
fn a_file_rewritten_between_the_hashing_of_its_start_and_the_fetch_is_not_stored() {
    let (mut bench, key) = synced();
    let archived = bench.agent.content(&key);
    bench.agent.append(&key, 5);
    let rewritten = altered(&bench.agent.content(&key));

    /* The pass has seen that the file's start is what is archived, and plans to append. */
    let report = bench
        .pass_with(bench.meddling(Meddle::at(When::AfterHashing, || {
            bench.agent.overwrite(&key, &rewritten);
        })))
        .unwrap();
    assert_eq!((report.appended, report.deferred), (0, 1));
    assert!(report.failed.is_empty());
    let versions = bench.versions(&key);
    assert_eq!(versions.len(), 1);
    assert!(bench.read(&versions[0]) == archived);
    assert_eq!(bench.segments(&versions[0]).len(), 1);
    assert!(common::files_under(&bench.dir().join("tmp")).is_empty());

    /* The next pass finds the rewrite for what it is. */
    assert_eq!(bench.sync().new_versions, 1);
    let versions = bench.versions(&key);
    assert_eq!(versions[1].reason, Reason::Rewritten);
    assert!(bench.read(&versions[0]) == archived);
    bench.assert_mirrors(&[key]);
}

#[test]
fn the_same_rewrite_with_no_trace_in_the_files_times_is_not_stored_either() {
    let (mut bench, key) = synced();
    let archived = bench.agent.content(&key);
    bench.agent.append(&key, 5);
    /* Written just now: a second write in the same tick leaves no trace but in the bytes. */
    bench.agent.tree().freshen(&key);
    let rewritten = altered(&bench.agent.content(&key));

    let report = bench
        .pass_with(bench.meddling(Meddle::at(When::AfterHashing, || {
            bench.agent.overwrite_unseen(&key, &rewritten);
        })))
        .unwrap();
    assert_eq!((report.appended, report.deferred), (0, 1));
    let versions = bench.versions(&key);
    assert_eq!(versions.len(), 1);
    assert!(bench.read(&versions[0]) == archived);

    assert_eq!(bench.sync().new_versions, 1);
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
        .pass_with(bench.meddling(Meddle::at(When::AfterHashing, || {
            bench
                .agent
                .append_raw(&key, "{\"type\":\"assistant\"}\n{\"type\":\"us");
        })))
        .unwrap();
    assert_eq!((report.appended, report.deferred), (1, 0));
    let version = bench.versions(&key).remove(0);
    assert!(bench.read(&version) == scanned);
    bench.assert_verifies();

    bench.agent.append_raw(&key, "er\"}\n");
    assert_eq!(bench.sync().appended, 1);
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_file_rewritten_while_it_is_read_is_not_stored() {
    let mut agent = FileAgent::new();
    let session = agent.add_session(2);
    /* Large enough to be read in several pieces. */
    let key = agent.session(&session).tool_result(700_000);
    let bench = Bench::new(agent);
    bench.sync();
    let before = bench.agent.content(&key);
    bench.agent.overwrite(&key, &vec![b'b'; before.len()]);

    /* The pass fetches the rewritten file whole, and after its first piece was read the agent
    rewrites it again: what arrives is the start of one state and the rest of another. */
    let report = bench
        .pass_with(bench.meddling(Meddle::at(When::InsideARange, || {
            bench.agent.overwrite(&key, &before);
        })))
        .unwrap();
    assert_eq!((report.new_versions, report.deferred), (0, 1));
    assert!(report.failed.is_empty());
    let versions = bench.versions(&key);
    assert_eq!(versions.len(), 1);
    assert!(bench.read(&versions[0]) == before);

    assert_eq!(bench.sync().touched, 1);
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_whole_file_that_grew_after_the_scan_waits_for_the_next_pass() {
    let mut agent = FileAgent::new();
    let session = agent.add_session(2);
    let key = agent.session(&session).tool_result(1000);
    let bench = Bench::new(agent);

    /* It is not made of lines, so its first bytes alone are not a state to keep. */
    let report = bench
        .pass_with(bench.meddling(Meddle::at(When::AfterHashing, || {
            bench.agent.append_raw(&key, " and more output");
        })))
        .unwrap();
    assert_eq!((report.new_items, report.deferred), (1, 1));
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
        .pass_with(bench.meddling(Meddle::at(When::BeforeHashing, || {
            bench.agent.overwrite(&shrinks, b"{}\n");
            fs::remove_file(bench.agent.tree().path(&goes)).unwrap();
        })))
        .unwrap();
    assert_eq!(
        (report.deferred, report.appended, report.new_versions),
        (2, 0, 0)
    );
    assert!(report.failed.is_empty());

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
        .pass_with(bench.meddling(Meddle::at(When::AfterHashing, || {
            bench.agent.append_raw(&key, r#"age":"it goes on"#);
        })))
        .unwrap();
    assert_eq!((report.appended, report.deferred), (0, 1));
    assert_eq!(bench.versions(&key)[0].length, stored);

    bench.agent.append_raw(&key, "\"}\n");
    assert_eq!(bench.sync().appended, 1);
    bench.assert_mirrors(&[key]);
}
