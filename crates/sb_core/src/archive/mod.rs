//! The archive on disk (PLAN §4): the folder scrollback owns, holding the stored bytes and the
//! catalogue. [`layout`] says where things are, [`segment`] writes and reads the stored bytes,
//! [`disk`] gets files into place and durable, [`verify`] re-hashes everything against the
//! catalogue, and [`items`] hands a session's archived items to its provider's parser.
//!
//! What keeps archived bytes safe (PLAN §2.3):
//! - a segment is a new file, written whole before the catalogue records it, and it is moved
//!   into place with a move that fails if a file is already there ([`Archive::store`]), so the
//!   file system itself refuses to replace one;
//! - nothing here deletes a file under `files/`, and none is ever moved without being looked
//!   at. A file the catalogue doesn't record (a store that was interrupted between the move
//!   and the commit, or a catalogue restored from an older backup) stays where it is until a
//!   segment is to be placed there. If it holds the very bytes being placed, it is kept and
//!   recorded; if not, it is set aside in `orphans/`. [`Archive::verify`] reports all there
//!   are, and [`Archive::set_aside_uncatalogued`] sets them aside when the owner asks;
//! - a catalogue is only ever used with the archive it was made for: the archive's id is in
//!   `archive.json` and in the catalogue, an archive whose catalogue is missing or another's
//!   is not opened, and one is not created over a folder that holds a catalogue or files.

mod disk;
mod items;
mod layout;
mod segment;
mod verify;

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError, RwLock};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::catalogue::{self, Catalogue, CatalogueError, Commit, Recorded, Stored};
use crate::location::LocationId;

pub use items::ArchivedSession;
pub(crate) use layout::item_folder;
pub use segment::{Problem, SEGMENT_BYTES, VersionReader};
pub(crate) use segment::{RangeWriter, Sealed};
pub use verify::{Damage, Unread, VerifyReport};

/// The layout this build writes, in `archive.json`.
const FORMAT: u32 = 1;

#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error("could not {action} {}: {source}", .path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The folder has no `archive.json`. One is never created in its place silently (PLAN §4.4).
    #[error("{} holds no archive", .0.display())]
    NotAnArchive(PathBuf),
    #[error("{} already holds an archive", .0.display())]
    AlreadyAnArchive(PathBuf),
    /// The folder has no `archive.json` but holds an archive's catalogue or files. It is not
    /// taken for the place of a new archive.
    #[error(
        "{} holds the catalogue or the files of an archive without its archive.json; a new \
         archive is not created over them",
        .0.display()
    )]
    HoldsAnArchivesFiles(PathBuf),
    #[error("the archive at {} is open in another scrollback", .0.display())]
    Locked(PathBuf),
    #[error("{} is not an archive's manifest: {source}", .path.display())]
    Manifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "the archive at {} was made by a newer scrollback (format {found}; this one reads {FORMAT})",
        .dir.display()
    )]
    NewerFormat { dir: PathBuf, found: u32 },
    /// The archive's catalogue is gone. A new one is not started in its place: that would make
    /// everything archived look unknown.
    #[error(
        "the archive at {} has lost its catalogue.{}",
        .dir.display(),
        match .newest_backup {
            Some(backup) => format!(" The newest backup of it is {}.", backup.display()),
            None => " There is no backup of it.".to_string(),
        }
    )]
    CatalogueMissing {
        dir: PathBuf,
        newest_backup: Option<PathBuf>,
    },
    /// The catalogue in the archive's folder was made for another archive, or for none.
    #[error(
        "the catalogue in {} is not this archive's (the archive is {archive}; the catalogue is {})",
        .dir.display(),
        .catalogue.as_deref().unwrap_or("no archive's")
    )]
    AnothersCatalogue {
        dir: PathBuf,
        archive: String,
        catalogue: Option<String>,
    },
    /// A segment was about to be stored where the catalogue already records one.
    #[error("{} is already archived and is never overwritten", .0.display())]
    SegmentExists(PathBuf),
    /// The catalogue records nothing while `files/` holds files: it is this archive's from
    /// before its first sync, back from a backup. Setting aside what it doesn't record would
    /// empty the archive.
    #[error(
        "the catalogue of {} records nothing, so nothing under its files/ is set aside",
        .0.display()
    )]
    NothingRecorded(PathBuf),
    #[error("version {0} is not in the catalogue")]
    UnknownVersion(i64),
    #[error("verify was cancelled")]
    Cancelled,
    #[error(transparent)]
    Catalogue(#[from] CatalogueError),
}

