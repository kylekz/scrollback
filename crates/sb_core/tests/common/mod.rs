//! What the archive and sync tests share: an agent's fixture beside an archive in a temp dir,
//! with one location that reads the fixture, and a provider that meddles with a pass at a
//! chosen moment.

#![allow(
    dead_code,
    unused_macros,
    unused_imports,
    reason = "each test binary uses its own part of this module"
)]

#[cfg(unix)]
pub mod shell;

use std::fs;
use std::io::Read as _;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use sb_core::CancelToken;
use sb_core::archive::{Archive, ArchiveError, Deleted};
use sb_core::catalogue::{Item, Segment, Session, SessionId, Shelf, Version};
use sb_core::hash::Hash;
use sb_core::location::{Location, LocationId, LocationKind};
use sb_core::provider::{
    Hashed, ItemKind, ItemPrefix, ItemRange, Provider, ProviderError, ProviderId, RangeFailure,
    Scan, SessionItems, Source,
};
use sb_core::sync::{
    IndexReport, Pass, PassReport, Progress, SyncError, index, index_session, sync_location,
};
use sb_core::transcript::{Subagent, Transcript};
use sb_core::transport::{Halt, RangeSink, Transport};
use sb_test_support::Agent;
use tempfile::TempDir;

/// Runs each named test against both agents: the one that keeps files and the made-up one
/// that keeps a database.
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

pub(crate) use for_both_agents;

pub struct Bench<A> {
    pub agent: A,
    pub location: LocationId,
    dir: TempDir,
    archive: Option<Archive>,
}

