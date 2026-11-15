//! Verify (PLAN §4.4): reads every version back through [`VersionReader`], which checks each
//! segment and the version's content against the catalogue, and reports what doesn't match or
//! is missing. It repairs nothing; it marks the versions it finds damaged, and the next sync of
//! an item whose newest version is damaged fetches the source again as a new version.
//!
//! A read the file system failed (a file in use, a disk error) says nothing about the stored
//! bytes. Such a version is reported as unread and is not marked.
//!
//! It also reports what `files/` holds that the catalogue doesn't record, which a start only
//! looks for when it has a reason to (the archive's module docs).

use std::fs;
use std::io::{self, Read as _};
use std::path::PathBuf;
use std::sync::PoisonError;

use toolbox_process::CancelToken;

use super::{Archive, ArchiveError, Problem, layout, uncatalogued};
use crate::location::LocationId;

/// A version whose stored bytes are not the ones that were archived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Damage {
    pub location: LocationId,
    pub provider: String,
    pub key: String,
    pub version: u32,
    pub problems: Vec<Problem>,
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
    /// Item folders and segment files under `files/` that the catalogue doesn't record, by
    /// their paths relative to the archive.
    pub uncatalogued: Vec<PathBuf>,
}

impl Archive {
    /// Checks every stored byte against the catalogue (the module docs).
    pub fn verify(&self, cancel: &CancelToken) -> Result<VerifyReport, ArchiveError> {
        let files = self.files()?;
        /* The catalogue is read while no segment is between its place and its commit: every
        file found above is then either recorded or not going to be. */
        let stored = {
            let _quiet = self.placing.write().unwrap_or_else(PoisonError::into_inner);
            self.catalogue.stored()?
        };
        let mut report = VerifyReport {
            uncatalogued: uncatalogued(files, &stored),
            ..VerifyReport::default()
        };
        let mut damaged = Vec::new();
        let mut repaired = Vec::new();
        let mut buffer = vec![0; 128 * 1024];
        for stored in stored {
            let (id, number, was_damaged) = (
                stored.version.id,
                stored.version.number,
                stored.version.damaged,
            );
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
            let mut reader = self.reader(stored);
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
                damaged.push(id);
                report.damaged.push(Damage {
                    location: location.clone(),
                    provider: provider.clone(),
                    key: key.clone(),
                    version: number,
                    problems,
                });
            } else if unread.is_none() && was_damaged {
                repaired.push(id);
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
        }
        self.catalogue.set_damaged(&damaged, true)?;
        self.catalogue.set_damaged(&repaired, false)?;
        Ok(report)
    }
}
