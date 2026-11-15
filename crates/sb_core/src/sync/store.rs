//! Store (PLAN §5, step 4): the sink a provider's fetch streams into. Each range becomes a
//! segment, durable on disk, and then its rows are committed (PLAN §4.1), one item at a time,
//! so whatever a stopped pass finished stays and the next pass continues from the catalogue.
//!
//! Every state of a stored version is bytes the source held at one instant: for an item made
//! of lines, the start of the source at that instant up to the end of a line. The source is
//! read while its agent writes, so that is checked, not assumed:
//! - a range the provider reports as read from a source that stood still since the scan
//!   ([`Fetched::Stable`]) is stored as it arrives;
//! - any other range waits in `tmp/` until the fetch is over. Then the source hashes its own
//!   start, as far as the version would reach with the range, and the range is stored only if
//!   that is the hash of the version's bytes (and, for an item that isn't lines, the source
//!   ends there too). Otherwise nothing is stored and the item is left for the next pass.

use std::io::{self, Read as _};

use toolbox_process::CancelToken;

use super::plan::Planned;
use super::{Failure, PassReport, Progress, SyncError};
use crate::archive::{Archive, ArchiveError, Problem, Sealed, SegmentWriter, item_folder};
use crate::catalogue::{Commit, Target};
use crate::hash::Hasher;
use crate::location::LocationId;
use crate::provider::{ItemPrefix, ProviderError, Source};
use crate::transport::{Fetched, Halt, RangeFailure, RangeSink};

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
    /// How many ranges have ended. They end in request order, so it is the next one's index.
    pub ended: usize,
    /// Ranges that arrived whole from a source that may have changed around them.
    pub unproven: Vec<Unproven>,
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

/// A range waiting in `tmp/` to be proven (the module docs).
pub(super) struct Unproven {
    request: usize,
    segment: Sealed,
    /// The version's hash with the segment.
    content: Hasher,
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

    /// After the provider's fetch returned with `fetched`: fails the pass with why the stream
    /// was stopped, if it was; counts every range the provider never ended as a failure of
    /// that item; then proves the ranges that wait for it against `source`.
    pub(super) fn finish(
        mut self,
        fetched: Result<(), ProviderError>,
        source: &mut dyn Source,
    ) -> Result<(), SyncError> {
        if let Some(stopped) = self.stopped.take() {
            return Err(stopped);
        }
        fetched?;
        for planned in &self.fetches[self.ended..] {
            self.report.failed.push(Failure {
                key: planned.scanned.key.clone(),
                reason: "the provider did not send it".to_string(),
            });
        }
        let unproven = std::mem::take(&mut self.unproven);
        if unproven.is_empty() {
            return Ok(());
        }
        let requests: Vec<_> = unproven
            .iter()
            .map(|unproven| {
                let planned = self.fetches[unproven.request];
                ItemPrefix {
                    key: planned.scanned.key.clone(),
                    length: planned.offset + unproven.segment.length,
                }
            })
            .collect();
        let mut hashes = source.hash_prefixes(&requests)?.into_iter();
        for (unproven, request) in unproven.into_iter().zip(requests) {
            if self.cancel.is_cancelled() {
                return Err(SyncError::Cancelled);
            }
            let planned = self.fetches[unproven.request];
            let proven = hashes.next().and_then(Result::ok).is_some_and(|hashed| {
                hashed.hash == unproven.content.clone().finish()
                    && (planned.scanned.lines || hashed.size == request.length)
            });
            if proven {
                self.commit(planned, Some(unproven.segment), unproven.content)?;
            } else {
                self.report.deferred += 1;
            }
        }
        Ok(())
    }

    /// Stops the stream, keeping why.
    fn stop(&mut self, error: impl Into<SyncError>) -> Halt {
        self.stopped = Some(error.into());
        Halt
    }

