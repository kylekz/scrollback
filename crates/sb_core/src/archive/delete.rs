//! Deleting a session for good (PLAN §4.5): what the archive does once the catalogue has
//! committed the delete ([`crate::catalogue::Catalogue::delete_for_good`]): its text leaves
//! the search index, and in the background the files its items had leave `files/`, with what of
//! them was set aside in `orphans/` before (those are the session's bytes all the same).
//!
//! What is removed is decided by the catalogue. A deleted item's folder is named by its key,
//! which the tombstone says the owner deleted: while the catalogue records no item in that
//! folder, everything in it goes, whatever its bytes (a damaged segment of the session is still
//! the session's). Once the catalogue records an item there again (the session was archived
//! anew, or a catalogue from before was restored that holds the removal still to do), only a
//! file it does not record, and that holds the very bytes deleted, goes. Nothing is removed
//! through a link: such an item stays to be removed, and verify reports it. An item is removed
//! with no segment between its place and its commit ([`Archive::store`] holds `placing`), so a
//! store into the same folder is never removed with it.
//!
//! **In the background.** Removing a large session's files takes seconds, so neither a delete
//! nor a start waits for it: a thread of the archive's removes what is pending, and goes on
//! with what a delete adds meanwhile. What is still to be removed stays pending until it is
//! gone, and every start tries again; until then verify counts it as what it is, not as a
//! stray, and nothing sets it aside. The day's backup of the catalogue is taken by the thread a
//! start begins, once nothing is pending, so no backup holds a removal still to do.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::{self, JoinHandle};

use toolbox_store::rusqlite;

use super::disk::{Lack, Room};
use super::{Archive, ArchiveError, Result, accounted, disk, layout, open_search, segment};
use crate::catalogue::{
    AllowedAgain, Catalogue, Deletion, ItemFile, Removal, Scrub, SessionId, Stored,
};
use crate::location::LocationId;
use crate::search::{SearchError, SearchIndex};

/// What deleting a session for good did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deleted {
    /// What was deleted: what the confirmation showed.
    pub deletion: Deletion,
    /// How many of the archive's items' folders are still to be removed, by the archive's
    /// background removal: the session's, and any another delete left.
    /// [`Archive::pending_removals`] says how many are left later.
    pub pending: usize,
}

/// What is still to be erased of what was deleted for good ([`Archive::unscrubbed`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Unscrubbed {
    /// The room a disk lacks for it, when that is what it waits for.
    #[serde(default)]
    pub lacks: Option<Lack>,
    /// The catalogue is too large to be written anew here (PLAN §4.5): that part waits for
    /// the next start, and is never done while it is.
    #[serde(default)]
    pub catalogue_too_large: bool,
}

/// The largest catalogue [`vacuum_catalogue`] writes anew: one written in about a second, so
/// that no write waits for it long enough to give up (five seconds). A catalogue holds a few
/// megabytes per thousand sessions.
const VACUUM_MOST: u64 = 128 * 1024 * 1024;

/// How often, and how far apart, [`vacuum_catalogue`] tries for the database while another
/// connection writes: ten seconds in all.
const VACUUM_TRIES: u32 = 100;
const VACUUM_APART: std::time::Duration = std::time::Duration::from_millis(100);

/// What became of [`vacuum_catalogue`].
enum Vacuumed {
    Done,
    /// The catalogue is larger than [`VACUUM_MOST`]: left as it is.
    TooLarge,
}

/// The room [`vacuum_catalogue`] takes, for the catalogue in `dir`: the file once more, in
/// its log; its copy is made in memory.
fn catalogue_room(dir: &Path) -> Room {
    let size = fs::metadata(dir.join(crate::catalogue::DATABASE)).map_or(0, |it| it.len());
    Room {
        archive: size,
        temp: 0,
    }
}

/**
 * Writes the catalogue's file anew (`VACUUM`), on a connection of its own, so no page of it
 * keeps a copy of a cell a delete's rebalance left there, which `secure_delete` doesn't
 * overwrite (PLAN §4.5). Its copy is made in memory (`temp_store = MEMORY`), as the catalogue
 * is small, and its pages go through its log, on the archive's disk, which is to have the room.
 *
 * It never waits on a write: while one holds the database it tries again a moment later, and
 * gives up after a while. And a write never waits on it long: it is done only for a catalogue
 * that is written anew in about a second ([`VACUUM_MOST`]). `stop` ends it between tries, and
 * interrupts the `VACUUM` under way, with nothing changed.
 */
