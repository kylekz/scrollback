//! The archive on disk (PLAN §4): the folder scrollback owns, holding the stored bytes and the
//! catalogue. [`layout`] says where things are, [`segment`] writes and reads the stored bytes,
//! and [`verify`] re-hashes them against the catalogue.
//!
//! What keeps archived bytes safe (PLAN §2.3):
//! - a segment is a new file, written whole before the catalogue records it, and it is moved
//!   into place with a move that fails if a file is already there ([`Archive::store`]), so the
//!   file system itself refuses to replace one;
//! - nothing here deletes a file under `files/`. A file there that the catalogue doesn't know (a
//!   store that was interrupted between the move and the commit, or a catalogue restored from
//!   an older backup) is moved to `orphans/` when the archive is opened, so the tree holds only
//!   what the catalogue records and no bytes are lost;
//! - an archive whose catalogue is missing is not opened, and one whose catalogue is empty
//!   while `files/` is not has nothing set aside: neither is taken for a fresh archive.
//!
//! Looking for such files means walking all of `files/`, so a start only does it when there
//! can be any. `state.json` says so: it is written, and flushed, when a sync first stores
//! something (`syncing`) and again when the last sync under way has ended with everything it
//! stored recorded, then with how many segments the catalogue records and when the newest was
//! stored. A start that finds it saying `syncing`, or saying something else than the catalogue
//! does (a catalogue from a backup, or one that lost its last commits to a power cut), or
//! doesn't find it, looks.

mod layout;
mod segment;
mod verify;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::catalogue::{self, Catalogue, CatalogueError, Commit, Stored};
use crate::location::LocationId;