impl<A: Agent> Bench<A> {
    pub fn new(agent: A) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::create(dir.path()).unwrap();
        let location = archive
            .catalogue()
            .add_location(LocationKind::Local, "fixture")
            .unwrap()
            .id;
        Self {
            agent,
            location,
            dir,
            archive: Some(archive),
        }
    }

    pub fn archive(&self) -> &Archive {
        self.archive.as_ref().unwrap()
    }

    /// The archive's folder.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Closes the archive, as quitting the app does.
    pub fn close(&mut self) {
        self.archive = None;
    }

    /// Closes the archive and opens it again, as a restart of the app does.
    pub fn reopen(&mut self) {
        self.close();
        self.archive = Some(Archive::open(self.dir.path()).unwrap());
    }

    /// A sync of the fixture's location with the fixture's provider.
    pub fn try_sync(
        &self,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(&Progress),
    ) -> Result<Vec<Pass>, SyncError> {
        self.try_sync_with(self.agent.provider(), cancel, progress)
    }

    pub fn try_sync_with(
        &self,
        provider: Box<dyn Provider + '_>,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(&Progress),
    ) -> Result<Vec<Pass>, SyncError> {
        sync_location(
            self.archive(),
            &self.location,
            &self.agent.transport(),
            &[provider],
            cancel,
            progress,
        )
    }

    /// The one pass of a sync with `provider` that was not cancelled, whatever it found.
    pub fn pass_with(&self, provider: Box<dyn Provider + '_>) -> Result<PassReport, SyncError> {
        self.pass(provider).outcome
    }

    /// The one pass of a sync with `provider`, and what was indexed after it.
    pub fn pass(&self, provider: Box<dyn Provider + '_>) -> Pass {
        let mut passes = self
            .try_sync_with(provider, &CancelToken::new(), &mut |_| {})
            .unwrap();
        assert_eq!(passes.len(), 1);
        passes.remove(0)
    }

    /// A sync with `provider` whose pass and index must both come through.
    pub fn synced_with(&self, provider: Box<dyn Provider + '_>) -> Synced {
        let pass = self.pass(provider);
        Synced {
            report: pass.outcome.unwrap(),
            index: pass.index.unwrap(),
        }
    }

    /// The fixture's provider, meddled with.
    pub fn meddling<'a>(&self, meddle: Meddle<'a>) -> Box<dyn Provider + 'a> {
        Box::new(Meddling {
            inner: self.agent.provider(),
            meddle,
        })
    }

    /// A sync that must succeed with nothing failed: its one pass.
    pub fn sync(&self) -> Synced {
        let report = self.synced_with(self.agent.provider());
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(report.deferred.is_empty(), "{:?}", report.deferred);
        assert!(report.unlisted.is_empty(), "{:?}", report.unlisted);
        report
    }

    /// An index of every session of the fixture's agent that is out of date, as a start runs.
    pub fn index(&self) -> IndexReport {
        self.index_with(self.agent.provider())
    }

    pub fn index_with(&self, provider: Box<dyn Provider + '_>) -> IndexReport {
        index(
            self.archive(),
            None,
            &[provider],
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap()
    }

    /// An index of the one session whose transcript is the item `key`, as follows a choice of
    /// the owner's.
    pub fn index_session(&self, key: &str) -> IndexReport {
        index_session(
            self.archive(),
            self.agent.provider().as_ref(),
            &self.session_id(key),
        )
        .unwrap()
    }

    /// The identity of the session whose transcript is the item `key`.
    pub fn session_id(&self, key: &str) -> SessionId {
        SessionId {
            location: self.location.clone(),
            provider: self.agent.provider().id().to_string(),
            key: self.item(key).session.unwrap(),
        }
    }

    /// The indexed session whose transcript is the item `key`.
    pub fn session(&self, key: &str) -> Session {
        self.archive()
            .catalogue()
            .session(&self.session_id(key))
            .unwrap()
            .unwrap_or_else(|| panic!("{key} is not indexed"))
    }

    /// Every indexed session, the one that did something last first.
    pub fn sessions(&self) -> Vec<Session> {
        self.archive().catalogue().sessions(Shelf::All).unwrap()
    }

    pub fn location(&self) -> Location {
        self.archive()
            .catalogue()
            .location(&self.location)
            .unwrap()
            .unwrap()
    }

    pub fn items(&self) -> Vec<Item> {
        self.archive()
            .catalogue()
            .items(&self.location, self.agent.provider().id().as_str())
            .unwrap()
    }

    pub fn item(&self, key: &str) -> Item {
        self.archive()
            .catalogue()
            .item(&self.location, self.agent.provider().id().as_str(), key)
            .unwrap()
            .unwrap_or_else(|| panic!("{key} is not archived"))
    }

    pub fn versions(&self, key: &str) -> Vec<Version> {
        self.archive()
            .catalogue()
            .versions(self.item(key).id)
            .unwrap()
    }

    pub fn segments(&self, version: &Version) -> Vec<Segment> {
        self.archive().catalogue().segments(version.id).unwrap()
    }

    /// The archived content of a version.
    pub fn read(&self, version: &Version) -> Vec<u8> {
        let mut content = Vec::new();
        self.archive()
            .read_version(version.id)
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        content
    }

    /// Asserts the newest version of each of `keys` holds exactly what the source holds now,
    /// with its segments end to end, and that the whole archive verifies.
    pub fn assert_mirrors(&self, keys: &[String]) {
        for key in keys {
            let newest = self.versions(key).pop().unwrap();
            let content = self.read(&newest);
            assert!(
                content == self.agent.content(key),
                "{key}: the archive holds {} bytes, the source {}",
                content.len(),
                self.agent.content(key).len()
            );
            assert_eq!(newest.length, content.len() as u64, "{key}");
            let mut end = 0;
            for segment in self.segments(&newest) {
                assert_eq!(segment.offset, end, "{key}");
                end += segment.length;
            }
            assert_eq!(end, newest.length, "{key}");
        }
        self.assert_verifies();
    }

    /// Asserts the archive verifies: every byte as recorded, nothing unrecorded or still to
    /// be removed, and the search index as the text it indexes.
    pub fn assert_verifies(&self) {
        let report = self.archive().verify(&CancelToken::new()).unwrap();
        assert!(report.damaged.is_empty(), "{:?}", report.damaged);
        assert!(report.unread.is_empty(), "{:?}", report.unread);
        assert!(report.uncatalogued.is_empty(), "{:?}", report.uncatalogued);
        assert!(report.removing.is_empty(), "{:?}", report.removing);
        assert!(!report.search_rebuilt);
        self.assert_search_index_intact();
    }

    /// FTS5's own check of the search index against the text it indexes, on a copy.
    pub fn assert_search_index_intact(&self) {
        let copy = tempfile::tempdir().unwrap();
        for suffix in ["", "-wal"] {
            let name = format!("search.db{suffix}");
            if self.dir().join(&name).exists() {
                fs::copy(self.dir().join(&name), copy.path().join(&name)).unwrap();
            }
        }
        toolbox_store::rusqlite::Connection::open(copy.path().join("search.db"))
            .unwrap()
            .execute(
                "INSERT INTO span_text (span_text, rank) VALUES ('integrity-check', 1)",
                [],
            )
            .unwrap();
    }

    /// Deletes the session `id` for good, as the owner does once the confirmation showed what
    /// that removes.
    pub fn delete_for_good(&self, id: &SessionId) -> Result<Deleted, ArchiveError> {
        let shown = self.archive().catalogue().deletion(id)?;
        self.archive().delete_for_good(&shown)
    }

    /// Every segment file under the archive's `files/`.
    pub fn segment_files(&self) -> Vec<PathBuf> {
        files_under(&self.dir.path().join("files"))
            .into_iter()
            .filter(|file| file.extension().is_some_and(|extension| extension == "zst"))
            .collect()
    }

    /// The segment files of the item `key`, in the order of their versions and offsets.
    pub fn segment_files_of(&self, key: &str) -> Vec<PathBuf> {
        /* An item's folder is named by the start of its key's hash (PLAN §4.1). */
        let folder = &Hash::of(key.as_bytes()).to_hex()[..32];
        self.segment_files()
            .into_iter()
            .filter(|file| file.iter().any(|part| part == folder))
            .collect()
    }

    /// Every file set aside under the archive's `orphans/`.
    pub fn orphans(&self) -> Vec<PathBuf> {
        files_under(&self.dir.path().join("orphans"))
    }

    /// How many segments the catalogue records.
    pub fn catalogued_segments(&self) -> usize {
        self.items()
            .iter()
            .flat_map(|item| self.archive().catalogue().versions(item.id).unwrap())
            .map(|version| self.segments(&version).len())
            .sum()
    }
}

