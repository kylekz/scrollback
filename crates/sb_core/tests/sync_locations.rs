//! Locations and providers together (PLAN §5): several agents at one location, one after
//! another, several locations at once, and a location that can't be reached.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::location::LocationKind;
use sb_core::provider::{Hashed, Provider, ProviderError, ProviderId, RangeFailure, Source};
use sb_core::sync::{Pass, SyncError, sync_location};
use sb_core::transport::{
    Env, FileStat, Listing, PrefixRequest, RangeRequest, RangeSink, Transport, TransportError,
};
use sb_test_support::{Agent, DbAgent, FileAgent, HomeTransport};

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

/// What `ssh` says of a host that does not answer.
const ASLEEP: &str = "ssh: connect to host macbook.local port 22: Operation timed out";

/// A location on a machine that goes to sleep: it is read as `inner` reads it until `asleep`
/// is set, and can't be reached from then on. With `after_proofs`, it falls asleep once it
/// has proven a batch.
struct Sleepy {
    inner: HomeTransport,
    asleep: AtomicBool,
    after_proofs: bool,
}

impl Sleepy {
    fn awake(&self) -> Result<&HomeTransport, TransportError> {
        if self.asleep.load(Ordering::Relaxed) {
            Err(TransportError::Unreachable(ASLEEP.to_string()))
        } else {
            Ok(&self.inner)
        }
    }
}

impl Transport for Sleepy {
    fn env(&self, names: &[&str]) -> Result<Env, TransportError> {
        self.awake()?.env(names)
    }

    fn stat(&self, paths: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        self.awake()?.stat(paths)
    }

    fn list(&self, root: &str) -> Result<Listing, TransportError> {
        self.awake()?.list(root)
    }

    fn hash_prefixes(
        &self,
        requests: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError> {
        let hashes = self.awake()?.hash_prefixes(requests);
        self.asleep.store(self.after_proofs, Ordering::Relaxed);
        hashes
    }

    fn read_ranges(
        &self,
        requests: &[RangeRequest],
        sink: &mut dyn RangeSink,
    ) -> Result<(), TransportError> {
        self.awake()?.read_ranges(requests, sink)
    }

    fn read_small(&self, path: &str) -> Result<Option<Vec<u8>>, TransportError> {
        self.awake()?.read_small(path)
    }
}

/// A sync of the bench's location through `transport`.
fn sync_through(bench: &Bench<FileAgent>, transport: &Sleepy) -> Result<Vec<Pass>, SyncError> {
    sync_location(
        bench.archive(),
        &bench.location,
        transport,
        &[bench.agent.provider()],
        &CancelToken::new(),
        &mut |_| {},
    )
}

#[test]
fn a_location_that_cannot_be_reached_is_unreachable_and_that_is_no_error() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(5);
    let mut bench = Bench::new(agent);
    let asleep = Sleepy {
        inner: bench.agent.transport(),
        asleep: AtomicBool::new(true),
        after_proofs: false,
    };

    /* Never reached yet: there is no success to show, and no error either. */
    let tried = sync_through(&bench, &asleep);
    assert!(
        matches!(&tried, Err(SyncError::Unreachable(reason)) if reason == ASLEEP),
        "{tried:?}"
    );
    let location = bench.location();
    assert!(location.last_attempt.is_some());
    assert_eq!((location.last_success, location.last_error), (None, None));
    let unreachable = location.unreachable.unwrap();
    assert_eq!(unreachable.reason, ASLEEP);

    /* Awake: the sync that reaches it clears the state. */
    bench.sync();
    let synced = bench.location();
    assert_eq!(synced.unreachable, None);
    assert!(synced.last_success.is_some());

    /* Asleep again, try after try: unreachable since the first of them, and what the last
    sync that reached it left stays as it was. */
    bench.agent.append(&key, 2);
    sync_through(&bench, &asleep).unwrap_err();
    let first = bench.location();
    thread::sleep(std::time::Duration::from_millis(5));
    sync_through(&bench, &asleep).unwrap_err();
    let second = bench.location();
    assert_eq!(second.unreachable, first.unreachable);
    assert!(first.unreachable.unwrap().since >= synced.last_success.unwrap());
    assert!(second.last_attempt > first.last_attempt);
    assert_eq!(
        (second.last_success, second.last_error),
        (synced.last_success, None)
    );

    assert_eq!(bench.sync().appended, 1);
    assert_eq!(bench.location().unreachable, None);
    bench.assert_mirrors(&[key]);
}

#[test]
fn a_location_that_drops_away_mid_pass_keeps_what_was_committed() {
    /* More items than one batch holds. */
    let mut agent = FileAgent::new();
    let keys: Vec<_> = (0..300).map(|_| agent.add_session(1)).collect();
    let bench = Bench::new(agent);
    let dozing = Sleepy {
        inner: bench.agent.transport(),
        asleep: AtomicBool::new(false),
        after_proofs: true,
    };

    let dropped = sync_through(&bench, &dozing);
    assert!(
        matches!(dropped, Err(SyncError::Unreachable(_))),
        "{dropped:?}"
    );
    /* The batch that was proven is archived; the one in flight is not, and nothing of it is
    in the way. */
    let archived = bench.items().len();
    assert!(archived > 0 && archived < keys.len(), "{archived}");
    assert_eq!(bench.segment_files().len(), archived);
    bench.assert_verifies();
    let location = bench.location();
    assert!(location.unreachable.is_some());
    assert_eq!((location.last_success, location.last_error), (None, None));

    let report = bench.sync();
    assert_eq!(
        (report.unchanged, report.new_items),
        (archived, keys.len() - archived)
    );
    assert_eq!(bench.location().unreachable, None);
    bench.assert_mirrors(&keys);
}

/// Every test here once more, over the shell transport.
#[cfg(unix)]
#[test]
fn the_same_with_the_locations_read_through_a_shell() {
    common::again_through_a_shell();
}