pub(crate) use layout::item_folder;
pub use segment::{Problem, VersionReader};
pub(crate) use segment::{Sealed, SegmentWriter};
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
    /// A sync is storing, or one stored something that was never recorded.
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
    /// A segment was put in place and its commit failed: `files/` holds a file the catalogue
    /// doesn't record.
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
    /// Creates an archive in `dir`, which is made if missing and must not hold one already.
    pub fn create(dir: &Path) -> Result<Self> {
        let dir = &absolute(dir)?;
        fs::create_dir_all(dir).map_err(io_error("create", dir))?;
        let lock = lock(dir)?;
        let manifest_path = dir.join(layout::MANIFEST);
        if manifest_path.exists() {
            return Err(ArchiveError::AlreadyAnArchive(dir.to_path_buf()));
        }
        let manifest = Manifest {
            format: FORMAT,
            id: uuid::Uuid::new_v4().simple().to_string(),
            created: catalogue::now_ms(),
        };
        let archive = Self::start(dir, lock, manifest.id.clone())?;
        /* The manifest comes last: it is what says the folder holds an archive, catalogue
        included. */
        let json = serde_json::to_vec_pretty(&manifest)
            .expect("a struct of numbers and text always serializes");
        segment::write_new(&archive.tmp(), &manifest_path, &json)
            .and_then(|()| segment::sync_dir(dir))
            .map_err(io_error("write", &manifest_path))?;
        Ok(archive)
    }

    /// Opens the archive in `dir`. A folder without one is [`ArchiveError::NotAnArchive`], and
    /// an archive without its catalogue is [`ArchiveError::CatalogueMissing`].
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
        Self::start(dir, lock, manifest.id)
    }

    /// What every start does once the lock is held: empties `tmp/`, opens the catalogue and,
    /// when `files/` may hold what the catalogue doesn't record, sets that aside.
    fn start(dir: &Path, lock: File, id: String) -> Result<Self> {
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
        let archive = Self {
            dir: dir.to_path_buf(),
            id,
            catalogue: Catalogue::open(dir)?,
            syncing: Mutex::default(),
            placing: RwLock::default(),
            _lock: lock,
        };
        let recorded = archive.recorded_state()?;
        let state = fs::read(dir.join(layout::STATE))
            .ok()
            .and_then(|json| serde_json::from_slice::<State>(&json).ok());
        if state.as_ref() != Some(&recorded) && archive.set_aside_uncatalogued()? {
            archive.write_state(&recorded)?;
        }
        Ok(archive)
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
        let stored = self
            .catalogue
            .stored_version(version)?
            .ok_or(ArchiveError::UnknownVersion(version))?;
        Ok(self.reader(stored))
    }

    fn reader(&self, stored: Stored) -> VersionReader {
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

    /// A segment to fill, in `tmp/`.
    pub(crate) fn segment_writer(&self) -> Result<SegmentWriter> {
        let tmp = self.tmp();
        SegmentWriter::create(&tmp).map_err(io_error("create a file in", &tmp))
    }

    /**
     * Stores what `commit` records (PLAN §4.1): makes the item durable on disk, with its folder,
     * its `source.json` and `segment`, if there is one, in its place as the bytes of the version
     * from the offset `commit` gives, and then commits to the catalogue. Returns whether a
     * version this starts is held back.
     *
     * A file already where the segment goes is one the catalogue never recorded (it is what an
     * interrupted store leaves) and is set aside. One the catalogue does record is never
     * replaced: [`ArchiveError::SegmentExists`].
     */
    pub(crate) fn store(&self, segment: Option<Sealed>, commit: &Commit<'_>) -> Result<bool> {
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
        let item = layout::item_dir(commit.location, commit.provider, commit.folder);
        let item_dir = self.dir.join(&item);
        /* The folders that gain an entry here. Each is flushed once, at the end. */
        let mut changed = BTreeSet::new();
        segment::create_dirs(&item_dir, &mut changed).map_err(io_error("create", &item_dir))?;
        let source = item_dir.join(layout::SOURCE);
        if !source.exists() {
            let json = serde_json::to_vec_pretty(&SourceManifest {
                provider: commit.provider,
                location: commit.location.as_str(),
                key: &commit.scanned.key,
            })
            .expect("a struct of text always serializes");
            segment::write_new(&self.tmp(), &source, &json).map_err(io_error("write", &source))?;
            changed.insert(item_dir.clone());
        }
        if let (Some(sealed), Some((offset, ..))) = (segment, commit.segment) {
            let version = item.join(layout::version_dir(commit.number));
            let version_dir = self.dir.join(&version);
            segment::create_dirs(&version_dir, &mut changed)
                .map_err(io_error("create", &version_dir))?;
            let relative = version.join(layout::segment_file(offset));
            let to = self.dir.join(&relative);
            let mut file = sealed.file;
            /* The move refuses to replace a file, so nothing here can overwrite one. */
            while let Err((error, unplaced)) = segment::place(file, &to) {
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(io_error("store", &to)(error));
                }
                if self.catalogue.has_segment(
                    commit.location,
                    commit.provider,
                    commit.folder,
                    commit.number,
                    offset,
                )? {
                    return Err(ArchiveError::SegmentExists(to));
                }
                self.set_aside(&relative)?;
                file = unplaced;
            }
            changed.insert(version_dir);
        }
        for dir in changed {
            segment::sync_dir(&dir).map_err(io_error("flush", &dir))?;
        }
        self.catalogue.commit(commit).map_err(|error| {
            self.syncing().unrecorded = true;
            error.into()
        })
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
        segment::write_over(&self.tmp(), &path, &json).map_err(io_error("write", &path))
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
     * Sets aside every segment file and item folder under `files/` that the catalogue doesn't
     * record (the module docs). Returns whether `files/` now holds nothing but what the
     * catalogue records. It doesn't when one could not be moved (a file another program holds),
     * which stays where it is, to be set aside when it is in the way or at a later start.
     */
    fn set_aside_uncatalogued(&self) -> Result<bool> {
        /* A catalogue that records nothing beside files that are there is not a fresh archive:
        it is the wrong catalogue, and setting everything aside would empty the archive. */
        if !self.catalogue.has_items()? {
            let empty = self.entries(Path::new(layout::FILES), true)?.is_empty();
            if !empty {
                log::warn!(
                    "the catalogue of {} records nothing, but its files/ is not empty; \
                     everything there is left where it is",
                    self.dir.display()
                );
            }
            return Ok(empty);
        }
        let uncatalogued = uncatalogued(self.files()?, &self.catalogue.stored()?);
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

    /// Every item folder under `files/` and the files in its version folders, by their paths
    /// relative to the archive, each file with the version and offset its name says it is.
    fn files(&self) -> Result<Vec<ItemFiles>> {
        /* files/<location>/<provider>/<ab>/<item>: item folders are four levels down. */
        let mut items = vec![PathBuf::from(layout::FILES)];
        for _ in 0..4 {
            let mut below = Vec::new();
            for dir in &items {
                for name in self.entries(dir, true)? {
                    below.push(dir.join(name));
                }
            }
            items = below;
        }
        items
            .into_iter()
            .map(|item| {
                let mut segments = Vec::new();
                for version in self.entries(&item, true)? {
                    let dir = item.join(&version);
                    let number = layout::version_of(&version);
                    for file in self.entries(&dir, false)? {
                        segments.push((number.zip(layout::offset_of(&file)), dir.join(file)));
                    }
                }
                Ok((item, segments))
            })
            .collect()
    }

    /// The names of the folders (`dirs`) or of the files in `relative`.
    fn entries(&self, relative: &Path, dirs: bool) -> Result<Vec<String>> {
        let dir = self.dir.join(relative);
        let mut names = Vec::new();
        for entry in fs::read_dir(&dir).map_err(io_error("list", &dir))? {
            let entry = entry.map_err(io_error("list", &dir))?;
            let is_dir = entry.file_type().map_err(io_error("list", &dir))?.is_dir();
            if is_dir == dirs
                && let Ok(name) = entry.file_name().into_string()
            {
                names.push(name);
            }
        }
        Ok(names)
    }
}

/// An item folder under `files/` and the files in its version folders ([`Archive::files`]).
type ItemFiles = (PathBuf, Vec<(Option<(u32, u64)>, PathBuf)>);

/// Of `files`, the item folders and segment files that are not among `stored`.
fn uncatalogued(files: Vec<ItemFiles>, stored: &[Stored]) -> Vec<PathBuf> {
    let mut known: HashMap<PathBuf, HashSet<(u32, u64)>> = HashMap::new();
    for stored in stored {
        known
            .entry(layout::item_dir(
                &stored.location,
                &stored.provider,
                &stored.folder,
            ))
            .or_default()
            .extend(
                stored
                    .segments
                    .iter()
                    .map(|segment| (stored.version.number, segment.offset)),
            );
    }
    let mut uncatalogued = Vec::new();
    for (item, segments) in files {
        match known.get(&item) {
            None => uncatalogued.push(item),
            Some(recorded) => uncatalogued.extend(
                segments
                    .into_iter()
                    .filter(|(segment, _)| !segment.is_some_and(|s| recorded.contains(&s)))
                    .map(|(_, file)| file),
            ),
        }
    }
    uncatalogued
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
    /// The sync is over. When it was the last one under way and everything that was stored is
    /// recorded, `state.json` says so again. If that can't be written, the next start looks
    /// through `files/`, which costs time and nothing else.
    fn drop(&mut self) {
        let mut syncing = self.archive.syncing();
        syncing.locations.remove(&self.location);
        if syncing.locations.is_empty() && syncing.storing && !syncing.unrecorded {
            let written = self
                .archive
                .recorded_state()
                .and_then(|state| self.archive.write_state(&state));
            match written {
                Ok(()) => syncing.storing = false,
                Err(error) => log::warn!("{error}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use crate::location::LocationKind;
    use crate::provider::{ItemKind, ScannedItem};

    fn sealed(archive: &Archive, bytes: &[u8]) -> Sealed {
        let mut writer = archive.segment_writer().unwrap();
        writer.write(bytes).unwrap();
        writer.seal().unwrap()
    }

    #[test]
    fn a_segment_is_set_aside_or_refused_but_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::create(dir.path()).unwrap();
        let location = archive
            .catalogue()
            .add_location(LocationKind::Local, "fixture")
            .unwrap()
            .id;
        let scanned = ScannedItem {
            key: "items/one".to_string(),
            kind: ItemKind::Sidecar,
            session: None,
            length: 5,
            stamp: "1".to_string(),
            lines: false,
            export_version: None,
        };
        let folder = item_folder(&scanned.key);
        let commit = |bytes: &[u8]| Commit {
            location: &location,
            provider: "fixture",
            scanned: &scanned,
            folder: &folder,
            target: catalogue::Target::NewItem,
            number: 1,
            segment: Some((0, bytes.len() as u64, Hash::of(bytes))),
            length: bytes.len() as u64,
            hash: Hash::of(bytes),
        };
        let relative =
            layout::item_dir(&location, "fixture", &folder).join("v0001/0000000000000000.zst");
        let place = archive.dir().join(&relative);
        let orphan = archive.dir().join(layout::ORPHANS).join(&relative);

        /* A file the catalogue doesn't record is where the segment goes: it is set aside. */
        fs::create_dir_all(place.parent().unwrap()).unwrap();
        fs::write(&place, "left by an interrupted store").unwrap();
        archive
            .store(Some(sealed(&archive, b"first")), &commit(b"first"))
            .unwrap();
        let recorded = fs::read(&place).unwrap();
        assert_eq!(fs::read(&orphan).unwrap(), b"left by an interrupted store");

        /* Now the catalogue records the segment there: storing another is refused. */
        let refused = archive.store(Some(sealed(&archive, b"other")), &commit(b"other"));
        assert!(
            matches!(refused, Err(ArchiveError::SegmentExists(_))),
            "{refused:?}"
        );
        assert!(fs::read(&place).unwrap() == recorded);
        assert_eq!(fs::read_dir(archive.tmp()).unwrap().count(), 0);
        assert_eq!(fs::read_dir(orphan.parent().unwrap()).unwrap().count(), 1);
    }
}
