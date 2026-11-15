//! The archive on disk (PLAN §4): the folder scrollback owns, holding the stored bytes and the
//! catalogue. [`layout`] says where things are, [`segment`] writes and reads the stored bytes,
//! [`disk`] gets files into place and durable, and [`verify`] re-hashes everything against the
//! catalogue.
//!
//! What keeps archived bytes safe (PLAN §2.3):
//! - a segment is a new file, written whole before the catalogue records it, and it is moved
//!   into place with a move that fails if a file is already there ([`Archive::store`]), so the
//!   file system itself refuses to replace one;
//! - nothing here deletes a file under `files/`. A file there that the catalogue doesn't know (a
//!   store that was interrupted between the move and the commit, or a catalogue restored from
//!   an older backup) is moved to `orphans/` when the archive is opened, so the tree holds only
//!   what the catalogue records and no bytes are lost;
//! - a catalogue is only ever used with the archive it was made for: the archive's id is in
//!   `archive.json` and in the catalogue, an archive whose catalogue is missing or another's
//!   is not opened, and one is not created over a folder that holds a catalogue or files.
//!
//! Looking for files the catalogue doesn't record means walking all of `files/`, so a start
//! only does it when there can be any. `state.json` says so: it is written, and flushed, when
//! a sync first stores something (`syncing`) and again when the last sync under way has ended
//! with everything it stored recorded, then with how many segments the catalogue records and
//! when the newest was stored. A start that finds it saying `syncing`, or saying something
//! else than the catalogue does (a catalogue from a backup, or one that lost its last commits
//! to a power cut), or doesn't find it, looks.

mod disk;
mod layout;
mod segment;
mod verify;

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::catalogue::{self, Catalogue, CatalogueError, Commit, Recorded, Stored};
use crate::location::LocationId;

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

/// `state.json` (the module docs).
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct State {
    /// A sync is storing, or `files/` is known to hold something the catalogue doesn't record.
    syncing: bool,
    /// How many segments the catalogue recorded when this was written, and when the newest of
    /// them was stored.
    segments: u64,
    newest: Option<i64>,
}

/// The syncs under way.
#[derive(Debug, Default)]
struct Syncing {
    locations: HashSet<LocationId>,
    /// `state.json` says `syncing`.
    storing: bool,
    /// `files/` may hold a file the catalogue doesn't record: a store did not end well after
    /// it had put one in place, or verify found one.
    unrecorded: bool,
}