fn vacuum_catalogue(dir: &Path, stop: &(dyn Fn() -> bool + Sync)) -> Result<Vacuumed> {
    let path = dir.join(crate::catalogue::DATABASE);
    if fs::metadata(&path).map_or(0, |it| it.len()) > VACUUM_MOST {
        return Ok(Vacuumed::TooLarge);
    }
    if let Some(lack) = disk::lacking(dir, catalogue_room(dir)) {
        return Err(SearchError::Unavailable(format!(
            "writing the catalogue anew needs about {} MB free on the archive's disk",
            lack.needs >> 20
        ))
        .into());
    }
    let connection = rusqlite::Connection::open(disk::for_sqlite(&path, disk::SQLITE_FILE))
        .map_err(SearchError::from)?;
    let ready = connection
        .busy_timeout(std::time::Duration::ZERO)
        .and_then(|()| connection.pragma_update(None, "temp_store", "MEMORY"));
    ready.map_err(SearchError::from)?;
    for _ in 0..VACUUM_TRIES {
        if stop() {
            return Err(SearchError::Cancelled.into());
        }
        match watched(&connection, stop, || connection.execute_batch("VACUUM")) {
            Err(error)
                if matches!(
                    error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ) =>
            {
                thread::sleep(VACUUM_APART);
            }
            Err(error)
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) =>
            {
                return Err(SearchError::Cancelled.into());
            }
            vacuumed => {
                vacuumed.map_err(SearchError::from)?;
                return Ok(Vacuumed::Done);
            }
        }
    }
    Err(SearchError::Unavailable(
        "the catalogue was written to all along, and is written anew later".to_string(),
    )
    .into())
}

/// Runs `statement` on `connection` while a thread asks `stop` every few tens of milliseconds
/// and interrupts it when it says to. With no thread to be had, it just runs.
fn watched(
    connection: &rusqlite::Connection,
    stop: &(dyn Fn() -> bool + Sync),
    statement: impl FnOnce() -> rusqlite::Result<()>,
) -> rusqlite::Result<()> {
    let (handle, ended) = (connection.get_interrupt_handle(), AtomicBool::new(false));
    thread::scope(|scope| {
        let watching = thread::Builder::new()
            .name("catalogue-vacuum".to_string())
            .spawn_scoped(scope, || {
                while !ended.load(Ordering::Relaxed) {
                    if stop() {
                        handle.interrupt();
                    }
                    thread::sleep(std::time::Duration::from_millis(25));
                }
            });
        if let Err(error) = watching {
            log::warn!("the catalogue's VACUUM can't be stopped once it begins: {error}");
        }
        let ran = statement();
        ended.store(true, Ordering::Relaxed);
        ran
    })
}

/**
 * Erases what is left in the archive's files of the sessions deleted for good since this last
 * did (PLAN §4.5), if any were. A delete for good marks the catalogue in its own transaction
 * ([`Catalogue::to_scrub`]), once for each file, and each mark is taken off only once its file
 * is erased: a crash or a failure leaves it for the next try, and the other file is not done
 * again for it.
 *
 * - The catalogue's part: its file written anew ([`vacuum_catalogue`]) and its log emptied. A
 *   catalogue too large for it is left until the next start: `too_large` says so meanwhile.
 * - The search index's: the index made again without the entries it marked deleted, and its
 *   file written anew ([`SearchIndex::compact`]), when its disks have the room.
 *
 * `stop` ends either at once.
 */
