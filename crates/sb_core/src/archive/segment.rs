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
        file.persist(to).map_err(|error| error.error)?;
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
    file.persist(to).map_err(|error| error.error)?;
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

/// Flushes `dir`'s entries to disk, which is what makes a rename into it or a file created in
/// it survive a power cut.
#[cfg(unix)]
pub(super) fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows cannot flush a directory; NTFS journals the rename itself.
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
