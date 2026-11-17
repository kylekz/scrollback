//! Moving the archive into another folder (PLAN §4.4 "Moving the archive", DESIGN §4.13
//! Move…). The archive's folder is only ever read: nothing in it is changed, moved or deleted.
//!
//! Lifted from folio's `folio_core::relocate` (the owner's, MIT; PLAN D15) and adapted: the
//! check of the folder moved to, the copy that hashes each file as it reads it and again from
//! the copy, the database copied with `VACUUM INTO` and checked by `integrity_check` and its
//! tables' row counts, and the progress the copy reports. folio reaches its database through a
//! copy of Zed's `sqlez`, which is GPL and not lifted: the copy here is toolbox_store's checked
//! one. scrollback's own: the move finishes in the running app, with nothing running on the
//! archive, rather than at the next start; every segment is checked against the catalogue
//! too; each copy goes into place through the new folder's `tmp/`, as the archive's own files
//! do; and a marker in the new folder names the move, so a start after a crash can tell the
//! partial copy apart ([`clear_move`]).
//!
//! A move, which the caller keeps the settings file in step with:
//! 1. [`Archive::plan_move`] looks at the folder and measures what goes, making nothing: a
//!    folder that can't take the archive is a [`Refusal`].
//! 2. [`Archive::start_move`] makes the folder if it is missing, takes its `lock` and writes
//!    the marker ([`MARKER`]); [`Move::copy`] copies every file while the archive is in use.
//!    Segments are write-once, so what is copied stays right.
//! 3. [`Move::finish`], once nothing runs on the archive: the catalogue, the files added or
//!    changed since and `archive.json` are copied, every segment the copied catalogue records
//!    is checked against it, and the copy is opened as an archive. The marker goes, and then
//!    `commit` points the app's settings at the new folder: the point of no return.
//!
//! [`Move::abandon`], after a stop or a failure, and [`clear_move`], at a start after a crash,
//! remove what the move made in the new folder: the entries the marker names and the
//! archive's own, never anything else there.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tempfile::{NamedTempFile, TempPath};
use thiserror::Error;
use toolbox_process::CancelToken;
use toolbox_store::Backups;
use toolbox_store::rusqlite::{self, OpenFlags};

use super::stats::{Holds, archive_around, is_system_file, resolved, what_is_in};
use super::{Archive, ArchiveError, disk, layout, lock, segment};
use crate::catalogue::{self, Stored};
use crate::hash::{Hash, Hasher};
use crate::search::SEARCH_DATABASE;

/// The new folder's marker while a move fills it: the move's id and what it copies there.
pub const MARKER: &str = "moving.json";

/// The marker's version this build writes and reads.
const MARKER_VERSION: u32 = 1;

/// A database file's name and its journals'.
const JOURNALS: [&str; 4] = ["", "-wal", "-shm", "-journal"];

/// What an archive holds besides its databases and its `lock`: a move copies what of it
/// appears while it runs, and removes all of it when it is abandoned.
const MADE: [&str; 5] = [
    layout::MANIFEST,
    layout::TMP,
    layout::FILES,
    layout::ORPHANS,
    layout::BACKUPS,
];

/// How much is read and written at a time.
const BUFFER: usize = 256 * 1024;

