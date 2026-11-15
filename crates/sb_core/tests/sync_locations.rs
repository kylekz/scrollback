//! Locations and providers together (PLAN §5): several agents at one location, one after
//! another, and several locations at once.

mod common;

use std::path::Path;
use std::thread;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::location::LocationKind;
use sb_core::provider::{Provider, ProviderError, ProviderId, Source};
use sb_core::sync::{Pass, SyncError, sync_location};
use sb_core::transport::{Transport, TransportError};
use sb_test_support::{Agent, DbAgent, FileAgent};

use common::Bench;

/// A provider whose agent can't even be looked for.
struct Unreachable;

impl Provider for Unreachable {
    fn id(&self) -> ProviderId {
        const ID: ProviderId = ProviderId::new("unreachable");
        ID
    }

    fn detect<'a>(
        &'a self,
        _transport: &'a dyn Transport,
        _scratch: &'a Path,
    ) -> Result<Option<Box<dyn Source + 'a>>, ProviderError> {
        Err(TransportError::NoHome.into())
    }
}

/// A bench on a home folder that holds both agents' data, and the database agent.
fn two_agents() -> (Bench<FileAgent>, DbAgent, Vec<String>) {
    let mut files = FileAgent::new();
    let mut database = DbAgent::in_home(files.tree().home());
    let keys = vec![files.add_session(5), database.add_session(7)];
    (Bench::new(files), database, keys)
}

fn sync(bench: &Bench<FileAgent>, providers: &[Box<dyn Provider>]) -> Vec<Pass> {
    sync_location(
        bench.archive(),
        &bench.location,
        &bench.agent.transport(),
        providers,
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap()
}

#[test]
fn each_agent_at_a_location_is_synced_in_turn() {
    let (bench, database, keys) = two_agents();
    let passes = sync(&bench, &[bench.agent.provider(), database.provider()]);
    assert_eq!(passes.len(), 2);
    for (pass, agent) in passes.iter().zip([&bench.agent as &dyn Agent, &database]) {
        assert_eq!(pass.provider, agent.provider().id());
        assert_eq!(pass.outcome.as_ref().unwrap().new_items, 1);
    }
    /* Each provider's items are its own, in its own folder of the location. */
    let catalogue = bench.archive().catalogue();
    for (key, agent) in keys.iter().zip([&bench.agent as &dyn Agent, &database]) {
        let provider = agent.provider().id();
        let items = catalogue.items(&bench.location, provider.as_str()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(&items[0].key, key);
        assert!(
            bench
                .dir()
                .join("files")
                .join(bench.location.as_str())
                .join(provider.as_str())
                .is_dir()
        );
    }
    bench.assert_verifies();
    let found = catalogue.providers_found(&bench.location).unwrap();
    assert_eq!(found.len(), 2);
}

#[test]
fn one_providers_failure_does_not_keep_the_others_from_their_passes() {
    let (bench, database, _) = two_agents();
    let passes = sync(
        &bench,
        &[
            Box::new(Unreachable),
            bench.agent.provider(),
            database.provider(),
        ],
    );
    assert_eq!(passes.len(), 3);
    assert!(matches!(passes[0].outcome, Err(SyncError::Provider(_))));
    assert_eq!(passes[1].outcome.as_ref().unwrap().new_items, 1);
    assert_eq!(passes[2].outcome.as_ref().unwrap().new_items, 1);
    /* The location has not synced cleanly, and says which provider failed. */
    let location = bench.location();
    assert_eq!(location.last_success, None);
    let error = location.last_error.unwrap();
    assert!(error.starts_with("unreachable: "), "{error}");
}

#[test]
fn locations_sync_at_the_same_time() {
    let dir = tempfile::tempdir().unwrap();
    let archive = Archive::create(dir.path()).unwrap();
    let agents: Vec<_> = (0..4)
        .map(|n| {
            let mut agent = FileAgent::new();
            let keys: Vec<_> = (0..6)
                .map(|records| agent.add_session(records + n))
                .collect();
            let location = archive
                .catalogue()
                .add_location(LocationKind::Local, &format!("fixture {n}"))
                .unwrap()
                .id;
            (agent, location, keys)
        })
        .collect();

    thread::scope(|scope| {
        for (agent, location, _) in &agents {
            let archive = &archive;
            scope.spawn(move || {
                let passes = sync_location(
                    archive,
                    location,
                    &agent.transport(),
                    &[agent.provider()],
                    &CancelToken::new(),
                    &mut |_| {},
                )
                .unwrap();
                assert_eq!(passes[0].outcome.as_ref().unwrap().new_items, 6);
            });
        }
    });

    for (agent, location, keys) in &agents {
        let catalogue = archive.catalogue();
        for key in keys {
            let item = catalogue
                .item(location, agent.provider().id().as_str(), key)
                .unwrap()
                .unwrap();
            let version = catalogue.versions(item.id).unwrap().remove(0);
            let mut content = Vec::new();
            std::io::Read::read_to_end(
                &mut archive.read_version(version.id).unwrap(),
                &mut content,
            )
            .unwrap();
            assert!(content == agent.content(key), "{key}");
        }
    }
    let report = archive.verify(&CancelToken::new()).unwrap();
    assert_eq!((report.versions, report.damaged.len()), (24, 0));
}
