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
use std::io::{self, Read, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{PoisonError, mpsc};
use std::thread;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tempfile::{NamedTempFile, TempPath};
use thiserror::Error;
use toolbox_process::CancelToken;
use toolbox_store::Backups;
use toolbox_store::rusqlite::{self, OpenFlags};

use super::stats::{
    Holds, archive_around, is_system_file, on_network, resolved, synced_by, what_is_in,
};
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

/**
 * How many files a move copies at once ([`Move::copy_files`]). A copy waits on the drive more
 * than on the processor: on Windows on each file's flush and on the antivirus that scans each
 * file written, which several copies overlap well (robocopy copies 8 at once for the same
 * reason). APFS puts each file's creation and rename one after another, so on macOS more than
 * 4 only contend: a move of thousands of generated files ran faster with 4 than with 8.
 */
const COPIERS: usize = if cfg!(windows) { 8 } else { 4 };

/// How many copied files can wait for the thread that keeps them ([`Move::copy_files`]):
/// enough that the copies seldom wait for it.
const COPIED_WAITING: usize = 64;

/**
 * What copying a file costs besides its bytes, counted in bytes: opening it, flushing it and
 * moving it into place take about as long as copying this much does. A move's progress
 * ([`MoveProgress::fraction`]) weighs its files and their bytes together so: by bytes alone,
 * the catalogue's backups, a few large files copied first, would fill most of the bar in the
 * first moments of a copy of thousands of small segments, and by files alone a few large
 * transcripts would hold it still.
 */
const FILE_COST: u64 = 256 * 1024;

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
    /// It is on a network file system, which SQLite's write-ahead log isn't safe on.
    #[error("it is on a network drive")]
    OnNetwork,
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
    /// Windows kept refusing the file past the retries: another program holds it (an
    /// antivirus scan, say), or its permissions don't let scrollback in.
    #[error(
        "{} can't be read or written: permissions, or another program holds it",
        .0.display()
    )]
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
    /// Whether it isn't there yet: the move makes it.
    pub made: bool,
    /// The service that syncs the folder to the cloud, if one does (OneDrive, say): the
    /// archive changes often, and its copies there can conflict (PLAN §4.4).
    pub synced_by: Option<&'static str>,
    /// The move's own id, which the app's settings and the new folder's marker both keep.
    pub id: String,
    /// The files copied, besides the catalogue, and how big they are.
    pub files: usize,
    pub bytes: u64,
    /// What the move needs on the folder's volume: those files, the catalogue and the search
    /// index as big as they are here, and a twentieth more for what syncs add meanwhile.
    pub needed: u64,
    /// The free space of the folder's volume, when it could be told, and the volume as the
    /// owner knows it ([`volume_of`]).
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

impl MoveProgress {
    /// How much of its copy is done, from 0 to 1: its files and their bytes weighed together,
    /// each file as [`FILE_COST`] bytes besides its own.
    pub fn fraction(&self) -> f64 {
        let weight = |files: usize, bytes: u64| files as u64 * FILE_COST + bytes;
        let all = weight(self.files, self.bytes);
        if all == 0 {
            return 0.;
        }
        weight(self.files_done, self.bytes_done) as f64 / all as f64
    }
}

