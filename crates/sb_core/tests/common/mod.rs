//! What the archive and sync tests share: an agent's fixture beside an archive in a temp dir,
//! with one location that reads the fixture.

#![allow(
    dead_code,
    reason = "each test binary uses its own part of this module"
)]

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::catalogue::{Item, Segment, Version};
use sb_core::location::{Location, LocationId, LocationKind};
use sb_core::provider::Provider;
use sb_core::sync::{PassReport, Progress, SyncError, sync_location};
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

    /// Closes the archive and opens it again, as a restart of the app does.
    pub fn reopen(&mut self) {
        self.archive = None;
        self.archive = Some(Archive::open(self.dir.path()).unwrap());
    }

    /// A sync of the fixture's location with the fixture's provider.
    pub fn try_sync(
        &self,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(&Progress),
    ) -> Result<Vec<PassReport>, SyncError> {
        self.try_sync_with(self.agent.provider(), cancel, progress)
    }

    pub fn try_sync_with(
        &self,
        provider: Box<dyn Provider>,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(&Progress),
    ) -> Result<Vec<PassReport>, SyncError> {
        sync_location(
            self.archive(),
            &self.location,
            &self.agent.transport(),
            &[provider],
            cancel,
            progress,
        )
    }

    /// A sync that must succeed with nothing failed: its one pass.
    pub fn sync(&self) -> PassReport {
        let mut passes = self.try_sync(&CancelToken::new(), &mut |_| {}).unwrap();
        assert_eq!(passes.len(), 1);
        let pass = passes.remove(0);
        assert!(pass.failed.is_empty(), "{:?}", pass.failed);
        pass
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
            .items(&self.location, self.agent.provider().id())
            .unwrap()
    }

    pub fn item(&self, key: &str) -> Item {
        self.archive()
            .catalogue()
            .item(&self.location, self.agent.provider().id(), key)
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
