//! Store (PLAN §5, step 4): the sink a provider's fetch streams into. Each range becomes a
//! segment, durable on disk, and then its rows are committed (PLAN §4.1), one item at a time,
//! so whatever a stopped pass finished stays and the next pass continues from the catalogue.

use std::io::{self, Read as _};

use toolbox_process::CancelToken;

use super::plan::Planned;
use super::{Failure, PassReport, Progress, SyncError};
use crate::archive::{Archive, ArchiveError, SegmentWriter, item_folder};
use crate::catalogue::{Commit, Target};
use crate::hash::Hasher;
use crate::location::LocationId;
use crate::transport::{Halt, RangeFailure, RangeSink};

pub(super) struct Store<'a> {
    pub archive: &'a Archive,
    pub location: &'a LocationId,
    pub provider: &'a str,
    /// The ranges being fetched, in request order.
    pub fetches: &'a [&'a Planned<'a>],
    pub cancel: &'a CancelToken,
    pub progress: &'a mut dyn FnMut(&Progress),
    pub report: &'a mut PassReport,
    /// The range arriving now.
    pub open: Option<Open>,
    /// How many ranges have ended.
    pub ended: usize,
    /// Why the sink stopped the stream, once it has.
    pub stopped: Option<SyncError>,
}

/// A range in flight.
pub(super) struct Open {
    /// `None` when the range can't be stored; the rest of it is read and dropped.
    writer: Option<SegmentWriter>,
    /// The version's hash so far, with what was written of the range.
    content: Hasher,
    /// The bytes since the last newline, when the range is cut there.
    held: Vec<u8>,
    received: u64,
    failed: Option<String>,
}

impl Open {
    /// Takes the next bytes of the range. When it is `cut`, what follows the last newline so
    /// far is held until another newline arrives, and never stored if none does.
    fn write(&mut self, bytes: &[u8], cut: bool) -> io::Result<()> {
        self.received += bytes.len() as u64;
        let Some(writer) = &mut self.writer else {
            return Ok(());
        };
        let mut write = |bytes: &[u8]| {
            self.content.update(bytes);
            writer.write(bytes)
        };
        if !cut {
            return write(bytes);
        }
        match bytes.iter().rposition(|byte| *byte == b'\n') {
            Some(newline) => {
                write(&self.held)?;
                write(&bytes[..=newline])?;
                self.held.clear();
                self.held.extend_from_slice(&bytes[newline + 1..]);
            }
            None => self.held.extend_from_slice(bytes),
        }
        Ok(())
    }
}