    /// The planned range the provider is at with `request`, which must be the one after the
    /// last that ended; anything else stops the pass, since the bytes could not be told apart.
    fn current(&mut self, request: usize) -> Result<&Planned<'_>, Halt> {
        if request != self.ended || request >= self.fetches.len() {
            return Err(self.stop(SyncError::Protocol(format!(
                "it sent request {request} when request {} of {} was due",
                self.ended,
                self.fetches.len()
            ))));
        }
        Ok(self.fetches[request])
    }

    /// Starts the range of `planned`. Appending to a version starts from the hash of what the
    /// archive holds of it, read back and checked against the catalogue on the way. A version
    /// that fails the check is marked damaged, and one that can't be read is left alone;
    /// neither is extended.
    fn begin(&self, planned: &Planned<'_>) -> Result<Open, SyncError> {
        let mut content = Hasher::new();
        let mut failed = None;
        if let Target::Append { version, .. } = planned.target {
            let mut reader = self.archive.read_version(version)?;
            let mut buffer = vec![0; 128 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) if Problem::of(&error).is_some() => {
                        self.archive.catalogue().set_damaged(&[version], true)?;
                        failed = Some(format!(
                            "its archived version is damaged ({error}); the next sync fetches \
                             it again whole"
                        ));
                        break;
                    }
                    Err(error) => {
                        failed = Some(format!("its archived version could not be read: {error}"));
                        break;
                    }
                }
            }
            content = reader.into_content();
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
        segment: Option<Sealed>,
        content: Hasher,
    ) -> Result<(), SyncError> {
        /* Nothing to store yet when the source is empty, or is lines with no newline so far. */
        let segment = segment.filter(|segment| segment.length > 0);
        let recorded = segment
            .as_ref()
            .map(|segment| (planned.offset, segment.length, segment.hash));
        let stored = recorded.map_or(0, |(_, length, _)| length);
        let held_back = self.archive.store(
            segment,
            &Commit {
                location: self.location,
                provider: self.provider,
                scanned: planned.scanned,
                folder: &item_folder(&planned.scanned.key),
                target: planned.target,
                number: planned.number,
                segment: recorded,
                length: planned.offset + stored,
                hash: content.finish(),
            },
        )?;
        match planned.target {
            Target::NewItem => self.report.new_items += 1,
            Target::NewVersion { .. } => self.report.new_versions += 1,
            Target::Append { .. } => self.report.appended += 1,
        }
        self.report.held_back += usize::from(held_back);
        self.report.bytes_stored += stored;
        Ok(())
    }

    /// What became of a range that arrived whole: stored, kept to be proven, or left for the
    /// next pass.
    fn arrived(
        &mut self,
        request: usize,
        fetched: Fetched,
        writer: SegmentWriter,
        content: Hasher,
    ) -> Result<(), SyncError> {
        let planned = self.fetches[request];
        let segment = writer.seal().map_err(|source| ArchiveError::Io {
            action: "write a segment in",
            path: self.archive.tmp(),
            source,
        })?;
        match fetched {
            Fetched::Stable => self.commit(planned, Some(segment), content)?,
            /* The unfinished last line of a file is only taken from one that has stopped
            changing, and a range with nothing to store has nothing to prove. */
            Fetched::Unstable if (planned.scanned.lines && !planned.cut) || segment.length == 0 => {
                self.report.deferred += 1;
            }
            Fetched::Unstable => self.unproven.push(Unproven {
                request,
                segment,
                content,
            }),
        }
        Ok(())
    }
}

impl RangeSink for Store<'_> {
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
        let cut = self.current(request)?.cut;
        if self.cancel.is_cancelled() {
            return Err(self.stop(SyncError::Cancelled));
        }
        let mut open = match self.open.take() {
            Some(open) => open,
            None => self
                .begin(self.fetches[request])
                .map_err(|error| self.stop(error))?,
        };
        let written = open.write(bytes, cut);
        self.open = Some(open);
        written.map_err(|source| {
            self.stop(ArchiveError::Io {
                action: "write a segment in",
                path: self.archive.tmp(),
                source,
            })
        })
    }

    fn end(&mut self, request: usize, result: Result<Fetched, RangeFailure>) -> Result<(), Halt> {
        let length = self.current(request)?.length;
        if self.cancel.is_cancelled() {
            return Err(self.stop(SyncError::Cancelled));
        }
        let open = match self.open.take() {
            Some(open) => open,
            None => self
                .begin(self.fetches[request])
                .map_err(|error| self.stop(error))?,
        };
        self.report.bytes_fetched += open.received;
        let failed = match (result, open.failed, open.writer) {
            (Err(failure), ..) => Some(failure.to_string()),
            (Ok(_), Some(reason), _) => Some(reason),
            (Ok(_), None, _) if open.received != length => Some(format!(
                "the provider sent {} of {length} bytes",
                open.received
            )),
            (Ok(fetched), None, Some(writer)) => {
                self.arrived(request, fetched, writer, open.content)
                    .map_err(|error| self.stop(error))?;
                None
            }
            (Ok(_), None, None) => None,
        };
        if let Some(reason) = failed {
            self.report.failed.push(Failure {
                key: self.fetches[request].scanned.key.clone(),
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
