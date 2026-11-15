//! How files get to disk and stay there (PLAN §4.1): written in `tmp/`, moved into place with a
//! move that never replaces a file, and durable before the catalogue records them.
//!
//! Making a file durable has two costs that differ by platform, so it is two steps here.
//! [`push`] hands a file's (or a folder's) data to the drive, and [`settle`] has the drive put
//! everything it was handed on stable storage. On macOS only the second is slow (it is what
//! `F_FULLFSYNC` does), so a batch of files is pushed one by one and settled once. Elsewhere
//! `fsync` (and on Windows `FlushFileBuffers`) does both, in `push`, and `settle` has nothing
//! left to do.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use tempfile::{NamedTempFile, TempPath};

/// Hands `file`'s data to the drive. It is durable once [`settle`] has run.
#[cfg(target_vendor = "apple")]
pub(super) fn push(file: &File) -> io::Result<()> {
    Ok(rustix::fs::fsync(file)?)
}

/// Makes `file`'s data durable.
#[cfg(not(target_vendor = "apple"))]
pub(super) fn push(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// [`push`] for a folder's entries, which is what makes a file moved into it or created in it
/// stay there.
#[cfg(unix)]
pub(super) fn push_dir(dir: &Path) -> io::Result<()> {
    push(&File::open(dir)?)
}

/// Windows cannot flush a directory; [`place`] makes its move durable itself.
#[cfg(not(unix))]
pub(super) fn push_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Makes everything pushed so far durable, by a full flush through the folder `dir` on the
/// same volume.
#[cfg(target_vendor = "apple")]
pub(super) fn settle(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// What was pushed is durable already.
#[cfg(not(target_vendor = "apple"))]
pub(super) fn settle(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Writes `bytes` to the new file `to` through a file in `tmp`, so `to` is either absent or
/// whole, and pushes it. A `to` that already exists is left as it is.
pub(super) fn write_new(tmp: &Path, to: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = NamedTempFile::new_in(tmp)?;
    file.write_all(bytes)?;
    push(file.as_file())?;
    match place(file.into_temp_path(), to) {
        Err((error, _)) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

/// Writes `bytes` to `to`, in place of what is there, through a file in `tmp`, and makes it
/// durable: `to` holds either what it held or `bytes`, whole.
pub(super) fn write_over(tmp: &Path, to: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = NamedTempFile::new_in(tmp)?;
    file.write_all(bytes)?;
    push(file.as_file())?;
    file.persist(to).map_err(|error| error.error)?;
    match to.parent() {
        Some(dir) => push_dir(dir).and_then(|()| settle(dir)),
        None => Ok(()),
    }
}

/// Creates `dir` and whichever of its parents are missing, and adds to `changed` each folder
/// that gained one of them.
pub(super) fn create_dirs(dir: &Path, changed: &mut BTreeSet<PathBuf>) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let Some(parent) = dir.parent() else {
        return fs::create_dir(dir);
    };
    create_dirs(parent, changed)?;
    match fs::create_dir(dir) {
        Ok(()) => {
            changed.insert(parent.to_path_buf());
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/**
 * Moves the written, pushed `file` to `to`, where nothing may be: a file already there fails the
 * move with `AlreadyExists` and stays as it is, and `file` is handed back with the error.
 * [`push_dir`] on `to`'s folder then makes the move last.
 *
 * Some volumes (exFAT and FAT drives, some network shares) have neither a rename that refuses
 * to replace nor hard links. There the file is copied into a `to` created new, which the file
 * system refuses when one exists just as well. A copy cut short leaves a partial file under
 * `to`'s name, which no catalogue records.
 */
#[cfg(not(windows))]
pub(super) fn place(file: TempPath, to: &Path) -> Result<(), (io::Error, TempPath)> {
    match file.persist_noclobber(to) {
        Ok(()) => Ok(()),
        Err(blocked) if blocked.error.kind() == io::ErrorKind::AlreadyExists => {
            Err((blocked.error, blocked.path))
        }
        Err(blocked) => copy_new(&blocked.path, to).map_err(|error| (error, blocked.path)),
    }
}

/// Copies the file at `from` to `to`, which must not exist, and pushes it.
#[cfg(not(windows))]
fn copy_new(from: &Path, to: &Path) -> io::Result<()> {
    let mut target = File::options().write(true).create_new(true).open(to)?;
    io::copy(&mut File::open(from)?, &mut target)?;
    push(&target)
}

/**
 * Moves the written, flushed `file` to `to`, where nothing may be: a file already there fails
 * the move and stays as it is, and `file` is handed back with the error.
 *
 * Windows has no flush for a folder ([`push_dir`]), and a plain rename returns before NTFS has
 * written its log record, so a catalogue commit made right after it could reach the disk first
 * and record a segment a power cut then leaves in `tmp/`. `MOVEFILE_WRITE_THROUGH` returns once
 * the move is on disk. This is `tempfile`'s own persist with that flag: it also clears the
 * temporary attribute `tempfile` creates its files with.
 */
#[cfg(windows)]
pub(super) fn place(mut file: TempPath, to: &Path) -> Result<(), (io::Error, TempPath)> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TEMPORARY, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        SetFileAttributesW,
    };

    let wide = |path: &Path| -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    /* The file is closed already; dropping `file` removes it unless the move succeeds. */
    let (from_wide, to_wide) = (wide(&file), wide(to));
    // SAFETY: both are NUL-terminated UTF-16 strings that outlive the calls.
    unsafe {
        if SetFileAttributesW(from_wide.as_ptr(), FILE_ATTRIBUTE_NORMAL) == 0 {
            return Err((io::Error::last_os_error(), file));
        }
        /* Without `MOVEFILE_REPLACE_EXISTING` a file at `to` fails the move. */
        if MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), MOVEFILE_WRITE_THROUGH) == 0 {
            let error = io::Error::last_os_error();
            SetFileAttributesW(from_wide.as_ptr(), FILE_ATTRIBUTE_TEMPORARY);
            return Err((error, file));
        }
    }
    file.disable_cleanup(true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(dir: &Path, bytes: &[u8]) -> TempPath {
        let mut file = NamedTempFile::new_in(dir).unwrap();
        file.write_all(bytes).unwrap();
        file.into_temp_path()
    }

    #[test]
    fn a_file_already_in_place_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("placed");
        place(temp(dir.path(), b"first"), &to).unwrap();
        let (error, unplaced) = place(temp(dir.path(), b"second"), &to).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&to).unwrap(), b"first");
        /* The file that could not be placed is handed back, and goes when it is dropped. */
        assert_eq!(fs::read(&unplaced).unwrap(), b"second");
        drop(unplaced);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        /* What is placed is an ordinary file, not one marked temporary. */
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt as _;
            const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
            let attributes = fs::metadata(&to).unwrap().file_attributes();
            assert_eq!(attributes & FILE_ATTRIBUTE_TEMPORARY, 0);
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn a_copy_into_place_does_not_replace_a_file_either() {
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("placed");
        let (first, second) = (temp(dir.path(), b"first"), temp(dir.path(), b"second"));
        copy_new(&first, &to).unwrap();
        let error = copy_new(&second, &to).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&to).unwrap(), b"first");
    }
}
