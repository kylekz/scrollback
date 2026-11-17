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

use super::{Archive, ArchiveError, Result, accounted, layout, open_search, segment};
use crate::catalogue::{AllowedAgain, Catalogue, Deletion, ItemFile, Removal, SessionId, Stored};
use crate::location::LocationId;
use crate::search::SearchIndex;

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

/// The archive's background removal of deleted items' files.
#[derive(Debug, Default)]
pub(super) struct Removals {
    job: Mutex<Job>,
    /// The archive is closing: the removal stops before its next item.
    stop: AtomicBool,
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
        self.catalogue.empty_log();
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
            match self.forget_gone_text() {
                Ok(()) => {
                    if let Err(error) = self.scrub() {
                        log::warn!("the files are scrubbed of deleted sessions later: {error}");
                    }
                }
                Err(error) => log::warn!("the text of deleted sessions is left for later: {error}"),
            }
            if backup && !self.removals.stop.load(Ordering::Relaxed) {
                backup = false;
                match self.catalogue.pending_removals() {
                    Ok(0) => {
                        /* What a write before took out is in no file, nor in the backup. */
                        self.catalogue.empty_log();
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

    /**
     * Scrubs the search index and the catalogue of what is left in their files of the sessions
     * deleted for good since they last were (PLAN §4.5), once their text is out of the index:
     * a delete for good leaves the mark ([`Catalogue::to_scrub`]) in the transaction that
     * deletes, so a crash before this leaves it for the next start.
     */
    fn scrub(&self) -> Result<()> {
        if self.removals.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some(mark) = self.catalogue.to_scrub()? else {
            return Ok(());
        };
        open_search(&self.dir, &self.search, &self.catalogue)?.scrub()?;
        self.catalogue.scrub()?;
        Ok(self.catalogue.scrubbed(&mark)?)
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