/// Why a folder can't take the archive (PLAN §4.4). The app says each in its own words.
#[derive(Debug, Error)]
pub enum Refusal {
    #[error("it is the archive's own folder")]
    Same,
    #[error("it is inside the archive's folder")]
    Inside,
    #[error("it holds the archive's folder")]
    Holds,
    #[error("it holds an archive")]
    AnArchive,
    /// It is inside the archive in `other`, or holds one there.
    #[error("it is inside the archive in {}, or holds one", .0.display())]
    Nested(PathBuf),
    /// It holds something besides the files an operating system leaves in any folder.
    #[error("it isn't empty")]
    NotEmpty,
    #[error("it is a file")]
    NotAFolder,
    #[error("it can't be read: {0}")]
    Unreadable(#[source] io::Error),
    #[error("it can't be written: {0}")]
    Unwritable(#[source] io::Error),
    /// The volume of the folder `volume` has `free` bytes, and the move needs `needed`.
    #[error("{} has {free} bytes free and the move needs {needed}", .volume.display())]
    NoRoom {
        volume: PathBuf,
        free: u64,
        needed: u64,
    },
}

#[derive(Debug, Error)]
pub enum MoveError {
    #[error("the archive can't be moved there: {0}")]
    Refused(#[from] Refusal),
    #[error("could not {action} {}: {source}", .path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// Another program kept the file open past the retries (an antivirus scan, say).
    #[error("{} is held open by another program", .0.display())]
    Held(PathBuf),
    /// A copy doesn't hold the bytes read from the archive, or a segment's copy not those the
    /// catalogue records when the archive's own file does.
    #[error("the copy {} doesn't hold what the archive does", .0.display())]
    Mismatch(PathBuf),
    /// The copied catalogue's rows aren't the archive's: which tables, and their counts.
    #[error("the copied catalogue doesn't match the archive's ({0})")]
    CatalogueMismatch(String),
    #[error("the move was stopped")]
    Cancelled,
    /// The app's settings couldn't be pointed at the new folder: the move didn't happen.
    #[error("the settings couldn't be written: {0}")]
    Commit(#[source] io::Error),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Store(#[from] toolbox_store::Error),
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    /// A crash a test made at `0`.
    #[cfg(test)]
    #[error("crashed at {0:?}")]
    Crashed(Step),
}

/// `action` on `path` failed: [`MoveError::Held`] when another program holds it, else
/// [`MoveError::Io`].
fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> MoveError + use<> {
    let path = path.to_path_buf();
    move |source| {
        if disk::is_held(&source) {
            MoveError::Held(path)
        } else {
            MoveError::Io {
                action,
                path,
                source,
            }
        }
    }
}

/// What moving the archive into a folder copies and needs ([`Archive::plan_move`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovePlan {
    /// The folder, absolute.
    pub target: PathBuf,
    /// The move's own id, which the app's settings and the new folder's marker both keep.
    pub id: String,
    /// The files copied, besides the catalogue, and how big they are.
    pub files: usize,
    pub bytes: u64,
    /// What the move needs on the folder's volume: those files and the catalogue, and a
    /// twentieth more for what syncs add meanwhile.
    pub needed: u64,
    /// The volume's free space, when it could be told, and the folder it was asked of: the
    /// nearest there is of the folder and those it is in.
    pub free: Option<u64>,
    pub volume: PathBuf,
}

/// What a move is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MoveStage {
    /// Copying the archive's files while it is in use.
    #[default]
    Copying,
    /// Copying what changed since, the catalogue and `archive.json`, and checking.
    Finishing,
    /// Pointing the app's settings at the new folder: it can't be stopped now.
    Switching,
}

/// How far a move is: the files, and their bytes, done of those it has to copy now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MoveProgress {
    pub stage: MoveStage,
    pub files_done: usize,
    pub files: usize,
    pub bytes_done: u64,
    pub bytes: u64,
}

/// What became of a partial copy ([`Move::abandon`], [`clear_move`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cleared {
    /// Everything the move made there is gone.
    Removed,
    /// These, relative to the folder, could not be removed: another program holds them.
    Partly(Vec<PathBuf>),
    /// The folder holds no marker of this move, or another scrollback has it open: nothing
    /// there was touched.
    Left,
}

/// The new folder's [`MARKER`].
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Marker {
    v: u32,
    id: String,
    /// The archive's own entries the move copies into the folder, by name.
    names: Vec<String>,
    /// Whether the move made the folder itself.
    made: bool,
}

/// Where a move can be cut short, for the tests that crash it there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The first file is copied.
    Copying,
    /// Every file is copied.
    Copied,
    /// The catalogue is copied, in the new folder's `tmp/`.
    Catalogue,
    /// What changed since the copy is copied.
    Caught,
    /// The catalogue and `archive.json` are in place.
    Placed,
    /// The copy is open as an archive, its segments checked.
    Checked,
    /// The marker is gone.
    Unmarked,
    /// The settings name the new folder.
    Committed,
}

/// A file the move copies, as it was listed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Listed {
    /// Relative to the archive's folder.
    relative: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

/// A file the move copied: the size and time its original had when listed, and the hash of
/// the bytes the catalogue records when the copy was found to hold them.
#[derive(Clone, Copy, Debug)]
struct Copied {
    len: u64,
    modified: Option<SystemTime>,
    checked: Option<Hash>,
}