type Result<T, E = ArchiveError> = std::result::Result<T, E>;

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> ArchiveError {
    let path = path.to_path_buf();
    move |source| ArchiveError::Io {
        action,
        path,
        source,
    }
}

/// `archive.json`.
#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    id: String,
    /// Unix milliseconds.
    created: i64,
}

/// An item's `source.json`: enough to say what its folder holds without the catalogue.
#[derive(Serialize)]
struct SourceManifest<'a> {
    provider: &'a str,
    location: &'a str,
    key: &'a str,
}

/// An open archive. Only one can be open on a folder at a time (PLAN §4.4): the lock is held
/// until this is dropped.
#[derive(Debug)]
pub struct Archive {
    dir: PathBuf,
    id: String,
    catalogue: Catalogue,
    /// The locations a sync is running on.
    syncing: Mutex<HashSet<LocationId>>,
    /// Held shared from a segment's move into place until its commit, and exclusively by
    /// whoever has to tell the files the catalogue doesn't record from those it is about to.
    placing: RwLock<()>,
    _lock: File,
}

impl Archive {
    /// Creates an archive in `dir`, which is made if missing and must not hold one already,
    /// nor what is left of one.
    pub fn create(dir: &Path) -> Result<Self> {
        let dir = &absolute(dir)?;
        fs::create_dir_all(dir).map_err(io_error("create", dir))?;
        let lock = lock(dir)?;
        let manifest_path = dir.join(layout::MANIFEST);
        if manifest_path.exists() {
            return Err(ArchiveError::AlreadyAnArchive(dir.to_path_buf()));
        }
        let holds_files =
            fs::read_dir(dir.join(layout::FILES)).is_ok_and(|mut entries| entries.next().is_some());
        if holds_files {
            return Err(ArchiveError::HoldsAnArchivesFiles(dir.to_path_buf()));
        }
        let (catalogue, tmp) = Self::prepare(dir)?;
        /* A catalogue that is here already is what a create that was cut short left, with
        nothing in it, or it is an archive's. */
        if catalogue.has_locations()? {
            return Err(ArchiveError::HoldsAnArchivesFiles(dir.to_path_buf()));
        }
        let id = match catalogue.archive_id()? {
            Some(id) => id,
            None => {
                let id = uuid::Uuid::new_v4().simple().to_string();
                catalogue.set_archive_id(&id)?;
                id
            }
        };
        /* The manifest comes last: it is what says the folder holds an archive, catalogue
        included. */
        let json = serde_json::to_vec_pretty(&Manifest {
            format: FORMAT,
            id: id.clone(),
            created: catalogue::now_ms(),
        })
        .expect("a struct of numbers and text always serializes");
        disk::write_new(&tmp, &manifest_path, &json)
            .and_then(|()| disk::push_dir(dir))
            .and_then(|()| disk::settle(dir))
            .map_err(io_error("write", &manifest_path))?;
        Ok(Self::opened(dir, lock, id, catalogue))
    }

