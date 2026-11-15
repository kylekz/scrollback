//! Sync over fixtures (PLAN §12). Every case that applies to both shapes of source runs against
//! both: the file-tree agent and the made-up agent that keeps a database.

mod common;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::catalogue::Reason;
use sb_core::provider::{
    ItemKind, ItemPrefix, ItemRange, Provider, ProviderError, ScannedItem, Source,
};
use sb_core::sync::{PassReport, Progress, SyncError};
use sb_core::transport::{Halt, RangeFailure, RangeSink, Transport};
use sb_test_support::{Agent, DbAgent, FileAgent, break_segment_commits, mend_segment_commits};

use common::{Bench, files_under};

/// Runs each named test against both agents.
macro_rules! for_both_agents {
    ($($test:ident),* $(,)?) => {
        mod files {
            $(
                #[test]
                fn $test() {
                    super::$test::<sb_test_support::FileAgent>();
                }
            )*
        }

        mod database {
            $(
                #[test]
                fn $test() {
                    super::$test::<sb_test_support::DbAgent>();
                }
            )*
        }
    };
}

for_both_agents!(
    first_sync_archives_every_item,
    appended_bytes_extend_the_version,
    a_rewrite_starts_a_new_version,
    a_truncation_starts_a_version_that_is_held_back,
    a_touch_stores_nothing,
    an_item_deleted_at_the_source_is_marked_gone_and_kept,
    an_item_that_comes_back_changed_is_a_new_version,
    a_pass_cancelled_at_each_step_resumes,
    a_pass_cancelled_inside_a_range_stores_none_of_it,
    a_crash_between_rename_and_commit_loses_nothing,
    a_failed_commit_is_retried_without_a_restart,
    a_location_runs_one_sync_at_a_time,
);

/// A bench whose agent has three sessions of different sizes.
fn bench<A: Agent + Default>() -> (Bench<A>, Vec<String>) {
    let mut agent = A::default();
    let keys = vec![
        agent.add_session(8),
        agent.add_session(3),
        agent.add_session(40),
    ];
    (Bench::new(agent), keys)
}

fn first_sync_archives_every_item<A: Agent + Default>() {
    let (bench, keys) = bench::<A>();
    let report = bench.sync();
    assert_eq!(
        (report.scanned, report.new_items, report.unchanged),
        (3, 3, 0)
    );
    let bytes: u64 = keys
        .iter()
        .map(|key| bench.agent.content(key).len() as u64)
        .sum();
    assert_eq!((report.bytes_fetched, report.bytes_stored), (bytes, bytes));
    for key in &keys {
        let versions = bench.versions(key);
        assert_eq!(versions.len(), 1, "{key}");
        assert_eq!(
            (
                versions[0].number,
                versions[0].reason,
                versions[0].held_back
            ),
            (1, Reason::FirstSync, false)
        );
        assert_eq!(bench.segments(&versions[0]).len(), 1);
        assert_eq!(bench.item(key).kind, ItemKind::Transcript);
    }
    bench.assert_mirrors(&keys);
    let location = bench.location();
    assert!(location.last_attempt.is_some() && location.last_success.is_some());
    assert_eq!(location.last_error, None);

    /* Nothing changed: the next pass skips everything. */
    let again = bench.sync();
    assert_eq!(
        (again.unchanged, again.new_items, again.bytes_fetched),
        (3, 0, 0)
    );
}

fn appended_bytes_extend_the_version<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    let before = bench.agent.content(&keys[0]).len() as u64;
    bench.agent.append(&keys[0], 5);
    let after = bench.agent.content(&keys[0]).len() as u64;

    let report = bench.sync();
    assert_eq!(
        (report.appended, report.unchanged, report.new_versions),
        (1, 2, 0)
    );
    assert_eq!(report.bytes_fetched, after - before);
    let versions = bench.versions(&keys[0]);
    assert_eq!(versions.len(), 1);
    let segments: Vec<_> = bench
        .segments(&versions[0])
        .iter()
        .map(|segment| (segment.offset, segment.length))
        .collect();
    assert_eq!(segments, [(0, before), (before, after - before)]);
    bench.assert_mirrors(&keys);
}