/// A move under way ([`Archive::start_move`]). Dropped, it leaves what it made where it is,
/// as a crash would: [`Move::abandon`] removes it.
#[derive(Debug)]
pub struct Move {
    target: PathBuf,
    marker: Marker,
    /// The new folder's lock, until it passes to the archive opened there.
    lock: Option<File>,
    copied: HashMap<PathBuf, Copied>,
    /// The folders made in the new folder, whose entries are flushed before the commit.
    folders: BTreeSet<PathBuf>,
    #[cfg(test)]
    crash: Option<Step>,
}

impl Archive {
    /**
     * Looks at `target` as the folder to move the archive into, and measures what the move
     * copies (the module docs). Nothing is made. A folder that can't take the archive is a
     * [`Refusal`]: the volume too full among them, and a folder another program won't let be
     * read or written.
     */
    pub fn plan_move(&self, target: &Path) -> Result<MovePlan, MoveError> {
        let target = refusal(&self.dir, target)?;
        let files = carried(&self.dir)?;
        let bytes = files.iter().map(|file| file.len).sum::<u64>();
        let catalogue = JOURNALS
            .iter()
            .filter_map(|journal| {
                fs::metadata(self.dir.join(name_of(catalogue::DATABASE, journal))).ok()
            })
            .map(|metadata| metadata.len())
            .sum::<u64>();
        let needed = (bytes + catalogue).saturating_add((bytes + catalogue) / 20);
        let volume = nearest(&target);
        let free = disk::free_space(&volume)
            .inspect_err(|error| {
                log::warn!(
                    "{}'s free space couldn't be read: {error}",
                    volume.display()
                );
            })
            .ok();
        room(&volume, free, needed)?;
        Ok(MovePlan {
            target,
            id: uuid::Uuid::new_v4().simple().to_string(),
            files: files.len(),
            bytes,
            needed,
            free,
            volume,
        })
    }

    /**
     * Starts the move `plan` says: looks at its folder again, makes it if it is missing, takes
     * its lock (so no scrollback opens or makes an archive there while the move fills it) and
     * writes the marker. The app's settings have the move recorded already.
     */
    pub fn start_move(&self, plan: &MovePlan) -> Result<Move, MoveError> {
        let target = refusal(&self.dir, &plan.target)?;
        let made = fs::symlink_metadata(&target).is_err();
        fs::create_dir_all(&target).map_err(Refusal::Unwritable)?;
        let lock = lock(&target)?;
        /* Again under the lock: something may have been put there since it was looked at. */
        let held = fs::read_dir(&target)
            .map_err(Refusal::Unreadable)?
            .flatten()
            .any(|entry| {
                let name = entry.file_name();
                name != layout::LOCK && !is_system_file(&name.to_string_lossy())
            });
        if held {
            drop(lock);
            if let Err(error) = fs::remove_file(target.join(layout::LOCK)) {
                log::warn!("{error}");
            }
            return Err(Refusal::NotEmpty.into());
        }
        /* The archive's own entries, and those an archive makes, which may appear meanwhile. */
        let names = fs::read_dir(&self.dir)
            .map_err(io_error("list", &self.dir))?
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_symlink()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .chain(MADE.map(str::to_string))
            .filter(|name| !left_out(name))
            .collect::<BTreeSet<_>>();
        let started = Move {
            target,
            marker: Marker {
                v: MARKER_VERSION,
                id: plan.id.clone(),
                names: names.into_iter().collect(),
                made,
            },
            lock: Some(lock),
            copied: HashMap::new(),
            folders: BTreeSet::new(),
            #[cfg(test)]
            crash: None,
        };
        match started.mark() {
            Ok(()) => Ok(started),
            Err(error) => {
                started.abandon();
                Err(error)
            }
        }
    }
}

impl Move {
    /// The folder the archive is moving into, absolute.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Makes the new folder's `tmp/`, where every copy is written first, and its marker.
    fn mark(&self) -> Result<(), MoveError> {
        let tmp = self.target.join(layout::TMP);
        fs::create_dir(&tmp).map_err(io_error("create", &tmp))?;
        let json =
            serde_json::to_vec_pretty(&self.marker).expect("a struct of text always serializes");
        let path = self.target.join(MARKER);
        disk::write_new(&tmp, &path, &json)
            .and_then(|()| disk::push_dir(&self.target))
            .map_err(io_error("write", &path))
    }