    /// Opens the archive in `dir`. A folder without one is [`ArchiveError::NotAnArchive`]; an
    /// archive without its catalogue, or with another's, is not opened either.
    pub fn open(dir: &Path) -> Result<Self> {
        let dir = &absolute(dir)?;
        let manifest_path = dir.join(layout::MANIFEST);
        if !manifest_path.is_file() {
            return Err(ArchiveError::NotAnArchive(dir.to_path_buf()));
        }
        let lock = lock(dir)?;
        if !dir.join(catalogue::DATABASE).is_file() {
            return Err(ArchiveError::CatalogueMissing {
                dir: dir.to_path_buf(),
                newest_backup: catalogue::newest_backup(dir),
            });
        }
        let json = fs::read(&manifest_path).map_err(io_error("read", &manifest_path))?;
        let manifest: Manifest =
            serde_json::from_slice(&json).map_err(|source| ArchiveError::Manifest {
                path: manifest_path,
                source,
            })?;
        if manifest.format > FORMAT {
            return Err(ArchiveError::NewerFormat {
                dir: dir.to_path_buf(),
                found: manifest.format,
            });
        }
        let (catalogue, _) = Self::prepare(dir)?;
        let owner = catalogue.archive_id()?;
        if owner.as_deref() != Some(&manifest.id) {
            return Err(ArchiveError::AnothersCatalogue {
                dir: dir.to_path_buf(),
                archive: manifest.id,
                catalogue: owner,
            });
        }
        Ok(Self::opened(dir, lock, manifest.id, catalogue))
    }

    /// What every start does, once the lock is held: makes sure the archive's folders are
    /// there, empties `tmp/` and opens the catalogue. Returns the catalogue and `tmp/`.
    fn prepare(dir: &Path) -> Result<(Catalogue, PathBuf)> {
        let tmp = dir.join(layout::TMP);
        for sub in [&tmp, &dir.join(layout::FILES)] {
            fs::create_dir_all(sub).map_err(io_error("create", sub))?;
        }
        for entry in fs::read_dir(&tmp).map_err(io_error("list", &tmp))? {
            let path = entry.map_err(io_error("list", &tmp))?.path();
            let removed = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            /* A leftover another program holds open is no reason not to start. */
            if let Err(error) = removed {
                log::warn!("could not remove {}: {error}", path.display());
            }
        }
        Ok((Catalogue::open(dir)?, tmp))
    }

