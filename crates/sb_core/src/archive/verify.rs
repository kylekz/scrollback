//! Verify (PLAN §4.4): reads every version back through [`VersionReader`], which checks each
//! segment and the version's content against the catalogue, and reports what doesn't match or
//! is missing. It repairs nothing; it marks the versions it finds damaged, and the next sync of
//! an item whose newest version is damaged fetches the source again as a new version.
//!
//! A read the file system failed (a file in use, a disk error) says nothing about the stored
//! bytes. Such a version is reported as unread and is not marked.
//!
//! It also reports what `files/` holds that the catalogue doesn't record. Nothing else looks
//! for that, and nothing moves it but [`Archive::set_aside_uncatalogued`], when the owner asks.
//! The files of a session deleted for good that are still to be removed are reported apart:
//! they are the archive's to remove, not the owner's to set aside.
//!
//! A version deleted for good while verify reads it is neither reported nor marked: verify
//! marks a version only while its id is still of the version it read.
//!
//! And it checks the search index against the text it indexes, making it again if they differ.
//!
//! A verify that runs to its end is kept in the catalogue as the archive's last
//! ([`LastVerify`]), so Settings shows what it found after the app is started again
//! (DESIGN §4.13). One that is cancelled leaves the last as it was.

use std::fs;
use std::io::{self, Read as _};
use std::path::PathBuf;
use std::sync::PoisonError;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use toolbox_process::CancelToken;

use super::{Archive, ArchiveError, Problem, item_folder, layout};
use crate::catalogue::{self, Stored};
use crate::location::LocationId;

/// A version whose stored bytes are not the ones that were archived.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Damage {
    pub location: LocationId,
    pub provider: String,
    pub key: String,
    pub version: u32,
    pub problems: Vec<Problem>,
}

impl Damage {
    /// The stored file its first problem is in, relative to the archive: a segment, or the
    /// version's folder when the problem is not one segment's.
    pub fn file(&self) -> PathBuf {
        let version = layout::item_dir(&self.location, &self.provider, &item_folder(&self.key))
            .join(layout::version_dir(self.version));
        match self.problems.first() {
            Some(
                Problem::SegmentMissing { offset }
                | Problem::SegmentUnreadable { offset, .. }
                | Problem::SegmentChanged { offset },
            ) => version.join(layout::segment_file(*offset)),
            _ => version,
        }
    }
}

/// How far a verify is: the segments read back of all there are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VerifyProgress {
    pub segments_done: usize,
    pub segments: usize,
}

/// What the archive's last verify that ran to its end found, as the catalogue keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastVerify {
    /// When it ended, in Unix milliseconds, and how long it took.
    pub at: i64,
    pub took_ms: u64,
    pub versions: usize,
    pub segments: usize,
    pub damaged: Vec<Damage>,
    /// How many versions could not be read to the end, for a reason that isn't their bytes.
    pub unread: usize,
    /// How many paths under `files/` the catalogue doesn't record.
    pub uncatalogued: usize,
    /// How many folders of sessions deleted for good are still to be removed.
    pub removing: usize,
}

/// A version that could not be read to the end, for a reason that isn't its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unread {
    pub location: LocationId,
    pub provider: String,
    pub key: String,
    pub version: u32,
    pub error: String,
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
    pub unread: Vec<Unread>,
    /// What is under `files/` that the catalogue doesn't record, by its path relative to the
    /// archive. A folder is one path, whatever it holds; a folder that holds no file is none.
    pub uncatalogued: Vec<PathBuf>,
    /// The folders of items of sessions deleted for good that are still to be removed, as
    /// removing them failed (another program holds a file in them) or would go through a link,
    /// by their paths relative to the archive. Every start tries again.
    pub removing: Vec<PathBuf>,
    /// The search index did not match the text it indexes, and was made again from it.
    pub search_rebuilt: bool,
}

impl Archive {
    /// Checks every stored byte against the catalogue (the module docs).
    pub fn verify(&self, cancel: &CancelToken) -> Result<VerifyReport, ArchiveError> {
        self.verify_reporting(cancel, &mut |_| {})
    }