    /**
     * Copies every file of `archive` but the catalogue and `archive.json` into the new folder,
     * while the archive is in use, telling `progress` after each, until `cancel` stops it
     * ([`MoveError::Cancelled`]). Each copy is checked against what was read, and a segment's
     * against the catalogue too. A file that goes before it is read (a delete for good) is
     * passed over.
     */
    pub fn copy(
        &mut self,
        archive: &Archive,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(&MoveProgress),
    ) -> Result<(), MoveError> {
        let recorded = recorded(&archive.catalogue.stored().map_err(ArchiveError::from)?);
        let files = carried(&archive.dir)?;
        let mut done = MoveProgress {
            stage: MoveStage::Copying,
            files: files.len(),
            bytes: files.iter().map(|file| file.len).sum(),
            ..MoveProgress::default()
        };
        progress(&done);
        for file in &files {
            if cancel.is_cancelled() {
                return Err(MoveError::Cancelled);
            }
            if self.copy_file(&archive.dir, file, cancel)?
                && let Some(&(length, hash)) = recorded.get(&file.relative)
            {
                self.check(&file.relative, length, hash)?;
            }
            done.files_done += 1;
            done.bytes_done += file.len;
            progress(&done);
            self.step(Step::Copying)?;
        }
        self.step(Step::Copied)
    }

    /**
     * Finishes the move, once nothing runs on `archive` and nothing new will start (the app's
     * services are closed): the module docs' third step, telling `progress` how the files
     * changed since are copied, and `commit` last, which points the app's settings at the new
     * folder. Returns the archive there, open. Until `commit` returns, the move can fail or be
     * stopped, and the archive is as it was: [`Move::abandon`] then removes the copy. Nothing
     * is written to `archive` from here on, by this or by its background removal.
     */
    pub fn finish(
        &mut self,
        archive: &Archive,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(&MoveProgress),
        commit: impl FnOnce(&Path) -> io::Result<()>,
    ) -> Result<Archive, MoveError> {
        /* No segment is between its place and its commit, and nothing is removed. */
        let _quiet = archive
            .placing
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let removals = archive.hold_removals();
        let moved = self.finishing(archive, cancel, progress, commit)?;
        removals.for_good();
        moved.start_removals(true);
        Ok(moved)
    }

    fn finishing(
        &mut self,
        archive: &Archive,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(&MoveProgress),
        commit: impl FnOnce(&Path) -> io::Result<()>,
    ) -> Result<Archive, MoveError> {
        let tmp = self.target.join(layout::TMP);
        let database = archive.dir.join(catalogue::DATABASE);
        /* Nothing writes the catalogue now, so its copy is the whole of it: rows that differ
        are a failure, not a reason to take it again. */
        let copy = Backups::new(&tmp, "scrollback")
            .before(&database, "move")?
            .path;
        let copy = TempPath::try_from_path(&copy).map_err(io_error("read", &copy))?;
        same_rows(&database, &copy)?;
        disk::push_at(&copy).map_err(io_error("flush", &copy))?;
        self.step(Step::Catalogue)?;

        let files = carried(&archive.dir)?;
        let there: HashSet<&Path> = files.iter().map(|file| file.relative.as_path()).collect();
        let gone: Vec<PathBuf> = (self.copied.keys())
            .filter(|relative| !there.contains(relative.as_path()))
            .cloned()
            .collect();
        for relative in gone {
            remove_file(&self.target.join(&relative))?;
            self.copied.remove(&relative);
        }
        let changed: Vec<&Listed> = files
            .iter()
            .filter(|file| {
                self.copied
                    .get(&file.relative)
                    .is_none_or(|copied| (copied.len, copied.modified) != (file.len, file.modified))
            })
            .collect();
        let mut done = MoveProgress {
            stage: MoveStage::Finishing,
            files: changed.len(),
            bytes: changed.iter().map(|file| file.len).sum(),
            ..MoveProgress::default()
        };
        progress(&done);
        for file in changed {
            if cancel.is_cancelled() {
                return Err(MoveError::Cancelled);
            }
            self.copy_file(&archive.dir, file, cancel)?;
            done.files_done += 1;
            done.bytes_done += file.len;
            progress(&done);
        }
        self.step(Step::Caught)?;

        let place = self.target.join(catalogue::DATABASE);
        disk::place_retrying(copy, &place)
            .map_err(|(error, _)| io_error("store", &place)(error))?;
        let manifest = archive.dir.join(layout::MANIFEST);
        let metadata = fs::metadata(&manifest).map_err(io_error("read", &manifest))?;
        let listed = Listed {
            relative: PathBuf::from(layout::MANIFEST),
            len: metadata.len(),
            modified: metadata.modified().ok(),
        };
        if !self.copy_file(&archive.dir, &listed, cancel)? {
            return Err(io_error("read", &manifest)(io::ErrorKind::NotFound.into()));
        }
        self.settle()?;
        self.step(Step::Placed)?;

        let lock = self
            .lock
            .take()
            .expect("a move is finished once: its lock passes to the archive opened");
        let moved = Archive::open_locked(&self.target, lock)?;
        self.check_all(&archive.dir, &moved, cancel)?;
        self.step(Step::Checked)?;

        /* The marker goes before the settings name the folder, so a folder that holds one
        is never the archive in use, and a start after a crash may remove what it holds. */
        let marker = self.target.join(MARKER);
        disk::retrying(|| fs::remove_file(&marker))
            .and_then(|()| disk::push_dir(&self.target))
            .and_then(|()| disk::settle(&self.target))
            .map_err(io_error("remove", &marker))?;
        self.step(Step::Unmarked)?;
        progress(&MoveProgress {
            stage: MoveStage::Switching,
            ..done
        });
        commit(&self.target).map_err(MoveError::Commit)?;
        self.step(Step::Committed)?;
        Ok(moved)
    }

