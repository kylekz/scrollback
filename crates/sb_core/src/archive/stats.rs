//! How big an archive is and what it holds (DESIGN §4.13: Settings › Archive), and what a
//! folder holds as a place to open an archive in or make one (PLAN §4.4: "Use a different
//! folder").

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
    /// Measures the archive (the module docs): it walks the whole folder once, so it is for a
    /// background job.
    pub fn stats(&self) -> Result<ArchiveStats> {
        let (mut on_disk, mut stored, mut databases) = (0, 0, 0);
        for entry in fs::read_dir(&self.dir)
            .map_err(io_error("list", &self.dir))?
            .flatten()
        {
            let bytes = size(&entry);
            on_disk += bytes;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == layout::FILES {
                stored = bytes;
            } else if name.starts_with(catalogue::DATABASE) || name.starts_with(SEARCH_DATABASE) {
                databases += bytes;
            }
        }
        let backups = catalogue::backups(&self.dir);
        Ok(ArchiveStats {
            on_disk,
            totals: self.catalogue.totals()?,
            stored,
            databases,
            backups: backups.list().map_or(0, |list| list.len()),
            backups_bytes: within(backups.dir()),
        })
    }
}

/**
 * How many bytes `entry` holds, however deep, without following links, from what each folder's
 * listing says of its files. No file is opened: on Windows, opening one a sync has just
 * written waits while it is scanned, and a walk of thousands would wait as long as the pass.
 * What can't be read, or goes while it is counted, counts as nothing.
 */
fn size(entry: &fs::DirEntry) -> u64 {
    match entry.file_type() {
        Ok(kind) if kind.is_dir() => within(&entry.path()),
        Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |metadata| metadata.len()),
        _ => 0,
    }
}

/// How many bytes the folder `dir` holds, however deep ([`size`]).
fn within(dir: &Path) -> u64 {
    fs::read_dir(dir).map_or(0, |entries| {
        entries.flatten().map(|entry| size(&entry)).sum()
    })
}

/// What a folder holds, as the place to open an archive in or to make one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Holds {
    /// An archive, by its `archive.json`: it is opened.
    Archive,
    /// Nothing, or the folder is not there yet: an archive is made in it.
    Nothing,
    /// It is inside the archive in this folder, which no archive is made in (PLAN §4.4).
    InsideAnArchive(PathBuf),
    /// Something else, which a new archive is never made beside.
    Other,
}

/// Whether `name` is a file an operating system leaves in any folder it has shown (Finder's,
/// with the AppleDouble `._` files of a drive formatted for Windows, and Explorer's), which
/// doesn't make a folder hold something.
pub(super) fn is_system_file(name: &str) -> bool {
    matches!(name, ".DS_Store" | "Thumbs.db" | "desktop.ini") || name.starts_with("._")
}

/// What `dir` holds (see [`Holds`]).
pub fn what_is_in(dir: &Path) -> Result<Holds> {
    let dir = &std::path::absolute(dir).map_err(io_error("resolve", dir))?;
    if dir.join(layout::MANIFEST).is_file() {
        return Ok(Holds::Archive);
    }
    if let Some(other) = archive_around(dir) {
        return Ok(Holds::InsideAnArchive(other));
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Holds::Nothing),
        Err(error) => return Err(io_error("list", dir)(error)),
    };
    for entry in entries {
        let entry = entry.map_err(io_error("list", dir))?;
        if !is_system_file(&entry.file_name().to_string_lossy()) {
            return Ok(Holds::Other);
        }
    }
    Ok(Holds::Nothing)
}

/**
 * `path` as the file system knows it: absolute, with what exists of it resolved (links, `..`,
 * the case a folder is spelt in), and the rest as given. Two spellings of one folder resolve
 * alike.
 */
pub fn resolved(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    absolute
        .ancestors()
        .find_map(|there| {
            let found = plain(fs::canonicalize(there).ok()?);
            Some(match absolute.strip_prefix(there) {
                Ok(rest) if !rest.as_os_str().is_empty() => found.join(rest),
                _ => found,
            })
        })
        .unwrap_or(absolute)
}