/// An open archive. Only one can be open on a folder at a time (PLAN §4.4): the lock is held
/// until this is dropped.
#[derive(Debug)]
pub struct Archive {
    dir: PathBuf,
    id: String,
    catalogue: Catalogue,
    syncing: Mutex<Syncing>,
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
        Ok(Self::start(dir, lock, id, catalogue))
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
        Ok(Self::start(dir, lock, manifest.id, catalogue))
    }

    /// What every start does first, once the lock is held: makes sure the archive's folders
    /// are there, empties `tmp/` and opens the catalogue. Returns the catalogue and `tmp/`.
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

    /**
     * The open archive. When `files/` may hold what the catalogue doesn't record (the module
     * docs), that is set aside first. Nothing here keeps the archive from opening: when the
     * look or the note of it fails (a full disk), the next start looks again.
     */
    fn start(dir: &Path, lock: File, id: String, catalogue: Catalogue) -> Self {
        let archive = Self {
            dir: dir.to_path_buf(),
            id,
            catalogue,
            syncing: Mutex::default(),
            placing: RwLock::default(),
            _lock: lock,
        };
        let tidied = archive.recorded_state().and_then(|recorded| {
            let state = fs::read(dir.join(layout::STATE))
                .ok()
                .and_then(|json| serde_json::from_slice::<State>(&json).ok());
            if state.as_ref() != Some(&recorded) && archive.set_aside_uncatalogued()? {
                archive.write_state(&recorded)?;
            }
            Ok(())
        });
        if let Err(error) = tidied {
            log::warn!("{error}");
        }
        archive
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

    fn syncing(&self) -> MutexGuard<'_, Syncing> {
        self.syncing.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Marks `location` as being synced until the guard is dropped; `None` while it already is
    /// (PLAN §5: one location runs one pass at a time).
    pub(crate) fn begin_sync(&self, location: &LocationId) -> Option<SyncGuard<'_>> {
        self.syncing()
            .locations
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
     * Stores a batch (PLAN §4.1): for each commit, makes its item durable on disk, with its
     * folder, its `source.json` and the range's segments in their places, and then commits all
     * of them to the catalogue in one transaction. Returns, for each, whether a version it
     * starts is held back.
     *
     * A file already where a segment goes is one the catalogue never recorded (it is what an
     * interrupted store leaves) and is set aside. One the catalogue does record is never
     * replaced: [`ArchiveError::SegmentExists`].
     */
    pub(crate) fn store(&self, batch: Vec<(Commit<'_>, Vec<Sealed>)>) -> Result<Vec<bool>> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        {
            let mut syncing = self.syncing();
            if !syncing.storing {
                self.write_state(&State {
                    syncing: true,
                    ..self.recorded_state()?
                })?;
                syncing.storing = true;
            }
        }
        let _placing = self.placing.read().unwrap_or_else(PoisonError::into_inner);
        /* Until the commit, whatever goes wrong may leave files the catalogue doesn't record. */
        let mut unrecorded = Unrecorded {
            archive: self,
            recorded: false,
        };
        /* The folders that gain an entry here. Each is pushed once, at the end. */
        let mut changed = BTreeSet::new();
        let mut commits = Vec::new();
        for (commit, segments) in batch {
            let folder = item_folder(&commit.scanned.key);
            let item = layout::item_dir(commit.location, commit.provider, &folder);
            let item_dir = self.dir.join(&item);
            disk::create_dirs(&item_dir, &mut changed).map_err(io_error("create", &item_dir))?;
            let source = item_dir.join(layout::SOURCE);
            if !source.exists() {
                let json = serde_json::to_vec_pretty(&SourceManifest {
                    provider: commit.provider,
                    location: commit.location.as_str(),
                    key: &commit.scanned.key,
                })
                .expect("a struct of text always serializes");
                disk::write_new(&self.tmp(), &source, &json).map_err(io_error("write", &source))?;
                changed.insert(item_dir.clone());
            }
            let version = item.join(layout::version_dir(commit.number));
            let version_dir = self.dir.join(&version);
            let mut placed = Vec::new();
            for sealed in segments {
                disk::create_dirs(&version_dir, &mut changed)
                    .map_err(io_error("create", &version_dir))?;
                let relative = version.join(layout::segment_file(sealed.offset));
                let to = self.dir.join(&relative);
                let mut file = sealed.file;
                /* The move refuses to replace a file, so nothing here can overwrite one. */
                while let Err((error, unplaced)) = disk::place(file, &to) {
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(io_error("store", &to)(error));
                    }
                    if self.catalogue.has_segment(
                        commit.location,
                        commit.provider,
                        &folder,
                        commit.number,
                        sealed.offset,
                    )? {
                        return Err(ArchiveError::SegmentExists(to));
                    }
                    self.set_aside(&relative)?;
                    file = unplaced;
                }
                changed.insert(version_dir.clone());
                placed.push((sealed.offset, sealed.length, sealed.hash));
            }
            commits.push(Recorded {
                commit,
                segments: placed,
            });
        }
        let files = self.dir.join(layout::FILES);
        changed
            .iter()
            .try_for_each(|dir| disk::push_dir(dir))
            .and_then(|()| disk::settle(&files))
            .map_err(io_error("flush", &files))?;
        let held_back = self.catalogue.commit(&commits)?;
        unrecorded.recorded = true;
        Ok(held_back)
    }

    /// What `state.json` says when nothing is being stored: what the catalogue records.
    fn recorded_state(&self) -> Result<State> {
        let (segments, newest) = self.catalogue.segments_recorded()?;
        Ok(State {
            syncing: false,
            segments,
            newest,
        })
    }

    /// Writes `state.json`, flushed to disk.
    fn write_state(&self, state: &State) -> Result<()> {
        let path = self.dir.join(layout::STATE);
        let json =
            serde_json::to_vec(state).expect("a struct of numbers and a flag always serializes");
        disk::write_over(&self.tmp(), &path, &json).map_err(io_error("write", &path))
    }

    /// Notes that `files/` holds something the catalogue doesn't record, so that the next
    /// start looks (the module docs).
    fn note_unrecorded(&self) {
        let mut syncing = self.syncing();
        syncing.unrecorded = true;
        if !syncing.storing {
            let written = self.recorded_state().and_then(|recorded| {
                self.write_state(&State {
                    syncing: true,
                    ..recorded
                })
            });
            if let Err(error) = written {
                log::warn!("{error}");
            }
        }
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
     * Sets aside everything under `files/` that the catalogue doesn't record (the module docs).
     * Returns whether `files/` now holds nothing but what the catalogue records. It doesn't
     * when something could not be moved (a file another program holds), which stays where it
     * is, to be set aside when it is in the way or at a later start.
     */
    fn set_aside_uncatalogued(&self) -> Result<bool> {
        /* A catalogue that records nothing beside files that are there is this archive's from
        before its first sync, back from a backup: setting everything aside would empty the
        archive. */
        if !self.catalogue.has_items()? {
            let empty = self.entries(Path::new(layout::FILES))?.is_empty();
            if !empty {
                log::warn!(
                    "the catalogue of {} records nothing, but its files/ is not empty; \
                     everything there is left where it is",
                    self.dir.display()
                );
            }
            return Ok(empty);
        }
        let uncatalogued = self.uncatalogued(&accounted(&self.catalogue.stored()?))?;
        let mut left = 0;
        for relative in &uncatalogued {
            if let Err(error) = self.set_aside(relative) {
                log::warn!("{error}");
                left += 1;
            }
            /* A folder left empty goes; one with anything in it stays. */
            if let Some(dir) = relative.parent() {
                let _ = fs::remove_dir(self.dir.join(dir));
            }
        }
        if !uncatalogued.is_empty() {
            log::warn!(
                "set aside {} files or folders the catalogue doesn't record, in {}",
                uncatalogued.len() - left,
                self.dir.join(layout::ORPHANS).display()
            );
        }
        Ok(left == 0)
    }

    /// Every path under `files/`, relative to the archive, that is not among `accounted`
    /// ([`accounted`]). A folder that is not is one path, whatever it holds.
    fn uncatalogued(&self, accounted: &HashSet<PathBuf>) -> Result<Vec<PathBuf>> {
        let mut uncatalogued = Vec::new();
        let mut folders = vec![PathBuf::from(layout::FILES)];
        while let Some(folder) = folders.pop() {
            for (name, is_dir) in self.entries(&folder)? {
                let path = folder.join(name);
                if !accounted.contains(&path) {
                    uncatalogued.push(path);
                } else if is_dir {
                    folders.push(path);
                }
            }
        }
        Ok(uncatalogued)
    }

    /// What is in the folder `relative`: each entry's name, and whether it is a folder.
    fn entries(&self, relative: &Path) -> Result<Vec<(std::ffi::OsString, bool)>> {
        let dir = self.dir.join(relative);
        fs::read_dir(&dir)
            .map_err(io_error("list", &dir))?
            .map(|entry| {
                let entry = entry?;
                Ok((entry.file_name(), entry.file_type()?.is_dir()))
            })
            .collect::<io::Result<_>>()
            .map_err(io_error("list", &dir))
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

/// A store under way. Dropped before its commit, by an error or a panic, it may have left
/// files in place that the catalogue doesn't record.
struct Unrecorded<'a> {
    archive: &'a Archive,
    recorded: bool,
}

impl Drop for Unrecorded<'_> {
    fn drop(&mut self) {
        if !self.recorded {
            self.archive.syncing().unrecorded = true;
        }
    }
}

