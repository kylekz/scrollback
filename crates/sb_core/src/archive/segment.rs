//! Segments (PLAN §4.1): each is the zstd-compressed bytes of one range of a version, written
//! once and never changed. A version's content is its segments decompressed in order.
//!
//! A segment is written in `tmp/` and moved into place whole, and it is durable before the
//! catalogue records it (PLAN §2.3, invariant 2).
//!
//! Reading goes through [`VersionReader`], the one place stored bytes are decompressed. It
//! checks each segment against what the catalogue recorded before handing out any of it, and
//! the version as a whole at its end, so no reader is handed bytes other than the ones that
//! were archived.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use tempfile::{NamedTempFile, TempPath};
use thiserror::Error;
use zstd::stream::{read::Decoder, write::Encoder};

use super::disk;
use crate::catalogue::Segment;
use crate::hash::{Hash, Hasher};

/**
 * Whether the segment file at `path` holds exactly the `length` bytes of SHA-256 `hash`. A
 * file that is not a frame, or holds other bytes, does not; one that can't be read is an error.
 */
pub(super) fn holds(path: &Path, length: u64, hash: Hash) -> io::Result<bool> {
    holds_in(File::open(path)?, length, hash)
}

/// [`holds`], of the segment's file that `file` reads from where it is.
pub(super) fn holds_in(file: impl Read, length: u64, hash: Hash) -> io::Result<bool> {
    let mut bytes = Vec::with_capacity(length.min(SEGMENT_BYTES) as usize + 1);
    let read = Decoder::new(file)?.take(length + 1).read_to_end(&mut bytes);
    match read {
        Ok(_) => Ok(bytes.len() as u64 == length && Hash::of(&bytes) == hash),
        Err(error) if error.raw_os_error().is_some() => Err(error),
        Err(_) => Ok(false),
    }
}

/// zstd's default level: transcripts are mostly JSON and compress several times over at it,
/// faster than a location can be read.
const LEVEL: i32 = 3;

/**
 * The most a segment holds, uncompressed. A longer range is stored as several segments.
 *
 * A segment is read whole, so this bounds what is decompressed to reach any byte (a few
 * milliseconds' worth) and what a reader holds in memory. It is four times the window
 * zstd compresses with at this level, so cutting a range here costs next to nothing in size,
 * and small enough that a transcript of a hundred megabytes is a dozen files, not hundreds.
 */
pub const SEGMENT_BYTES: u64 = 8 * 1024 * 1024;

/// A whole segment in `tmp/`, not yet in its place. Dropped, it is removed.
pub(crate) struct Sealed {
    pub(super) file: TempPath,
    /// Where in its version the segment starts.
    pub offset: u64,
    /// The length and hash of its uncompressed bytes.
    pub length: u64,
    pub hash: Hash,
}

/// A segment being written in `tmp/`.
struct Open {
    encoder: Encoder<'static, NamedTempFile>,
    hasher: Hasher,
    length: u64,
}

/// A range of a version being written in `tmp/`, as one segment after another. Dropped, it
/// leaves nothing behind.
pub(crate) struct RangeWriter {
    tmp: PathBuf,
    /// Where in the version the next byte goes.
    end: u64,
    open: Option<Open>,
    sealed: Vec<Sealed>,
}

impl RangeWriter {
    /// A range that starts at `offset` of its version, written in `tmp`.
    pub(super) fn new(tmp: PathBuf, offset: u64) -> Self {
        Self {
            tmp,
            end: offset,
            open: None,
            sealed: Vec::new(),
        }
    }

    pub(crate) fn write(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let open = match &mut self.open {
                Some(open) => open,
                None => {
                    let mut encoder = Encoder::new(NamedTempFile::new_in(&self.tmp)?, LEVEL)?;
                    /* The frame carries a checksum of its content, so damage shows without the
                    catalogue. */
                    encoder.include_checksum(true)?;
                    self.open.insert(Open {
                        encoder,
                        hasher: Hasher::new(),
                        length: 0,
                    })
                }
            };
            let room = usize::try_from(SEGMENT_BYTES - open.length).unwrap_or(usize::MAX);
            let (now, later) = bytes.split_at(bytes.len().min(room));
            open.encoder.write_all(now)?;
            open.hasher.update(now);
            open.length += now.len() as u64;
            self.end += now.len() as u64;
            bytes = later;
            if open.length == SEGMENT_BYTES {
                self.seal()?;
            }
        }
        Ok(())
    }

    /// Ends the segment being written and hands its data to the drive.
    fn seal(&mut self) -> io::Result<()> {
        if let Some(open) = self.open.take() {
            let file = open.encoder.finish()?;
            disk::push(file.as_file())?;
            self.sealed.push(Sealed {
                file: file.into_temp_path(),
                offset: self.end - open.length,
                length: open.length,
                hash: open.hasher.finish(),
            });
        }
        Ok(())
    }

    /// The range's segments, whole, still in `tmp/`: none when nothing was written.
    pub(crate) fn finish(mut self) -> io::Result<Vec<Sealed>> {
        self.seal()?;
        Ok(self.sealed)
    }
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
 * Segments readers decoded and checked, kept to be read again without decoding them anew: a
 * reader that opens one body after another of a session (PLAN §9) reads the same few segments.
 * It holds the segments read last, at most `segments` of them and `bytes` together (each is up
 * to [`SEGMENT_BYTES`]). A stored segment never changes, so a kept one is always its file's
 * bytes.
 */