fn scrub(
    dir: &Path,
    search: &Mutex<Option<Arc<SearchIndex>>>,
    catalogue: &Catalogue,
    too_large: &AtomicBool,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<()> {
    if stop() {
        return Ok(());
    }
    let mut failed = None;
    if let Some(mark) = catalogue.to_scrub(Scrub::Catalogue)?
        && !too_large.load(Ordering::Relaxed)
    {
        match vacuum_catalogue(dir, stop) {
            Ok(Vacuumed::Done) if catalogue.empty_log() => {
                catalogue.scrubbed(Scrub::Catalogue, &mark)?;
            }
            Ok(Vacuumed::Done) => {
                failed = Some(SearchError::Unavailable(
                    "a read held the catalogue's log".to_string(),
                ));
            }
            Ok(Vacuumed::TooLarge) => {
                log::warn!(
                    "the catalogue is too large to write anew without holding writes up: \
                     deleted text may stay in its pages until it is"
                );
                too_large.store(true, Ordering::Relaxed);
            }
            Err(error) => failed = Some(SearchError::Unavailable(error.to_string())),
        }
    }
    if let Some(mark) = catalogue.to_scrub(Scrub::Search)? {
        if !catalogue.text_gone()?.is_empty() {
            return Err(
                SearchError::Unavailable("deleted text is still in the index".into()).into(),
            );
        }
        let search = open_search(dir, search, catalogue)?;
        search.compact(true, stop)?;
        catalogue.scrubbed(Scrub::Search, &mark)?;
    }
    failed.map_or(Ok(()), |error| Err(error.into()))
}

/// The archive's background removal of deleted items' files.
#[derive(Debug, Default)]
pub(super) struct Removals {
    job: Mutex<Job>,
    /// The archive is closing: the removal stops before its next item.
    stop: AtomicBool,
    /// The catalogue was found too large to write anew since the archive was opened: that
    /// part of erasing what was deleted for good waits for the next start ([`scrub`]).
    pub(super) catalogue_too_large: AtomicBool,
}

#[derive(Debug, Default)]
struct Job {
    running: bool,
    /// More was deleted while it ran: it goes round again before it ends.
    again: bool,
    handle: Option<JoinHandle<()>>,
}

impl Removals {
    /// Stops the removal before its next item, and waits for it.
    pub(super) fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wait();
    }

    /// Waits for the removal under way, if one is.
    fn wait(&self) {
        let handle = self
            .job
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .handle
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

/// What the background removal works with: the archive's, shared.
struct Remover {
    dir: PathBuf,
    catalogue: Catalogue,
    placing: Arc<RwLock<()>>,
    search: Arc<Mutex<Option<Arc<SearchIndex>>>>,
    removals: Arc<Removals>,
}

impl Archive {
    /**
     * Deletes a session in Trash for good (PLAN §4.5), with the copies of it that go with it
     * (PLAN §7.7): `shown` is what the confirmation showed the owner, and only that is deleted
     * ([`crate::catalogue::CatalogueError::DeletionChanged`] otherwise). Their rows go and a
     * tombstone takes the place of each, in one transaction; their text leaves the search
     * index and both databases' logs are emptied; and the files the catalogue had for their
     * items, and any of them set aside in `orphans/`, are removed in the background (the module
     * docs). None of their locations is being synced when it is committed. Nothing at a
     * location is touched: the archive has no way to.
     *
     * Once the delete is committed this returns `Ok`, whatever is left to do: what is still to
     * be removed is counted in [`Deleted::pending`], and what went wrong is said in the log.
     */
    pub fn delete_for_good(&self, shown: &Deletion) -> Result<Deleted> {
        let ids: Vec<SessionId> = shown.sessions().cloned().collect();
        {
            /* Its copies are kept at other locations (PLAN §7.7): none of them is syncing. */
            let _syncing = (ids.iter().map(|id| &id.location))
                .collect::<HashSet<&LocationId>>()
                .into_iter()
                .map(|location| {
                    self.begin_sync(location)
                        .ok_or_else(|| ArchiveError::Syncing(location.clone()))
                })
                .collect::<Result<Vec<_>>>()?;
            self.catalogue.delete_for_good(shown)?;
        }
        let forgotten = self.search_index().and_then(|search| search.forget(&ids));
        match forgotten {
            Ok(()) => {
                if let Err(error) = self.catalogue.text_forgotten(&ids) {
                    log::warn!("{error}");
                }
            }
            Err(error) => log::warn!("the text of a deleted session is left for later: {error}"),
        }
        let _ = self.catalogue.empty_log();
        let pending = self.catalogue.pending_removals().unwrap_or_else(|error| {
            log::warn!("{error}");
            shown.files
        });
        self.start_removals(false);
        Ok(Deleted {
            deletion: shown.clone(),
            pending,
        })
    }

    /**
     * Puts back the tombstones the owner removed ([`Catalogue::restore_tombstone`]), unless the
     * location of any of them is being synced ([`ArchiveError::Syncing`]): the sync may be
     * archiving the session again as it is put back.
     */
    pub fn restore_tombstone(&self, allowed: &AllowedAgain) -> Result<()> {
        let _syncing = allowed
            .locations()
            .into_iter()
            .map(|location| {
                self.begin_sync(location)
                    .ok_or_else(|| ArchiveError::Syncing(location.clone()))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(self.catalogue.restore_tombstone(allowed)?)
    }

    /**
     * Erases what is left in the archive's files of what was deleted for good, if anything is
     * ([`scrub`]), until `stop` says to stop: what the background removal does, and what a
     * move does before it copies.
     */
    pub(crate) fn scrub_deleted(&self, stop: &(dyn Fn() -> bool + Sync)) -> Result<()> {
        let too_large = &self.removals.catalogue_too_large;
        scrub(&self.dir, &self.search, &self.catalogue, too_large, stop)
    }

    /**
     * Whether there is erasing of what was deleted for good ([`scrub`]) to try: of the search
     * index, or of a catalogue not found too large since the archive was opened.
     */
    pub(crate) fn scrub_wanted(&self) -> Result<bool> {
        let catalogue = self.catalogue.to_scrub(Scrub::Catalogue)?.is_some()
            && !self.removals.catalogue_too_large.load(Ordering::Relaxed);
        Ok(catalogue || self.catalogue.to_scrub(Scrub::Search)?.is_some())
    }

    /**
     * What is still to be erased of what was deleted for good (PLAN §4.5), when the background
     * removal has tried and is not trying now: `None` when nothing is, or it is under way.
     */
    pub fn unscrubbed(&self) -> Result<Option<Unscrubbed>> {
        let search = self.catalogue.to_scrub(Scrub::Search)?.is_some();
        let catalogue = self.catalogue.to_scrub(Scrub::Catalogue)?.is_some();
        if !search && !catalogue {
            return Ok(None);
        }
        let job = self
            .removals
            .job
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if job.running {
            return Ok(None);
        }
        drop(job);
        let too_large = catalogue
            && (self.removals.catalogue_too_large.load(Ordering::Relaxed)
                || fs::metadata(self.dir.join(crate::catalogue::DATABASE))
                    .is_ok_and(|it| it.len() > VACUUM_MOST));
        let mut room = Room::default();
        if search && let Some(index) = self.search_index_if_open() {
            room = index.compaction_room(true).unwrap_or_default();
        }
        if catalogue && !too_large {
            room.archive = room
                .archive
                .saturating_add(catalogue_room(&self.dir).archive);
        }
        Ok(Some(Unscrubbed {
            lacks: disk::lacking(&self.dir, room),
            catalogue_too_large: too_large,
        }))
    }

    /// How many deleted items' folders are still to be removed (the module docs).
    pub fn pending_removals(&self) -> Result<usize> {
        Ok(self.catalogue.pending_removals()?)
    }

    /// Waits for the background removal (the module docs) to end, if it is under way.
    pub fn wait_for_removals(&self) {
        self.removals.wait();
    }

    /// Stops the background removal before its next item and waits for it; nothing is removed
    /// until the guard is dropped, which starts it again (a move's finish, `relocate`).
    pub(super) fn hold_removals(&self) -> RemovalsHeld<'_> {
        self.removals.stop();
        RemovalsHeld { archive: self }
    }

    /**
     * Starts the background removal, or has the one under way go round again. With `backup`,
     * as a start does, it then takes the day's backup of the catalogue, once nothing is left to
     * remove.
     */
    pub(crate) fn start_removals(&self, backup: bool) {
        let mut job = self
            .removals
            .job
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if job.running {
            job.again = true;
            return;
        }
        if let Some(ended) = job.handle.take() {
            let _ = ended.join();
        }
        let remover = Remover {
            dir: self.dir.clone(),
            catalogue: self.catalogue.clone(),
            placing: Arc::clone(&self.placing),
            search: Arc::clone(&self.search),
            removals: Arc::clone(&self.removals),
        };
        job.running = true;
        job.handle = Some(thread::spawn(move || remover.run(backup)));
    }

    /// The folders of deleted items still to be removed, relative to the archive.
    pub(super) fn removing(&self) -> Result<HashSet<PathBuf>> {
        Ok(self
            .catalogue
            .removals()?
            .into_iter()
            .map(|removal| layout::item_dir(&removal.location, &removal.provider, &removal.folder))
            .collect())
    }

    /// The paths under `files/` that `stored` accounts for, and the folders of deleted items
    /// still to be removed with the folders they are in: none of them is a stray.
    pub(super) fn accounted(&self, stored: &[Stored]) -> Result<HashSet<PathBuf>> {
        let mut accounted = accounted(stored);
        for item in self.removing()? {
            accounted.extend(
                item.ancestors()
                    .take_while(|folder| !folder.as_os_str().is_empty())
                    .map(Path::to_path_buf),
            );
        }
        Ok(accounted)
    }
}

/// The background removal held ([`Archive::hold_removals`]).
pub(super) struct RemovalsHeld<'a> {
    archive: &'a Archive,
}

impl RemovalsHeld<'_> {
    /// Keeps the removal stopped: the archive has been left for good, and nothing more is to
    /// be removed from it.
    pub(super) fn for_good(self) {
        std::mem::forget(self);
    }
}

impl Drop for RemovalsHeld<'_> {
    /// Lets the removal go on with what is pending, as a delete would.
    fn drop(&mut self) {
        self.archive.removals.stop.store(false, Ordering::Relaxed);
        self.archive.start_removals(false);
    }
}