    /**
     * Removes what the move made in the new folder (the module docs), after a stop or a
     * failure: the archive the app is on is the one it was. What another program holds is left,
     * and said.
     */
    pub fn abandon(mut self) -> Cleared {
        let lock = self.lock.take();
        clear(&self.target, &self.marker, lock)
    }

    /**
     * Copies the archive's file `listed`, in its folder `from`, into the new folder: read and
     * hashed into a file in the new folder's `tmp/`, flushed, read back and hashed again, then
     * moved into its place, over a copy made of it before. Returns whether it was there to
     * copy. An entry at the top that isn't among those the marker names is left where it is:
     * the move would not know to remove it again.
     */
    fn copy_file(
        &mut self,
        from: &Path,
        listed: &Listed,
        cancel: &CancelToken,
    ) -> Result<bool, MoveError> {
        let top = listed.relative.iter().next().unwrap_or_default();
        if !self.marker.names.iter().any(|name| OsStr::new(name) == top) && top != layout::MANIFEST
        {
            log::warn!(
                "{} appeared in the archive's folder during the move, and stays there",
                from.join(top).display()
            );
            return Ok(false);
        }
        let source = from.join(&listed.relative);
        let mut reader = match disk::retrying(|| File::open(&source)) {
            Ok(reader) => reader,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_error("read", &source)(error)),
        };
        let tmp = self.target.join(layout::TMP);
        let mut staged = NamedTempFile::new_in(&tmp).map_err(io_error("write", &tmp))?;
        let (mut hasher, mut buffer, mut len) = (Hasher::new(), vec![0; BUFFER], 0u64);
        loop {
            if cancel.is_cancelled() {
                return Err(MoveError::Cancelled);
            }
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_error("read", &source)(error)),
            };
            hasher.update(&buffer[..read]);
            staged
                .write_all(&buffer[..read])
                .map_err(io_error("write", staged.path()))?;
            len += read as u64;
        }
        drop(reader);
        disk::push(staged.as_file()).map_err(io_error("flush", staged.path()))?;
        let staged = staged.into_temp_path();
        let to = self.target.join(&listed.relative);
        if hash_file(&staged)? != (len, hasher.finish()) {
            return Err(MoveError::Mismatch(to));
        }
        if let Some(parent) = listed.relative.parent() {
            self.make_folders(parent)?;
        }
        /* A copy there already is the move's own, of the file as it was. */
        if self.copied.contains_key(&listed.relative) {
            remove_file(&to)?;
        }
        disk::place_retrying(staged, &to).map_err(|(error, _)| io_error("store", &to)(error))?;
        self.copied.insert(
            listed.relative.clone(),
            Copied {
                len: listed.len,
                modified: listed.modified,
                checked: None,
            },
        );
        Ok(true)
    }

    /// Makes the folder `relative` (to the new folder) and those it is in, keeping each to be
    /// flushed ([`Move::settle`]).
    fn make_folders(&mut self, relative: &Path) -> Result<(), MoveError> {
        let dir = self.target.join(relative);
        if self.folders.contains(&dir) {
            return Ok(());
        }
        disk::retrying(|| fs::create_dir_all(&dir)).map_err(io_error("create", &dir))?;
        self.folders.extend(
            relative
                .ancestors()
                .take_while(|folder| !folder.as_os_str().is_empty())
                .map(|folder| self.target.join(folder)),
        );
        Ok(())
    }

    /// Makes what was copied durable: the entries of every folder made, and the new folder's.
    fn settle(&self) -> Result<(), MoveError> {
        self.folders
            .iter()
            .chain([&self.target])
            .try_for_each(|folder| disk::push_dir(folder))
            .and_then(|()| disk::settle(&self.target))
            .map_err(io_error("flush", &self.target))
    }

    /// Whether the copy of the segment `relative` holds the `length` bytes of `hash`, which
    /// the catalogue records; kept when it does.
    fn check(&mut self, relative: &Path, length: u64, hash: Hash) -> Result<bool, MoveError> {
        let copy = self.target.join(relative);
        let holds = holds(&copy, length, hash)?;
        if holds && let Some(copied) = self.copied.get_mut(relative) {
            copied.checked = Some(hash);
        }
        Ok(holds)
    }

    /**
     * Checks every segment the copied catalogue of `moved` records: one checked as it was
     * copied and unchanged since counts. A copy that doesn't hold what the catalogue records
     * fails the move unless the archive's own file, in `from`, doesn't either: then the copy
     * is as the archive has it, and a verify of either finds the same.
     */
    fn check_all(
        &mut self,
        from: &Path,
        moved: &Archive,
        cancel: &CancelToken,
    ) -> Result<(), MoveError> {
        for (relative, (length, hash)) in
            recorded(&moved.catalogue.stored().map_err(ArchiveError::from)?)
        {
            if cancel.is_cancelled() {
                return Err(MoveError::Cancelled);
            }
            let checked = self.copied.get(&relative).and_then(|copied| copied.checked);
            if checked == Some(hash) || self.check(&relative, length, hash)? {
                continue;
            }
            if holds(&from.join(&relative), length, hash)? {
                return Err(MoveError::Mismatch(self.target.join(&relative)));
            }
            log::warn!(
                "{} doesn't hold what the catalogue records, in the archive as in its copy",
                from.join(&relative).display()
            );
        }
        Ok(())
    }

    /// Fails as a crash would at `step`, when a test asked for one there.
    #[cfg(test)]
    fn step(&self, step: Step) -> Result<(), MoveError> {
        match self.crash {
            Some(crash) if crash == step => Err(MoveError::Crashed(step)),
            _ => Ok(()),
        }
    }

    #[cfg(not(test))]
    fn step(&self, _: Step) -> Result<(), MoveError> {
        Ok(())
    }

    /// Has the move fail at `step` as a crash would: it stops there and leaves what it made.
    #[cfg(test)]
    pub(super) fn crash_at(&mut self, step: Step) {
        self.crash = Some(step);
    }
}