pub struct SegmentCache {
    segments: usize,
    bytes: usize,
    /// The one read last at the back.
    kept: Mutex<VecDeque<Kept>>,
}

/// A segment kept: its file, the hash the catalogue records for it, and its bytes.
struct Kept {
    file: PathBuf,
    hash: Hash,
    bytes: Arc<Vec<u8>>,
}

impl SegmentCache {
    pub fn new(segments: usize, bytes: usize) -> Self {
        Self {
            segments,
            bytes,
            kept: Mutex::new(VecDeque::with_capacity(segments)),
        }
    }

    fn get(&self, path: &Path, hash: Hash) -> Option<Arc<Vec<u8>>> {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        let at = kept
            .iter()
            .position(|it| it.file == path && it.hash == hash)?;
        let entry = kept.remove(at)?;
        let bytes = entry.bytes.clone();
        kept.push_back(entry);
        Some(bytes)
    }

    fn put(&self, path: PathBuf, hash: Hash, bytes: Arc<Vec<u8>>) {
        if self.segments == 0 || bytes.len() > self.bytes {
            return;
        }
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        kept.retain(|it| it.file != path);
        while kept.len() >= self.segments
            || kept.iter().map(|it| it.bytes.len()).sum::<usize>() + bytes.len() > self.bytes
        {
            kept.pop_front();
        }
        kept.push_back(Kept {
            file: path,
            hash,
            bytes,
        });
    }
}

/**
 * A version's content from some offset on. Each segment is decompressed whole and checked
 * against the length and hash the catalogue recorded for it before any byte of it is handed
 * out, so a reader that stops early has still only seen archived bytes. Read from the start to
 * the end, the segments together are checked against the version's hash as well.
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
    /// The checked bytes of the segment being handed out, and how many of them have been.
    segment: Arc<Vec<u8>>,
    served: usize,
    /// How many bytes of the segment read next come before the offset reading starts at.
    skip: u64,
    /// The hash of everything handed out.
    content: Hasher,
    /// The version's recorded hash.
    recorded: Hash,
    /// Whether every byte of the version so far was handed out and passed its checks.
    intact: bool,
    ended: bool,
    /// Where segments decoded before are kept, and where this reader's go.
    cache: Option<Arc<SegmentCache>>,
}

impl VersionReader {
    /// The version of `length` bytes hashing to `hash`, whose `segments` are in `dir`, from
    /// its byte `start` on, its segments kept in `cache` when there is one.
    pub(super) fn new(
        dir: PathBuf,
        segments: Vec<Segment>,
        length: u64,
        hash: Hash,
        start: u64,
        cache: Option<Arc<SegmentCache>>,
    ) -> Self {
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
        /* The segments that end at or before `start` are not read at all. */
        let before = segments
            .iter()
            .take_while(|segment| segment.offset + segment.length <= start)
            .count();
        let skip = segments
            .get(before)
            .map_or(0, |first| start.saturating_sub(first.offset));
        let mut segments = segments.into_iter();
        segments.by_ref().take(before).for_each(drop);
        Self {
            dir,
            segments,
            intact: gaps.is_empty() && start == 0,
            gaps: gaps.into_iter(),
            segment: Arc::default(),
            served: 0,
            skip,
            content: Hasher::new(),
            recorded: hash,
            ended: false,
            cache,
        }
    }

    /// The hash state of what was read: once the reader has reached the end from the start
    /// without an error, that of the version's content, ready to be continued with what is
    /// appended to it.
    pub(crate) fn into_content(self) -> Hasher {
        self.content
    }

    /// The bytes of the segment recorded as `recorded`, if its file holds exactly them: from
    /// the cache when they were decoded and checked before.
    fn checked(&self, recorded: &Segment) -> io::Result<Arc<Vec<u8>>> {
        let path = self.dir.join(super::layout::segment_file(recorded.offset));
        let cache = self.cache.as_ref();
        if let Some(bytes) = cache.and_then(|cache| cache.get(&path, recorded.hash)) {
            return Ok(bytes);
        }
        let bytes = Arc::new(decode(&path, recorded)?);
        if let Some(cache) = cache {
            cache.put(path, recorded.hash, bytes.clone());
        }
        Ok(bytes)
    }
}

