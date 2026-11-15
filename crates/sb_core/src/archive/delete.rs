//! Deleting a session for good (PLAN §4.5): what the archive does once the catalogue has
//! committed the delete ([`crate::catalogue::Catalogue::delete_for_good`]): the files its items
//! had leave `files/`, what of them was set aside in `orphans/` before goes too (those are the
//! session's bytes all the same), and its text leaves the search index.
//!
//! What is removed is decided by the catalogue. A deleted item's folder is named by its key,
//! which the tombstone says the owner deleted: while the catalogue records no item in that
//! folder, everything in it goes, whatever its bytes (a damaged segment of the session is still
//! the session's). Once the catalogue records an item there again (the session was archived
//! anew, or a catalogue from before was restored that holds the removal still to do), only a
//! file it does not record, and that holds the very bytes deleted, goes. Nothing is removed
//! through a link: such an item stays to be removed, and verify reports it. What can't be
//! removed now stays pending, and every start tries again; until then verify counts it as what
//! it is, not as a stray, and nothing sets it aside.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::PoisonError;

use super::{Archive, ArchiveError, Result, accounted, layout, segment};
use crate::catalogue::{Deletion, ItemFile, Removal, Stored};

/// What deleting a session for good did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deleted {
    /// What was deleted: what the confirmation showed.
    pub deletion: Deletion,
    /// How many of the archive's files of it are still to be removed, as removing them failed
    /// (another program holds them) or would go through a link. Every start tries again.
    pub pending: usize,
}

impl Archive {
    /**
     * Deletes a session in Trash for good (PLAN §4.5): `shown` is what the confirmation showed
     * the owner, and only that is deleted ([`crate::catalogue::CatalogueError::DeletionChanged`]
     * otherwise). Its rows go and a tombstone takes their place, in one transaction; then the
     * files the catalogue had for its items, and any of them set aside in `orphans/`, are
     * removed, its text leaves the search index, and both databases' logs are emptied. Its
     * location is not synced, and nothing under `files/` is stored or set aside, while its
     * files are removed. Nothing at a location is touched: the archive has no way to.
     *
     * Once the delete is committed this returns `Ok`, whatever is left to do: what could not
     * be removed is counted in [`Deleted::pending`], said in the log, and done at a start.
     */
    pub fn delete_for_good(&self, shown: &Deletion) -> Result<Deleted> {
        let id = &shown.session;
        {
            let _syncing = self
                .begin_sync(&id.location)
                .ok_or_else(|| ArchiveError::Syncing(id.location.clone()))?;
            let _quiet = self.placing.write().unwrap_or_else(PoisonError::into_inner);
            self.catalogue.delete_for_good(shown)?;
            if let Err(error) = self.remove_deleted() {
                log::warn!("files of a deleted session are left for the next start: {error}");
            }
        }
        /* With no lock held: emptying a log may wait a moment on a read. */
        match self.search_index() {
            Ok(search) => {
                if let Err(error) = search.forget(slice::from_ref(id)) {
                    log::warn!("the text of a deleted session is left for the next start: {error}");
                }
            }
            Err(error) => {
                log::warn!("the text of a deleted session is left for the next start: {error}");
            }
        }
        self.catalogue.empty_log();
        let pending = self.catalogue.pending_removals().unwrap_or_else(|error| {
            log::warn!("{error}");
            shown.files
        });
        Ok(Deleted {
            deletion: shown.clone(),
            pending,
        })
    }

    /// Removes the files of deleted items that are pending (the module docs).
    pub(super) fn remove_deleted(&self) -> Result<()> {
        for removal in self.catalogue.removals()? {
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
                "could not remove {}: {error}; a start tries again",
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