/**
 * Runs this test binary's tests once more with every fixture location read through the
 * shell transport, by this machine's `sh` (PLAN §12). The run it starts has the shell named
 * in its environment and starts none itself; naming another there (`SB_TEST_SHELL=dash
 * cargo test`) runs the tests through that one.
 *
 * It is a test like any other, so a run that names tests runs it only if it is among them,
 * and then with every test of the binary: `SB_TEST_SHELL=sh cargo test <name>` is how to
 * run some of them through a shell.
 */
pub fn again_through_a_shell() {
    if sb_test_support::through_a_shell() {
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env(sb_test_support::SHELL, "sh")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "with the locations read through sh:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every file under `dir`, however deep; none when `dir` is missing.
pub fn files_under(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// What an item is, said again: from its key and what its provider says of it.
pub type Classifier = fn(&str, (ItemKind, Option<String>)) -> (ItemKind, Option<String>);

/// What a sync's one pass archived, with what was indexed after it.
#[derive(Debug)]
pub struct Synced {
    pub report: PassReport,
    pub index: IndexReport,
}

impl Deref for Synced {
    type Target = PassReport;

    fn deref(&self) -> &PassReport {
        &self.report
    }
}

/// A moment in a pass.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// After the agent was found at the location, before the scan.
    BeforeScan,
    /// After the scan, before anything is fetched.
    AfterScan,
    /// After the first bytes of the first range reached the engine.
    InsideARange,
    /// After a batch was fetched, before the engine has it proven.
    BeforeProof,
}

/// What a [`Meddling`] provider does to its pass.
pub enum Meddle<'a> {
    /// Runs something once, at a moment: the agent writing while the pass reads.
    At(When, Mutex<Option<Box<dyn FnOnce() + Send + 'a>>>),
    /// Hands the engine one byte less than the first range holds, and ends the range well.
    DropsAByte,
    /// Hands the engine the first range's bytes as those of the request after it.
    WrongRequest,
    /// Answers one proof fewer than it is asked for.
    FewerProofs,
    /// Fails when it is asked for proofs.
    NoProofs,
    /// Says its parser is of this version.
    ParserVersion(u32),
    /// Runs something once, as it parses a session for the `nth` time (from 1), before it
    /// does: the owner acting while an index parses.
    AtParse {
        nth: usize,
        parsed: AtomicUsize,
        action: Mutex<Option<Box<dyn FnOnce() + Send + 'a>>>,
    },
    /// Its parser stops, with a panic, on the session that has the item of this key.
    FaultyParser(String),
    /// Says its parser is of the next version, and of each item what this makes of its key and
    /// of what the provider says of it, in its scan as in its classifier.
    Classifier(Classifier),
    /// The same, with the parser's version as it was.
    SameParser(Classifier),
    /// Fails when it is asked for its second lot of proofs, as a connection that drops after
    /// a batch was stored: `asked` counts the lots.
    SecondProofsFail(AtomicUsize),
    /// Runs something before the first proofs it is asked for, and answers none of the second
    /// lot it is asked for: `asked` counts the lots.
    SecondProofsUnanswered {
        before_first: Mutex<Option<Box<dyn FnOnce() + Send + 'a>>>,
        asked: AtomicUsize,
    },
}

impl<'a> Meddle<'a> {
    pub fn at(when: When, action: impl FnOnce() + Send + 'a) -> Self {
        Self::At(when, Mutex::new(Some(Box::new(action))))
    }

    pub fn at_parse(nth: usize, action: impl FnOnce() + Send + 'a) -> Self {
        Self::AtParse {
            nth,
            parsed: AtomicUsize::new(0),
            action: Mutex::new(Some(Box::new(action))),
        }
    }

    pub fn second_proofs_unanswered(before_first: impl FnOnce() + Send + 'a) -> Self {
        Self::SecondProofsUnanswered {
            before_first: Mutex::new(Some(Box::new(before_first))),
            asked: AtomicUsize::new(0),
        }
    }

    /// Runs the action if this is its moment and it has not run.
    fn reached(&self, moment: When) {
        if let Self::At(when, action) = self
            && *when == moment
            && let Some(action) = action.lock().unwrap().take()
        {
            action();
        }
    }
}

/// A provider that passes everything to another, meddling as it goes.
struct Meddling<'a> {
    inner: Box<dyn Provider>,
    meddle: Meddle<'a>,
}

struct MeddlingSource<'s, 'a> {
    inner: Box<dyn Source + 's>,
    meddle: &'s Meddle<'a>,
}

struct MeddlingSink<'k, 'a> {
    inner: &'k mut dyn RangeSink,
    meddle: &'k Meddle<'a>,
    /// Whether the first bytes have gone by.
    begun: bool,
}

impl Provider for Meddling<'_> {
    fn id(&self) -> ProviderId {
        self.inner.id()
    }

    fn detect<'s>(
        &'s self,
        transport: &'s dyn Transport,
        scratch: &'s Path,
    ) -> Result<Option<Box<dyn Source + 's>>, ProviderError> {
        Ok(self.inner.detect(transport, scratch)?.map(|inner| {
            Box::new(MeddlingSource {
                inner,
                meddle: &self.meddle,
            }) as Box<dyn Source + 's>
        }))
    }

    fn parser_version(&self) -> u32 {
        match self.meddle {
            Meddle::ParserVersion(version) => version,
            Meddle::Classifier(_) => self.inner.parser_version() + 1,
            _ => self.inner.parser_version(),
        }
    }

    fn parse(&self, items: &dyn SessionItems) -> Transcript {
        if let Meddle::AtParse {
            nth,
            parsed,
            action,
        } = &self.meddle
            && parsed.fetch_add(1, Ordering::Relaxed) + 1 == *nth
            && let Some(action) = action.lock().unwrap().take()
        {
            action();
        }
        if let Meddle::FaultyParser(key) = &self.meddle
            && items.list().iter().any(|item| item.key == *key)
        {
            panic!("the test's parser can't read {key}");
        }
        self.inner.parse(items)
    }

    fn subagent(&self, items: &dyn SessionItems, item: &str) -> Subagent {
        self.inner.subagent(items, item)
    }

    fn classify(&self, key: &str) -> (ItemKind, Option<String>) {
        match &self.meddle {
            Meddle::Classifier(classify) | Meddle::SameParser(classify) => {
                classify(key, self.inner.classify(key))
            }
            _ => self.inner.classify(key),
        }
    }
}

