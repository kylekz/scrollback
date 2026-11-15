//! Sync over fixtures (PLAN §12): the cases that apply to both shapes of source, each run
//! against both, the file-tree agent and the made-up agent that keeps a database.

mod common;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::catalogue::Reason;
use sb_core::provider::ItemKind;
use sb_core::sync::{Progress, SyncError};
use sb_test_support::{Agent, break_segment_commits, mend_segment_commits};

use common::{Bench, Meddle, When, files_under};

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
    a_rewrite_of_the_same_size_starts_a_new_version,
    a_truncation_starts_a_version_that_is_held_back_until_it_outgrows_the_one_read,
    a_touch_stores_nothing,
    an_item_deleted_at_the_source_is_marked_gone_and_kept,
    an_item_that_comes_back_changed_is_a_new_version,
    a_pass_cancelled_at_each_step_resumes,
    a_pass_cancelled_inside_a_range_stores_none_of_it,
    a_crash_between_rename_and_commit_loses_nothing,
    a_failed_commit_is_retried_without_a_restart,
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
        (2, Reason::Rewritten, false)
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
        (Reason::Shrank, true)
    );
    assert!(bench.read(&versions[0]) == before);
    bench.assert_mirrors(&keys);

    /* What is appended after that extends the new version, which is still the shorter one. */
    bench.agent.append(&keys[2], 2);
    assert_eq!(bench.sync().appended, 1);
    let versions = bench.versions(&keys[2]);
    assert_eq!(versions.len(), 2);
    assert!(versions[1].held_back);

    /* Once it is as long as the version being read, it is no loss any more. */
    bench.agent.append(&keys[2], 12);
    assert_eq!(bench.sync().appended, 1);
    let versions = bench.versions(&keys[2]);
    assert!(versions[1].length > versions[0].length);
    assert!(!versions[1].held_back);
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

        break_segment_commits(bench.dir());
        let crashed = bench.pass_with(bench.agent.provider());
        assert!(
            matches!(crashed, Err(SyncError::Catalogue(_))),
            "{crashed:?}"
        );
        /* The segment reached its place and the catalogue never heard of it. */
        assert_eq!(bench.segment_files().len(), recorded + 1);
        assert_eq!(bench.catalogued_segments(), recorded);
        assert!(bench.location().last_error.is_some());
        mend_segment_commits(bench.dir());

        /* A start sets it aside, unless the catalogue is still empty: then nothing under
        files/ is touched, and the segment is set aside when the next one takes its place.
        Nothing is deleted either way. */
        bench.reopen();
        assert_eq!(
            bench.segment_files().len(),
            if grown { recorded } else { recorded + 1 }
        );

        /* And the next sync fetches the range again. */
        let report = bench.sync();
        assert_eq!(
            (report.new_items, report.appended),
            if grown { (0, 1) } else { (3, 0) }
        );
        let set_aside = bench
            .orphans()
            .iter()
            .filter(|file| file.extension().is_some_and(|extension| extension == "zst"))
            .count();
        assert_eq!(set_aside, 1);
        bench.assert_mirrors(&keys);
        assert_eq!(bench.segment_files().len(), bench.catalogued_segments());
        assert_eq!(bench.location().last_error, None);
    }
}

fn a_failed_commit_is_retried_without_a_restart<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    bench.agent.append(&keys[0], 4);
    break_segment_commits(bench.dir());
    bench.pass_with(bench.agent.provider()).unwrap_err();
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