    /// [`Archive::verify`], telling `progress` how far it is after each version.
    pub fn verify_reporting(
        &self,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(VerifyProgress),
    ) -> Result<VerifyReport, ArchiveError> {
        let started = Instant::now();
        /* What `files/` holds beyond what the catalogue records now, less what it records once
        no segment is between its place and its commit: every file found is then either
        recorded or not going to be. */
        let removing = self.removing()?;
        let mut uncatalogued =
            self.uncatalogued(&self.accounted(&self.catalogue.stored()?)?, &removing)?;
        let stored = {
            let _quiet = self.placing.write().unwrap_or_else(PoisonError::into_inner);
            self.catalogue.stored()?
        };
        if !uncatalogued.is_empty() {
            let accounted = self.accounted(&stored)?;
            uncatalogued.retain(|path| !accounted.contains(path));
        }
        let mut report = VerifyReport {
            uncatalogued,
            removing: removing
                .into_iter()
                .filter(|path| self.dir.join(path).exists())
                .collect(),
            /* Search that is not available is the next start's to make again. */
            search_rebuilt: match self.search_index() {
                Ok(search) => !search.check()?,
                Err(_) => false,
            },
            ..VerifyReport::default()
        };
        let mut damaged = Vec::new();
        let mut repaired = Vec::new();
        let mut buffer = vec![0; 128 * 1024];
        let mut done = VerifyProgress {
            segments_done: 0,
            segments: stored.iter().map(|stored| stored.segments.len()).sum(),
        };
        progress(done);
        for stored in stored {
            let (number, was_damaged) = (stored.version.number, stored.version.damaged);
            let dir = self
                .dir
                .join(layout::item_dir(
                    &stored.location,
                    &stored.provider,
                    &stored.folder,
                ))
                .join(layout::version_dir(number));
            report.versions += 1;
            report.segments += stored.segments.len();
            report.stored_bytes += stored
                .segments
                .iter()
                .filter_map(|segment| {
                    fs::metadata(dir.join(layout::segment_file(segment.offset))).ok()
                })
                .map(|metadata| metadata.len())
                .sum::<u64>();
            let (location, provider, key) = (
                stored.location.clone(),
                stored.provider.clone(),
                stored.key.clone(),
            );
            let read = Stored {
                segments: Vec::new(),
                ..stored.clone()
            };
            let mut reader = self.reader(stored, 0);
            let mut problems = Vec::new();
            let mut unread = None;
            loop {
                if cancel.is_cancelled() {
                    return Err(ArchiveError::Cancelled);
                }
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => report.bytes += read as u64,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => match Problem::of(&error) {
                        Some(problem) => problems.push(problem.clone()),
                        None => {
                            unread = Some(error.to_string());
                            break;
                        }
                    },
                }
            }
            if !problems.is_empty() {
                damaged.push((
                    read,
                    Damage {
                        location: location.clone(),
                        provider: provider.clone(),
                        key: key.clone(),
                        version: number,
                        problems,
                    },
                ));
            } else if unread.is_none() && was_damaged {
                repaired.push(read);
            }
            if let Some(error) = unread {
                report.unread.push(Unread {
                    location,
                    provider,
                    key,
                    version: number,
                    error,
                });
            }
            done.segments_done = report.segments;
            progress(done);
        }
        let marks: Vec<(&Stored, bool)> = damaged
            .iter()
            .map(|(read, _)| (read, true))
            .chain(repaired.iter().map(|read| (read, false)))
            .collect();
        let there = self.catalogue.mark_as_read(&marks)?;
        report.damaged = damaged
            .into_iter()
            .zip(there)
            .filter_map(|((_, damage), there)| there.then_some(damage))
            .collect();
        self.catalogue.record_verify(&LastVerify {
            at: catalogue::now_ms(),
            took_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            versions: report.versions,
            segments: report.segments,
            damaged: report.damaged.clone(),
            unread: report.unread.len(),
            uncatalogued: report.uncatalogued.len(),
            removing: report.removing.len(),
        })?;
        Ok(report)
    }
}
