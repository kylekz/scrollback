//! Deleting a session for good (PLAN §4.5): what the archive does once the catalogue has
//! committed the delete ([`crate::catalogue::Catalogue::delete_for_good`]): the session's text
//! leaves the search index, the files its items had leave `files/`, and what of them was set
//! aside in `orphans/` before goes too, since those are the session's bytes all the same.
//!
//! A file is removed by its name in its item's folder, and only while it holds what the
//! catalogue recorded it held when the session was deleted. So a file that is another's by now
//! (the session was archived again into the same folder, and then a catalogue from before that
//! was restored) stays. An item's folder that the catalogue records an item in again is not
//! touched at all. Nothing is removed through a link. What can't be removed now stays pending,
//! and every start tries again; until then verify counts it as what it is, not as a stray, and
//! nothing sets it aside.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::PoisonError;

use super::{Archive, ArchiveError, Result, accounted, layout, segment};
use crate::catalogue::{self, Deletion, ItemFile, Removal, Stored};

/// What deleting a session for good did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deleted {
    /// What was deleted: what the confirmation showed.
    pub deletion: Deletion,
    /// How many of the archive's files of it are still to be removed, as removing them failed
    /// (another program holds them). Every start tries again.
    pub pending: usize,
}

impl Archive {
    /**
     * Deletes a session in Trash for good (PLAN §4.5): `shown` is what the confirmation showed
     * the owner, and only that is deleted ([`crate::catalogue::CatalogueError::DeletionChanged`]
     * otherwise). Its rows go and a tombstone takes their place, in one transaction; then its
     * text leaves the search index, and the files the catalogue had for its items, and any of
     * them set aside in `orphans/`, are removed. Its location is not synced meanwhile, and no
     * file under `files/` is set aside, so nothing is stored in, or taken from, the folders
     * this removes from. Nothing at a location is touched: the archive has no way to.
     *
     * Once the delete is committed this returns `Ok`, whatever is left to do: what could not
     * be removed is counted in [`Deleted::pending`], said in the log, and done at a start.
     */
    pub fn delete_for_good(&self, shown: &Deletion) -> Result<Deleted> {
        let id = &shown.session;
        let _syncing = self
            .begin_sync(&id.location)
            .ok_or_else(|| ArchiveError::Syncing(id.location.clone()))?;
        let _quiet = self.placing.write().unwrap_or_else(PoisonError::into_inner);
        self.catalogue.delete_for_good(shown)?;
        if let Err(error) = self.search.forget(slice::from_ref(id)) {
            log::warn!("the search text of a deleted session is left for the next start: {error}");
        }
        crate::search::checkpoint(&self.dir.join(catalogue::DATABASE));
        if let Err(error) = self.remove_deleted() {
            log::warn!("files of a deleted session are left for the next start: {error}");
        }
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
            let rows: Vec<i64> = removal.files.iter().map(|(row, _)| *row).collect();
            if self
                .catalogue
                .has_item_in(&removal.location, &removal.provider, &removal.folder)?
            {
                self.catalogue.removed(&rows)?;
                continue;
            }
            let done = self.remove(&removal);
            self.catalogue.removed(&done)?;
        }
        Ok(())
    }

    /// Removes what it can of one deleted item's files. Returns the rows of `removal` that
    /// are done.
    fn remove(&self, removal: &Removal) -> Vec<i64> {
        let relative = layout::item_dir(&removal.location, &removal.provider, &removal.folder);
        let item = self.dir.join(&relative);
        let top = self.dir.join(layout::FILES);
        let all: Vec<i64> = removal.files.iter().map(|(row, _)| *row).collect();
        /* What a link leads to is not the archive's: none of it is touched. */
        let linked = item
            .ancestors()
            .take_while(|folder| *folder != top)
            .any(is_link);
        if linked {
            log::warn!(
                "{} is reached through a link; nothing in it is removed",
                item.display()
            );
            return all;
        }
        let failed = |path: &Path, error: io::Error| {
            log::warn!(
                "could not remove {}: {error}; a start tries again",
                path.display()
            );
        };
        let mut done = Vec::new();
        let mut source = None;
        /* Whether everything recorded of the item is gone from its folder: only then is the
        folder the deleted item's alone, with its source.json and what was set aside of it. */
        let mut gone = true;
        for (row, file) in &removal.files {
            let ItemFile::Segment {
                version,
                offset,
                length,
                hash,
            } = *file
            else {
                source = Some(*row);
                continue;
            };
            let folder = item.join(layout::version_dir(version));
            let path = folder.join(layout::segment_file(offset));
            if is_link(&folder) {
                done.push(*row);
                gone = false;
                continue;
            }
            match segment::holds(&path, length, hash) {
                Ok(true) => match fs::remove_file(&path) {
                    Ok(()) => done.push(*row),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => done.push(*row),
                    Err(error) => {
                        failed(&path, error);
                        gone = false;
                    }
                },
                /* Another's bytes now: not this deletion's to remove. */
                Ok(false) => {
                    done.push(*row);
                    gone = false;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => done.push(*row),
                Err(error) => {
                    failed(&path, error);
                    gone = false;
                }
            }
        }
        if let Some(source) = source {
            if !gone {
                /* It stays with what stays; it is done when the rest is. */
                if done.len() == removal.files.len() - 1 {
                    done.push(source);
                }
            } else {
                let orphans = self.dir.join(layout::ORPHANS).join(&relative);
                let path = item.join(layout::SOURCE);
                let removed =
                    remove_set_aside(&orphans).and_then(|()| match fs::remove_file(&path) {
                        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                        _ => Ok(()),
                    });
                match removed {
                    Ok(()) => done.push(source),
                    Err(error) => failed(&path, error),
                }
            }
        }
        /* The version folders, then the item's and those above it, while empty. */
        let mut folders: BTreeSet<PathBuf> = removal
            .files
            .iter()
            .filter_map(|(_, file)| match *file {
                ItemFile::Segment { version, .. } => Some(item.join(layout::version_dir(version))),
                ItemFile::Source => None,
            })
            .collect();
        folders.extend(
            item.ancestors()
                .take_while(|folder| *folder != top)
                .map(Path::to_path_buf),
        );
        for folder in folders.iter().rev() {
            /* One that is not empty holds what is not this item's. */
            let _ = fs::remove_dir(folder);
        }
        done
    }

    /// The files of deleted items still to be removed, relative to the archive.
    pub(super) fn removing(&self) -> Result<Vec<PathBuf>> {
        Ok(self
            .catalogue
            .removals()?
            .into_iter()
            .flat_map(|removal| {
                let item = layout::item_dir(&removal.location, &removal.provider, &removal.folder);
                removal.files.into_iter().map(move |(_, file)| match file {
                    ItemFile::Source => item.join(layout::SOURCE),
                    ItemFile::Segment {
                        version, offset, ..
                    } => item
                        .join(layout::version_dir(version))
                        .join(layout::segment_file(offset)),
                })
            })
            .collect())
    }

    /// The paths under `files/` that `stored` accounts for, and those still to be removed
    /// with the folders they are in: none of them is a stray.
    pub(super) fn accounted(&self, stored: &[Stored]) -> Result<HashSet<PathBuf>> {
        let mut accounted = accounted(stored);
        for path in self.removing()? {
            accounted.extend(
                path.ancestors()
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
 * Removes what was set aside from a deleted item's folder: under `orphans/`, at the item's
 * folder's place, and at the places numbered after it that a later set-aside took because that
 * one was taken (`<folder>.1`, …).
 */
fn remove_set_aside(orphans: &Path) -> io::Result<()> {
    let (Some(parent), Some(name)) = (orphans.parent(), orphans.file_name()) else {
        return Ok(());
    };
    let entries = match fs::read_dir(parent) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        entries => entries?,
    };
    let name = name.to_string_lossy();
    for entry in entries {
        let entry = entry?;
        let found = entry.file_name().to_string_lossy().into_owned();
        let numbered = found
            .strip_prefix(&*name)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()));
        if found == name || numbered {
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}