fn a_rewrite_starts_a_new_version<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    let before = bench.agent.content(&keys[1]);
    bench.agent.rewrite(&keys[1]);

    let report = bench.sync();
    assert_eq!(
        (report.new_versions, report.held_back, report.unchanged),
        (1, 0, 2)
    );
    let versions = bench.versions(&keys[1]);
    assert_eq!(versions.len(), 2);
    assert_eq!(
        (
            versions[1].number,
            versions[1].reason,
            versions[1].held_back
        ),
        (2, Reason::Rewritten, false)
    );
    /* The earlier version is as it was. */
    assert!(bench.read(&versions[0]) == before);
    bench.assert_mirrors(&keys);
}

fn a_truncation_starts_a_version_that_is_held_back<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    let before = bench.agent.content(&keys[2]);
    bench.agent.truncate(&keys[2], 10);

    let report = bench.sync();
    assert_eq!((report.new_versions, report.held_back), (1, 1));
    let versions = bench.versions(&keys[2]);
    assert_eq!(
        (versions[1].reason, versions[1].held_back),
        (Reason::Shrank, true)
    );
    assert!(bench.read(&versions[0]) == before);
    bench.assert_mirrors(&keys);

    /* What is appended after that extends the new version. */
    bench.agent.append(&keys[2], 2);
    assert_eq!(bench.sync().appended, 1);
    assert_eq!(bench.versions(&keys[2]).len(), 2);
    bench.assert_mirrors(&keys);
}

fn a_touch_stores_nothing<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    let segments = bench.segment_files();
    bench.agent.touch(&keys[0]);

    let report = bench.sync();
    assert_eq!(
        (report.touched, report.unchanged, report.bytes_fetched),
        (1, 2, 0)
    );
    assert_eq!(bench.segment_files(), segments);
    assert_eq!(bench.versions(&keys[0]).len(), 1);
    /* The new stamp was recorded: the pass after it has nothing to check. */
    let again = bench.sync();
    assert_eq!((again.touched, again.unchanged), (0, 3));
    bench.assert_mirrors(&keys);
}

fn an_item_deleted_at_the_source_is_marked_gone_and_kept<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    let before = bench.agent.content(&keys[0]);
    bench.agent.delete(&keys[0]);

    let report = bench.sync();
    assert_eq!((report.gone, report.scanned, report.unchanged), (1, 2, 2));
    let gone_since = bench.item(&keys[0]).gone_since;
    assert!(gone_since.is_some());
    let versions = bench.versions(&keys[0]);
    assert_eq!(versions.len(), 1);
    assert!(bench.read(&versions[0]) == before);

    /* It stays gone since the pass that first missed it. */
    assert_eq!(bench.sync().gone, 0);
    assert_eq!(bench.item(&keys[0]).gone_since, gone_since);
    bench.assert_mirrors(&keys[1..]);
}

fn an_item_that_comes_back_changed_is_a_new_version<A: Agent + Default>() {
    let mut agent = A::default();
    let key = agent.add_session(6);
    let mut bench = Bench::new(agent);
    bench.sync();
    let before = bench.agent.content(&key);
    bench.agent.delete(&key);
    bench.sync();

    bench.agent.recreate(&key, 2);
    let report = bench.sync();
    assert_eq!(report.new_versions, 1);
    assert_eq!(bench.item(&key).gone_since, None);
    let versions = bench.versions(&key);
    assert_eq!(versions[1].reason, Reason::Replaced);
    assert!(bench.read(&versions[0]) == before);
    bench.assert_mirrors(&[key]);
}

/// Every progress event of a sync that is left to finish.
fn steps<A: Agent>(bench: &Bench<A>) -> Vec<Progress> {
    let mut steps = Vec::new();
    bench
        .try_sync(&CancelToken::new(), &mut |progress| steps.push(*progress))
        .unwrap();
    steps
}