impl Store<'_> {
    /// Records an item or version whose source is empty: there is nothing to fetch.
    pub(super) fn store_empty(&mut self, planned: &Planned<'_>) -> Result<(), SyncError> {
        self.commit(planned, None, Hasher::new())
    }

    /// After the provider's fetch returned: why the stream was stopped, if it was. Every range
    /// the provider never ended is a failure of that item.
    pub(super) fn finish(mut self) -> Result<(), SyncError> {
        if let Some(stopped) = self.stopped.take() {
            return Err(stopped);
        }
        for planned in &self.fetches[self.ended..] {
            self.report.failed.push(Failure {
                key: planned.scanned.key.clone(),
                reason: "the provider did not send it".to_string(),
            });
        }
        Ok(())
    }

    /// Stops the stream, keeping why.
    fn stop(&mut self, error: impl Into<SyncError>) -> Halt {
        self.stopped = Some(error.into());
        Halt
    }

    /// Starts the range of `planned`. Appending to a version starts from the hash of what the
    /// archive holds of it, read back, which also checks those bytes are still the ones
    /// recorded; a version that fails is marked damaged and not extended.
    fn begin(&self, planned: &Planned<'_>) -> Result<Open, SyncError> {
        let mut content = Hasher::new();
        let mut failed = None;
        if let (Target::Append { version, .. }, Some(recorded)) = (planned.target, planned.hash) {
            let mut reader = self.archive.read_version(version)?;
            let mut buffer = vec![0; 128 * 1024];
            let read = loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break Ok(()),
                    Ok(read) => content.update(&buffer[..read]),
                    Err(error) => break Err(error),
                }
            };
            if read.is_err() || content.clone().finish() != recorded {
                self.archive.catalogue().set_damaged(&[version], true)?;
                failed = Some(
                    "its archived version is damaged; the next sync fetches it again whole"
                        .to_string(),
                );
            }
        }
        Ok(Open {
            writer: match failed {
                None => Some(self.archive.segment_writer()?),
                Some(_) => None,
            },
            content,
            held: Vec::new(),
            received: 0,
            failed,
        })
    }

    /// Puts a finished range in the archive: the segment into place, then its rows.
    fn commit(
        &mut self,
        planned: &Planned<'_>,
        writer: Option<SegmentWriter>,
        content: Hasher,
    ) -> Result<(), SyncError> {
        let key = &planned.scanned.key;
        self.archive
            .prepare_item(self.location, self.provider, key)?;
        /* Nothing to store yet when the source is empty, or is lines with no newline so far. */
        let segment = match writer.filter(|writer| writer.length() > 0) {
            Some(writer) => {
                let length = writer.length();
                let hash = self.archive.place_segment(
                    writer,
                    self.location,
                    self.provider,
                    key,
                    planned.number,
                    planned.offset,
                )?;
                Some((planned.offset, length, hash))
            }
            None => None,
        };
        let stored = segment.map_or(0, |(_, length, _)| length);
        let held_back = self.archive.catalogue().commit(&Commit {
            location: self.location,
            provider: self.provider,
            scanned: planned.scanned,
            folder: &item_folder(key),
            target: planned.target,
            segment,
            length: planned.offset + stored,
            hash: content.finish(),
        })?;
        match planned.target {
            Target::NewItem => self.report.new_items += 1,
            Target::NewVersion { .. } => self.report.new_versions += 1,
            Target::Append { .. } => self.report.appended += 1,
        }
        self.report.held_back += usize::from(held_back);
        self.report.bytes_stored += stored;
        Ok(())
    }
}

impl RangeSink for Store<'_> {
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
        if self.cancel.is_cancelled() {
            return Err(self.stop(SyncError::Cancelled));
        }
        let planned = self.fetches[request];
        let mut open = match self.open.take() {
            Some(open) => open,
            None => self.begin(planned).map_err(|error| self.stop(error))?,
        };
        let written = open.write(bytes, planned.cut);
        self.open = Some(open);
        written.map_err(|source| {
            self.stop(ArchiveError::Io {
                action: "write a segment in",
                path: self.archive.dir().join("tmp"),
                source,
            })
        })
    }

    fn end(&mut self, request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        if self.cancel.is_cancelled() {
            return Err(self.stop(SyncError::Cancelled));
        }
        let planned = self.fetches[request];
        let open = match self.open.take() {
            Some(open) => open,
            None => self.begin(planned).map_err(|error| self.stop(error))?,
        };
        self.report.bytes_fetched += open.received;
        let failed = match (result, open.failed) {
            (Err(failure), _) => Some(failure.to_string()),
            (Ok(()), Some(reason)) => Some(reason),
            (Ok(()), None) if open.received != planned.length => Some(format!(
                "the provider sent {} of {} bytes",
                open.received, planned.length
            )),
            (Ok(()), None) => {
                self.commit(planned, open.writer, open.content)
                    .map_err(|error| self.stop(error))?;
                None
            }
        };
        if let Some(reason) = failed {
            self.report.failed.push(Failure {
                key: planned.scanned.key.clone(),
                reason,
            });
        }
        self.ended = request + 1;
        (self.progress)(&Progress::Fetched {
            items: self.ended,
            bytes: self.report.bytes_fetched,
        });
        Ok(())
    }
}