/**
 * Removes the partial copy a move cut short by a crash left in `target`: what its marker
 * names, if the marker there is of the move `id` and no scrollback has the folder open
 * (otherwise [`Cleared::Left`]). The archive the app is on is never this folder: its marker
 * goes before the app's settings name it.
 */
pub fn clear_move(target: &Path, id: &str) -> Cleared {
    let marker = fs::read(target.join(MARKER))
        .ok()
        .and_then(|json| serde_json::from_slice::<Marker>(&json).ok())
        .filter(|marker| marker.v == MARKER_VERSION && marker.id == id);
    let Some(marker) = marker else {
        return Cleared::Left;
    };
    match lock(target) {
        Ok(lock) => clear(target, &marker, Some(lock)),
        Err(error) => {
            log::warn!("{} is left as it is: {error}", target.display());
            Cleared::Left
        }
    }
}

/// Removes from `target` what the move `marker` is of made there, with its `lock` held while
/// it does; the lock and the marker go last, and the folder too if the move made it and it is
/// empty then.
fn clear(target: &Path, marker: &Marker, lock: Option<File>) -> Cleared {
    let mut left = Vec::new();
    let ours = marker
        .names
        .iter()
        .map(String::as_str)
        .chain(MADE)
        .map(str::to_string)
        .chain(
            [catalogue::DATABASE, SEARCH_DATABASE]
                .iter()
                .flat_map(|database| JOURNALS.map(|journal| name_of(database, journal))),
        )
        .collect::<BTreeSet<_>>();
    for name in &ours {
        remove_all(target, Path::new(name), &mut left);
    }
    drop(lock);
    for name in [layout::LOCK, MARKER] {
        remove_all(target, Path::new(name), &mut left);
    }
    if marker.made
        && left.is_empty()
        && let Err(error) = fs::remove_dir(target)
    {
        log::warn!("{} is left: {error}", target.display());
    }
    if left.is_empty() {
        Cleared::Removed
    } else {
        Cleared::Partly(left)
    }
}