/// What became of a partial copy ([`Move::abandon`], [`clear_move`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cleared {
    /// Everything the move made there is gone.
    Removed,
    /// These, relative to the folder, could not be removed: another program holds them.
    Partly(Vec<PathBuf>),
    /// Nothing had been copied yet: what the move had made (its lock, its `tmp/`, the folder
    /// itself if it made that) is gone, or there was nothing.
    Nothing,
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
    /// The settings name the new folder; its marker is still there.
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
        /* The catalogue's copy, and the search index the new archive makes again: about as
        big as they are here. */
        let databases = [catalogue::DATABASE, SEARCH_DATABASE]
            .iter()
            .flat_map(|database| JOURNALS.map(|journal| name_of(database, journal)))
            .filter_map(|name| fs::metadata(self.dir.join(name)).ok())
            .map(|metadata| metadata.len())
            .sum::<u64>();
        let needed = (bytes + databases).saturating_add((bytes + databases) / 20);
        let there = nearest(&target);
        let free = disk::free_space(&there)
            .inspect_err(|error| {
                log::warn!("{}'s free space couldn't be read: {error}", there.display());
            })
            .ok();
        let volume = volume_of(&there);
        room(&volume, free, needed)?;
        Ok(MovePlan {
            made: fs::symlink_metadata(&target).is_err(),
            synced_by: synced_by(&target),
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
            .and_then(|()| disk::flush_dir(&self.target))
            .map_err(io_error("write", &path))
    }

    /**
     * Copies every file of `archive` but the catalogue and `archive.json` into the new folder,
     * while the archive is in use, several at once ([`Move::copy_files`]), telling `progress`
     * after each, until `cancel` stops it ([`MoveError::Cancelled`]). Each copy is checked
     * against what was read, and a segment's against the catalogue too. A file that goes before
     * it is read (a delete for good) is passed over.
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
        let (placed, copied) = self.copy_files(
            &archive.dir,
            &files.iter().collect::<Vec<_>>(),
            &recorded,
            cancel,
            &mut |file| {
                done.files_done += 1;
                done.bytes_done += file.len;
                progress(&done);
                self.step(Step::Copying)
            },
        );
        for (relative, placed) in placed {
            self.keep(relative, placed);
        }
        copied?;
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
        /* Nothing is removed, and no segment is between its place and its commit. The
        removal is held first: it takes `placing` for each item, so it is waited for before
        `placing` is taken, never while it is held. */
        let removals = archive.hold_removals();
        let quiet = archive
            .placing
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let finished = self.finishing(archive, cancel, progress, commit);
        drop(quiet);
        let moved = finished?;
        removals.for_good();
        moved.start_removals(true);
        /* Once the settings name the folder, its marker says nothing more. A start finds
        any left by a crash before this and takes it away ([`forget_marker`]). */
        forget_marker(&self.target);
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
        let copy = Backups::new(disk::for_sqlite(&tmp, disk::SQLITE_FOLDER), "scrollback")
            .before(&disk::for_sqlite(&database, disk::SQLITE_FILE), "move")?
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
        let (placed, copied) = self.copy_files(
            &archive.dir,
            &changed,
            &BTreeMap::new(),
            cancel,
            &mut |file| {
                done.files_done += 1;
                done.bytes_done += file.len;
                progress(&done);
                Ok(())
            },
        );
        for (relative, placed) in placed {
            self.keep(relative, placed);
        }
        copied?;
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
        let Some(copied) =
            self.copy_file(&archive.dir, &listed, None, &|| cancel.is_cancelled())?
        else {
            return Err(io_error("read", &manifest)(io::ErrorKind::NotFound.into()));
        };
        self.keep(listed.relative, copied);
        self.settle()?;
        self.step(Step::Placed)?;

        let lock = self
            .lock
            .take()
            .expect("a move is finished once: its lock passes to the archive opened");
        let moved = Archive::open_locked(&self.target, lock)?;
        self.check_all(&archive.dir, &moved, cancel)?;
        self.step(Step::Checked)?;

        /* The marker stays until the settings name the folder: a start after a crash before
        then removes the copy it marks, so no whole copy is left that nothing uses. */
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
     * Copies `files` of the archive's folder `from` into the new folder ([`Move::copy_file`]),
     * a segment that `recorded` holds checked against it on the way, on [`COPIERS`] threads at
     * once, and calls `each` on this thread as each is done, in the order they are done. The
     * first failure (`each`'s among them) or `cancel` stops every thread at its next file, or
     * at the next read of the file it copies; it is returned once all have stopped, so nothing
     * is being written in the new folder when this returns and what the move made there can be
     * removed. Returns what was put in place, which the move keeps ([`Move::keep`]) however it
     * went, with how it went.
     *
     * Each file is flushed before it is moved into place, as one copied alone is. Flushing a
     * batch of files after they are all in place would be as safe from a crash: nothing in
     * the new folder is used until the settings name it, which comes after every folder's
     * entries are flushed ([`Move::settle`]), and a crash before then has the next start
     * remove the copy whole ([`clear_move`]). But it would flush as many files, and that is
     * where the time goes on Windows, where each `FlushFileBuffers` waits on the drive; copying
     * several files at once overlaps those waits instead.
     */
    fn copy_files(
        &self,
        from: &Path,
        files: &[&Listed],
        recorded: &BTreeMap<PathBuf, (u64, Hash)>,
        cancel: &CancelToken,
        each: &mut dyn FnMut(&Listed) -> Result<(), MoveError>,
    ) -> (Vec<(PathBuf, Copied)>, Result<(), MoveError>) {
        let (next, halted) = (AtomicUsize::new(0), AtomicBool::new(false));
        let stopped = || cancel.is_cancelled() || halted.load(Ordering::Relaxed);
        /* Bounded, so that `each` holding this thread (a test's pause) soon holds them all. */
        let (sender, receiver) = mpsc::sync_channel(COPIED_WAITING);
        let mut placed = Vec::new();
        let copied = thread::scope(|scope| {
            for _ in 0..COPIERS.min(files.len()) {
                let (sender, next, stopped) = (sender.clone(), &next, &stopped);
                scope.spawn(move || {
                    while !stopped() {
                        let Some(&file) = files.get(next.fetch_add(1, Ordering::Relaxed)) else {
                            break;
                        };
                        let copied = self.copy_file(
                            from,
                            file,
                            recorded.get(&file.relative).copied(),
                            stopped,
                        );
                        if sender.send((file, copied)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(sender);
            let copied = receiver.iter().try_for_each(|(file, copied)| {
                if cancel.is_cancelled() {
                    return Err(MoveError::Cancelled);
                }
                if let Some(copied) = copied? {
                    placed.push((file.relative.clone(), copied));
                }
                each(file)
            });
            halted.store(true, Ordering::Relaxed);
            drop(receiver);
            copied
        });
        let copied = copied.and_then(|()| {
            if cancel.is_cancelled() {
                Err(MoveError::Cancelled)
            } else {
                Ok(())
            }
        });
        (placed, copied)
    }

    /**
     * Copies the archive's file `listed`, in its folder `from`, into the new folder: read and
     * hashed into a file in the new folder's `tmp/`, flushed, read back and hashed again, then
     * moved into its place, over a copy made of it before. A segment the catalogue records as
     * `recorded` is checked against it as it is read back. Returns the copy, or `None` when the
     * file wasn't there to copy. An entry at the top that isn't among those the marker names is
     * left where it is: the move would not know to remove it again. Stops when `stopped` says
     * so, at the next read ([`MoveError::Cancelled`]).
     */
    fn copy_file(
        &self,
        from: &Path,
        listed: &Listed,
        recorded: Option<(u64, Hash)>,
        stopped: &dyn Fn() -> bool,
    ) -> Result<Option<Copied>, MoveError> {
        let top = listed.relative.iter().next().unwrap_or_default();
        if !self.marker.names.iter().any(|name| OsStr::new(name) == top) && top != layout::MANIFEST
        {
            log::warn!(
                "{} appeared in the archive's folder during the move, and stays there",
                from.join(top).display()
            );
            return Ok(None);
        }
        let source = from.join(&listed.relative);
        let mut reader = match disk::retrying(|| File::open(&source)) {
            Ok(reader) => reader,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("read", &source)(error)),
        };
        let tmp = self.target.join(layout::TMP);
        let mut staged = NamedTempFile::new_in(&tmp).map_err(io_error("write", &tmp))?;
        let (mut hasher, mut buffer, mut len) = (Hasher::new(), vec![0; BUFFER], 0u64);
        loop {
            if stopped() {
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
        let (back, checked) =
            read_back(staged.as_file_mut(), recorded).map_err(io_error("read", staged.path()))?;
        let to = self.target.join(&listed.relative);
        if back != (len, hasher.finish()) {
            return Err(MoveError::Mismatch(to));
        }
        let staged = staged.into_temp_path();
        if let Some(folder) = to.parent() {
            disk::retrying(|| fs::create_dir_all(folder)).map_err(io_error("create", folder))?;
        }
        /* A copy there already is the move's own, of the file as it was. */
        if self.copied.contains_key(&listed.relative) {
            remove_file(&to)?;
        }
        disk::place_retrying(staged, &to).map_err(|(error, _)| io_error("store", &to)(error))?;
        Ok(Some(Copied {
            len: listed.len,
            modified: listed.modified,
            checked,
        }))
    }

    /// Keeps `copied` as the copy of the file `relative`, and the folders it is in to be
    /// flushed ([`Move::settle`]).
    fn keep(&mut self, relative: PathBuf, copied: Copied) {
        if let Some(folder) = relative.parent() {
            self.folders.extend(
                folder
                    .ancestors()
                    .take_while(|folder| !folder.as_os_str().is_empty())
                    .map(|folder| self.target.join(folder)),
            );
        }
        self.copied.insert(relative, copied);
    }

    /// Makes what was copied durable: the entries of every folder made, and the new folder's.
    fn settle(&self) -> Result<(), MoveError> {
        self.folders
            .iter()
            .chain([&self.target])
            .try_for_each(|folder| disk::push_dir(folder))
            .and_then(|()| disk::flush_dir(&self.target))
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
 * (otherwise [`Cleared::Left`]). A move cut short before its marker was written left at most
 * its lock and an empty `tmp/`, and the folder itself when it `made` it: those go too
 * ([`Cleared::Nothing`]). The archive the app is on is never this folder: the app's settings
 * named the folder only once the move was whole.
 */
pub fn clear_move(target: &Path, id: &str, made: bool) -> Cleared {
    if fs::symlink_metadata(target).is_err() {
        return Cleared::Nothing;
    }
    let marker = fs::read(target.join(MARKER))
        .ok()
        .and_then(|json| serde_json::from_slice::<Marker>(&json).ok())
        .filter(|marker| marker.v == MARKER_VERSION && marker.id == id);
    let lock = match lock(target) {
        Ok(lock) => lock,
        Err(error) => {
            log::warn!("{} is left as it is: {error}", target.display());
            return Cleared::Left;
        }
    };
    match marker {
        Some(marker) => clear(target, &marker, Some(lock)),
        None if only_a_start(target) => {
            let mut left = Vec::new();
            remove_all(target, Path::new(layout::TMP), &mut left);
            drop(lock);
            remove_all(target, Path::new(layout::LOCK), &mut left);
            if made
                && left.is_empty()
                && let Err(error) = fs::remove_dir(target)
            {
                log::warn!("{} is left: {error}", target.display());
            }
            if left.is_empty() {
                Cleared::Nothing
            } else {
                Cleared::Partly(left)
            }
        }
        None => Cleared::Left,
    }
}

/// Whether `target` holds nothing but what a move makes before its marker: its lock and an
/// empty `tmp/`, beside the files an operating system leaves in any folder.
fn only_a_start(target: &Path) -> bool {
    fs::read_dir(target).is_ok_and(|entries| {
        entries.flatten().all(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == layout::LOCK
                || is_system_file(&name)
                || name == layout::TMP
                    && fs::read_dir(entry.path()).is_ok_and(|mut tmp| tmp.next().is_none())
        })
    })
}

/**
 * Takes away the marker a move left in the folder `dir` the app's settings name, the archive
 * in use: the move was whole when they did, and the marker says nothing more. A start calls
 * this, for a move whose last steps a crash cut short.
 */
pub fn forget_marker(dir: &Path) {
    let marker = dir.join(MARKER);
    match disk::retrying(|| fs::remove_file(&marker)) {
        Ok(()) => {
            if let Err(error) = disk::flush_dir(dir) {
                log::warn!("{error}");
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => log::warn!("{} is left: {error}", marker.display()),
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
 * holding one; it holds nothing but system files, or isn't there yet; it isn't on a network
 * drive; and it, or the nearest folder it is in, can be written. Nothing is left behind by
 * looking.
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
    if on_network(&target) {
        return Err(Refusal::OnNetwork);
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

/// The volume `path` is on, as the owner knows it: its drive (`D:\`), or the share's root.
#[cfg(windows)]
pub fn volume_of(path: &Path) -> PathBuf {
    use std::path::Component;

    match path.components().next() {
        Some(Component::Prefix(prefix)) => {
            PathBuf::from(format!(r"{}\", prefix.as_os_str().to_string_lossy()))
        }
        _ => path.to_path_buf(),
    }
}

/// The volume `path` is on, as the owner knows it: where it is mounted (`/`, or
/// `/Volumes/Backup`), the highest folder of `path` on the same device as the nearest of them
/// there is.
#[cfg(unix)]
pub fn volume_of(path: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt as _;

    let device = |folder: &Path| fs::metadata(folder).ok().map(|it| it.dev());
    let Some((there, on)) = path
        .ancestors()
        .find_map(|folder| Some((folder, device(folder)?)))
    else {
        return path.to_path_buf();
    };
    there
        .ancestors()
        .take_while(|folder| device(folder) == Some(on))
        .last()
        .unwrap_or(there)
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
 * which the finish copies last. So are the files an operating system leaves in any folder
 * (`.DS_Store`, `Thumbs.db`, `desktop.ini`): each folder keeps its own, the new folder may
 * have them already, and they are never copied, named in the marker or removed.
 */
fn left_out(name: &str) -> bool {
    is_system_file(name)
        || [layout::LOCK, layout::TMP, layout::MANIFEST, MARKER].contains(&name)
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

/**
 * Reads `file` back from its start: how long it is and the hash of its bytes, and when it is a
 * segment the catalogue records as `recorded`, that hash if it holds those bytes. It is read
 * through the handle it was written with, and once: on Windows each open of a file just
 * written has an antivirus scan it again.
 */
fn read_back(
    file: &mut File,
    recorded: Option<(u64, Hash)>,
) -> io::Result<((u64, Hash), Option<Hash>)> {
    file.seek(SeekFrom::Start(0))?;
    let mut back = Hashing {
        inner: file,
        hasher: Hasher::new(),
        len: 0,
    };
    let checked = match recorded {
        Some((length, hash)) => segment::holds_in(&mut back, length, hash)?.then_some(hash),
        None => None,
    };
    /* What the check didn't read, or all of it. */
    let mut buffer = vec![0; BUFFER];
    loop {
        match back.read(&mut buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(((back.len, back.hasher.finish()), checked))
}

/// A reader that hashes what is read through it, and counts it.
struct Hashing<R> {
    inner: R,
    hasher: Hasher,
    len: u64,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        self.len += read as u64;
        Ok(read)
    }
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
        disk::for_sqlite(path, disk::SQLITE_FILE),
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
