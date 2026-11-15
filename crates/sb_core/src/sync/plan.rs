//! Plan (PLAN §5, step 2): what a pass does with each item, from the scan and the catalogue
//! (PLAN §4.2). With the archived length `L` and hash `H` of the item's newest version, the
//! source's length `S`, and "as last seen" meaning the length and stamp a pass last recorded:
//!
//! | Source | Meaning | Action |
//! |---|---|---|
//! | Not in the catalogue | New | Fetch it whole as version 1 |
//! | As last seen, `S == L` | Unchanged | Skip |
//! | `S > L`, and the first `L` bytes hash to `H` | Appended | Fetch `[L, S)` into the version |
//! | `S == L`, not as last seen, hashes to `H` | Touched | Record the new stamp |
//! | `S < L`, or the first `L` bytes hash otherwise | Shrank or rewritten | Fetch it whole as the next version |
//! | Its newest version is damaged, or its export format changed | | Fetch it whole as the next version |
//! | In the catalogue, not in the scan | Gone | Mark it; keep everything |
//!
//! An item made of lines is stored up to its last newline, since its writer may be mid-record
//! (PLAN §4.2). When it is still as last seen on a later pass, nothing is writing it, and the
//! rest is fetched as it is: otherwise the bytes after a final newline that never comes would
//! never be archived.

use std::collections::HashMap;

use super::Failure;
use crate::catalogue::{Archived, Reason, Target};
use crate::provider::{ItemPrefix, ProviderError, RangeFailure, ScannedItem, Source};

/// A range of one item to fetch and store.
pub(super) struct Planned<'a> {
    pub scanned: &'a ScannedItem,
    pub target: Target,
    /// The number of the version the range goes into.
    pub number: u32,
    /// Where the range starts, which is the version's length before it.
    pub offset: u64,
    /// How many bytes to fetch: the rest of the source. 0 when the source is empty, which is
    /// recorded without a fetch.
    pub length: u64,
    /// Whether to store only up to the last newline of the range.
    pub cut: bool,
}

pub(super) struct Plan<'a> {
    pub fetches: Vec<Planned<'a>>,
    /// Items whose source is as archived, to record as seen: touched ones, and ones that were
    /// gone and are back.
    pub seen: Vec<(i64, &'a ScannedItem)>,
    /// Items missing at the source that are not marked gone yet.
    pub gone: Vec<i64>,
    pub unchanged: usize,
    /// Items that changed between the scan and the hashing of their prefix. The next pass
    /// decides about them.
    pub deferred: usize,
    /// Items whose archived bytes could not be compared with the source's.
    pub failed: Vec<Failure>,
}

/// Decides about each of `scanned`. `whole` says the scan saw everything the agent has: only
/// then is an archived item that is not in it missing at the source.
pub(super) fn plan<'a>(
    scanned: &'a [ScannedItem],
    whole: bool,
    archived: Vec<Archived>,
    source: &mut dyn Source,
) -> Result<Plan<'a>, ProviderError> {
    let mut archived: HashMap<String, Archived> = archived
        .into_iter()
        .map(|archived| (archived.item.key.clone(), archived))
        .collect();
    let mut plan = Plan {
        fetches: Vec::new(),
        seen: Vec::new(),
        gone: Vec::new(),
        unchanged: 0,
        deferred: 0,
        failed: Vec::new(),
    };
    /* Items whose first `L` bytes must be hashed at the source before they can be decided. */
    let mut undecided = Vec::new();
    for scanned in scanned {
        let Some(archived) = archived.remove(&scanned.key) else {
            plan.fetches.push(Planned {
                scanned,
                target: Target::NewItem,
                number: 1,
                offset: 0,
                length: scanned.length,
                cut: scanned.lines,
            });
            continue;
        };
        let (item, current) = (&archived.item, &archived.current);
        let as_seen = (scanned.length, &scanned.stamp) == (item.seen_length, &item.seen_stamp);
        let was_gone = item.gone_since.is_some();
        if current.damaged {
            plan.fetches
                .push(next_version(scanned, &archived, Reason::Refetched));
        } else if scanned.export_version != current.export_version {
            plan.fetches
                .push(next_version(scanned, &archived, Reason::Replaced));
        } else if as_seen && scanned.length == current.length {
            if was_gone {
                plan.seen.push((item.id, scanned));
            } else {
                plan.unchanged += 1;
            }
        } else if scanned.length < current.length {
            let reason = if was_gone {
                Reason::Replaced
            } else {
                Reason::Shrank
            };
            plan.fetches.push(next_version(scanned, &archived, reason));
        } else {
            undecided.push((scanned, archived, as_seen));
        }
    }
    if whole {
        plan.gone = archived
            .into_values()
            .filter(|archived| archived.item.gone_since.is_none())
            .map(|archived| archived.item.id)
            .collect();
    }

    /* An empty version's prefix is nothing, which needs no asking. */
    let requests: Vec<_> = undecided
        .iter()
        .filter(|(_, archived, _)| archived.current.length > 0)
        .map(|(scanned, archived, _)| ItemPrefix {
            key: scanned.key.clone(),
            length: archived.current.length,
        })
        .collect();
    let mut hashes = if requests.is_empty() {
        Vec::new()
    } else {
        source.hash_prefixes(&requests)?
    }
    .into_iter();
    for (scanned, archived, as_seen) in undecided {
        let current = &archived.current;
        let prefix = if current.length > 0 {
            /* A provider that answers fewer requests than it was sent has not hashed this. */
            hashes
                .next()
                .unwrap_or(Err(RangeFailure::Missing))
                .map(|hashed| hashed.hash)
        } else {
            Ok(current.hash)
        };
        match prefix {
            Err(RangeFailure::Missing | RangeFailure::Short { .. }) => plan.deferred += 1,
            Err(failure @ RangeFailure::Unreadable(_)) => plan.failed.push(Failure {
                key: scanned.key.clone(),
                reason: failure.to_string(),
            }),
            Ok(prefix) if prefix != current.hash => {
                let reason = if archived.item.gone_since.is_some() {
                    Reason::Replaced
                } else {
                    Reason::Rewritten
                };
                plan.fetches.push(next_version(scanned, &archived, reason));
            }
            Ok(_) if scanned.length == current.length => {
                plan.seen.push((archived.item.id, scanned));
            }
            Ok(_) => plan.fetches.push(Planned {
                scanned,
                target: Target::Append {
                    item: archived.item.id,
                    version: current.id,
                },
                number: current.number,
                offset: current.length,
                length: scanned.length - current.length,
                /* As last seen: nothing is writing it, so its unfinished last line is taken. */
                cut: scanned.lines && !as_seen,
            }),
        }
    }
    Ok(plan)
}

/// The whole source as the version after `archived`'s newest.
fn next_version<'a>(scanned: &'a ScannedItem, archived: &Archived, reason: Reason) -> Planned<'a> {
    let number = archived.current.number + 1;
    Planned {
        scanned,
        target: Target::NewVersion {
            item: archived.item.id,
            number,
            reason,
        },
        number,
        offset: 0,
        length: scanned.length,
        cut: scanned.lines,
    }
}
