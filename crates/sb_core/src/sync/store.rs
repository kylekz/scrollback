//! Fetch and store (PLAN §5, steps 3 and 4), a batch of planned ranges at a time:
//!
//! 1. **Fetch.** The provider streams the batch's ranges; each is written as segments in
//!    `tmp/`.
//! 2. **Prove.** The source is read while its agent writes, and nothing a read returns says
//!    whether its bytes ever stood together. So, once the batch is fetched, the source hashes
//!    its own start for each range, as far as the version would reach with it. A range is
//!    kept only if that is the hash of the version's bytes with the range (and, where the
//!    version must be all of the source, the source ends there). Every state of a stored
//!    version is therefore bytes the source held at one instant: for an item made of lines,
//!    its start up to the end of a line. A line too long to hold in memory until its end
//!    arrives ([`HELD_MAX`]) ends that for its range, which is then stored as far as it goes
//!    and proven like an item that is not made of lines: the source must end there.
//! 3. **Store.** The kept ranges' segments are moved into place, made durable together, and
//!    committed to the catalogue in one transaction (PLAN §4.1).
//!
//! A range that fails its proof where the plan took the source for a continuation of the
//! archived version is fetched again whole, as the next version, in a later batch, if the
//! source no longer starts with what is archived. Any other is left for the next pass. A pass
//! that stops loses the batch it was in, and nothing else.

use std::collections::VecDeque;
use std::io::{self, Read as _};

use toolbox_process::CancelToken;

use super::plan::Planned;
use super::{Failure, PassReport, Progress, SyncError};
use crate::archive::{Archive, ArchiveError, Problem, RangeWriter, Sealed};
use crate::catalogue::{Commit, Target};
use crate::hash::{Hash, Hasher};
use crate::location::LocationId;
use crate::provider::{Hashed, ItemPrefix, ItemRange, ProviderError, Source};
use crate::transport::{Halt, RangeFailure, RangeSink};

/// The most ranges fetched, proven and committed together, and the most bytes their proofs
/// have the source hash: each range's version from its start to the range's end (one range may
/// need more on its own). A batch is what a stopped pass loses, what waits in `tmp/`, and what
/// is made durable with one flush.
const BATCH_ITEMS: usize = 256;
const BATCH_BYTES: u64 = 64 * 1024 * 1024;

/// The most of an unfinished line that is held in memory waiting for its end. Records are
/// far shorter; a file that is one line of gigabytes must not be held whole.
const HELD_MAX: usize = 32 * 1024 * 1024;

/// What a pass fetches and stores with.
pub(super) struct Storing<'a> {
    pub archive: &'a Archive,
    pub location: &'a LocationId,
    pub provider: &'a str,
    pub cancel: &'a CancelToken,
    pub progress: &'a mut dyn FnMut(&Progress),
    pub report: &'a mut PassReport,
    /// How many planned ranges are dealt with: stored, or left for another pass.
    pub done: usize,
}

/// What a batch does with one of its ranges.
enum Outcome {
    /// It is in `tmp/` as these segments, which hold `stored` bytes; with them the version
    /// hashes to `content`. It is stored once it is proven. `cut`: it ends at the range's
    /// last newline, and the source may go on after it.
    Fetched {
        segments: Vec<Sealed>,
        stored: u64,
        content: Hash,
        cut: bool,
    },
    /// The source's start with the range is not the version with it. Whether the start alone
    /// is still what is archived decides what that means.
    Doubted(Target),
    /// The source's start is not what is archived: it is fetched whole, as this.
    Rewritten(Target),
    /// Nothing of it is stored. It could not be read (`failed`), or it changed under the pass
    /// and the next pass sees what it is then.
    Left { failed: bool, reason: String },
}

impl Outcome {
    /// What an item is left as when the source could not give a range of it, or its proof.
    fn unread(failure: &RangeFailure) -> Self {
        Self::Left {
            failed: matches!(failure, RangeFailure::Unreadable(_)),
            reason: failure.to_string(),
        }
    }

    fn changed(when: &str) -> Self {
        Self::Left {
            failed: false,
            reason: format!("it changed {when} it was read"),
        }
    }
}