/// The bytes of the segment file at `path`, decompressed, if they are what `recorded` says.
fn decode(path: &Path, recorded: &Segment) -> io::Result<Vec<u8>> {
    let offset = recorded.offset;
    let file = File::open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            Problem::SegmentMissing { offset }.into()
        } else {
            error
        }
    })?;
    /* No more is decompressed than the segment may hold, whatever the file claims. The buffer
    is as large as the segment from the start: one that grows as it is filled leaves the
    allocator holding every size it passed through, which over the segments of a long
    session's items is hundreds of megabytes. */
    let mut bytes = Vec::with_capacity(recorded.length.min(SEGMENT_BYTES) as usize + 1);
    Decoder::new(file)?
        .take(recorded.length + 1)
        .read_to_end(&mut bytes)
        /* An error with no code from the operating system is the decoder's: the file is not
        the frame that was written. */
        .map_err(|error| match error.raw_os_error() {
            Some(_) => error,
            None => Problem::SegmentUnreadable {
                offset,
                detail: error.to_string(),
            }
            .into(),
        })?;
    if Hash::of(&bytes) != recorded.hash {
        return Err(Problem::SegmentChanged { offset }.into());
    }
    Ok(bytes)
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
            let left = &self.segment[self.served..];
            if !left.is_empty() {
                let read = left.len().min(buffer.len());
                buffer[..read].copy_from_slice(&left[..read]);
                self.content.update(&left[..read]);
                self.served += read;
                return Ok(read);
            }
            let Some(recorded) = self.segments.next() else {
                /* The end: each segment was what was recorded; are they the version? */
                let ended = std::mem::replace(&mut self.ended, true);
                if !ended && self.intact && self.content.clone().finish() != self.recorded {
                    return Err(Problem::VersionChanged.into());
                }
                return Ok(0);
            };
            /* What comes before the offset reading starts at is checked like the rest and not
            handed out. */
            let skip = std::mem::take(&mut self.skip);
            self.segment = Arc::default();
            self.served = 0;
            match self.checked(&recorded) {
                Ok(bytes) => {
                    self.served =
                        usize::try_from(skip).map_or(bytes.len(), |skip| skip.min(bytes.len()));
                    self.segment = bytes;
                }
                Err(error) => {
                    self.intact = false;
                    return Err(error);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_keeps_what_its_count_and_its_bytes_allow_the_last_read_last() {
        let cache = SegmentCache::new(3, 10);
        let hash = Hash::of(b"");
        let put = |name: &str, bytes: usize| {
            cache.put(PathBuf::from(name), hash, Arc::new(vec![0; bytes]));
        };
        let kept = |name: &str| cache.get(Path::new(name), hash).is_some();
        put("a", 4);
        put("b", 4);
        // Reading a again makes b the oldest; c needs b's room.
        assert!(kept("a"));
        put("c", 4);
        assert!(!kept("b") && kept("a") && kept("c"));
        // Over the bytes on its own: not kept, and nothing is given way for it.
        put("huge", 11);
        assert!(!kept("huge") && kept("a") && kept("c"));
        // The count bounds it too.
        put("d", 1);
        put("e", 1);
        assert!(!kept("a") && kept("c") && kept("d") && kept("e"));
    }

    #[test]
    fn a_range_is_cut_into_segments_wherever_its_writes_fall() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = RangeWriter::new(dir.path().to_path_buf(), 100);
        /* Writes of a length that the most a segment holds is no multiple of. */
        let bytes: Vec<u8> = (0..20_000_000_u32).map(|n| (n % 251) as u8).collect();
        for piece in bytes.chunks(200_001) {
            writer.write(piece).unwrap();
        }
        let sealed = writer.finish().unwrap();
        let shape: Vec<_> = sealed.iter().map(|s| (s.offset, s.length)).collect();
        assert_eq!(
            shape,
            [
                (100, SEGMENT_BYTES),
                (100 + SEGMENT_BYTES, SEGMENT_BYTES),
                (100 + SEGMENT_BYTES * 2, 20_000_000 - SEGMENT_BYTES * 2)
            ]
        );
        let mut at = 0;
        for segment in &sealed {
            let stored = zstd::decode_all(File::open(&segment.file).unwrap()).unwrap();
            let end = at + stored.len();
            assert!(stored == bytes[at..end]);
            assert_eq!(segment.hash, Hash::of(&stored));
            at = end;
        }
        assert_eq!(at, bytes.len());
    }
}
