//! How files get to disk and stay there (PLAN §4.1): written in `tmp/`, moved into place with a
//! move that never replaces a file, and durable before the catalogue records them.
//!
//! Making a file durable has two costs that differ by platform, so it is two steps here.
//! [`push`] hands a file's (or a folder's) data to the drive, and [`settle`] has the drive put
//! everything it was handed on stable storage. On macOS only the second is slow (it is what
//! `F_FULLFSYNC` does), so a batch of files is pushed one by one and settled once. Elsewhere
//! `fsync` (and on Windows `FlushFileBuffers`) does both, in `push`, and `settle` has nothing
//! left to do.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::Path;

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

/// On Windows a move into a folder is written through already ([`place`]); where that is
/// not enough, [`flush_dir`] flushes the folder too.
#[cfg(not(unix))]
pub(super) fn push_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// [`push_dir`], then [`settle`]: the folder's entries durable now.
#[cfg(unix)]
pub(super) fn flush_dir(dir: &Path) -> io::Result<()> {
    push_dir(dir).and_then(|()| settle(dir))
}

/**
 * Flushes the folder `dir`'s entries, through a handle opened to write it with
 * `FILE_FLAG_BACKUP_SEMANTICS`, which is how Windows opens a folder: what was moved into it or
 * out of it is then on disk. Some volumes (FAT drives, shares) and some permissions refuse a
 * folder's flush; the moves themselves were written through, so that is said in the log only.
 */
#[cfg(windows)]
pub(super) fn flush_dir(dir: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;

    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    let flushed = File::options()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .and_then(|folder| folder.sync_all());
    if let Err(error) = flushed {
        log::info!("{} couldn't be flushed: {error}", dir.display());
    }
    Ok(())
}

/// Makes everything pushed so far durable, by a full flush through the folder `dir` on the
/// same volume. A volume that has no full flush has had all it can do in [`push`]; a full
/// flush that fails for any other reason is a failure.
#[cfg(target_vendor = "apple")]
pub(super) fn settle(dir: &Path) -> io::Result<()> {
    use rustix::io::Errno;

    let dir = File::open(dir)?;
    dir.sync_all().or_else(
        |error| match error.raw_os_error().map(Errno::from_raw_os_error) {
            Some(Errno::NOTSUP | Errno::OPNOTSUPP | Errno::NOTTY | Errno::INVAL) => push(&dir),
            _ => Err(error),
        },
    )
}