impl Storing<'_> {
    /// Fetches, proves and stores all of `work`, a batch at a time.
    pub(super) fn all<'p>(
        &mut self,
        work: Vec<Planned<'p>>,
        source: &mut dyn Source,
    ) -> Result<(), SyncError> {
        let mut queue = VecDeque::from(work);
        while !queue.is_empty() {
            let mut batch = Vec::new();
            let mut bytes = 0;
            while let Some(next) = queue.front()
                && batch.len() < BATCH_ITEMS
                && (batch.is_empty() || bytes + next.offset + next.length <= BATCH_BYTES)
            {
                bytes += next.offset + next.length;
                batch.extend(queue.pop_front());
            }
            self.batch(&batch, source, &mut queue)?;
        }
        Ok(())
    }

    fn stop_if_cancelled(&self) -> Result<(), SyncError> {
        if self.cancel.is_cancelled() {
            Err(SyncError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// The source's hash of each of `prefixes`, in order. It is asked for them only when there
    /// are any, and when the pass has not been cancelled.
    fn hashes(
        &self,
        source: &mut dyn Source,
        prefixes: &[ItemPrefix],
    ) -> Result<std::vec::IntoIter<Result<Hashed, RangeFailure>>, SyncError> {
        Ok(if prefixes.is_empty() {
            Vec::new()
        } else {
            self.stop_if_cancelled()?;
            source.hash_prefixes(prefixes)?
        }
        .into_iter())
    }

    /// One batch (the module docs). Ranges to fetch again whole are added to `queue`.
    fn batch<'p>(
        &mut self,
        batch: &[Planned<'p>],
        source: &mut dyn Source,
        queue: &mut VecDeque<Planned<'p>>,
    ) -> Result<(), SyncError> {
        self.stop_if_cancelled()?;
        /* Fetch. A range of no bytes has nothing to fetch, only a start to prove. */
        let fetching: Vec<&Planned<'_>> =
            batch.iter().filter(|planned| planned.length > 0).collect();
        let mut fetched = Vec::new();
        if !fetching.is_empty() {
            let requests: Vec<_> = fetching
                .iter()
                .map(|planned| ItemRange {
                    key: planned.scanned.key.clone(),
                    offset: planned.offset,
                    length: planned.length,
                })
                .collect();
            let mut sink = Sink {
                storing: self,
                fetching: &fetching,
                open: None,
                arrived: Vec::new(),
                stopped: None,
            };
            let ended = source.fetch(&requests, &mut sink);
            fetched = sink.finish(ended)?;
        }
        let mut fetched = fetched.into_iter();
        let mut outcomes: Vec<Outcome> = batch
            .iter()
            .map(|planned| {
                if planned.length == 0 {
                    Outcome::Fetched {
                        segments: Vec::new(),
                        stored: 0,
                        content: planned.base,
                        cut: planned.cut,
                    }
                } else {
                    fetched.next().unwrap_or_else(|| Outcome::Left {
                        failed: true,
                        reason: "the provider did not send it".to_string(),
                    })
                }
            })
            .collect();

        /* Prove: the source's start, as far as each version would reach with its range. */
        let prefix = |planned: &Planned<'_>, length| ItemPrefix {
            key: planned.scanned.key.clone(),
            length,
        };
        let proofs: Vec<_> = batch
            .iter()
            .zip(&outcomes)
            .filter_map(|(planned, outcome)| match outcome {
                Outcome::Fetched { stored, .. } => Some(prefix(planned, planned.offset + stored)),
                _ => None,
            })
            .collect();
        /* A provider that answers fewer proofs than it was asked has not proven the rest. */
        let mut proven = self.hashes(source, &proofs)?;
        for (planned, outcome) in batch.iter().zip(&mut outcomes) {
            let Outcome::Fetched {
                stored,
                content,
                cut,
                ..
            } = outcome
            else {
                continue;
            };
            let (end, cut) = (planned.offset + *stored, *cut);
            match proven.next().unwrap_or(Err(RangeFailure::Missing)) {
                Ok(hashed) if hashed.hash != *content => {
                    *outcome = match planned.otherwise {
                        Some(target) if *stored > 0 => Outcome::Doubted(target),
                        Some(target) => Outcome::Rewritten(target),
                        None => Outcome::changed("while"),
                    }
                }
                /* A version that must be all of its source: the source must end there. */
                Ok(hashed) if !cut && hashed.size != end => {
                    *outcome = Outcome::changed("after");
                }
                Ok(_) => {}
                Err(failure) => *outcome = Outcome::unread(&failure),
            }
        }
        /* An append that failed its proof: when the source still starts with what is archived,
        it was the new bytes that changed under the pass, and the item is left for the next
        one. Only a source whose start changed was rewritten. */
        let doubts: Vec<_> = batch
            .iter()
            .zip(&outcomes)
            .filter(|(_, outcome)| matches!(outcome, Outcome::Doubted(_)))
            .map(|(planned, _)| prefix(planned, planned.offset))
            .collect();
        let mut starts = self.hashes(source, &doubts)?;
        for (planned, outcome) in batch.iter().zip(&mut outcomes) {
            if let Outcome::Doubted(target) = *outcome {
                *outcome = match starts.next().unwrap_or(Err(RangeFailure::Missing)) {
                    Ok(hashed) if hashed.hash == planned.base => Outcome::changed("while"),
                    Ok(_) => Outcome::Rewritten(target),
                    Err(failure) => Outcome::unread(&failure),
                };
            }
        }

        /* Store what was proven. */
        let mut commits = Vec::new();
        let mut counts = Vec::new();
        let mut again = 0;
        for (planned, outcome) in batch.iter().zip(outcomes) {
            match outcome {
                Outcome::Fetched {
                    segments,
                    stored,
                    content,
                    ..
                } => {
                    counts.push((planned, stored));
                    commits.push((
                        Commit {
                            location: self.location,
                            provider: self.provider,
                            scanned: planned.scanned,
                            target: planned.target,
                            number: planned.number,
                            from: planned.offset,
                            length: planned.offset + stored,
                            hash: content,
                        },
                        segments,
                    ));
                }
                Outcome::Rewritten(target) | Outcome::Doubted(target) => {
                    queue.push_back(Planned::whole(planned.scanned, target, planned.number + 1));
                    again += 1;
                }
                Outcome::Left { failed, reason } => {
                    let left = Failure {
                        key: planned.scanned.key.clone(),
                        reason,
                    };
                    if failed {
                        self.report.failed.push(left);
                    } else {
                        self.report.deferred.push(left);
                    }
                }
            }
        }
        self.stop_if_cancelled()?;
        let outcomes = self.archive.store(commits)?;
        for ((planned, stored), outcome) in counts.into_iter().zip(outcomes) {
            let held_back = match outcome {
                Ok(held_back) => held_back,
                Err(error) => {
                    self.report.failed.push(Failure {
                        key: planned.scanned.key.clone(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            match planned.target {
                Target::NewItem => self.report.new_items += 1,
                Target::NewVersion { .. } => self.report.new_versions += 1,
                Target::Append { .. } if stored > 0 => self.report.appended += 1,
                /* Its start is what is archived and there was nothing more to store. */
                Target::Append { .. } => self.report.touched += 1,
            }
            self.report.held_back += usize::from(held_back);
            self.report.bytes_stored += stored;
        }
        self.done += batch.len() - again;
        (self.progress)(&Progress::Stored { items: self.done });
        Ok(())
    }
}

/// Where a batch's fetch streams to: each range into segments in `tmp/`.
struct Sink<'a, 'b, 'p> {
    storing: &'a mut Storing<'b>,
    /// The ranges being fetched, in request order.
    fetching: &'a [&'a Planned<'p>],
    /// The range arriving now.
    open: Option<Open>,
    /// What became of the ranges that have ended. They end in request order, so the next one's
    /// index is how many there are.
    arrived: Vec<Outcome>,
    /// Why the sink stopped the stream, once it has.
    stopped: Option<SyncError>,
}

/// A range in flight.
struct Open {
    writer: RangeWriter,
    /// The version's hash so far, with what was written of the range.
    content: Hasher,
    /// Whether the range is cut at its last newline: so for an item made of lines, until a
    /// line of it is longer than [`HELD_MAX`].
    cut: bool,
    /// The bytes since the last newline, when the range is cut there.
    held: Vec<u8>,
    received: u64,
    stored: u64,
    /// Why the range can't be stored, when it can't; the rest of it is read and dropped.
    refused: Option<String>,
}

impl Open {
    /// Takes the next bytes of the range. When it is cut, what follows the last newline so
    /// far is held until another newline arrives, and never stored if none does.
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.received += bytes.len() as u64;
        if self.refused.is_some() {
            return Ok(());
        }
        let mut write = |bytes: &[u8]| {
            self.content.update(bytes);
            self.stored += bytes.len() as u64;
            self.writer.write(bytes)
        };
        if !self.cut {
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
        /* A line too long to wait for the end of: from here on the range is stored as it
        comes, and holds only if the source ends where it does. */
        if self.held.len() > HELD_MAX {
            self.cut = false;
            write(&std::mem::take(&mut self.held))?;
        }
        Ok(())
    }
}

impl Sink<'_, '_, '_> {
    /// After the provider's fetch returned with `ended`: what became of each range, or why the
    /// pass stops.
    fn finish(mut self, ended: Result<(), ProviderError>) -> Result<Vec<Outcome>, SyncError> {
        if let Some(stopped) = self.stopped.take() {
            return Err(stopped);
        }
        ended?;
        Ok(self.arrived)
    }

    /// Stops the stream, keeping why.
    fn stop(&mut self, error: impl Into<SyncError>) -> Halt {
        self.stopped = Some(error.into());
        Halt
    }

    /// The planned range the provider is at with `request`, which must be the one after the
    /// last that ended; anything else stops the pass, since the bytes could not be told apart.
    /// So does a cancelled pass.
    fn current(&mut self, request: usize) -> Result<&Planned<'_>, Halt> {
        if request != self.arrived.len() || request >= self.fetching.len() {
            return Err(self.stop(SyncError::Protocol(format!(
                "it sent request {request} when request {} of {} was due",
                self.arrived.len(),
                self.fetching.len()
            ))));
        }
        if self.storing.cancel.is_cancelled() {
            return Err(self.stop(SyncError::Cancelled));
        }
        Ok(self.fetching[request])
    }

    /// Starts the range of `planned`. Appending to a version starts from the hash of what the
    /// archive holds of it, read back and checked against the catalogue on the way. A version
    /// that fails the check is marked damaged, and one that can't be read is left alone;
    /// neither is extended.
    fn begin(&self, planned: &Planned<'_>) -> Result<Open, SyncError> {
        let archive = self.storing.archive;
        let mut content = Hasher::new();
        let mut refused = None;
        if let Target::Append { version, .. } = planned.target {
            let mut reader = archive.read_version(version)?;
            let mut buffer = vec![0; 128 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) if Problem::of(&error).is_some() => {
                        archive.catalogue().set_damaged(&[version], true)?;
                        refused = Some(format!(
                            "its archived version is damaged ({error}); the next sync fetches \
                             it again whole"
                        ));
                        break;
                    }
                    Err(error) => {
                        refused = Some(format!("its archived version could not be read: {error}"));
                        break;
                    }
                }
            }
            content = reader.into_content();
        }
        Ok(Open {
            writer: archive.range_writer(planned.offset),
            content,
            cut: planned.cut,
            held: Vec::new(),
            received: 0,
            stored: 0,
            refused,
        })
    }

    fn tmp_error(&self, source: io::Error) -> ArchiveError {
        ArchiveError::Io {
            action: "write a segment in",
            path: self.storing.archive.tmp(),
            source,
        }
    }
}

impl RangeSink for Sink<'_, '_, '_> {
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
        self.current(request)?;
        let mut open = match self.open.take() {
            Some(open) => open,
            None => self
                .begin(self.fetching[request])
                .map_err(|error| self.stop(error))?,
        };
        let written = open.write(bytes);
        self.open = Some(open);
        written.map_err(|source| {
            let error = self.tmp_error(source);
            self.stop(error)
        })
    }

    fn end(&mut self, request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        let length = self.current(request)?.length;
        let open = self.open.take();
        self.storing.report.bytes_fetched += open.as_ref().map_or(0, |open| open.received);
        let failed = |reason| Outcome::Left {
            failed: true,
            reason,
        };
        let arrived = match (result, open) {
            (Err(failure), _) => Outcome::unread(&failure),
            (
                Ok(()),
                Some(Open {
                    refused: Some(reason),
                    ..
                }),
            ) => failed(reason),
            (Ok(()), Some(open)) if open.received == length => Outcome::Fetched {
                segments: open.writer.finish().map_err(|source| {
                    let error = self.tmp_error(source);
                    self.stop(error)
                })?,
                stored: open.stored,
                content: open.content.finish(),
                cut: open.cut,
            },
            (Ok(()), open) => failed(format!(
                "the provider sent {} of {length} bytes",
                open.map_or(0, |open| open.received)
            )),
        };
        self.arrived.push(arrived);
        (self.storing.progress)(&Progress::Fetched {
            bytes: self.storing.report.bytes_fetched,
        });
        Ok(())
    }
}
