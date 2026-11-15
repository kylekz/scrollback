//! Segments (PLAN §4.1): each is the zstd-compressed bytes of one range of a version, written
//! once and never changed. A version's content is its segments decompressed in order.
//!
//! A segment is written in `tmp/`, flushed to disk and only then renamed into place, so a file
//! under `files/` is always whole. The catalogue records it after that (PLAN §2.3, invariant 2).

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write as _};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;
use zstd::stream::{read::Decoder, write::Encoder};

use crate::hash::{Hash, Hasher};

/// zstd's default level: transcripts are mostly JSON and compress several times over at it,
/// faster than a location can be read.
const LEVEL: i32 = 3;

/// A segment being written in `tmp/`. Dropped before [`SegmentWriter::finish`], it leaves
/// nothing behind.
pub(crate) struct SegmentWriter {
    encoder: Encoder<'static, NamedTempFile>,
    hasher: Hasher,
    length: u64,
}

impl SegmentWriter {
    pub(super) fn create(tmp: &Path) -> io::Result<Self> {
        let mut encoder = Encoder::new(NamedTempFile::new_in(tmp)?, LEVEL)?;
        /* The frame carries a checksum of its content, so damage shows without the catalogue. */
        encoder.include_checksum(true)?;
        Ok(Self {
            encoder,
            hasher: Hasher::new(),
            length: 0,
        })
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.encoder.write_all(bytes)?;
        self.hasher.update(bytes);
        self.length += bytes.len() as u64;
        Ok(())
    }

    /// How many uncompressed bytes were written.
    pub(crate) fn length(&self) -> u64 {
        self.length
    }

    /// Makes the segment durable at `to` and returns the hash of its uncompressed bytes.
    pub(super) fn finish(self, to: &Path) -> io::Result<Hash> {
        let file = self.encoder.finish()?;
        file.as_file().sync_all()?;
        place(file, to)?;
        if let Some(dir) = to.parent() {
            sync_dir(dir)?;
        }
        Ok(self.hasher.finish())
    }
}

/// Writes `bytes` to `to` through a file in `tmp`, so `to` is either absent or whole.
pub(super) fn write_durable(tmp: &Path, to: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = NamedTempFile::new_in(tmp)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    place(file, to)?;
    match to.parent() {
        Some(dir) => sync_dir(dir),
        None => Ok(()),
    }
}

/// Creates `dir` and whichever of its parents are missing, each one durable in its parent.
pub(super) fn create_dir_durable(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let Some(parent) = dir.parent() else {
        return fs::create_dir(dir);
    };
    create_dir_durable(parent)?;
    match fs::create_dir(dir) {
        Ok(()) => sync_dir(parent),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Moves the written, flushed `file` to `to`, where nothing may be: a file already there fails
/// the move and stays as it is. [`sync_dir`] on `to`'s folder then makes the move durable.
#[cfg(not(windows))]
fn place(file: NamedTempFile, to: &Path) -> io::Result<()> {
    file.persist_noclobber(to)
        .map(drop)
        .map_err(|error| error.error)
}

/**
 * Moves the written, flushed `file` to `to`, where nothing may be: a file already there fails
 * the move and stays as it is.
 *
 * Windows has no flush for a folder ([`sync_dir`]), and a plain rename returns before NTFS has
 * written its log record, so a catalogue commit made right after it could reach the disk first
 * and record a segment a power cut then leaves in `tmp/`. `MOVEFILE_WRITE_THROUGH` returns once
 * the move is on disk. This is `tempfile`'s own persist with that flag: it also clears the
 * temporary attribute `tempfile` creates its files with.
 */
#[cfg(windows)]
fn place(file: NamedTempFile, to: &Path) -> io::Result<()> {
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
    /* Closed first; dropping `from` removes the file unless the move succeeds. */
    let (handle, mut from) = file.into_parts();
    drop(handle);
    let (from_wide, to_wide) = (wide(&from), wide(to));
    // SAFETY: both are NUL-terminated UTF-16 strings that outlive the calls.
    unsafe {
        if SetFileAttributesW(from_wide.as_ptr(), FILE_ATTRIBUTE_NORMAL) == 0 {
            return Err(io::Error::last_os_error());
        }
        /* Without `MOVEFILE_REPLACE_EXISTING` a file at `to` fails the move. */
        if MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), MOVEFILE_WRITE_THROUGH) == 0 {
            let error = io::Error::last_os_error();
            SetFileAttributesW(from_wide.as_ptr(), FILE_ATTRIBUTE_TEMPORARY);
            return Err(error);
        }
    }
    from.disable_cleanup(true);
    Ok(())
}

/// Flushes `dir`'s entries to disk, which is what makes a rename into it or a file created in
/// it survive a power cut.
#[cfg(unix)]
pub(super) fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows cannot flush a directory; [`place`] makes its move durable itself.
#[cfg(not(unix))]
pub(super) fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// The uncompressed bytes of the segment file at `path`, fed to `each`.
pub(super) fn read_segment(path: &Path, mut each: impl FnMut(&[u8])) -> io::Result<()> {
    let mut decoder = Decoder::new(File::open(path)?)?;
    let mut buffer = vec![0; 128 * 1024];
    loop {
        match decoder.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => each(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// A version's content: its segments decompressed one after another.
pub struct VersionReader {
    files: VecDeque<PathBuf>,
    open: Option<Decoder<'static, BufReader<File>>>,
}

impl VersionReader {
    pub(super) fn new(files: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            files: files.into_iter().collect(),
            open: None,
        }
    }
}

impl Read for VersionReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let decoder = match &mut self.open {
                Some(decoder) => decoder,
                None => match self.files.pop_front() {
                    Some(file) => self.open.insert(Decoder::new(File::open(file)?)?),
                    None => return Ok(0),
                },
            };
            let read = decoder.read(buffer)?;
            if read > 0 || buffer.is_empty() {
                return Ok(read);
            }
            self.open = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_already_in_place_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("placed");
        write_durable(dir.path(), &to, b"first").unwrap();
        let error = write_durable(dir.path(), &to, b"second").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&to).unwrap(), b"first");
        /* The file that could not be placed is gone. */
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
}
