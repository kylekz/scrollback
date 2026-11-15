//! What the archive and sync tests share: an agent's fixture beside an archive in a temp dir,
//! with one location that reads the fixture, and a provider that meddles with a pass at a
//! chosen moment.

#![allow(
    dead_code,
    reason = "each test binary uses its own part of this module"
)]

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::catalogue::{Item, Segment, Version};
use sb_core::location::{Location, LocationId, LocationKind};
use sb_core::provider::{
    Hashed, ItemPrefix, ItemRange, Provider, ProviderError, ProviderId, RangeFailure, Scan,
    SessionItems, Source,
};
use sb_core::sync::{Pass, PassReport, Progress, SyncError, sync_location};
use sb_core::transcript::Transcript;
use sb_core::transport::{Halt, RangeSink, Transport};
use sb_test_support::Agent;
use tempfile::TempDir;

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
        let mut passes = self
            .try_sync_with(provider, &CancelToken::new(), &mut |_| {})
            .unwrap();
        assert_eq!(passes.len(), 1);
        passes.remove(0).outcome
    }

    /// The fixture's provider, meddled with.
    pub fn meddling<'a>(&self, meddle: Meddle<'a>) -> Box<dyn Provider + 'a> {
        Box::new(Meddling {
            inner: self.agent.provider(),
            meddle,
        })
    }

    /// A sync that must succeed with nothing failed: its one pass.
    pub fn sync(&self) -> PassReport {
        let report = self.pass_with(self.agent.provider()).unwrap();
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(report.deferred.is_empty(), "{:?}", report.deferred);
        assert!(report.unlisted.is_empty(), "{:?}", report.unlisted);
        report
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

    pub fn assert_verifies(&self) {
        let report = self.archive().verify(&CancelToken::new()).unwrap();
        assert!(report.damaged.is_empty(), "{:?}", report.damaged);
        assert!(report.unread.is_empty(), "{:?}", report.unread);
        assert!(report.uncatalogued.is_empty(), "{:?}", report.uncatalogued);
    }

    /// Every segment file under the archive's `files/`.
    pub fn segment_files(&self) -> Vec<PathBuf> {
        files_under(&self.dir.path().join("files"))
            .into_iter()
            .filter(|file| file.extension().is_some_and(|extension| extension == "zst"))
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
}

impl<'a> Meddle<'a> {
    pub fn at(when: When, action: impl FnOnce() + Send + 'a) -> Self {
        Self::At(when, Mutex::new(Some(Box::new(action))))
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

    fn parse(&self, items: &dyn SessionItems) -> Transcript {
        self.inner.parse(items)
    }
}

impl Source for MeddlingSource<'_, '_> {
    fn scan(&mut self) -> Result<Scan, ProviderError> {
        self.meddle.reached(When::BeforeScan);
        let scan = self.inner.scan();
        self.meddle.reached(When::AfterScan);
        scan
    }

    fn hash_prefixes(
        &mut self,
        requests: &[ItemPrefix],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, ProviderError> {
        self.meddle.reached(When::BeforeProof);
        let mut hashes = self.inner.hash_prefixes(requests)?;
        match self.meddle {
            Meddle::FewerProofs => drop(hashes.pop()),
            Meddle::NoProofs => return Err(ProviderError::Halted),
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
