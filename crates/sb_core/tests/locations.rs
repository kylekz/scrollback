//! The owner's changes to a location (FEATURES: Locations; DESIGN §4.11): renaming and
//! removing one, adding a removed place again, and what the archive says it holds from each.

mod common;

use pretty_assertions::assert_eq;
use sb_core::catalogue::{Holding, Shelf};
use sb_core::location::LocationKind;
use sb_core::sync::survey;
use sb_test_support::Agent;

use common::{Bench, for_both_agents};

for_both_agents!(
    removing_a_location_keeps_everything_archived_from_it,
    a_removed_place_added_again_carries_on_from_its_archive,
    the_archive_says_what_it_holds_from_each_agent_there,
    a_survey_says_how_much_a_first_sync_copies,
);

fn bench<A: Agent + Default>() -> Bench<A> {
    let mut agent = A::default();
    agent.add_session(8);
    agent.add_session(3);
    let bench = Bench::new(agent);
    bench.sync();
    bench
}

fn removing_a_location_keeps_everything_archived_from_it<A: Agent + Default>() {
    let bench = bench::<A>();
    let catalogue = bench.archive().catalogue();
    let before = bench.sessions();
    catalogue.set_starred(&before[0].id, true).unwrap();
    let held = catalogue.holdings().unwrap();

    catalogue
        .rename_location(&bench.location, "renamed")
        .unwrap();
    catalogue.remove_location(&bench.location).unwrap();
    let removed = bench.location();
    assert_eq!(removed.name, "renamed");
    let when = removed.removed.expect("the location is marked removed");
    /* Removing it again keeps when it first was. */
    catalogue.remove_location(&bench.location).unwrap();
    assert_eq!(bench.location().removed, Some(when));

    /* Its sessions, what the owner said of them, and every stored byte are as they were. */
    let after = bench.sessions();
    assert_eq!(after.len(), before.len());
    assert!(after.iter().any(|session| session.starred));
    assert_eq!(catalogue.library_counts().unwrap().all, before.len());
    assert_eq!(catalogue.holdings().unwrap(), held);
    assert_eq!(catalogue.locations().unwrap(), [removed]);
    bench.assert_verifies();
}

fn a_removed_place_added_again_carries_on_from_its_archive<A: Agent + Default>() {
    let mut bench = bench::<A>();
    let catalogue = bench.archive().catalogue().clone();
    catalogue.remove_location(&bench.location).unwrap();
    catalogue
        .restore_location(&bench.location, "back again")
        .unwrap();
    let restored = bench.location();
    assert_eq!(
        (restored.removed, restored.name.as_str()),
        (None, "back again")
    );
    assert_eq!(restored.kind, LocationKind::Local);

    /* Its next sync finds what it archived before: nothing is archived twice. */
    let report = bench.sync().report;
    assert_eq!(report.new_items, 0);
    assert_eq!(report.unchanged, report.scanned);
    bench.agent.add_session(2);
    assert_eq!(bench.sync().report.new_items, 1);
    assert_eq!(bench.sessions().len(), 3);
}

fn the_archive_says_what_it_holds_from_each_agent_there<A: Agent + Default>() {
    let bench = bench::<A>();
    let catalogue = bench.archive().catalogue();
    let provider = bench.agent.provider();
    let bytes: u64 = bench
        .items()
        .iter()
        .flat_map(|item| catalogue.versions(item.id).unwrap())
        .map(|version| version.length)
        .sum();
    let holdings = catalogue.holdings().unwrap();
    assert_eq!(
        holdings,
        [Holding {
            location: bench.location.clone(),
            provider: provider.id().to_string(),
            sessions: 2,
            bytes,
            since: holdings[0].since,
        }]
    );
    assert!(holdings[0].since.is_some());
    assert!(bytes > 0);

    /* And where the agent keeps its data there, as the pass found it. */
    let found = catalogue.providers_at(&bench.location).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].provider, provider.id().as_str());
    let survey = survey(bench.archive(), &bench.agent.transport(), &[provider]);
    let Ok(surveyed) = &survey[0] else {
        panic!("the agent is found: {survey:?}");
    };
    assert_eq!(found[0].root.as_deref(), Some(surveyed.root.as_str()));
    assert_eq!(found[0].retention_days, surveyed.retention_days);
    assert_eq!(catalogue.sessions(Shelf::All).unwrap().len(), 2);
}

fn a_survey_says_how_much_a_first_sync_copies<A: Agent + Default>() {
    let mut agent = A::default();
    agent.add_session(8);
    agent.add_session(3);
    let bench = Bench::new(agent);
    let survey = survey(
        bench.archive(),
        &bench.agent.transport(),
        &[bench.agent.provider()],
    );
    let Ok(found) = &survey[0] else {
        panic!("the agent is found: {survey:?}");
    };
    assert_eq!(found.sessions, 2);
    /* What the first sync then stores is that many bytes. */
    let report = bench.sync().report;
    assert_eq!(found.bytes, report.bytes_stored);
}