fn a_pass_cancelled_at_each_step_resumes<A: Agent + Default>() {
    /* The first sync, and a sync after every session grew, each stopped at every step. */
    for grown in [false, true] {
        let prepare = || {
            let (mut bench, keys) = bench::<A>();
            if grown {
                bench.sync();
                for key in &keys {
                    bench.agent.append(key, 3);
                }
            }
            (bench, keys)
        };
        let all = steps(&prepare().0);
        assert_eq!(all[0], Progress::Scanned { items: 3 });
        assert!(matches!(all[1], Progress::Planned { items: 3, .. }));
        assert_eq!(all.len(), 5);

        for stop_at in 0..all.len() {
            let (bench, keys) = prepare();
            let cancel = CancelToken::new();
            let mut seen = 0;
            let stopped = bench.try_sync(&cancel, &mut |_| {
                if seen == stop_at {
                    cancel.cancel();
                }
                seen += 1;
            });
            /* After the last item is stored there is nothing left to cancel. */
            if stop_at + 1 < all.len() {
                assert!(
                    matches!(stopped, Err(SyncError::Cancelled)),
                    "stopped at step {stop_at}: {stopped:?}"
                );
                assert_eq!(seen, stop_at + 1);
            }
            /* What was committed before the stop is whole. */
            bench.assert_verifies();

            let resumed = bench.sync();
            let stored_before = stop_at.saturating_sub(1);
            assert_eq!(
                resumed.new_items + resumed.appended,
                3 - stored_before.min(3),
                "resumed after step {stop_at}"
            );
            bench.assert_mirrors(&keys);
            assert_eq!(bench.segment_files().len(), bench.catalogued_segments());
        }
    }
}

/// A provider that cancels its pass as soon as the first bytes of a fetch arrive.
struct CancelsMidRange {
    inner: Box<dyn Provider>,
    cancel: CancelToken,
}

struct CancellingSource<'a> {
    inner: Box<dyn Source + 'a>,
    cancel: CancelToken,
}

struct CancellingSink<'a> {
    inner: &'a mut dyn RangeSink,
    cancel: CancelToken,
}

impl Provider for CancelsMidRange {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn detect<'a>(
        &self,
        transport: &'a dyn Transport,
    ) -> Result<Option<Box<dyn Source + 'a>>, ProviderError> {
        Ok(self.inner.detect(transport)?.map(|inner| {
            Box::new(CancellingSource {
                inner,
                cancel: self.cancel.clone(),
            }) as Box<dyn Source>
        }))
    }
}

impl Source for CancellingSource<'_> {
    fn scan(&mut self) -> Result<Vec<ScannedItem>, ProviderError> {
        self.inner.scan()
    }

    fn hash_prefixes(
        &mut self,
        requests: &[ItemPrefix],
    ) -> Result<Vec<Option<sb_core::hash::Hash>>, ProviderError> {
        self.inner.hash_prefixes(requests)
    }

    fn fetch(
        &mut self,
        requests: &[ItemRange],
        sink: &mut dyn RangeSink,
    ) -> Result<(), ProviderError> {
        self.inner.fetch(
            requests,
            &mut CancellingSink {
                inner: sink,
                cancel: self.cancel.clone(),
            },
        )
    }
}

impl RangeSink for CancellingSink<'_> {
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
        let taken = self.inner.data(request, bytes);
        self.cancel.cancel();
        taken
    }

    fn end(&mut self, request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        self.inner.end(request, result)
    }
}

fn a_pass_cancelled_inside_a_range_stores_none_of_it<A: Agent + Default>() {
    let (bench, keys) = bench::<A>();
    let cancel = CancelToken::new();
    let stopped = bench.try_sync_with(
        Box::new(CancelsMidRange {
            inner: bench.agent.provider(),
            cancel: cancel.clone(),
        }),
        &cancel,
        &mut |_| {},
    );
    assert!(matches!(stopped, Err(SyncError::Cancelled)), "{stopped:?}");
    /* The half-written segment is gone with the pass, and nothing was recorded. */
    assert_eq!(
        files_under(&bench.dir().join("tmp")),
        Vec::<std::path::PathBuf>::new()
    );
    assert_eq!(bench.segment_files(), Vec::<std::path::PathBuf>::new());
    assert!(bench.items().is_empty());
    /* A cancelled sync is not a failed one. */
    assert_eq!(bench.location().last_error, None);

    assert_eq!(bench.sync().new_items, 3);
    bench.assert_mirrors(&keys);
}

