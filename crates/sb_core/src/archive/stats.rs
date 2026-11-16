//! How big an archive is and what it holds (DESIGN §4.13: Settings › Archive), and what a
//! folder holds as a place to open an archive in or make one (PLAN §4.4: "Use a different
//! folder").

use std::fs;
use std::io;
use std::path::Path;

use super::{Archive, ArchiveError, Result, io_error, layout};
use crate::catalogue::{self, Totals};
use crate::search::SEARCH_DATABASE;

/// How big the archive is and what it holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArchiveStats {
    /// Everything in the archive's folder.
    pub on_disk: u64,
    /// What the catalogue records.
    pub totals: Totals,
    /// The stored files under `files/`, compressed.
    pub stored: u64,
    /// The catalogue's database and the search index's, with their journals.
    pub databases: u64,
    /// The catalogue's checked backups: how many, and how big together.
    pub backups: usize,
    pub backups_bytes: u64,
}

impl Archive {
    /// Measures the archive (the module docs): it walks the whole folder, so it is for a
    /// background job.
    pub fn stats(&self) -> Result<ArchiveStats> {
        let databases = fs::read_dir(&self.dir)
            .map_err(io_error("list", &self.dir))?
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with(catalogue::DATABASE) || name.starts_with(SEARCH_DATABASE)
            })
            .map(|entry| size(&entry.path()))
            .sum();
        let backups = catalogue::backups(&self.dir);
        Ok(ArchiveStats {
            on_disk: size(&self.dir),
            totals: self.catalogue.totals()?,
            stored: size(&self.dir.join(layout::FILES)),
            databases,
            backups: backups.list().map_or(0, |list| list.len()),
            backups_bytes: size(backups.dir()),
        })
    }
}

/// How many bytes the files at `path` hold, however deep, without following links. What
/// can't be read, or goes while it is counted, counts as nothing.
fn size(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    fs::read_dir(path).map_or(0, |entries| {
        entries.flatten().map(|entry| size(&entry.path())).sum()
    })
}

/// What a folder holds, as the place to open an archive in or to make one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holds {
    /// An archive, by its `archive.json`: it is opened.
    Archive,
    /// Nothing, or the folder is not there yet: an archive is made in it.
    Nothing,
    /// Something else, which a new archive is never made beside.
    Other,
}

/// The files an operating system leaves in any folder it has shown (Finder's, Explorer's), which
/// don't make a folder hold something.
const SYSTEM_FILES: [&str; 3] = [".DS_Store", "Thumbs.db", "desktop.ini"];

/// What `dir` holds (see [`Holds`]).
pub fn what_is_in(dir: &Path) -> Result<Holds> {
    if dir.join(layout::MANIFEST).is_file() {
        return Ok(Holds::Archive);
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Holds::Nothing),
        Err(error) => return Err(io_error("list", dir)(error)),
    };
    for entry in entries {
        let entry = entry.map_err(io_error("list", dir))?;
        if !SYSTEM_FILES.contains(&entry.file_name().to_string_lossy().as_ref()) {
            return Ok(Holds::Other);
        }
    }
    Ok(Holds::Nothing)
}

/**
 * Opens the archive in `dir`, or makes one there when it holds nothing: what pointing the app
 * at another folder does (PLAN §4.4). A folder that holds something else is refused with
 * [`ArchiveError::NotAnArchive`], and nothing is made in it.
 */
pub fn open_or_make(dir: &Path) -> Result<Archive> {
    match what_is_in(dir)? {
        Holds::Archive => Archive::open(dir),
        Holds::Nothing => Archive::create(dir),
        Holds::Other => Err(ArchiveError::NotAnArchive(dir.to_path_buf())),
    }
}
