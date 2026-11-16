//! Sync over fixtures (PLAN §12): the cases that apply to both shapes of source, each run
//! against both, the file-tree agent and the made-up agent that keeps a database.

mod common;

use std::fs;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::ArchiveError;
use sb_core::catalogue::{Held, Reason};
use sb_core::provider::ItemKind;
use sb_core::sync::{Progress, SyncError};
use sb_test_support::{Agent, break_segment_commits, mend_segment_commits};

use common::{Bench, Meddle, When, files_under, for_both_agents};

for_both_agents!(
    first_sync_archives_every_item,
    appended_bytes_extend_the_version,
    a_rewrite_starts_a_new_version,
    a_rewrite_of_the_same_size_starts_a_new_version,
    a_truncation_starts_a_version_that_is_held_back_until_it_outgrows_the_one_read,
    a_touch_stores_nothing,
    an_item_deleted_at_the_source_is_marked_gone_and_kept,
    an_item_that_comes_back_changed_is_a_new_version,
    a_pass_cancelled_at_each_step_resumes,
    a_pass_stopped_between_batches_keeps_the_batches_it_stored,
    a_pass_cancelled_inside_a_range_stores_none_of_it,
    a_crash_between_rename_and_commit_loses_nothing,
    a_commit_that_keeps_failing_sets_nothing_aside,
    a_file_left_where_another_segment_goes_is_set_aside,
    a_location_runs_one_sync_at_a_time,
    a_provider_that_sends_less_than_was_asked_stores_nothing,
    a_provider_that_breaks_the_order_of_ranges_stops_the_pass,
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
            (1, Reason::FirstSync, None)
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

    /* The append recorded what it saw of the source: the pass after it has nothing to do. */
    let again = bench.sync();
    assert_eq!((again.unchanged, again.touched, again.appended), (3, 0, 0));
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
        (2, Reason::Rewritten, None)
    );
    /* The earlier version is as it was. */
    assert!(bench.read(&versions[0]) == before);
    bench.assert_mirrors(&keys);
}

fn a_rewrite_of_the_same_size_starts_a_new_version<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    let before = bench.agent.content(&keys[1]);
    bench.agent.rewrite_same_size(&keys[1]);
    assert_eq!(bench.agent.content(&keys[1]).len(), before.len());

    /* Only its bytes tell it from a touch. */
    let report = bench.sync();
    assert_eq!((report.new_versions, report.touched), (1, 0));
    let versions = bench.versions(&keys[1]);
    assert_eq!(versions[1].reason, Reason::Rewritten);
    assert!(bench.read(&versions[0]) == before);
    bench.assert_mirrors(&keys);
}

fn a_truncation_starts_a_version_that_is_held_back_until_it_outgrows_the_one_read<
    A: Agent + Default,