impl Remover {
    /// Removes what is pending, and what is added while it does; takes the text of sessions
    /// whose rows went out of the search index; and with `backup`, then the day's backup.
    fn run(self, mut backup: bool) {
        loop {
            if let Err(error) = self.remove_deleted() {
                log::warn!("files of deleted sessions are left for later: {error}");
            }
            if let Err(error) = self.forget_gone_text() {
                log::warn!("the text of deleted sessions is left for later: {error}");
            }
            let stop = || self.removals.stop.load(Ordering::Relaxed);
            let too_large = &self.removals.catalogue_too_large;
            if let Err(error) = scrub(&self.dir, &self.search, &self.catalogue, too_large, &stop) {
                log::warn!("deleted text is erased from the archive's files later: {error}");
            }
            if backup && !self.removals.stop.load(Ordering::Relaxed) {
                backup = false;
                match self.catalogue.pending_removals() {
                    Ok(0) => {
                        /* What a write before took out is in no file, nor in the backup. */
                        let _ = self.catalogue.empty_log();
                        self.catalogue.daily_backup();
                    }
                    Ok(_) => log::warn!(
                        "files of deleted sessions are still to be removed: today's backup \
                         waits for the next start"
                    ),
                    Err(error) => log::warn!("{error}"),
                }
            }
            let mut job = self
                .removals
                .job
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !job.again || self.removals.stop.load(Ordering::Relaxed) {
                job.running = false;
                return;
            }
            job.again = false;
        }
    }

