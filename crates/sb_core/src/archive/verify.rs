//! Verify (PLAN §4.4): re-hashes every segment against the catalogue, and every version's
//! content against its running hash, and reports what doesn't match or is missing. It repairs
//! nothing; it marks the versions it finds damaged, and the next sync of an item whose newest
//! version is damaged fetches the source again as a new version.

use std::io;

use toolbox_process::CancelToken;

use super::{Archive, ArchiveError, layout, segment};
use crate::hash::Hasher;
use crate::location::LocationId;

/// What is wrong with a version's stored bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The segment's file is not there.
    SegmentMissing { offset: u64 },
    /// The segment's file can't be read or decompressed.
    SegmentUnreadable { offset: u64, detail: String },
    /// The segment's bytes are not the ones the catalogue recorded.
    SegmentChanged { offset: u64 },
    /// The segments don't follow one another: the catalogue has none for the bytes before
    /// `offset`.
    Gap { offset: u64 },
    /// Each segment matches, but together they are not the version the catalogue recorded.
    VersionChanged,
}

/// A version with something wrong, and the item it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Damage {
    pub location: LocationId,
    pub provider: String,
    pub key: String,
    pub version: u32,
    pub problems: Vec<Problem>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub versions: usize,
    pub segments: usize,
    /// Uncompressed bytes read back and hashed.
    pub bytes: u64,
    /// The size of the segment files those bytes are stored in.
    pub stored_bytes: u64,
    pub damaged: Vec<Damage>,
}

impl Archive {
    /// Checks every stored byte against the catalogue (the module docs).
    pub fn verify(&self, cancel: &CancelToken) -> Result<VerifyReport, ArchiveError> {
        let mut report = VerifyReport::default();
        let mut damaged = Vec::new();
        let mut repaired = Vec::new();
        for stored in self.catalogue.stored()? {
            let dir = self
                .dir
                .join(layout::item_dir(
                    &stored.location,
                    &stored.provider,
                    &stored.folder,
                ))
                .join(layout::version_dir(stored.version.number));
            let mut problems = Vec::new();
            let mut content = Hasher::new();
            let mut end = 0;
            for recorded in &stored.segments {
                if cancel.is_cancelled() {
                    return Err(ArchiveError::Cancelled);
                }
                let offset = recorded.offset;
                if offset != end {
                    problems.push(Problem::Gap { offset });
                }
                end = offset + recorded.length;
                let file = dir.join(layout::segment_file(offset));
                let mut hasher = Hasher::new();
                let mut length = 0;
                let read = segment::read_segment(&file, |bytes| {
                    hasher.update(bytes);
                    content.update(bytes);
                    length += bytes.len() as u64;
                });
                match read {
                    Ok(()) if length == recorded.length && hasher.finish() == recorded.hash => {}
                    Ok(()) => problems.push(Problem::SegmentChanged { offset }),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        problems.push(Problem::SegmentMissing { offset });
                    }
                    Err(error) => problems.push(Problem::SegmentUnreadable {
                        offset,
                        detail: error.to_string(),
                    }),
                }
                report.segments += 1;
                report.bytes += length;
                report.stored_bytes += file.metadata().map_or(0, |metadata| metadata.len());
            }
            if problems.is_empty()
                && (end != stored.version.length || content.finish() != stored.version.hash)
            {
                problems.push(Problem::VersionChanged);
            }
            report.versions += 1;
            if problems.is_empty() {
                if stored.version.damaged {
                    repaired.push(stored.version.id);
                }
            } else {
                damaged.push(stored.version.id);
                report.damaged.push(Damage {
                    location: stored.location,
                    provider: stored.provider,
                    key: stored.key,
                    version: stored.version.number,
                    problems,
                });
            }
        }
        self.catalogue.set_damaged(&damaged, true)?;
        self.catalogue.set_damaged(&repaired, false)?;
        Ok(report)
    }
}