>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    let before = bench.agent.content(&keys[2]);
    bench.agent.truncate(&keys[2], 10);

    let report = bench.sync();
    assert_eq!((report.new_versions, report.held_back), (1, 1));
    let versions = bench.versions(&keys[2]);
    assert_eq!(
        (versions[1].reason, versions[1].held_back),
        (Reason::Shrank, Some(Held::Shorter))
    );
    assert!(bench.read(&versions[0]) == before);
    bench.assert_mirrors(&keys);

    /* What is appended after that extends the new version, which is still the shorter one. */
    bench.agent.append(&keys[2], 2);
    assert_eq!(bench.sync().appended, 1);
    let versions = bench.versions(&keys[2]);
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[1].held_back, Some(Held::Shorter));

    /* Once it is as long as the version being read, it is no loss any more. */
    bench.agent.append(&keys[2], 12);
    assert_eq!(bench.sync().appended, 1);
    let versions = bench.versions(&keys[2]);
    assert!(versions[1].length > versions[0].length);
    assert_eq!(versions[1].held_back, None);
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
        /* Scanned, planned, each of the three ranges fetched, and the batch stored; then what
        there is to index, each of the three sessions indexed, and their text made
        searchable. */
        let all = steps(&prepare().0);
        assert_eq!(all[0], Progress::Scanned { items: 3 });
        assert!(matches!(all[1], Progress::Planned { items: 3, .. }));
        assert_eq!(all[5], Progress::Stored { items: 3 });
        assert!(matches!(all[6], Progress::Indexing { sessions: 3, .. }));
        assert!(matches!(all[9], Progress::Indexed { sessions: 3, .. }));
        assert_eq!(all[10], Progress::Searchable { sessions: 3 });
        assert_eq!(all.len(), 11);

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
            /* Once the last text is made searchable there is nothing left to cancel. */
            let stored = stop_at >= 5;
            if stop_at + 1 < all.len() {
                assert!(
                    matches!(stopped, Err(SyncError::Cancelled)),
                    "stopped at step {stop_at}: {stopped:?}"
                );
                assert_eq!(seen, stop_at + 1);
            }
            /* A stopped pass leaves nothing half done: no file in tmp/, none unrecorded. */
            assert!(files_under(&bench.dir().join("tmp")).is_empty());
            assert_eq!(bench.segment_files().len(), bench.catalogued_segments());
            bench.assert_verifies();

            let resumed = bench.sync();
            assert_eq!(
                resumed.new_items + resumed.appended,
                if stored { 0 } else { 3 },
                "resumed after step {stop_at}"
            );
            /* The sessions the stopped pass did not index are indexed by the next. */
            assert_eq!(
                resumed.index.sessions,
                3 - stop_at.saturating_sub(6).min(3),
                "resumed after step {stop_at}"
            );
            assert_eq!(bench.sessions().len(), 3);
            bench.assert_mirrors(&keys);
        }
    }
}

fn a_pass_stopped_between_batches_keeps_the_batches_it_stored<A: Agent + Default>() {
    let mut agent = A::default();
    let keys: Vec<_> = (0..600).map(|_| agent.add_session(1)).collect();
    let bench = Bench::new(agent);
    let cancel = CancelToken::new();
    let mut stored = Vec::new();
    let stopped = bench.try_sync(&cancel, &mut |progress| {
        if let Progress::Stored { items } = progress {
            stored.push(*items);
            cancel.cancel();
        }
    });
    assert!(matches!(stopped, Err(SyncError::Cancelled)), "{stopped:?}");
    /* One batch was stored, whole, and no other was begun. */
    assert_eq!(stored.len(), 1);
    assert_eq!(bench.items().len(), stored[0]);
    assert!((1..600).contains(&stored[0]));
    assert!(files_under(&bench.dir().join("tmp")).is_empty());

    let resumed = bench.sync();
    assert_eq!(
        (resumed.unchanged, resumed.new_items),
        (stored[0], 600 - stored[0])
    );
    bench.assert_mirrors(&keys);
}

fn a_pass_cancelled_inside_a_range_stores_none_of_it<A: Agent + Default>() {
    let (bench, keys) = bench::<A>();
    let cancel = CancelToken::new();
    let stopped = bench.try_sync_with(
        bench.meddling(Meddle::at(When::InsideARange, || cancel.cancel())),
        &cancel,
        &mut |_| {},
    );
    assert!(matches!(stopped, Err(SyncError::Cancelled)), "{stopped:?}");
    /* The half-written segment is gone with the pass, and nothing was recorded. */
    assert!(files_under(&bench.dir().join("tmp")).is_empty());
    assert!(bench.segment_files().is_empty());
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
        /* The batch that is in place when its commit fails: every new item, or the append. */
        let placed = if grown { 1 } else { 3 };

        break_segment_commits(bench.dir());
        let crashed = bench.pass_with(bench.agent.provider());
        assert!(
            matches!(crashed, Err(SyncError::Archive(ArchiveError::Catalogue(_)))),
            "{crashed:?}"
        );
        /* The segments reached their places and the catalogue never heard of them. */
        let left = bench.segment_files();
        let stored: Vec<_> = left.iter().map(|file| fs::read(file).unwrap()).collect();
        assert_eq!(left.len(), recorded + placed);
        assert_eq!(bench.catalogued_segments(), recorded);
        assert!(bench.location().last_error.is_some());
        mend_segment_commits(bench.dir());

        /* A start leaves them where they are. */
        bench.reopen();
        assert_eq!(bench.segment_files(), left);

        /* The next sync fetches the range again, finds its segments in place already, and
        records them: nothing is moved, and nothing set aside. */
        let report = bench.sync();
        assert_eq!(
            (report.new_items, report.appended),
            if grown { (0, 1) } else { (3, 0) }
        );
        assert_eq!(bench.segment_files(), left);
        for (file, stored) in left.iter().zip(&stored) {
            assert!(&fs::read(file).unwrap() == stored);
        }
        assert!(bench.orphans().is_empty());
        assert!(files_under(&bench.dir().join("tmp")).is_empty());
        bench.assert_mirrors(&keys);
        assert_eq!(bench.segment_files().len(), bench.catalogued_segments());
        assert_eq!(bench.location().last_error, None);
    }
}