/// What was pushed is durable already.
#[cfg(not(target_vendor = "apple"))]
pub(super) fn settle(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// A file in `tmp` holding `bytes`, whole and pushed. Dropped, it is removed.
pub(super) fn new_file(tmp: &Path, bytes: &[u8]) -> io::Result<TempPath> {
    let mut file = NamedTempFile::new_in(tmp)?;
    file.write_all(bytes)?;
    push(file.as_file())?;
    Ok(file.into_temp_path())
}

/// Writes `bytes` to the new file `to` through a file in `tmp`, so `to` is either absent or
/// whole, and pushes it. A `to` that already exists is left as it is.
pub(super) fn write_new(tmp: &Path, to: &Path, bytes: &[u8]) -> io::Result<()> {
    match place(new_file(tmp, bytes)?, to) {
        Err((error, _)) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

/// Whether what is at `there` is a regular file holding the bytes of the file at `new`. A
/// link, a folder, or a file that can't be read is not.
pub(super) fn same_file(new: &Path, there: &Path) -> io::Result<bool> {
    let bytes = fs::read(new)?;
    Ok(fs::symlink_metadata(there)
        .is_ok_and(|found| found.is_file() && found.len() == bytes.len() as u64)
        && fs::read(there).is_ok_and(|found| found == bytes))
}

/// [`push`] for the file at `path`. It is opened to write, which is what Windows flushes.
pub(super) fn push_at(path: &Path) -> io::Result<()> {
    push(&File::options().write(true).open(path)?)
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
pub(super) fn place(file: TempPath, to: &Path) -> Result<(), (io::Error, TempPath)> {
    move_through(file, to, 0)
}

/// Moves the written, pushed `file` over `to`, handing it back with the error when it isn't
/// moved.
#[cfg(not(windows))]
fn replace(file: TempPath, to: &Path) -> Result<(), (io::Error, TempPath)> {
    file.persist(to).map_err(|error| (error.error, error.path))
}

/// Moves the written, flushed `file` over `to`, written through ([`place`]), handing it back
/// with the error when it isn't moved.
#[cfg(windows)]
fn replace(file: TempPath, to: &Path) -> Result<(), (io::Error, TempPath)> {
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;

    move_through(file, to, MOVEFILE_REPLACE_EXISTING)
}

/**
 * Writes `bytes` to `path` through a file beside it that is moved over it, so a reader finds
 * the old bytes or the new, never part of them, then flushes the new file and its folder, so a
 * power cut after it doesn't bring the old file back. The move is tried again for a moment
 * while another program holds either file ([`retrying`]). The app's settings are written so,
 * since which archive they name decides which folder a start opens (PLAN §4.4).
 *
 * `Ok` once the new file is in place: every reader finds it from then on. A flush that fails
 * after the move can't take the move back, so it is said in the log, not as a failure, and the
 * caller acts on the file as written.
 */
pub fn write_durably(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let (mut file, mut tried) = (new_file(dir, bytes)?, 1);
    loop {
        match replace(file, path) {
            Ok(()) => break,
            Err((error, unplaced)) if is_held(&error) && tried < TRIES => {
                tried += 1;
                file = unplaced;
                std::thread::sleep(APART);
            }
            Err((error, _)) => return Err(error),
        }
    }
    if let Err(error) = push_at(path).and_then(|()| flush_dir(dir)) {
        log::warn!(
            "{} is in place, but couldn't be flushed: {error}",
            path.display()
        );
    }
    Ok(())
}

/// `MoveFileExW` of `file` to `to` with `MOVEFILE_WRITE_THROUGH` and `flags` ([`place`]).
#[cfg(windows)]
fn move_through(mut file: TempPath, to: &Path, flags: u32) -> Result<(), (io::Error, TempPath)> {
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
    let (from_wide, to_wide) = (wide(&verbatim(&file)), wide(&verbatim(to)));
    // SAFETY: both are NUL-terminated UTF-16 strings that outlive the calls.
    unsafe {
        if SetFileAttributesW(from_wide.as_ptr(), FILE_ATTRIBUTE_NORMAL) == 0 {
            return Err((io::Error::last_os_error(), file));
        }
        /* Without `MOVEFILE_REPLACE_EXISTING` a file at `to` fails the move. */
        if MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_WRITE_THROUGH | flags,
        ) == 0
        {
            let error = io::Error::last_os_error();
            SetFileAttributesW(from_wide.as_ptr(), FILE_ATTRIBUTE_TEMPORARY);
            return Err((error, file));
        }
    }
    file.disable_cleanup(true);
    Ok(())
}

/**
 * `path` as Windows' own calls take it past 260 characters: with `\\?\` (or `\\?\UNC\` for a
 * share) when it is that long. std's calls do this themselves; [`place`] calls `MoveFileExW`
 * directly. A verbatim path is taken as it is, so only an absolute one, which the archive's
 * paths are (`std::path::absolute` has already put its separators right and taken out `.`
 * and `..`), is given one.
 */
#[cfg(windows)]
fn verbatim(path: &Path) -> std::borrow::Cow<'_, Path> {
    verbatim_past(path, 0)
}

/// [`verbatim`] once `path` and `margin` more characters would run past 260.
#[cfg(windows)]
fn verbatim_past(path: &Path, margin: usize) -> std::borrow::Cow<'_, Path> {
    use std::borrow::Cow;
    use std::path::{Component, PathBuf, Prefix};

    const LONGEST: usize = 259;
    if path.as_os_str().len() + margin <= LONGEST {
        return Cow::Borrowed(path);
    }
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return Cow::Borrowed(path);
    };
    let text = path.as_os_str().to_string_lossy();
    match prefix.kind() {
        Prefix::Disk(_) => Cow::Owned(PathBuf::from(format!(r"\\?\{text}"))),
        Prefix::UNC(..) => Cow::Owned(PathBuf::from(format!(
            r"\\?\UNC\{}",
            text.trim_start_matches(r"\\")
        ))),
        _ => Cow::Borrowed(path),
    }
}

/// Room for the files SQLite names beside a database: `-journal`, `-wal`, `-shm`.
pub(crate) const SQLITE_FILE: usize = 16;
/// Room for the files a backup or a move names in a folder of its own: a prefix, a UTC time,
/// a label and SQLite's own beside each.
pub(crate) const SQLITE_FOLDER: usize = 96;

/**
 * `path` as SQLite takes it, a database or a folder it makes one in (`margin` is
 * [`SQLITE_FILE`] or [`SQLITE_FOLDER`]): on Windows, with `\\?\` once what SQLite names there
 * could run past 260 characters. std's calls add the prefix themselves, but SQLite's own file
 * calls don't, and fail ("unable to open database") on a longer path, so an archive in a deep
 * folder couldn't be opened, moved to, or backed up. Elsewhere the path is as it is.
 */