/// Removes the move's own copy at `path`, unless it is gone already.
fn remove_file(path: &Path) -> Result<(), MoveError> {
    match disk::retrying(|| fs::remove_file(path)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(io_error("remove", path)(error))
        }
        _ => Ok(()),
    }
}

/// Removes `relative` in `dir`, and all a folder there holds, without following a link;
/// `left` gains what can't be removed.
fn remove_all(dir: &Path, relative: &Path, left: &mut Vec<PathBuf>) {
    let path = dir.join(relative);
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return;
    };
    if metadata.is_dir() {
        for entry in fs::read_dir(&path).into_iter().flatten().flatten() {
            remove_all(dir, &relative.join(entry.file_name()), left);
        }
    }
    let removed = disk::retrying(|| {
        if metadata.is_dir() {
            fs::remove_dir(&path)
        } else {
            fs::remove_file(&path)
        }
    });
    match removed {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            log::warn!("{} is left: {error}", path.display());
            if !left.iter().any(|it| relative.starts_with(it)) {
                left.push(relative.to_path_buf());
            }
        }
    }
}

/**
 * Why `target` can't take the archive in `archive`, or `target` absolute when it can (PLAN
 * §4.4): it isn't the archive's folder, inside it or around it; nor inside another archive or
 * holding one; it holds nothing but system files, or isn't there yet; and it, or the nearest
 * folder it is in, can be written. Nothing is left behind by looking.
 */
fn refusal(archive: &Path, target: &Path) -> Result<PathBuf, Refusal> {
    let target = std::path::absolute(target).map_err(Refusal::Unreadable)?;
    let (here, there) = (resolved(archive), resolved(&target));
    if there == here {
        return Err(Refusal::Same);
    }
    if there.starts_with(&here) {
        return Err(Refusal::Inside);
    }
    if here.starts_with(&there) {
        return Err(Refusal::Holds);
    }
    match fs::metadata(&target) {
        Ok(metadata) if !metadata.is_dir() => return Err(Refusal::NotAFolder),
        Ok(_) => match what_is_in(&target) {
            Ok(Holds::Nothing) => {}
            Ok(Holds::Archive) => return Err(Refusal::AnArchive),
            Ok(Holds::InsideAnArchive(other)) => return Err(Refusal::Nested(other)),
            Ok(Holds::Other) => return Err(Refusal::NotEmpty),
            Err(ArchiveError::Io { source, .. }) => return Err(Refusal::Unreadable(source)),
            Err(error) => return Err(Refusal::Unreadable(io::Error::other(error))),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(other) = archive_around(&target) {
                return Err(Refusal::Nested(other));
            }
        }
        Err(error) => return Err(Refusal::Unreadable(error)),
    }
    /* A file with no name, gone when it is closed. */
    tempfile::tempfile_in(nearest(&target)).map_err(Refusal::Unwritable)?;
    Ok(target)
}

/// Refuses a move that `needed` bytes on the volume of the folder `volume` would not fit in,
/// when its `free` space is known.
fn room(volume: &Path, free: Option<u64>, needed: u64) -> Result<(), Refusal> {
    match free {
        Some(free) if free < needed => Err(Refusal::NoRoom {
            volume: volume.to_path_buf(),
            free,
            needed,
        }),
        _ => Ok(()),
    }
}

/// The nearest folder there is of `path` and those it is in.
fn nearest(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|folder| folder.is_dir())
        .unwrap_or(path)
        .to_path_buf()
}

/// `database`'s file with `journal`'s suffix (`-wal`, or none).
fn name_of(database: &str, journal: &str) -> String {
    format!("{database}{journal}")
}

/**
 * Whether the entry `name` at the top of the archive's folder is left out of a move: the live
 * catalogue and its journals (copied with `VACUUM INTO` instead), the search index (rebuilt by
 * the new archive), work in flight, the lock, a marker some move left, and `archive.json`,
 * which the finish copies last.
 */