impl Source for MeddlingSource<'_, '_> {
    fn scan(&mut self) -> Result<Scan, ProviderError> {
        self.meddle.reached(When::BeforeScan);
        let mut scan = self.inner.scan();
        /* A provider's scan and its classifier say the same of an item. */
        if let (Ok(scan), Meddle::Classifier(classify) | Meddle::SameParser(classify)) =
            (&mut scan, self.meddle)
        {
            for item in &mut scan.items {
                (item.kind, item.session) = classify(&item.key, (item.kind, item.session.take()));
            }
        }
        self.meddle.reached(When::AfterScan);
        scan
    }

    fn hash_prefixes(
        &mut self,
        requests: &[ItemPrefix],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, ProviderError> {
        self.meddle.reached(When::BeforeProof);
        if let Meddle::SecondProofsUnanswered {
            before_first,
            asked,
        } = self.meddle
        {
            match asked.fetch_add(1, Ordering::Relaxed) {
                0 => before_first
                    .lock()
                    .unwrap()
                    .take()
                    .into_iter()
                    .for_each(|run| run()),
                1 => return Ok(Vec::new()),
                _ => {}
            }
        }
        let mut hashes = self.inner.hash_prefixes(requests)?;
        match &self.meddle {
            Meddle::FewerProofs => drop(hashes.pop()),
            Meddle::NoProofs => return Err(ProviderError::Halted),
            Meddle::SecondProofsFail(asked) if asked.fetch_add(1, Ordering::Relaxed) == 1 => {
                return Err(ProviderError::Halted);
            }
            _ => {}
        }
        Ok(hashes)
    }

    fn fetch(
        &mut self,
        requests: &[ItemRange],
        sink: &mut dyn RangeSink,
    ) -> Result<(), ProviderError> {
        self.inner.fetch(
            requests,
            &mut MeddlingSink {
                inner: sink,
                meddle: self.meddle,
                begun: false,
            },
        )
    }

    fn retention_days(&mut self) -> Result<Option<u32>, ProviderError> {
        self.inner.retention_days()
    }
}

impl RangeSink for MeddlingSink<'_, '_> {
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
        let first = !std::mem::replace(&mut self.begun, true);
        let taken = match self.meddle {
            Meddle::DropsAByte if first => self.inner.data(request, &bytes[1..]),
            Meddle::WrongRequest if first => self.inner.data(request + 1, bytes),
            _ => self.inner.data(request, bytes),
        };
        if first {
            self.meddle.reached(When::InsideARange);
        }
        taken
    }

    fn end(&mut self, request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        self.inner.end(request, result)
    }
}