/// `path` without the `\\?\` that resolving a path with a drive letter gives it on Windows.
fn plain(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    if let Some(rest) = path
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        .filter(|rest| rest.as_bytes().get(1) == Some(&b':'))
    {
        return PathBuf::from(rest);
    }
    path
}

/**
 * Whether the folder `dir`, or the nearest one there is of those it is in, is on a network
 * file system (SMB, NFS, WebDAV, AFP): an archive is never put there (PLAN §4.4), as SQLite's
 * write-ahead log needs memory shared on one machine. One whose file system can't be told is
 * taken as local, and said in the log.
 */
pub fn on_network(dir: &Path) -> bool {
    let Some(there) = dir.ancestors().find(|folder| folder.is_dir()) else {
        return false;
    };
    super::disk::is_remote(there).unwrap_or_else(|error| {
        log::warn!(
            "{}'s file system couldn't be told: {error}",
            there.display()
        );
        false
    })
}

/**
 * The service that keeps `path` in step with the cloud, when one does, from the names its
 * folders are given (OneDrive's, Dropbox's, iCloud Drive's and Google Drive's own and their
 * defaults): an archive there is allowed, with a warning, as it changes often and its copies
 * there can conflict (PLAN §4.4).
 */
pub fn synced_by(path: &Path) -> Option<&'static str> {
    path.iter().find_map(|name| {
        let name = name.to_string_lossy().to_lowercase();
        if name == "onedrive" || name.starts_with("onedrive -") || name.starts_with("onedrive-") {
            Some("OneDrive")
        } else if name == "dropbox" || name.starts_with("dropbox (") || name.starts_with("dropbox-")
        {
            Some("Dropbox")
        } else if matches!(
            name.as_str(),
            "icloud drive" | "iclouddrive" | "mobile documents"
        ) {
            Some("iCloud Drive")
        } else if matches!(name.as_str(), "google drive" | "my drive")
            || name.starts_with("googledrive-")
        {
            Some("Google Drive")
        } else {
            None
        }
    })
}

/// The archive `dir`, absolute, is inside, if it is inside one: the nearest folder above it
/// that holds an `archive.json`, a link into an archive seen through.
pub(super) fn archive_around(dir: &Path) -> Option<PathBuf> {
    resolved(dir)
        .ancestors()
        .skip(1)
        .find(|folder| folder.join(layout::MANIFEST).is_file())
        .map(Path::to_path_buf)
}

/// An archive somewhere below `dir`, if one is: a folder there that holds an `archive.json`.
pub(super) fn archive_within(dir: &Path) -> Option<PathBuf> {
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(&folder).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                if path.join(layout::MANIFEST).is_file() {
                    return Some(path);
                }
                folders.push(path);
            }
        }
    }
    None
}

/// The id the archive in `dir` was made with, from its `archive.json`; `None` when the folder
/// holds no archive (DESIGN §4.14: locating an archive accepts only the one that was open).
pub fn id_in(dir: &Path) -> Result<Option<String>> {
    let path = dir.join(layout::MANIFEST);
    let json = match fs::read(&path) {
        Ok(json) => json,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("read", &path)(error)),
    };
    let manifest: super::Manifest =
        serde_json::from_slice(&json).map_err(|source| ArchiveError::Manifest { path, source })?;
    Ok(Some(manifest.id))
}

/**
 * Opens the archive in `dir`, or makes one there when it holds nothing: what pointing the app
 * at another folder does (PLAN §4.4). A folder that holds something else is refused with
 * [`ArchiveError::NotAnArchive`], one inside another archive with [`ArchiveError::Nested`],
 * and nothing is made in either.
 */
pub fn open_or_make(dir: &Path) -> Result<Archive> {
    if on_network(dir) {
        return Err(ArchiveError::OnNetwork(dir.to_path_buf()));
    }
    match what_is_in(dir)? {
        Holds::Archive => Archive::open(dir),
        Holds::Nothing => Archive::create(dir),
        Holds::InsideAnArchive(other) => Err(ArchiveError::Nested {
            dir: dir.to_path_buf(),
            other,
        }),
        Holds::Other => Err(ArchiveError::NotAnArchive(dir.to_path_buf())),
    }
}