    /// Takes the text of the sessions whose rows went (and have not come back) out of the
    /// search index, when there are any.
    fn forget_gone_text(&self) -> Result<()> {
        let gone = self.catalogue.text_gone()?;
        if gone.is_empty() {
            return Ok(());
        }
        let search = open_search(&self.dir, &self.search, &self.catalogue)?;
        search.forget(&gone)?;
        Ok(self.catalogue.text_forgotten(&gone)?)
    }

    /// Removes the files of deleted items that are pending, one item at a time (the module
    /// docs).
    fn remove_deleted(&self) -> Result<()> {
        for removal in self.catalogue.removals()? {
            if self.removals.stop.load(Ordering::Relaxed) {
                break;
            }
            /* No segment is between its place and its commit meanwhile. */
            let _quiet = self.placing.write().unwrap_or_else(PoisonError::into_inner);
            let done = if self.catalogue.has_item_in(
                &removal.location,
                &removal.provider,
                &removal.folder,
            )? {
                self.remove_unrecorded(&removal)?
            } else {
                self.remove_all(&removal)
            };
            self.catalogue.removed(&done)?;
        }
        Ok(())
    }

    /**
     * Removes the folder of a deleted item that the catalogue records no item in, and what was
     * set aside of it, and then the folders above it that that leaves empty. Returns the rows
     * of `removal` that are done: all of them, or none.
     */
    fn remove_all(&self, removal: &Removal) -> Vec<i64> {
        let relative = layout::item_dir(&removal.location, &removal.provider, &removal.folder);
        let item = self.dir.join(&relative);
        let top = self.dir.join(layout::FILES);
        /* What a link leads to is not the archive's: nothing is removed through one. */
        let linked = item
            .ancestors()
            .take_while(|folder| *folder != top)
            .any(is_link)
            || fs::read_dir(&item)
                .is_ok_and(|entries| entries.flatten().any(|entry| is_link(&entry.path())));
        if linked {
            log::warn!(
                "{} is reached through a link: it is not removed",
                item.display()
            );
            return Vec::new();
        }
        let removed = match fs::remove_dir_all(&item) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
        .and_then(|()| remove_set_aside(&self.dir.join(layout::ORPHANS), &relative));
        if let Err(error) = removed {
            log::warn!(
                "could not remove {}: {error}; it is tried again",
                item.display()
            );
            return Vec::new();
        }
        /* One that is not empty holds what is not this item's. */
        for folder in item.ancestors().skip(1).take_while(|folder| *folder != top) {
            let _ = fs::remove_dir(folder);
        }
        removal.files.iter().map(|(row, _)| *row).collect()
    }