    fn opened(dir: &Path, lock: File, id: String, catalogue: Catalogue) -> Self {
        Self {
            dir: dir.to_path_buf(),
            id,
            catalogue,
            syncing: Mutex::default(),
            placing: RwLock::default(),
            _lock: lock,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The id written when the archive was created (PLAN §2.1).
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn catalogue(&self) -> &Catalogue {
        &self.catalogue
    }

    /// The archive's folder for work in flight. What is left in it is removed at the next start.
    pub(crate) fn tmp(&self) -> PathBuf {
        self.dir.join(layout::TMP)
    }

    /// The content of a version, read from its segments in order and checked against the
    /// catalogue as it is read.
    pub fn read_version(&self, version: i64) -> Result<VersionReader> {
        self.read_version_from(version, 0)
    }

    /// The content of a version from its byte `offset` on. Only the segment that holds that
    /// byte and the ones after it are read; each is checked as [`Archive::read_version`] does.
    pub fn read_version_from(&self, version: i64, offset: u64) -> Result<VersionReader> {
        let stored = self
            .catalogue
            .stored_version(version)?
            .ok_or(ArchiveError::UnknownVersion(version))?;
        Ok(self.reader(stored, offset))
    }

    fn reader(&self, stored: Stored, start: u64) -> VersionReader {
        VersionReader::new(
            self.dir
                .join(layout::item_dir(
                    &stored.location,
                    &stored.provider,
                    &stored.folder,
                ))
                .join(layout::version_dir(stored.version.number)),
            stored.segments,
            stored.version.length,
            stored.version.hash,
            start,
        )
    }

    /// Marks `location` as being synced until the guard is dropped; `None` while it already is
    /// (PLAN §5: one location runs one pass at a time).
    pub(crate) fn begin_sync(&self, location: &LocationId) -> Option<SyncGuard<'_>> {
        self.syncing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(location.clone())
            .then(|| SyncGuard {
                archive: self,
                location: location.clone(),
            })
    }

    /// A range of a version to fill, starting at `offset` of the version, in `tmp/`.
    pub(crate) fn range_writer(&self, offset: u64) -> RangeWriter {
        RangeWriter::new(self.tmp(), offset)
    }

    /**
     * Stores a batch (PLAN §4.1): for each commit, puts its item on disk, with its folder, its
     * `source.json` and the range's segments in their places; makes all of it durable; and only
     * then commits all of them to the catalogue in one transaction. Returns, for each, whether
     * a version it starts is held back.
     *
     * A file already where one goes is one the catalogue never recorded: it is what a store
     * that was interrupted before its commit leaves. When it holds the very bytes that are
     * being placed, it stays and is recorded now, so a pass that takes up where another was
     * killed moves nothing. When it holds anything else, it is set aside. A segment the
     * catalogue does record is never touched: [`ArchiveError::SegmentExists`]. An item's
     * `source.json` says only what the catalogue says too, so one that is not what it should
     * be is set aside and written again.
     */
    pub(crate) fn store(&self, batch: Vec<(Commit<'_>, Vec<Sealed>)>) -> Result<Vec<bool>> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        let _placing = self.placing.read().unwrap_or_else(PoisonError::into_inner);
        /* The folders whose entries this batch relies on, each pushed once, at the end: an
        item's folder and the ones it is in, whether this store made them or an earlier one
        that never got as far as its flush. */
        let mut folders = BTreeSet::new();
        let mut commits = Vec::new();
        for (commit, segments) in batch {
            let folder = item_folder(&commit.scanned.key);
            let item = layout::item_dir(commit.location, commit.provider, &folder);
            let item_dir = self.dir.join(&item);
            fs::create_dir_all(&item_dir).map_err(io_error("create", &item_dir))?;
            folders.extend(
                item.ancestors()
                    .take_while(|folder| !folder.as_os_str().is_empty())
                    .map(|folder| self.dir.join(folder)),
            );
            let json = serde_json::to_vec_pretty(&SourceManifest {
                provider: commit.provider,
                location: commit.location.as_str(),
                key: &commit.scanned.key,
            })
            .expect("a struct of text always serializes");
            let source = item.join(layout::SOURCE);
            /* One that is there and is not this (a write cut short) is set aside by `place`. */
            if !fs::read(self.dir.join(&source)).is_ok_and(|existing| existing == json) {
                let file =
                    disk::new_file(&self.tmp(), &json).map_err(io_error("write", &item_dir))?;
                self.place(file, &source, || Ok(false))?;
            }

            let version = item.join(layout::version_dir(commit.number));
            let mut placed = Vec::new();
            for sealed in segments {
                let version_dir = self.dir.join(&version);
                fs::create_dir_all(&version_dir).map_err(io_error("create", &version_dir))?;
                folders.insert(version_dir);
                self.place(
                    sealed.file,
                    &version.join(layout::segment_file(sealed.offset)),
                    || {
                        Ok(self.catalogue.has_segment(
                            commit.location,
                            commit.provider,
                            &folder,
                            commit.number,
                            sealed.offset,
                        )?)
                    },
                )?;
                placed.push((sealed.offset, sealed.length, sealed.hash));
            }
            commits.push(Recorded {
                commit,
                segments: placed,
            });
        }
        let files = self.dir.join(layout::FILES);
        folders
            .iter()
            .try_for_each(|folder| disk::push_dir(folder))
            .and_then(|()| disk::settle(&files))
            .map_err(io_error("flush", &files))?;
        Ok(self.catalogue.commit(&commits)?)
    }

    /// Moves the whole, pushed `file` from `tmp/` to `relative` (to the archive, under
    /// `files/`), which nothing is ever replaced at ([`Archive::store`]). `recorded` says
    /// whether the catalogue records a file there.
    fn place(
        &self,
        mut file: tempfile::TempPath,
        relative: &Path,
        recorded: impl Fn() -> Result<bool>,
    ) -> Result<()> {
        let to = self.dir.join(relative);
        /* The move refuses to replace a file, so nothing here can overwrite one. */
        while let Err((error, unplaced)) = disk::place(file, &to) {
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(io_error("store", &to)(error));
            }
            if recorded()? {
                return Err(ArchiveError::SegmentExists(to));
            }
            if disk::same_bytes(&unplaced, &to).map_err(io_error("read", &to))? {
                return Ok(());
            }
            self.set_aside(relative)?;
            file = unplaced;
        }
        Ok(())
    }

    /// Moves the file or folder at `relative` (to the archive, under `files/`) to the same path
    /// under `orphans/`.
    fn set_aside(&self, relative: &Path) -> Result<()> {
        let from = self.dir.join(relative);
        let mut to = self.dir.join(layout::ORPHANS).join(relative);
        if let Some(dir) = to.parent() {
            fs::create_dir_all(dir).map_err(io_error("create", dir))?;
        }
        /* Never onto something set aside earlier. */
        let mut again = 0;
        while to.exists() {
            again += 1;
            let mut name = relative.as_os_str().to_owned();
            name.push(format!(".{again}"));
            to = self.dir.join(layout::ORPHANS).join(name);
        }
        fs::rename(&from, &to).map_err(io_error("set aside", &from))
    }

    /**
     * Sets aside, in `orphans/`, everything under `files/` that the catalogue doesn't account
     * for: what [`Archive::verify`] reports as uncatalogued. Nothing does this unasked. Returns
     * the paths that were moved, relative to the archive. One that can't be moved (a file
     * another program holds) stays where it is.
     */
    pub fn set_aside_uncatalogued(&self) -> Result<Vec<PathBuf>> {
        /* No segment is between its place and its commit while this looks and moves. */
        let _quiet = self.placing.write().unwrap_or_else(PoisonError::into_inner);
        let uncatalogued = self.uncatalogued(&accounted(&self.catalogue.stored()?))?;
        if !uncatalogued.is_empty() && !self.catalogue.has_items()? {
            return Err(ArchiveError::NothingRecorded(self.dir.clone()));
        }
        let mut moved = Vec::new();
        for relative in uncatalogued {
            match self.set_aside(&relative) {
                Ok(()) => moved.push(relative),
                Err(error) => log::warn!("{error}"),
            }
        }
        Ok(moved)
    }

    /// Every path under `files/`, relative to the archive, that is not among `accounted`
    /// ([`accounted`]). A folder that is not is one path, whatever it holds.
    fn uncatalogued(&self, accounted: &HashSet<PathBuf>) -> Result<Vec<PathBuf>> {
        let mut uncatalogued = Vec::new();
        let mut folders = vec![PathBuf::from(layout::FILES)];
        while let Some(folder) = folders.pop() {
            let dir = self.dir.join(&folder);
            for entry in fs::read_dir(&dir).map_err(io_error("list", &dir))? {
                let entry = entry.map_err(io_error("list", &dir))?;
                let path = folder.join(entry.file_name());
                if !accounted.contains(&path) {
                    uncatalogued.push(path);
                } else if entry.file_type().map_err(io_error("list", &dir))?.is_dir() {
                    folders.push(path);
                }
            }
        }
        Ok(uncatalogued)
    }
}

/// The paths under `files/`, relative to the archive, that `stored` accounts for: the folders
/// of its items and the folders those are in, their `source.json`, their versions' folders and
/// their segments, each under exactly the name the layout gives it.
fn accounted(stored: &[Stored]) -> HashSet<PathBuf> {
    let mut accounted = HashSet::new();
    for stored in stored {
        let item = layout::item_dir(&stored.location, &stored.provider, &stored.folder);
        if accounted.insert(item.clone()) {
            accounted.extend(item.ancestors().skip(1).map(Path::to_path_buf));
            accounted.insert(item.join(layout::SOURCE));
        }
        if !stored.segments.is_empty() {
            let version = item.join(layout::version_dir(stored.version.number));
            accounted.extend(
                stored
                    .segments
                    .iter()
                    .map(|segment| version.join(layout::segment_file(segment.offset))),
            );
            accounted.insert(version);
        }
    }
    accounted
}

/// `dir` as an absolute path: the archive stays the folder it was opened on wherever the
/// process's working directory goes, and the catalogue's own reads of its database, which name
/// it by a `file:` URI, need the whole path.
fn absolute(dir: &Path) -> Result<PathBuf> {
    std::path::absolute(dir).map_err(io_error("resolve", dir))
}

/// Takes the archive's lock, which lasts as long as the returned file is open.
fn lock(dir: &Path) -> Result<File> {
    let path = dir.join(layout::LOCK);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_error("open", &path))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(ArchiveError::Locked(dir.to_path_buf())),
        Err(TryLockError::Error(error)) => Err(io_error("lock", &path)(error)),
    }
}