/// A location's sync in progress.
pub(crate) struct SyncGuard<'a> {
    archive: &'a Archive,
    location: LocationId,
}

impl Drop for SyncGuard<'_> {
    /// The sync is over. When it was the last one under way and everything that was stored is
    /// recorded, `state.json` says so again. If it isn't, or that can't be written, the file
    /// goes on saying `syncing` and the next start looks through `files/`, which costs time
    /// and nothing else.
    fn drop(&mut self) {
        let mut syncing = self.archive.syncing();
        syncing.locations.remove(&self.location);
        if syncing.locations.is_empty() && syncing.storing {
            syncing.storing = false;
            if !syncing.unrecorded {
                let written = self
                    .archive
                    .recorded_state()
                    .and_then(|state| self.archive.write_state(&state));
                if let Err(error) = written {
                    log::warn!("{error}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::Target;
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
            length: bytes.len() as u64,
            hash: crate::hash::Hash::of(bytes),
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

    fn state(archive: &Archive) -> State {
        serde_json::from_slice(&fs::read(archive.dir().join(layout::STATE)).unwrap()).unwrap()
    }

    #[test]
    fn a_segment_is_set_aside_or_refused_but_never_replaced() {
        let (_dir, archive, location) = archive();
        let scanned = scanned("items/one");
        let relative = layout::item_dir(&location, "fixture", &item_folder(&scanned.key))
            .join("v0001/0000000000000000.zst");
        let place = archive.dir().join(&relative);
        let orphan = archive.dir().join(layout::ORPHANS).join(&relative);

        /* A file the catalogue doesn't record is where the segment goes: it is set aside. */
        fs::create_dir_all(place.parent().unwrap()).unwrap();
        fs::write(&place, "left by an interrupted store").unwrap();
        archive
            .store(vec![first(&archive, &location, &scanned, b"first")])
            .unwrap();
        let recorded = fs::read(&place).unwrap();
        assert_eq!(fs::read(&orphan).unwrap(), b"left by an interrupted store");

        /* Now the catalogue records the segment there: storing another is refused. */
        let refused = archive.store(vec![first(&archive, &location, &scanned, b"other")]);
        assert!(
            matches!(refused, Err(ArchiveError::SegmentExists(_))),
            "{refused:?}"
        );
        assert!(fs::read(&place).unwrap() == recorded);
        assert_eq!(fs::read_dir(archive.tmp()).unwrap().count(), 0);
        assert_eq!(fs::read_dir(orphan.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn a_store_that_fails_after_placing_a_file_keeps_the_state_saying_so() {
        let (_dir, archive, location) = archive();
        let (one, two) = (scanned("items/one"), scanned("items/two"));
        let syncing = archive.begin_sync(&location).unwrap();
        /* The second item of the batch can't be given a folder: a file is in the way. The
        first item's segment is in place by then, and never committed. */
        let folder = item_folder(&two.key);
        let blocked = archive
            .dir()
            .join(layout::item_dir(&location, "fixture", &folder));
        archive
            .store(vec![first(
                &archive,
                &location,
                &scanned("items/zero"),
                b"zero",
            )])
            .unwrap();
        fs::create_dir_all(blocked.parent().unwrap()).unwrap();
        fs::write(&blocked, "not a folder").unwrap();
        let failed = archive.store(vec![
            first(&archive, &location, &one, b"first"),
            first(&archive, &location, &two, b"second"),
        ]);
        assert!(matches!(failed, Err(ArchiveError::Io { .. })), "{failed:?}");
        assert!(
            archive
                .catalogue()
                .item(&location, "fixture", &one.key)
                .unwrap()
                .is_none()
        );

        /* The sync ends: the state still says to look, though no commit failed. */
        drop(syncing);
        assert!(state(&archive).syncing);
    }

    #[test]
    fn the_state_says_syncing_until_the_last_sync_under_way_ends() {
        let (_dir, archive, first_location) = archive();
        let second_location = archive
            .catalogue()
            .add_location(LocationKind::Local, "another")
            .unwrap()
            .id;
        assert_eq!(state(&archive), archive.recorded_state().unwrap());

        let (one, two) = (scanned("items/one"), scanned("items/two"));
        let first_sync = archive.begin_sync(&first_location).unwrap();
        let second_sync = archive.begin_sync(&second_location).unwrap();
        archive
            .store(vec![first(&archive, &first_location, &one, b"first")])
            .unwrap();
        assert!(state(&archive).syncing);
        drop(first_sync);
        assert!(state(&archive).syncing);
        archive
            .store(vec![first(&archive, &second_location, &two, b"second")])
            .unwrap();
        drop(second_sync);
        let ended = state(&archive);
        assert_eq!((ended.syncing, ended.segments), (false, 2));
    }
}