pub(crate) fn for_sqlite(path: &Path, margin: usize) -> std::borrow::Cow<'_, Path> {
    #[cfg(windows)]
    return verbatim_past(path, margin);
    #[cfg(not(windows))]
    {
        let _ = margin;
        std::borrow::Cow::Borrowed(path)
    }
}

/**
 * `path` as the owner writes it, without the `\\?\` that Windows' folder picker gives a long
 * path and that resolving a path gives any: `\\?\D:\…` is `D:\…`, and `\\?\UNC\server\share`
 * is `\\server\share`. A path comes in so (the folder picker, Locate, the settings file) and
 * is kept, shown and compared so; [`verbatim`] puts the prefix back where a call of Windows'
 * own needs it, and std's calls do that themselves. Elsewhere `\\?\` means nothing, and a path
 * is as it is.
 */
pub fn plain(path: &Path) -> std::borrow::Cow<'_, Path> {
    #[cfg(windows)]
    if let Some(plain) = path.to_str().and_then(plain_text) {
        return std::borrow::Cow::Owned(plain.into());
    }
    std::borrow::Cow::Borrowed(path)
}

/**
 * [`plain`] of the Windows path `text`, when it has a `\\?\` to take away: only from a path on
 * a drive or a share, and only when taking it away doesn't change which file it names. A
 * verbatim path is taken as it is, so `/`, `.` and `..`, and a name that ends in a dot or a
 * space, mean something else without the prefix; such a path is kept verbatim.
 */
#[cfg_attr(not(any(windows, test)), expect(dead_code))]
fn plain_text(text: &str) -> Option<String> {
    let rest = text.strip_prefix(r"\\?\")?;
    let plain = match rest.get(..4) {
        Some(unc) if unc.eq_ignore_ascii_case(r"UNC\") => format!(r"\\{}", &rest[4..]),
        _ => {
            let drive = rest.as_bytes();
            let on_a_drive = drive.len() >= 2
                && drive[0].is_ascii_alphabetic()
                && drive[1] == b':'
                && drive.get(2).is_none_or(|&separator| separator == b'\\');
            if !on_a_drive {
                return None;
            }
            rest.to_string()
        }
    };
    let names_the_same = !plain.contains('/')
        && plain.split('\\').skip(1).all(|name| {
            name != "." && name != ".." && !name.ends_with('.') && !name.ends_with(' ')
        });
    names_the_same.then_some(plain)
}

/**
 * Whether `error` is Windows saying another program has the file open in a way that keeps it
 * from being opened, replaced or removed: an antivirus scanning a file just written, an
 * indexer, a backup tool. It lets go after a moment, so what failed so is tried again
 * ([`retrying`]). Nothing else is ever retried.
 */
#[cfg(windows)]
pub(super) fn is_held(error: &io::Error) -> bool {
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION,
    };
    matches!(
        error
            .raw_os_error()
            .and_then(|code| u32::try_from(code).ok()),
        Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
    )
}

/**
 * Whether the folder `dir` is on a network file system: SMB, NFS, WebDAV or AFP, by the name
 * `statfs` gives its file system.
 */
#[cfg(target_vendor = "apple")]
pub(super) fn is_remote(dir: &Path) -> io::Result<bool> {
    let stats = rustix::fs::statfs(dir)?;
    let name: Vec<u8> = stats
        .f_fstypename
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte.cast_unsigned())
        .collect();
    Ok(matches!(
        name.as_slice(),
        b"smbfs" | b"nfs" | b"webdav" | b"afpfs" | b"cifs"
    ))
}

/// Whether the folder `dir` is on a network drive: a share (`\\server\share`), or a drive
/// letter mapped to one, as `GetDriveTypeW` says.
#[cfg(windows)]
pub(super) fn is_remote(dir: &Path) -> io::Result<bool> {
    use std::path::{Component, Prefix};

    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;

    let Some(Component::Prefix(prefix)) = dir.components().next() else {
        return Ok(false);
    };
    match prefix.kind() {
        Prefix::UNC(..) | Prefix::VerbatimUNC(..) => Ok(true),
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            let root = [u16::from(letter), u16::from(b':'), u16::from(b'\\'), 0];
            // SAFETY: `root` is a NUL-terminated UTF-16 string that outlives the call.
            Ok(unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE)
        }
        _ => Ok(false),
    }
}