fn a_crash_between_rename_and_commit_loses_nothing<A: Agent + Default>() {
    /* Once on the first sync, where the item itself is not recorded yet, and once on an append
    to an item that is. */
    for grown in [false, true] {
        let (mut bench, keys) = bench::<A>();
        if grown {
            bench.sync();
            bench.agent.append(&keys[0], 4);
        }
        let recorded = bench.catalogued_segments();

        break_segment_commits(bench.dir());
        let crashed = bench.try_sync(&CancelToken::new(), &mut |_| {});
        assert!(
            matches!(crashed, Err(SyncError::Catalogue(_))),
            "{crashed:?}"
        );
        /* The segment reached its place and the catalogue never heard of it. */
        assert_eq!(bench.segment_files().len(), recorded + 1);
        assert_eq!(bench.catalogued_segments(), recorded);
        assert!(bench.location().last_error.is_some());
        mend_segment_commits(bench.dir());

        /* The next start sets it aside, deleting nothing. */
        bench.reopen();
        assert_eq!(bench.segment_files().len(), recorded);
        let set_aside = bench
            .orphans()
            .iter()
            .filter(|file| file.extension().is_some_and(|extension| extension == "zst"))
            .count();
        assert_eq!(set_aside, 1);

        /* And the next sync fetches the range again. */
        let report = bench.sync();
        assert_eq!(
            (report.new_items, report.appended),
            if grown { (0, 1) } else { (3, 0) }
        );
        bench.assert_mirrors(&keys);
        assert_eq!(bench.location().last_error, None);
    }
}

fn a_failed_commit_is_retried_without_a_restart<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    bench.agent.append(&keys[0], 4);
    break_segment_commits(bench.dir());
    bench
        .try_sync(&CancelToken::new(), &mut |_| {})
        .unwrap_err();
    mend_segment_commits(bench.dir());

    /* The file the failed pass left where the segment goes is set aside, not replaced. */
    assert_eq!(bench.sync().appended, 1);
    assert_eq!(bench.orphans().len(), 1);
    bench.assert_mirrors(&keys);
    assert_eq!(bench.segment_files().len(), bench.catalogued_segments());
}

fn a_location_runs_one_sync_at_a_time<A: Agent + Default>() {
    let (bench, keys) = bench::<A>();
    let mut refused = Vec::new();
    bench
        .try_sync(&CancelToken::new(), &mut |_| {
            refused.push(bench.try_sync(&CancelToken::new(), &mut |_| {}));
        })
        .unwrap();
    assert!(!refused.is_empty());
    assert!(
        refused
            .iter()
            .all(|second| matches!(second, Err(SyncError::Busy(_))))
    );
    /* The refusal is not recorded as the location's failure. */
    assert_eq!(bench.location().last_error, None);
    bench.assert_mirrors(&keys);
}

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
    assert_eq!(bench.segment_files(), Vec::<std::path::PathBuf>::new());
    bench.assert_mirrors(std::slice::from_ref(&key));
    assert_eq!(bench.sync().unchanged, 1);

    bench.agent.append(&key, 2);
    assert_eq!(bench.sync().appended, 1);
    bench.assert_mirrors(&[key]);
}

#[test]
fn every_file_of_a_session_is_archived_under_its_session() {
    let mut agent = FileAgent::new();
    agent.tree().record_bytes(4096);
    let transcript = agent.add_session(12);
    let session = agent.session(&transcript);
    let id = session.id.clone();
    let subagent = session.subagent(6);
    let tool_result = session.tool_result(300_000);
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
fn a_new_export_format_starts_a_new_version() {
    let mut agent = DbAgent::new();
    let key = agent.add_session(5);
    let mut bench = Bench::new(agent);
    bench.sync();
    assert_eq!(bench.versions(&key)[0].export_version, Some(1));

    bench.agent.export_version = 2;
    let report: PassReport = bench.sync();
    assert_eq!((report.new_versions, report.held_back), (1, 0));
    let versions = bench.versions(&key);
    assert_eq!(
        (versions[1].reason, versions[1].export_version),
        (Reason::Replaced, Some(2))
    );
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_location_without_the_agent_is_not_synced() {
    let bench = Bench::new(FileAgent::new());
    let passes = bench
        .try_sync_with(
            Box::new(sb_test_support::DbProvider::default()),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert!(passes.is_empty());
}
