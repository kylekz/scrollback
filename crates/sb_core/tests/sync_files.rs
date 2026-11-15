//! Sync of an agent that keeps files (PLAN §7.2, §12): lines being written, files of every
//! kind and size, and what a scan can't trust or can't read.

mod common;

use std::fs;

use pretty_assertions::assert_eq;
use sb_core::catalogue::Reason;
use sb_core::provider::ItemKind;
use sb_test_support::{Agent, FileAgent, ProjectsTree};

use common::Bench;

fn file_bench() -> (Bench<FileAgent>, String) {
    let mut agent = FileAgent::new();
    let key = agent.add_session(4);
    (Bench::new(agent), key)
}

#[test]
fn a_file_that_ends_mid_line_is_stored_up_to_its_last_newline() {
    let (bench, key) = file_bench();
    let whole_lines = bench.agent.content(&key);
    bench
        .agent
        .append_raw(&key, r#"{"type":"user","message":"half a rec"#);

    let report = bench.sync();
    let version = bench.versions(&key).remove(0);
    assert!(bench.read(&version) == whole_lines);
    assert_eq!(report.bytes_stored, whole_lines.len() as u64);
    assert!(report.bytes_fetched > report.bytes_stored);

    /* The agent finishes the record and writes another: both arrive, as an append. */
    bench
        .agent
        .append_raw(&key, "ord\"}\n{\"type\":\"assistant\"}\n");
    let report = bench.sync();
    assert_eq!((report.appended, report.new_versions), (1, 0));
    bench.assert_mirrors(&[key]);
}

#[test]
fn an_unfinished_last_line_is_taken_once_the_file_stops_changing() {
    let (bench, key) = file_bench();
    bench
        .agent
        .append_raw(&key, r#"{"type":"user","message":"never finis"#);
    bench.sync();
    assert!(bench.versions(&key)[0].length < bench.agent.content(&key).len() as u64);

    /* The file is as the last pass saw it: nothing is writing it, so the rest is archived. */
    let report = bench.sync();
    assert_eq!((report.appended, report.unchanged), (1, 0));
    bench.assert_mirrors(std::slice::from_ref(&key));
    let settled = bench.sync();
    assert_eq!((settled.appended, settled.unchanged), (0, 1));

    /* Were the record finished after all, it would still be an append. */
    bench.agent.append_raw(&key, "hed\"}\n");
    assert_eq!(bench.sync().appended, 1);
    assert_eq!(bench.versions(&key).len(), 1);
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_file_with_no_whole_line_yet_is_recorded_and_stored_later() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(0);
    agent.append_raw(&key, r#"{"type":"us"#);
    let bench = Bench::new(agent);

    let report = bench.sync();
    assert_eq!((report.new_items, report.bytes_stored), (1, 0));
    let version = bench.versions(&key).remove(0);
    assert_eq!((version.length, bench.segments(&version).len()), (0, 0));
    bench.assert_verifies();

    bench.agent.append_raw(&key, "er\"}\n");
    assert_eq!(bench.sync().appended, 1);
    bench.assert_mirrors(&[key]);
}

#[test]
fn an_empty_file_is_an_item_with_nothing_stored() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(0);
    let mut bench = Bench::new(agent);

    assert_eq!(bench.sync().new_items, 1);
    assert!(bench.segment_files().is_empty());
    bench.assert_mirrors(std::slice::from_ref(&key));
    assert_eq!(bench.sync().unchanged, 1);

    bench.agent.append(&key, 2);
    assert_eq!(bench.sync().appended, 1);
    bench.assert_mirrors(&[key]);
}

/// A transcript many times the size the transport reads at once, so that every piece of it
/// ends inside a line.
fn long_transcript() -> (Bench<FileAgent>, String) {
    let mut agent = FileAgent::new();
    agent.tree_mut().record_bytes(5000);
    let key = agent.add_session(260);
    (Bench::new(agent), key)
}

#[test]
fn a_long_transcript_is_stored_whole() {
    let (bench, key) = long_transcript();
    let length = bench.agent.content(&key).len() as u64;
    assert!(length > 1_200_000);

    let report = bench.sync();
    assert_eq!(
        (report.bytes_fetched, report.bytes_stored),
        (length, length)
    );
    assert_eq!(bench.versions(&key)[0].length, length);
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_long_append_is_stored_whole_up_to_its_last_newline() {
    let (mut bench, key) = long_transcript();
    bench.sync();
    let before = bench.agent.content(&key).len() as u64;
    bench.agent.append(&key, 260);
    let whole_lines = bench.agent.content(&key);
    bench.agent.append_raw(&key, r#"{"type":"user","mess"#);

    let report = bench.sync();
    assert_eq!(report.appended, 1);
    assert_eq!(report.bytes_stored, whole_lines.len() as u64 - before);
    assert!(report.bytes_stored > 1_200_000);
    let version = bench.versions(&key).remove(0);
    assert!(bench.read(&version) == whole_lines);
    bench.assert_verifies();
}

#[test]
fn every_file_of_a_session_is_archived_under_its_session() {
    let mut agent = FileAgent::new();
    agent.tree_mut().record_bytes(4096);
    let transcript = agent.add_session(12);
    let session = agent.session(&transcript);
    let id = session.id.clone();
    let subagent = session.subagent(6);
    let tool_result = session.tool_result(700_000);
    let title = session.title("A generated session");
    let memory = agent
        .tree()
        .memory("/home/user/code/app", "notes", "Remember the fixtures.\n");
    agent.tree().write_settings(r#"{"cleanupPeriodDays": 45}"#);
    let bench = Bench::new(agent);

    let report = bench.sync();
    /* The subagent's metadata file is the sixth. */
    assert_eq!((report.scanned, report.new_items), (6, 6));
    let kinds = [
        (&transcript, ItemKind::Transcript, true),
        (&subagent, ItemKind::Subagent, true),
        (&tool_result, ItemKind::ToolResult, true),
        (&title, ItemKind::Sidecar, true),
        (&memory, ItemKind::Sidecar, false),
    ];
    for (key, kind, in_session) in kinds {
        let item = bench.item(key);
        assert_eq!(item.kind, kind, "{key}");
        assert_eq!(item.session, in_session.then(|| id.clone()), "{key}");
    }
    bench.assert_mirrors(&[transcript, subagent, tool_result, title, memory]);
    assert_eq!(
        bench
            .archive()
            .catalogue()
            .providers_found(&bench.location)
            .unwrap(),
        [(bench.agent.provider().id().to_string(), Some(45))]
    );
}

#[test]
fn the_agent_is_found_where_the_environment_says_its_data_is() {
    let mut agent = FileAgent::on(ProjectsTree::in_config_dir());
    let key = agent.add_session(3);
    let bench = Bench::new(agent);
    assert_eq!(bench.sync().new_items, 1);
    bench.assert_mirrors(&[key]);
    /* No settings file there: the agent's own default applies. */
    assert_eq!(
        bench
            .archive()
            .catalogue()
            .providers_found(&bench.location)
            .unwrap()[0]
            .1,
        Some(30)
    );
}

#[test]
fn a_file_modified_as_the_scan_ran_is_stored_and_checked_again() {
    let (bench, key) = file_bench();
    bench.agent.tree().freshen(&key);

    /* Its modification time can't vouch for it, so its bytes are proven before they are
    stored, and the next pass compares them again. */
    assert_eq!(bench.sync().new_items, 1);
    bench.assert_mirrors(std::slice::from_ref(&key));
    let again = bench.sync();
    assert_eq!((again.touched, again.unchanged), (1, 0));
}

#[test]
fn a_rewrite_in_the_same_tick_as_the_scan_is_not_missed() {
    let (bench, key) = file_bench();
    bench.agent.tree().freshen(&key);
    bench.sync();
    let before = bench.agent.content(&key);

    /* The same size and the same modification time, as a write in the same tick of the
    file system's clock leaves them. */
    let mut bytes = before.clone();
    bytes[12] ^= 1;
    bench.agent.overwrite_unseen(&key, &bytes);

    assert_eq!(bench.sync().new_versions, 1);
    let versions = bench.versions(&key);
    assert_eq!(versions[1].reason, Reason::Rewritten);
    assert!(bench.read(&versions[0]) == before);
    bench.assert_mirrors(&[key]);
}

#[test]
fn an_item_that_comes_back_as_it_was_or_longer_goes_on_in_its_version() {
    let mut agent = FileAgent::new();
    let (same, longer) = (agent.add_session(6), agent.add_session(6));
    let mut bench = Bench::new(agent);
    bench.sync();
    for key in [&same, &longer] {
        bench.agent.delete(key);
    }
    assert_eq!(bench.sync().gone, 2);

    /* The agent writes each again from its start: one as it was, one with more at its end. */
    bench.agent.recreate(&same, 6);
    bench.agent.recreate(&longer, 9);
    let report = bench.sync();
    assert_eq!(
        (report.touched, report.appended, report.new_versions),
        (1, 1, 0)
    );
    for key in [&same, &longer] {
        assert_eq!(bench.item(key).gone_since, None);
        assert_eq!(bench.versions(key).len(), 1);
    }
    bench.assert_mirrors(&[same, longer]);
}

#[cfg(unix)]
mod unreadable {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use pretty_assertions::assert_eq;

    use super::*;

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn an_archived_file_that_grew_and_cannot_be_read_fails_alone() {
        let mut agent = FileAgent::new();
        let (locked, open) = (agent.add_session(4), agent.add_session(4));
        let mut bench = Bench::new(agent);
        bench.sync();
        let succeeded = bench.location().last_success;
        bench.agent.append(&locked, 2);
        bench.agent.append(&open, 2);
        let path = bench.agent.tree().path(&locked);
        set_mode(&path, 0o000);

        let report = bench.pass_with(bench.agent.provider()).unwrap();
        set_mode(&path, 0o644);
        assert_eq!((report.appended, report.failed.len()), (1, 1));
        assert_eq!(report.failed[0].key, locked);
        /* The location has not synced cleanly, and says how many items it missed. */
        let location = bench.location();
        assert_eq!(location.last_success, succeeded);
        let error = location.last_error.unwrap();
        assert!(error.contains("1 could not be archived"), "{error}");

        assert_eq!(bench.sync().appended, 1);
        bench.assert_mirrors(&[locked, open]);
        assert_eq!(bench.location().last_error, None);
    }

    #[test]
    fn a_new_file_that_cannot_be_read_fails_alone() {
        let mut agent = FileAgent::new();
        let (locked, open) = (agent.add_session(4), agent.add_session(4));
        let bench = Bench::new(agent);
        let path = bench.agent.tree().path(&locked);
        set_mode(&path, 0o000);

        let report = bench.pass_with(bench.agent.provider()).unwrap();
        set_mode(&path, 0o644);
        assert_eq!((report.new_items, report.failed.len()), (1, 1));
        assert!(bench.location().last_error.is_some());

        assert_eq!(bench.sync().new_items, 1);
        bench.assert_mirrors(&[locked, open]);
    }

    #[test]
    fn a_folder_that_cannot_be_listed_is_reported_and_nothing_under_it_is_marked_gone() {
        let mut agent = FileAgent::new();
        let key = agent.add_session(4);
        let subagent = agent.session(&key).subagent(3);
        let bench = Bench::new(agent);
        bench.sync();
        let folder = bench.agent.tree().path(&subagent);
        let folder = folder.parent().unwrap();
        set_mode(folder, 0o000);

        let report = bench.pass_with(bench.agent.provider()).unwrap();
        set_mode(folder, 0o755);
        assert_eq!((report.unlisted.len(), report.gone), (1, 0));
        assert!(report.unlisted[0].folder);
        assert_eq!(bench.item(&subagent).gone_since, None);
        let error = bench.location().last_error.unwrap();
        assert!(error.contains("1 could not be archived"), "{error}");

        assert_eq!(bench.sync().unchanged, 3);
        assert_eq!(bench.location().last_error, None);
    }
}