/// A location's sync in progress.
pub(crate) struct SyncGuard<'a> {
    archive: &'a Archive,
    location: LocationId,
}

impl Drop for SyncGuard<'_> {
    fn drop(&mut self) {
        self.archive
            .syncing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.location);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::Target;
    use crate::hash::Hash;
    use crate::location::LocationKind;
    use crate::provider::{ItemKind, ScannedItem};

    fn scanned(key: &str) -> ScannedItem {
        ScannedItem {
            key: key.to_string(),
            kind: ItemKind::Sidecar,
            session: None,
            length: 5,
            stamp: "1".to_string(),
            lines: false,
            export_version: None,
        }
    }

    /// What storing `bytes` as version 1 of the new item `scanned` takes.
    fn first<'a>(
        archive: &Archive,
        location: &'a LocationId,
        scanned: &'a ScannedItem,
        bytes: &[u8],
    ) -> (Commit<'a>, Vec<Sealed>) {
        let mut writer = archive.range_writer(0);
        writer.write(bytes).unwrap();
        let commit = Commit {
            location,
            provider: "fixture",
            scanned,
            target: Target::NewItem,
            number: 1,
            from: 0,
            length: bytes.len() as u64,
            hash: Hash::of(bytes),
        };
        (commit, writer.finish().unwrap())
    }

    fn archive() -> (tempfile::TempDir, Archive, LocationId) {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::create(dir.path()).unwrap();
        let location = archive
            .catalogue()
            .add_location(LocationKind::Local, "fixture")
            .unwrap()
            .id;
        (dir, archive, location)
    }

    /// Where version 1's first segment of the item `scanned` goes, relative to the archive.
    fn first_segment(location: &LocationId, scanned: &ScannedItem) -> PathBuf {
        layout::item_dir(location, "fixture", &item_folder(&scanned.key))
            .join("v0001/0000000000000000.zst")
    }

    fn orphans(archive: &Archive) -> usize {
        fn count(dir: &Path) -> usize {
            fs::read_dir(dir).map_or(0, |entries| {
                entries
                    .map(|entry| entry.unwrap().path())
                    .map(|path| if path.is_dir() { count(&path) } else { 1 })
                    .sum()
            })
        }
        count(&archive.dir().join(layout::ORPHANS))
    }

    #[test]
    fn a_file_in_the_way_is_kept_if_it_is_the_segment_and_set_aside_if_not() {
        let (_dir, archive, location) = archive();
        let (same, other) = (scanned("items/same"), scanned("items/other"));
        /* What a store that never reached its commit left for the first item: its segment, as
        this store would write it. For the second: files of other bytes where its segment and
        its source.json go, the segment's as many as this store would write. */
        let place = archive.dir().join(first_segment(&location, &same));
        fs::create_dir_all(place.parent().unwrap()).unwrap();
        fs::copy(
            &first(&archive, &location, &same, b"first").1[0].file,
            &place,
        )
        .unwrap();
        let different = archive.dir().join(first_segment(&location, &other));
        let source = different.parent().unwrap().with_file_name(layout::SOURCE);
        fs::create_dir_all(different.parent().unwrap()).unwrap();
        let mut left = fs::read(&first(&archive, &location, &other, b"other").1[0].file).unwrap();
        *left.last_mut().unwrap() ^= 1;
        fs::write(&different, &left).unwrap();
        fs::write(&source, "{").unwrap();

        archive
            .store(vec![
                first(&archive, &location, &same, b"first"),
                first(&archive, &location, &other, b"other"),
            ])
            .unwrap();
        /* The first was adopted where it was, the second's files set aside with their bytes. */
        assert_eq!(orphans(&archive), 2);
        assert!(fs::read_to_string(source).unwrap().contains("items/other"));
        let orphan = archive
            .dir()
            .join(layout::ORPHANS)
            .join(first_segment(&location, &other));
        assert!(fs::read(orphan).unwrap() == left);
        assert_eq!(fs::read_dir(archive.tmp()).unwrap().count(), 0);
        let report = archive
            .verify(&toolbox_process::CancelToken::new())
            .unwrap();
        assert_eq!((report.versions, report.damaged.len()), (2, 0));
        assert!(report.uncatalogued.is_empty());
    }

    #[test]
    fn a_segment_the_catalogue_records_is_never_replaced_or_moved() {
        let (_dir, archive, location) = archive();
        let scanned = scanned("items/one");
        let place = archive.dir().join(first_segment(&location, &scanned));
        archive
            .store(vec![first(&archive, &location, &scanned, b"first")])
            .unwrap();
        let recorded = fs::read(&place).unwrap();

        /* Other bytes, and the very same ones: neither is put where a recorded segment is. */
        for bytes in [&b"other"[..], b"first"] {
            let refused = archive.store(vec![first(&archive, &location, &scanned, bytes)]);
            assert!(
                matches!(refused, Err(ArchiveError::SegmentExists(_))),
                "{refused:?}"
            );
            assert!(fs::read(&place).unwrap() == recorded);
            assert_eq!(fs::read_dir(archive.tmp()).unwrap().count(), 0);
            assert_eq!(orphans(&archive), 0);
        }
    }

    #[test]
    fn a_range_planned_for_a_version_of_another_length_is_not_recorded() {
        let (_dir, archive, location) = archive();
        let scanned = scanned("items/one");
        archive
            .store(vec![first(&archive, &location, &scanned, b"first")])
            .unwrap();
        let item = archive
            .catalogue()
            .item(&location, "fixture", &scanned.key)
            .unwrap()
            .unwrap();
        let version = archive.catalogue().versions(item.id).unwrap().remove(0);
        /* A plan made when the version was 7 bytes long; it is 5. */
        let mut writer = archive.range_writer(7);
        writer.write(b" more").unwrap();
        let stale = Commit {
            location: &location,
            provider: "fixture",
            scanned: &scanned,
            target: Target::Append {
                item: item.id,
                version: version.id,
            },
            number: 1,
            from: 7,
            length: 12,
            hash: Hash::of(b"first??more"),
        };
        let refused = archive.store(vec![(stale, writer.finish().unwrap())]);
        assert!(
            matches!(
                refused,
                Err(ArchiveError::Catalogue(CatalogueError::VersionMoved {
                    length: 7,
                    ..
                }))
            ),
            "{refused:?}"
        );
        let after = archive.catalogue().versions(item.id).unwrap().remove(0);
        assert_eq!((after.length, after.hash), (5, version.hash));
        assert_eq!(archive.catalogue().segments(version.id).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn nothing_is_committed_before_it_is_flushed() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, archive, location) = archive();
        let scanned = scanned("items/one");
        archive
            .store(vec![first(&archive, &location, &scanned, b"first")])
            .unwrap();
        let item = archive
            .catalogue()
            .item(&location, "fixture", &scanned.key)
            .unwrap()
            .unwrap();
        let version = archive.catalogue().versions(item.id).unwrap().remove(0);
        /* The version's folder can be written into and not opened, so the segment is placed
        and its folder can't be flushed. */
        let folder = archive.dir().join(first_segment(&location, &scanned));
        let folder = folder.parent().unwrap();
        fs::set_permissions(folder, fs::Permissions::from_mode(0o300)).unwrap();
        let mut writer = archive.range_writer(5);
        writer.write(b" more").unwrap();
        let append = Commit {
            location: &location,
            provider: "fixture",
            scanned: &scanned,
            target: Target::Append {
                item: item.id,
                version: version.id,
            },
            number: 1,
            from: 5,
            length: 10,
            hash: Hash::of(b"first more"),
        };
        let failed = archive.store(vec![(append, writer.finish().unwrap())]);
        fs::set_permissions(folder, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            matches!(
                failed,
                Err(ArchiveError::Io {
                    action: "flush",
                    ..
                })
            ),
            "{failed:?}"
        );
        assert!(folder.join("0000000000000005.zst").exists());
        /* The catalogue records nothing of what could not be made durable. */
        let version = archive.catalogue().versions(item.id).unwrap().remove(0);
        assert_eq!(version.length, 5);
        assert_eq!(archive.catalogue().segments(version.id).unwrap().len(), 1);
    }
}