    /**
     * Removes, from the folder of a deleted item that the catalogue records an item in again,
     * each segment that the catalogue does not record and that holds the bytes that were
     * deleted. Returns the rows of `removal` that are done.
     */
    fn remove_unrecorded(&self, removal: &Removal) -> Result<Vec<i64>> {
        let item = self.dir.join(layout::item_dir(
            &removal.location,
            &removal.provider,
            &removal.folder,
        ));
        let mut done = Vec::new();
        for (row, file) in &removal.files {
            let ItemFile::Segment {
                version,
                offset,
                length,
                hash,
            } = *file
            else {
                done.push(*row);
                continue;
            };
            let recorded = self.catalogue.has_segment(
                &removal.location,
                &removal.provider,
                &removal.folder,
                version,
                offset,
            )?;
            let folder = item.join(layout::version_dir(version));
            let path = folder.join(layout::segment_file(offset));
            if recorded || is_link(&folder) || !segment::holds(&path, length, hash).unwrap_or(false)
            {
                done.push(*row);
                continue;
            }
            match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    log::warn!("could not remove {}: {error}", path.display());
                }
                _ => done.push(*row),
            }
        }
        Ok(done)
    }
}

/// Whether `path` is a link, which nothing here follows.
fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|found| found.file_type().is_symlink())
}

/**
 * Removes what was set aside, under `orphans`, from the item folder at `relative` (to the
 * archive): a copy keeps the path it had, except that any part of it may have been numbered
 * (`name.1`, …) where that name was taken, so each part is matched with its numbered forms.
 */
fn remove_set_aside(orphans: &Path, relative: &Path) -> io::Result<()> {
    let parts: Vec<String> = relative
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect();
    let mut found = BTreeSet::new();
    let mut level = vec![orphans.to_path_buf()];
    for (depth, part) in parts.iter().enumerate() {
        let mut next = Vec::new();
        for dir in &level {
            let entries = match fs::read_dir(dir) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) if error.kind() == io::ErrorKind::NotADirectory => continue,
                entries => entries?,
            };
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let numbered = name
                    .strip_prefix(part.as_str())
                    .and_then(|rest| rest.strip_prefix('.'))
                    .is_some_and(|number| {
                        !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                    });
                if name == *part || numbered {
                    if depth + 1 == parts.len() {
                        found.insert(entry.path());
                    } else if entry.file_type()?.is_dir() {
                        next.push(entry.path());
                    }
                }
            }
        }
        level = next;
    }
    for path in found {
        if fs::symlink_metadata(&path)?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}