fn a_commit_that_keeps_failing_sets_nothing_aside<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    bench.agent.append(&keys[0], 4);
    break_segment_commits(bench.dir());
    for _ in 0..3 {
        bench.pass_with(bench.agent.provider()).unwrap_err();
        assert!(bench.orphans().is_empty());
    }
    mend_segment_commits(bench.dir());

    /* Without a restart: the file the failed passes left is the segment, and is recorded. */
    assert_eq!(bench.sync().appended, 1);
    assert!(bench.orphans().is_empty());
    bench.assert_mirrors(&keys);
    assert_eq!(bench.segment_files().len(), bench.catalogued_segments());
}

fn a_file_left_where_another_segment_goes_is_set_aside<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    bench.agent.append(&keys[0], 4);
    break_segment_commits(bench.dir());
    bench.pass_with(bench.agent.provider()).unwrap_err();
    mend_segment_commits(bench.dir());
    let before = bench.segment_files();
    let left: Vec<_> = before
        .iter()
        .filter(|file| !file.ends_with("0000000000000000.zst"))
        .collect();
    assert_eq!(left.len(), 1);
    let stored = fs::read(left[0]).unwrap();

    /* The source grows again, so the range the next pass stores from the same offset is not
    the one whose file is there. That file is set aside with its bytes, never replaced. */
    bench.agent.append(&keys[0], 2);
    assert_eq!(bench.sync().appended, 1);
    let orphans = bench.orphans();
    assert_eq!(orphans.len(), 1);
    assert!(fs::read(&orphans[0]).unwrap() == stored);
    assert_eq!(bench.segment_files(), before);
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

fn a_provider_that_sends_less_than_was_asked_stores_nothing<A: Agent + Default>() {
    let (bench, keys) = bench::<A>();
    let report = bench.pass_with(bench.meddling(Meddle::DropsAByte)).unwrap();
    /* The first range ended well a byte short: that item is not stored, the others are. */
    assert_eq!((report.new_items, report.failed.len()), (2, 1));
    assert_eq!(bench.items().len(), 2);
    assert!(bench.location().last_error.is_some());
    bench.assert_verifies();

    assert_eq!(bench.sync().new_items, 1);
    bench.assert_mirrors(&keys);
}

fn a_provider_that_breaks_the_order_of_ranges_stops_the_pass<A: Agent + Default>() {
    let (bench, keys) = bench::<A>();
    let stopped = bench.pass_with(bench.meddling(Meddle::WrongRequest));
    assert!(
        matches!(stopped, Err(SyncError::Protocol(_))),
        "{stopped:?}"
    );
    assert!(bench.items().is_empty());
    assert!(files_under(&bench.dir().join("tmp")).is_empty());

    assert_eq!(bench.sync().new_items, 3);
    bench.assert_mirrors(&keys);
}

/// Every test here once more, over the shell transport.
#[cfg(unix)]
#[test]
fn the_same_with_the_locations_read_through_a_shell() {
    common::again_through_a_shell();
}