fn left_out(name: &str) -> bool {
    [layout::LOCK, layout::TMP, layout::MANIFEST, MARKER].contains(&name)
        || [catalogue::DATABASE, SEARCH_DATABASE]
            .iter()
            .any(|database| {
                JOURNALS
                    .iter()
                    .any(|journal| name == name_of(database, journal))
            })
}

/**
 * Every file of the archive's folder `dir` a move copies, sorted ([`left_out`] isn't). Links
 * are neither followed nor copied. A folder or a file that goes while it is listed (a delete
 * for good removing it) is passed over.
 */
fn carried(dir: &Path) -> Result<Vec<Listed>, MoveError> {
    let mut files = Vec::new();
    let mut folders = vec![PathBuf::new()];
    while let Some(relative) = folders.pop() {
        let folder = dir.join(&relative);
        let entries = match fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound && relative.as_os_str() != "" => {
                continue;
            }
            Err(error) => return Err(io_error("list", &folder)(error)),
        };
        for entry in entries {
            let entry = entry.map_err(io_error("list", &folder))?;
            let name = entry.file_name();
            if relative.as_os_str().is_empty() && left_out(&name.to_string_lossy()) {
                continue;
            }
            let path = relative.join(&name);
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error("read", &entry.path())(error)),
            };
            if kind.is_symlink() {
                log::warn!("{} is a link; a move leaves it out", entry.path().display());
            } else if kind.is_dir() {
                folders.push(path);
            } else if kind.is_file() {
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(io_error("read", &entry.path())(error)),
                };
                files.push(Listed {
                    relative: path,
                    len: metadata.len(),
                    modified: metadata.modified().ok(),
                });
            }
        }
    }
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(files)
}

/// Every segment `stored` records, by its file's path relative to the archive: its length and
/// the hash of its bytes.
fn recorded(stored: &[Stored]) -> BTreeMap<PathBuf, (u64, Hash)> {
    stored
        .iter()
        .flat_map(|stored| {
            let version = layout::item_dir(&stored.location, &stored.provider, &stored.folder)
                .join(layout::version_dir(stored.version.number));
            stored.segments.iter().map(move |segment| {
                (
                    version.join(layout::segment_file(segment.offset)),
                    (segment.length, segment.hash),
                )
            })
        })
        .collect()
}

/// [`segment::holds`], with a file that isn't there holding nothing.
fn holds(path: &Path, length: u64, hash: Hash) -> Result<bool, MoveError> {
    match disk::retrying(|| segment::holds(path, length, hash)) {
        Ok(holds) => Ok(holds),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("read", path)(error)),
    }
}

/// How long the file at `path` is, and the hash of its bytes.
fn hash_file(path: &Path) -> Result<(u64, Hash), MoveError> {
    let mut file = disk::retrying(|| File::open(path)).map_err(io_error("read", path))?;
    let (mut hasher, mut buffer, mut len) = (Hasher::new(), vec![0; BUFFER], 0u64);
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error("read", path)(error)),
        };
        hasher.update(&buffer[..read]);
        len += read as u64;
    }
    Ok((len, hasher.finish()))
}

/// Fails unless every table of the catalogue `copy` has as many rows as the archive's at
/// `database`, which nothing writes meanwhile.
fn same_rows(database: &Path, copy: &Path) -> Result<(), MoveError> {
    let (theirs, ours) = (rows(database)?, rows(copy)?);
    if theirs == ours {
        return Ok(());
    }
    let tables: BTreeSet<&String> = theirs.keys().chain(ours.keys()).collect();
    Err(MoveError::CatalogueMismatch(
        tables
            .into_iter()
            .filter(|table| theirs.get(*table) != ours.get(*table))
            .map(|table| {
                format!(
                    "{table}: {} rows, the copy {}",
                    theirs.get(table).copied().unwrap_or(0),
                    ours.get(table).copied().unwrap_or(0)
                )
            })
            .collect::<Vec<_>>()
            .join(", "),
    ))
}

/// How many rows each table of the database at `path` has, read on a connection of its own
/// that can't write.
fn rows(path: &Path) -> Result<BTreeMap<String, i64>, MoveError> {
    let connection = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let tables: Vec<String> = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    tables
        .into_iter()
        .map(|table| {
            let quoted = table.replace('"', "\"\"");
            let count =
                connection.query_row(&format!("SELECT count(*) FROM \"{quoted}\""), [], |row| {
                    row.get(0)
                })?;
            Ok((table, count))
        })
        .collect()
}

#[cfg(test)]
mod tests;
