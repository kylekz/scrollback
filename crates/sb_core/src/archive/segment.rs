//! Segments (PLAN §4.1): each is the zstd-compressed bytes of one range of a version, written
//! once and never changed. A version's content is its segments decompressed in order.
//!
//! A segment is written in `tmp/`, flushed to disk and only then renamed into place, so a file
//! under `files/` is always whole. The catalogue records it after that (PLAN §2.3, invariant 2).
//!
//! Reading goes through [`VersionReader`], the one place stored bytes are decompressed. It
//! checks each segment, and the version as a whole, against what the catalogue recorded, so no
//! reader is handed bytes other than the ones that were archived.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write as _};
use std::path::{Path, PathBuf};

use tempfile::{NamedTempFile, TempPath};
use thiserror::Error;
use zstd::stream::{read::Decoder, write::Encoder};

use crate::catalogue::Segment;
use crate::hash::{Hash, Hasher};

/// zstd's default level: transcripts are mostly JSON and compress several times over at it,
/// faster than a location can be read.
const LEVEL: i32 = 3;

/// A segment being written in `tmp/`. Dropped, it leaves nothing behind.
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

    /// Ends the segment and flushes it to disk, still in `tmp/`.
    pub(crate) fn seal(self) -> io::Result<Sealed> {
        let file = self.encoder.finish()?;
        file.as_file().sync_all()?;
        Ok(Sealed {
            file: file.into_temp_path(),
            length: self.length,
            hash: self.hasher.finish(),
        })
    }
}

/// A whole segment in `tmp/`, durable there and not yet in its place. Dropped, it is removed.
pub(crate) struct Sealed {
    pub(super) file: TempPath,
    /// The length and hash of its uncompressed bytes.
    pub length: u64,
    pub hash: Hash,
}

/// Writes `bytes` to the new file `to` through a file in `tmp`, so `to` is either absent or
/// whole. A `to` that already exists is left as it is.
pub(super) fn write_new(tmp: &Path, to: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = NamedTempFile::new_in(tmp)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(to) {
        Err(blocked) if blocked.error.kind() != io::ErrorKind::AlreadyExists => Err(blocked.error),
        _ => Ok(()),
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

/// What is wrong with a version's stored bytes.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum Problem {
    #[error("the segment at {offset} is missing")]
    SegmentMissing { offset: u64 },
    /// The segment's file is not a whole zstd frame.
    #[error("the segment at {offset} can't be decompressed: {detail}")]
    SegmentUnreadable { offset: u64, detail: String },
    #[error("the segment at {offset} doesn't hold the bytes that were archived")]
    SegmentChanged { offset: u64 },
    /// The segments don't follow one another: the catalogue has none for the bytes before
    /// `offset`.
    #[error("no segment holds the bytes before {offset}")]
    Gap { offset: u64 },
    /// Each segment matches, but together they are not the version the catalogue recorded.
    #[error("the segments together aren't the version that was archived")]
    VersionChanged,
}

impl Problem {
    /// The problem a [`VersionReader`] reported as `error`; `None` when the error is the file
    /// system's (a read that failed), which says nothing about the stored bytes.
    pub fn of(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref()
    }
}

impl From<Problem> for io::Error {
    fn from(problem: Problem) -> Self {
        Self::new(io::ErrorKind::InvalidData, problem)
    }
}

/**
 * A version's content: its segments decompressed one after another, each checked against the
 * length and hash the catalogue recorded for it, and all of them together against the
 * version's.
 *
 * A check that fails is a read error holding the [`Problem`] ([`Problem::of`]). After it the
 * reader stands at the next segment, so reading on finds every problem a version has; what was
 * read is then not the version's content. Other errors are the file system's.
 */
pub struct VersionReader {
    dir: PathBuf,
    segments: std::vec::IntoIter<Segment>,
    /// Problems known before anything is read.
    gaps: std::vec::IntoIter<Problem>,
    open: Option<Open>,
    /// The hash of everything read.
    content: Hasher,
    /// The version's recorded length and hash.
    recorded: (u64, Hash),
    /// Whether every byte so far was read and passed its checks.
    whole: bool,
    ended: bool,
}

/// The segment being read.
struct Open {
    decoder: Decoder<'static, BufReader<File>>,
    recorded: Segment,
    hasher: Hasher,
    length: u64,
}

impl VersionReader {
    /// The version of `length` bytes hashing to `hash`, whose `segments` are in `dir`.
    pub(super) fn new(dir: PathBuf, segments: Vec<Segment>, length: u64, hash: Hash) -> Self {
        let mut end = 0;
        let mut gaps = Vec::new();
        for segment in &segments {
            if segment.offset != end {
                gaps.push(Problem::Gap {
                    offset: segment.offset,
                });
            }
            end = segment.offset + segment.length;
        }
        if gaps.is_empty() && end != length {
            gaps.push(Problem::VersionChanged);
        }
        Self {
            dir,
            segments: segments.into_iter(),
            whole: gaps.is_empty(),
            gaps: gaps.into_iter(),
            open: None,
            content: Hasher::new(),
            recorded: (length, hash),
            ended: false,
        }
    }

    /// The hash state of what was read: once the reader has reached the end without an error,
    /// that of the version's content, ready to be continued with what is appended to it.
    pub(crate) fn into_content(self) -> Hasher {
        self.content
    }

    /// Opens the next segment; `None` when there is none.
    fn open_next(&mut self) -> io::Result<Option<Open>> {
        let Some(recorded) = self.segments.next() else {
            return Ok(None);
        };
        let file = self.dir.join(super::layout::segment_file(recorded.offset));
        let file = File::open(file).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Problem::SegmentMissing {
                    offset: recorded.offset,
                }
                .into()
            } else {
                error
            }
        })?;
        Ok(Some(Open {
            decoder: Decoder::new(file)?,
            recorded,
            hasher: Hasher::new(),
            length: 0,
        }))
    }
}

impl Read for VersionReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(gap) = self.gaps.next() {
            return Err(gap.into());
        }
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let open = match &mut self.open {
                Some(open) => open,
                None => match self.open_next() {
                    Ok(Some(open)) => self.open.insert(open),
                    /* The end: each segment was what was recorded; are they the version? */
                    Ok(None) => {
                        let ended = std::mem::replace(&mut self.ended, true);
                        if !ended && self.whole && self.content.clone().finish() != self.recorded.1
                        {
                            return Err(Problem::VersionChanged.into());
                        }
                        return Ok(0);
                    }
                    Err(error) => {
                        self.whole = false;
                        return Err(error);
                    }
                },
            };
            let offset = open.recorded.offset;
            match open.decoder.read(buffer) {
                Ok(0) => {
                    let matches = open.length == open.recorded.length
                        && open.hasher.clone().finish() == open.recorded.hash;
                    self.open = None;
                    if !matches {
                        self.whole = false;
                        return Err(Problem::SegmentChanged { offset }.into());
                    }
                }
                Ok(read) => {
                    open.hasher.update(&buffer[..read]);
                    open.length += read as u64;
                    self.content.update(&buffer[..read]);
                    return Ok(read);
                }
                /* The caller reads again. */
                Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
                Err(error) => {
                    self.open = None;
                    self.whole = false;
                    /* An error with no code from the operating system is the decoder's: the
                    file is not the frame that was written. */
                    return Err(match error.raw_os_error() {
                        Some(_) => error,
                        None => Problem::SegmentUnreadable {
                            offset,
                            detail: error.to_string(),
                        }
                        .into(),
                    });
                }
            }
        }
    }
}
