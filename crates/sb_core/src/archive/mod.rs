//! The archive on disk (PLAN §4): the folder scrollback owns, holding the stored bytes and the
//! catalogue. [`layout`] says where things are, [`segment`] writes and reads the stored bytes,
//! and [`verify`] re-hashes them against the catalogue.
//!
//! What keeps archived bytes safe (PLAN §2.3):
//! - a segment is a new file, written whole before the catalogue records it, and it is moved
//!   into place with a rename that fails if a file is already there ([`Archive::store`]), so
//!   the file system itself refuses to replace one;
//! - nothing here deletes a file under `files/`. A file there that the catalogue doesn't know (a
//!   store that was interrupted between the rename and the commit, or a catalogue restored from
//!   an older backup) is moved to `orphans/` when the archive is opened, so the tree holds only
//!   what the catalogue records and no bytes are lost;
//! - an archive whose catalogue is missing is not opened, and one whose catalogue is empty
//!   while `files/` is not has nothing set aside: neither is taken for a fresh archive.

mod layout;
mod segment;
mod verify;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::catalogue::{self, Catalogue, CatalogueError};
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

/// An open archive. Only one can be open on a folder at a time (PLAN §4.4): the lock is held
/// until this is dropped.
#[derive(Debug)]
pub struct Archive {
    dir: PathBuf,
    id: String,
    catalogue: Catalogue,
    /// The locations a sync is running on.
    syncing: Mutex<HashSet<LocationId>>,
    _lock: File,
}

impl Archive {
    /// Creates an archive in `dir`, which is made if missing and must not hold one already.
    pub fn create(dir: &Path) -> Result<Self> {
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

    /// What every start does once the lock is held: empties `tmp/`, opens the catalogue and
    /// sets aside what it doesn't know.
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
            syncing: Mutex::new(HashSet::new()),
            _lock: lock,
        };
        let set_aside = archive.set_aside_uncatalogued()?;
        if set_aside > 0 {
            log::warn!(
                "set aside {set_aside} files or folders the catalogue doesn't record, in {}",
                dir.join(layout::ORPHANS).display()
            );
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

    fn reader(&self, stored: catalogue::Stored) -> VersionReader {
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

    /// A segment to fill, in `tmp/`.
    pub(crate) fn segment_writer(&self) -> Result<SegmentWriter> {
        let tmp = self.tmp();
        SegmentWriter::create(&tmp).map_err(io_error("create a file in", &tmp))
    }

    /**
     * Makes the item `key` durable on disk as far as the catalogue is about to record it: its
     * folder with its `source.json`, and `segment`, if there is one, in its place as the bytes
     * from `offset` of version `number`. The caller commits to the catalogue next.
     *
     * A file already where the segment goes is one the catalogue never recorded (it is what an
     * interrupted store leaves) and is set aside. One the catalogue does record is never
     * replaced: [`ArchiveError::SegmentExists`].
     */
    pub(crate) fn store(
        &self,
        segment: Option<Sealed>,
        location: &LocationId,
        provider: &str,
        key: &str,
        number: u32,
        offset: u64,
    ) -> Result<()> {
        let folder = item_folder(key);
        let item = layout::item_dir(location, provider, &folder);
        let item_dir = self.dir.join(&item);
        /* The folders that gain an entry here. Each is flushed once, at the end. */
        let mut changed = BTreeSet::new();
        segment::create_dirs(&item_dir, &mut changed).map_err(io_error("create", &item_dir))?;
        let source = item_dir.join(layout::SOURCE);
        if !source.exists() {
            let json = serde_json::to_vec_pretty(&SourceManifest {
                provider,
                location: location.as_str(),
                key,
            })
            .expect("a struct of text always serializes");
            segment::write_new(&self.tmp(), &source, &json).map_err(io_error("write", &source))?;
            changed.insert(item_dir.clone());
        }
        if let Some(sealed) = segment {
            let version = item.join(layout::version_dir(number));
            let version_dir = self.dir.join(&version);
            segment::create_dirs(&version_dir, &mut changed)
                .map_err(io_error("create", &version_dir))?;
            let relative = version.join(layout::segment_file(offset));
            let to = self.dir.join(&relative);
            let mut file = sealed.file;
            /* The rename refuses to replace a file, so nothing here can overwrite one. */
            while let Err(blocked) = file.persist_noclobber(&to) {
                if blocked.error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(io_error("store", &to)(blocked.error));
                }
                if self
                    .catalogue
                    .has_segment(location, provider, &folder, number, offset)?
                {
                    return Err(ArchiveError::SegmentExists(to));
                }
                self.set_aside(&relative)?;
                file = blocked.path;
            }
            changed.insert(version_dir);
        }
        for dir in changed {
            segment::sync_dir(&dir).map_err(io_error("flush", &dir))?;
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

    /// Sets aside every segment file and item folder under `files/` that the catalogue doesn't
    /// record (the module docs). Returns how many were moved. One that can't be moved now (a
    /// file another program holds) stays where it is, to be dealt with when it is in the way.
    fn set_aside_uncatalogued(&self) -> Result<usize> {
        let mut known: HashMap<PathBuf, HashSet<(u32, u64)>> = HashMap::new();
        for stored in self.catalogue.stored()? {
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
        /* A catalogue that records nothing beside files that are there is not a fresh archive:
        it is the wrong catalogue, and setting everything aside would empty the archive. */
        if !self.catalogue.has_items()? && !items.is_empty() {
            log::warn!(
                "the catalogue of {} records nothing, but its files/ holds {} items; they are \
                 left where they are",
                self.dir.display(),
                items.len()
            );
            return Ok(0);
        }
        let mut uncatalogued = Vec::new();
        for item in items {
            let Some(segments) = known.get(&item) else {
                uncatalogued.push(item);
                continue;
            };
            for version in self.entries(&item, true)? {
                let dir = item.join(&version);
                let number = layout::version_of(&version);
                for file in self.entries(&dir, false)? {
                    let recorded = number
                        .zip(layout::offset_of(&file))
                        .is_some_and(|segment| segments.contains(&segment));
                    if !recorded {
                        uncatalogued.push(dir.join(file));
                    }
                }
            }
        }
        let mut set_aside = 0;
        for relative in uncatalogued {
            match self.set_aside(&relative) {
                Ok(()) => set_aside += 1,
                Err(error) => log::warn!("{error}"),
            }
            /* A version folder left empty goes; one with anything in it stays. */
            if let Some(dir) = relative.parent() {
                let _ = fs::remove_dir(self.dir.join(dir));
            }
        }
        Ok(set_aside)
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
