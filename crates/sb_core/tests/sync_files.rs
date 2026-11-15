//! Sync of an agent that keeps files (PLAN §7.2, §12): lines being written, files of every
//! kind and size, and what a scan can't trust or can't read.

mod common;

use std::fs;
use std::io::Read as _;
use std::thread;
use std::time::{Duration, SystemTime};

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::{Problem, SEGMENT_BYTES};
use sb_core::catalogue::{Held, Reason};
use sb_core::provider::ItemKind;
use sb_core::sync::Progress;
use sb_test_support::{Agent, FileAgent, ProjectsTree, edit_catalogue};

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

#[test]
fn a_rewrite_is_held_back_by_what_was_stored_of_it_until_the_rest_arrives() {
    let (bench, key) = file_bench();
    bench.sync();
    let before = bench.agent.content(&key);

    /* The agent rewrites the file: one short line, and a long one it has not finished. The
    file is longer than it was, but what can be stored of it is not. */
    let unfinished = format!(
        "{{\"type\":\"user\",\"text\":\"{}",
        "x".repeat(before.len())
    );
    bench
        .agent
        .overwrite(&key, format!("{{}}\n{unfinished}").as_bytes());
    let report = bench.sync();
    assert_eq!((report.new_versions, report.held_back), (1, 1));
    let versions = bench.versions(&key);
    assert_eq!(
        (versions[1].length, versions[1].held_back),
        (3, Some(Held::Shorter))
    );

    /* The line is finished: the version has outgrown the one being read, and is no longer
    held back for its length. It is for what it holds, which is no conversation. */
    bench.agent.append_raw(&key, "\"}\n");
    let report = bench.sync();
    assert_eq!((report.appended, report.index.held_back), (1, 1));
    let version = &bench.versions(&key)[1];
    assert!(version.length > before.len() as u64);
    assert_eq!(version.held_back, Some(Held::ParsesWorse));
    assert!(bench.read(&bench.versions(&key)[0]) == before);
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
    let id = session.session_key().to_string();
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
fn every_file_under_a_project_is_archived_recognised_or_not() {
    use ItemKind::{Sidecar, Subagent, ToolResult, Transcript};
    const PROJECT: &str = "/home/user/code/app";

    let mut agent = FileAgent::new();
    let transcript = agent.add_session(5);
    let no_transcript = agent.add_session(1);
    let no_folder = agent.add_session(2);
    let empty = agent.add_session(0);
    let mut expected = vec![
        (transcript.clone(), Transcript, true),
        (no_folder.clone(), Transcript, true),
        (empty, Transcript, true),
    ];
    /* What the agent keeps in a session's folder, beside and under what the provider knows. */
    let session = agent.session(&transcript);
    for (path, kind) in [
        ("subagents/agent-a1.jsonl", Subagent),
        ("subagents/agent-a1.meta.json", Sidecar),
        ("subagents/agent-a1.prefix.json", Sidecar),
        ("subagents/workflows/wf_1/agent-a2.jsonl", Subagent),
        ("subagents/workflows/wf_1/agent-a2.meta.json", Sidecar),
        ("subagents/workflows/wf_1/agent-a2.prefix.json", Sidecar),
        ("subagents/workflows/wf_1/journal.jsonl", Sidecar),
        ("tool-results/toolu_1.txt", ToolResult),
        ("tool-results/toolu_2.json", ToolResult),
        ("tool-results/toolu_3.png", ToolResult),
        ("tool-results/toolu_4.jpg", ToolResult),
        ("tool-results/toolu_5.html", ToolResult),
        ("workflows/wf_1.json", Sidecar),
        ("workflows/wf_1/main.js", Sidecar),
        ("custom-title.json", Sidecar),
        ("notes.txt", Sidecar),
        ("something/new/in-a-later-release.bin", Sidecar),
        ("empty.txt", Sidecar),
    ] {
        let bytes: &[u8] = match path {
            "empty.txt" => b"",
            _ if path.ends_with(".jsonl") => b"{\"type\":\"user\"}\n",
            _ => b"\x89generated, not text\xff",
        };
        expected.push((session.file(path, bytes), kind, true));
    }
    /* A session whose transcript is gone and whose folder is not, and the other way round. */
    let subagent_only = agent
        .session(&no_transcript)
        .file("subagents/agent-a3.jsonl", b"{\"type\":\"user\"}\n");
    expected.push((subagent_only, Subagent, true));
    fs::remove_file(agent.tree().path(&no_transcript)).unwrap();
    fs::remove_dir(agent.tree().path(&no_folder).with_extension("")).unwrap();
    /* A released session's marker beside its transcript, and files of no session. */
    let id = agent.session(&transcript).id.clone();
    let session = agent.session(&transcript).session_key().to_string();
    let tree = agent.tree();
    expected.push((
        tree.project_file(PROJECT, &format!("{id}.released.json"), b"{}"),
        Sidecar,
        true,
    ));
    for path in [
        "sessions-index.json",
        "memory/notes.md",
        "notes.jsonl",
        "unknown/folder/file",
    ] {
        expected.push((tree.project_file(PROJECT, path, b"{}\n"), Sidecar, false));
    }
    fs::create_dir_all(tree.path("projects/-home-user-code-other/memory")).unwrap();
    let bench = Bench::new(agent);

    let report = bench.sync();
    assert_eq!(
        (report.scanned, report.new_items),
        (expected.len(), expected.len())
    );
    for (key, kind, in_session) in &expected {
        let item = bench.item(key);
        assert_eq!(
            (item.kind, item.session.is_some()),
            (*kind, *in_session),
            "{key}"
        );
    }
    assert_eq!(bench.item(&transcript).session, Some(session));
    let keys: Vec<_> = expected.into_iter().map(|(key, ..)| key).collect();
    bench.assert_mirrors(&keys);
}

#[test]
fn a_transcript_over_a_hundred_megabytes_is_stored_whole() {
    let mut agent = FileAgent::new();
    agent.tree_mut().record_bytes(200_000);
    let key = agent.add_session(520);
    let mut bench = Bench::new(agent);
    let length = bench.agent.content(&key).len() as u64;
    assert!(length > 100_000_000);

    let report = bench.sync();
    assert_eq!(
        (report.bytes_fetched, report.bytes_stored),
        (length, length)
    );
    /* In segments that are full, but for the last. */
    let segments = bench.segments(&bench.versions(&key)[0]);
    let (last, full) = segments.split_last().unwrap();
    assert_eq!(full.len() as u64, length / SEGMENT_BYTES);
    assert!(full.iter().all(|segment| segment.length == SEGMENT_BYTES));
    assert_eq!(last.length, length % SEGMENT_BYTES);

    bench.agent.append(&key, 3);
    assert_eq!(bench.sync().appended, 1);
    bench.assert_mirrors(&[key]);
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

/// How long before now the tests that follow give a file its modification time, and how long
/// they then wait: a time the scan can't trust yet, and one it can.
const JUST_NOW: Duration = Duration::from_millis(900);
const A_WHILE: Duration = Duration::from_millis(1600);

#[test]
fn a_file_modified_as_the_scan_ran_is_stored_and_checked_again() {
    let (bench, key) = file_bench();
    bench.agent.tree().modified_ago(&key, JUST_NOW);

    /* Its modification time can't vouch for it. A pass that finds it with that time, once
    the time is safely in the past, compares its bytes with what is archived. */
    assert_eq!(bench.sync().new_items, 1);
    bench.assert_mirrors(std::slice::from_ref(&key));
    thread::sleep(A_WHILE);
    let later = bench.sync();
    assert_eq!((later.touched, later.unchanged), (1, 0));
    let settled = bench.sync();
    assert_eq!((settled.touched, settled.unchanged), (0, 1));
}

#[test]
fn a_rewrite_in_the_same_tick_as_the_scan_is_not_missed() {
    let (bench, key) = file_bench();
    bench.agent.tree().modified_ago(&key, JUST_NOW);
    bench.sync();
    let before = bench.agent.content(&key);

    /* The same size and the same modification time, as a write in the same tick of the
    file system's clock leaves them. */
    let mut bytes = before.clone();
    bytes[12] ^= 1;
    bench.agent.overwrite_unseen(&key, &bytes);

    thread::sleep(A_WHILE);
    assert_eq!(bench.sync().new_versions, 1);
    let versions = bench.versions(&key);
    assert_eq!(versions[1].reason, Reason::Rewritten);
    assert!(bench.read(&versions[0]) == before);
    bench.assert_mirrors(&[key]);
}

#[test]
fn an_unfinished_last_line_is_taken_on_the_second_pass_whatever_the_files_time() {
    /* Modified long ago, just now, and at a time ahead of the clock. */
    for modified in [
        None,
        Some(SystemTime::now()),
        Some(SystemTime::now() + A_WHILE * 1000),
    ] {
        let (bench, key) = file_bench();
        bench.agent.append_raw(&key, r#"{"type":"user","mess"#);
        if let Some(modified) = modified {
            bench.agent.tree().set_modified(&key, modified);
        }
        bench.sync();
        assert!(bench.versions(&key)[0].length < bench.agent.content(&key).len() as u64);

        let second = bench.sync();
        assert_eq!((second.appended, second.unchanged), (1, 0), "{modified:?}");
        bench.assert_mirrors(std::slice::from_ref(&key));
        /* And it is not compared again on every pass after that. */
        if modified.is_none_or(|modified| modified > SystemTime::now()) {
            let third = bench.sync();
            assert_eq!((third.touched, third.unchanged), (0, 1), "{modified:?}");
        }
    }
}

#[test]
fn growth_that_holds_no_whole_line_is_not_counted_as_an_append() {
    let (bench, key) = file_bench();
    bench.sync();
    bench.agent.append_raw(&key, r#"{"type":"user","mess"#);
    let report = bench.sync();
    assert_eq!(
        (report.appended, report.touched, report.bytes_stored),
        (0, 1, 0)
    );
    assert_eq!(bench.segment_files().len(), 1);
}

#[test]
fn what_an_unchanged_item_is_follows_the_provider() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(4);
    let id = agent.session(&key).session_key().to_string();
    let bench = Bench::new(agent);
    bench.sync();
    /* As a build that classified it otherwise would have recorded it. */
    edit_catalogue(
        bench.dir(),
        "UPDATE item SET kind = 'sidecar', session = NULL",
    );
    assert_eq!(bench.item(&key).kind, ItemKind::Sidecar);

    let report = bench.sync();
    assert_eq!((report.unchanged, report.touched), (1, 0));
    let item = bench.item(&key);
    assert_eq!((item.kind, item.session), (ItemKind::Transcript, Some(id)));
}

#[test]
fn a_file_longer_than_a_segment_is_stored_as_several_and_read_from_any_offset() {
    let mut agent = FileAgent::new();
    let session = agent.add_session(1);
    let length = usize::try_from(SEGMENT_BYTES).unwrap() * 2 + 12_345;
    let key = agent.session(&session).tool_result(length);
    let bench = Bench::new(agent);
    bench.sync();

    let version = bench.versions(&key).remove(0);
    let segments: Vec<_> = bench
        .segments(&version)
        .iter()
        .map(|segment| (segment.offset, segment.length))
        .collect();
    assert_eq!(
        segments,
        [
            (0, SEGMENT_BYTES),
            (SEGMENT_BYTES, SEGMENT_BYTES),
            (SEGMENT_BYTES * 2, 12_345)
        ]
    );
    let content = bench.agent.content(&key);
    for offset in [
        0,
        1,
        SEGMENT_BYTES - 1,
        SEGMENT_BYTES,
        SEGMENT_BYTES + 7,
        SEGMENT_BYTES * 2 + 12_344,
        SEGMENT_BYTES * 2 + 12_345,
        SEGMENT_BYTES * 3,
    ] {
        let mut read = Vec::new();
        bench
            .archive()
            .read_version_from(version.id, offset)
            .unwrap()
            .read_to_end(&mut read)
            .unwrap();
        let from = usize::try_from(offset).unwrap().min(content.len());
        assert!(read == content[from..], "from {offset}");
    }
    bench.assert_mirrors(std::slice::from_ref(&key));

    /* A read from an offset checks the segments it reads like any other. */
    let last = bench
        .segment_files()
        .into_iter()
        .find(|file| file.ends_with(format!("{:016}.zst", SEGMENT_BYTES * 2)))
        .unwrap();
    fs::write(&last, zstd::encode_all(&b"other bytes"[..], 3).unwrap()).unwrap();
    let mut read = Vec::new();
    let error = bench
        .archive()
        .read_version_from(version.id, SEGMENT_BYTES + 7)
        .unwrap()
        .read_to_end(&mut read)
        .unwrap_err();
    assert_eq!(
        Problem::of(&error),
        Some(&Problem::SegmentChanged {
            offset: SEGMENT_BYTES * 2
        })
    );
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
    use sb_test_support::{break_segment_commits, mend_segment_commits};

    use super::*;
    use crate::common::{Meddle, When};

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_leftover_that_cannot_be_flushed_fails_its_item_alone() {
        let mut agent = FileAgent::new();
        let (held, open) = (agent.add_session(4), agent.add_session(4));
        let mut bench = Bench::new(agent);
        bench.sync();
        /* A store that was stopped before its commit leaves both appends' segments. */
        bench.agent.append(&held, 2);
        bench.agent.append(&open, 2);
        break_segment_commits(bench.dir());
        bench.pass_with(bench.agent.provider()).unwrap_err();
        mend_segment_commits(bench.dir());
        /* Another program has one of them open, in a way that lets nobody else write it. */
        let leftover = bench.segment_files_of(&held).pop().unwrap();
        set_mode(&leftover, 0o400);

        let report = bench.pass_with(bench.agent.provider()).unwrap();
        set_mode(&leftover, 0o644);
        /* The other item's leftover is adopted and recorded; this one is tried again. */
        assert_eq!((report.appended, report.failed.len()), (1, 1));
        assert_eq!(report.failed[0].key, held);
        assert_eq!(bench.segments(&bench.versions(&held)[0]).len(), 1);
        assert_eq!(bench.segments(&bench.versions(&open)[0]).len(), 2);

        assert_eq!(bench.sync().appended, 1);
        bench.assert_mirrors(&[held, open]);
        assert!(bench.orphans().is_empty());
    }

    #[test]
    fn an_items_folder_that_cannot_be_entered_fails_its_item_alone() {
        let mut agent = FileAgent::new();
        let (closed, open) = (agent.add_session(4), agent.add_session(4));
        let mut bench = Bench::new(agent);
        bench.sync();
        bench.agent.append(&closed, 2);
        bench.agent.append(&open, 2);
        /* The folder of one item in the archive can't be entered: nothing can be placed in
        it, nor can it be flushed. */
        let segment = bench.segment_files_of(&closed).remove(0);
        let folder = segment.parent().unwrap().parent().unwrap();
        set_mode(folder, 0o000);

        let report = bench.pass_with(bench.agent.provider());
        set_mode(folder, 0o755);
        /* The other item of the batch is stored all the same. */
        let report = report.unwrap();
        assert_eq!((report.appended, report.failed.len()), (1, 1));
        assert_eq!(report.failed[0].key, closed);
        assert_eq!(bench.sync().appended, 1);
        bench.assert_mirrors(&[closed, open]);
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
    fn what_cannot_be_listed_is_reported_and_nothing_is_marked_gone() {
        /* A folder that can't be read, and one whose files can't be looked at. */
        for mode in [0o000, 0o444] {
            let mut agent = FileAgent::new();
            let key = agent.add_session(4);
            let subagent = agent.session(&key).subagent(3);
            let bench = Bench::new(agent);
            bench.sync();
            let folder = bench.agent.tree().path(&subagent);
            let folder = folder.parent().unwrap();
            set_mode(folder, mode);

            let report = bench.pass_with(bench.agent.provider()).unwrap();
            set_mode(folder, 0o755);
            /* The folder as one, or its two files each. */
            assert_eq!(report.unlisted.len(), if mode == 0o000 { 1 } else { 2 });
            assert_eq!((report.scanned, report.gone), (1, 0));
            assert_eq!(bench.item(&subagent).gone_since, None);
            let error = bench.location().last_error.unwrap();
            assert!(error.contains("could not be archived"), "{error}");

            /* Readable again, and grown: it goes on in its version, as nothing happened. */
            bench.agent.append_raw(&subagent, "{\"type\":\"user\"}\n");
            let report = bench.sync();
            assert_eq!(
                (report.unchanged, report.appended, report.new_versions),
                (2, 1, 0)
            );
            assert_eq!(bench.location().last_error, None);
            bench.assert_mirrors(&[key, subagent]);
        }
    }

    #[test]
    fn a_tree_that_is_gone_when_it_is_listed_marks_nothing_gone() {
        let mut agent = FileAgent::new();
        let keys = [agent.add_session(4), agent.add_session(2)];
        let bench = Bench::new(agent);
        bench.sync();
        let projects = bench.agent.tree().path("projects");
        let away = projects.with_extension("away");

        /* The agent was found, and its folder is moved before the scan lists it. */
        let report = bench
            .pass_with(bench.meddling(Meddle::at(When::BeforeScan, || {
                fs::rename(&projects, &away).unwrap();
            })))
            .unwrap();
        fs::rename(&away, &projects).unwrap();
        assert_eq!(
            (report.scanned, report.gone, report.unlisted.len()),
            (0, 0, 1)
        );
        assert!(keys.iter().all(|key| bench.item(key).gone_since.is_none()));
        assert_eq!(bench.sync().unchanged, 2);
    }

    #[test]
    fn a_symbolic_link_is_reported_as_skipped_and_fails_nothing() {
        let mut agent = FileAgent::new();
        let key = agent.add_session(4);
        let bench = Bench::new(agent);
        let target = bench.agent.tree().path(&key);
        let link = target.with_file_name("linked.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let report = bench.sync();
        assert_eq!((report.scanned, report.new_items), (1, 1));
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].path, link.to_str().unwrap());
        assert_eq!(bench.location().last_error, None);
    }
}

#[test]
fn a_batch_is_bounded_by_what_its_proofs_hash() {
    let mut agent = FileAgent::new();
    let keys: Vec<_> = (0..3).map(|_| agent.add_session(1)).collect();
    let line = "{\"type\":\"user\",\"text\":\"filler\"}\n";
    let lines = line.repeat(24 * 1024 * 1024 / line.len());
    for key in &keys {
        agent.append_raw(key, &lines);
    }
    let mut bench = Bench::new(agent);
    bench.sync();
    for key in &keys {
        bench.agent.append(key, 1);
    }

    /* Little is fetched, but each range's proof hashes its file from the start, and the three
    files together are more than a batch may have hashed. */
    let mut stored = Vec::new();
    bench
        .try_sync(&CancelToken::new(), &mut |progress| {
            if let Progress::Stored { items } = progress {
                stored.push(*items);
            }
        })
        .unwrap();
    assert_eq!(stored, [2, 3]);
    bench.assert_mirrors(&keys);
}