/// Elsewhere network file systems aren't told apart: scrollback ships for Windows and macOS.
#[cfg(not(any(target_vendor = "apple", windows)))]
pub(super) fn is_remote(_dir: &Path) -> io::Result<bool> {
    Ok(false)
}

/// Elsewhere a program holding a file open keeps no other from it.
#[cfg(not(windows))]
pub(super) fn is_held(_error: &io::Error) -> bool {
    false
}

/// How many times, and how far apart, what another program holds is tried ([`retrying`]).
const TRIES: u32 = 20;
const APART: std::time::Duration = std::time::Duration::from_millis(100);

/// `op`, tried again for a moment while it fails because another program holds the file
/// ([`is_held`]); then its last error.
pub(super) fn retrying<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut tried = 1;
    loop {
        match op() {
            Err(error) if is_held(&error) && tried < TRIES => {
                tried += 1;
                std::thread::sleep(APART);
            }
            done => return done,
        }
    }
}

/// [`place`], tried again for a moment while another program holds a file it needs
/// ([`retrying`]).
pub(super) fn place_retrying(file: TempPath, to: &Path) -> Result<(), (io::Error, TempPath)> {
    let (mut file, mut tried) = (file, 1);
    loop {
        match place(file, to) {
            Err((error, unplaced)) if is_held(&error) && tried < TRIES => {
                tried += 1;
                file = unplaced;
                std::thread::sleep(APART);
            }
            done => return done,
        }
    }
}

/// How many bytes the volume `dir`, an existing folder, has free for this user.
#[cfg(unix)]
pub(super) fn free_space(dir: &Path) -> io::Result<u64> {
    let stats = rustix::fs::statvfs(dir)?;
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

/// How many bytes the volume `dir`, an existing folder, has free for this user (a quota
/// counts), from `GetDiskFreeSpaceExW`, which follows a folder that is another volume's
/// mount point.
#[cfg(windows)]
pub(super) fn free_space(dir: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    /* A share's root is taken only with a separator after it; any folder is with one. */
    let mut wide: Vec<u16> = verbatim(dir).as_os_str().encode_wide().collect();
    if wide.last() != Some(&u16::from(b'\\')) {
        wide.push(u16::from(b'\\'));
    }
    wide.push(0);
    let mut free = 0u64;
    // SAFETY: `wide` is a NUL-terminated UTF-16 string, and `free` outlives the call; the
    // totals it doesn't need are null, which the call allows.
    let done = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(free)
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

    #[test]
    fn a_path_comes_in_without_its_verbatim_prefix() {
        let cases = [
            (r"\\?\D:\scrollback", Some(r"D:\scrollback")),
            (r"\\?\d:\", Some(r"d:\")),
            (r"\\?\D:", Some(r"D:")),
            (
                r"\\?\UNC\server\share\archive",
                Some(r"\\server\share\archive"),
            ),
            (r"\\?\unc\server\share", Some(r"\\server\share")),
            /* Without the prefix these name another file, or none. */
            (r"\\?\D:\a\..\b", None),
            (r"\\?\D:\a\.\b", None),
            (r"\\?\D:\a/b", None),
            (r"\\?\D:\trailing dot.", None),
            (r"\\?\D:\trailing space ", None),
            /* Not a drive or a share. */
            (r"\\?\Volume{0b5c6d1e}\archive", None),
            (r"\\?\GLOBALROOT\Device\HarddiskVolume1", None),
            /* No prefix at all. */
            (r"D:\scrollback", None),
            (r"\\server\share", None),
            ("/Users/user/scrollback", None),
        ];
        for (text, plain) in cases {
            assert_eq!(plain_text(text).as_deref(), plain, "{text}");
        }
        /* A long one, as the picker gives it past 260 characters. */
        let long = format!(r"D:\{}\archive", ["folder"; 60].join(r"\"));
        assert_eq!(plain_text(&format!(r"\\?\{long}")), Some(long));
    }

    #[cfg(windows)]
    #[test]
    fn a_plain_path_gets_its_prefix_back_only_where_windows_needs_it() {
        let long = format!(r"D:\{}\archive", ["folder"; 60].join(r"\"));
        let prefixed = verbatim(Path::new(&long)).into_owned();
        assert_eq!(prefixed, Path::new(&format!(r"\\?\{long}")));
        assert_eq!(plain(&prefixed), Path::new(&long));
        assert_eq!(verbatim(Path::new(r"D:\short")), Path::new(r"D:\short"));
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
