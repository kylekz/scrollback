//! Plan (PLAN §5, step 2): what a pass does with each item, from the scan and the catalogue
//! alone (PLAN §4.2). With the archived length `L` of the item's newest version, the source's
//! length `S`, and "as last seen" meaning the length and stamp a pass last recorded:
//!
//! | Source | Planned | If the source's start is not what is archived |
//! |---|---|---|
//! | Not in the catalogue | Fetch it whole as version 1 | |
//! | As last seen, `S == L` | Nothing: unchanged | |
//! | `S == L`, not as last seen | Fetch nothing, compare | Fetch it whole as the next version |
//! | `S > L` | Fetch `[L, S)` into the version | Fetch it whole as the next version |
//! | `S < L`, its newest version damaged, or its export format changed | Fetch it whole as the next version | |
//! | In the catalogue, not in the scan | Mark it gone; keep everything | |
//!
//! Whether the source's start is what is archived is not decided here. Every planned range is
//! proven after it is fetched ([`super::store`]), and that proof is the comparison.
//!
//! An item made of lines is stored up to its last newline, since its writer may be mid-record
//! (PLAN §4.2). When it is still as last seen on a later pass, nothing is writing it, and the
//! rest is fetched as it is: otherwise the bytes after a final newline that never comes would
//! never be archived.

use std::collections::HashMap;

use crate::catalogue::{Archived, Reason, Target};
use crate::hash::Hash;
use crate::provider::ScannedItem;

/// A range of one item to fetch, prove and store.
pub(super) struct Planned<'a> {
    pub scanned: &'a ScannedItem,
    pub target: Target,
    /// The number of the version the range goes into.
    pub number: u32,
    /// Where the range starts, which is the version's length before it.
    pub offset: u64,
    /// The version's hash before the range.
    pub base: Hash,
    /// How many bytes to fetch: the rest of the source. 0 when there is nothing to fetch and
    /// only the source's start to compare with what is archived.
    pub length: u64,
    /// Whether to store only up to the last newline of the range.
    pub cut: bool,
    /// Whether the source must end where the stored bytes do for them to be kept: so for an
    /// item that is not lines, and for lines taken with their unfinished end.
    pub whole: bool,
    /// What to fetch the source whole as, when its start turns out not to be what is archived.
    pub otherwise: Option<Target>,
}

impl<'a> Planned<'a> {
    /// The whole source as version `number`.
    pub(super) fn whole(scanned: &'a ScannedItem, target: Target, number: u32) -> Self {
        Self {
            scanned,
            target,
            number,
            offset: 0,
            base: Hash::of(b""),
            length: scanned.length,
            cut: scanned.lines,
            whole: !scanned.lines,
            otherwise: None,
        }
    }
}

pub(super) struct Plan<'a> {
    pub work: Vec<Planned<'a>>,
    /// Items that are as archived and whose row is not: ones that were gone and are back, and
    /// ones whose kind or session the provider now gives differently.
    pub seen: Vec<(i64, &'a ScannedItem)>,
    /// How many of `seen` were gone.
    pub back: usize,
    /// Items missing at the source that are not marked gone yet.
    pub gone: Vec<i64>,
    pub unchanged: usize,
}

/// Decides about each of `scanned`. `whole` says the scan saw everything the agent has: only
/// then is an archived item that is not in it missing at the source.
pub(super) fn plan<'a>(
    scanned: &'a [ScannedItem],
    whole: bool,
    archived: Vec<Archived>,
) -> Plan<'a> {
    let mut archived: HashMap<String, Archived> = archived
        .into_iter()
        .map(|archived| (archived.item.key.clone(), archived))
        .collect();
    let mut plan = Plan {
        work: Vec::new(),
        seen: Vec::new(),
        back: 0,
        gone: Vec::new(),
        unchanged: 0,
    };
    for scanned in scanned {
        let Some(Archived { item, current }) = archived.remove(&scanned.key) else {
            plan.work.push(Planned::whole(scanned, Target::NewItem, 1));
            continue;
        };
        let as_seen = (scanned.length, &scanned.stamp) == (item.seen_length, &item.seen_stamp);
        let was_gone = item.gone_since.is_some();
        let next = |reason| {
            let target = Target::NewVersion {
                item: item.id,
                reason,
            };
            Planned::whole(scanned, target, current.number + 1)
        };
        /* What it is, when the source is not the archived version's continuation. */
        let changed = if was_gone {
            Reason::Replaced
        } else {
            Reason::Rewritten
        };
        if current.damaged {
            plan.work.push(next(Reason::Refetched));
        } else if scanned.export_version != current.export_version {
            plan.work.push(next(Reason::Replaced));
        } else if scanned.length < current.length {
            plan.work.push(next(if was_gone {
                Reason::Replaced
            } else {
                Reason::Shrank
            }));
        } else if as_seen && scanned.length == current.length {
            if was_gone || (scanned.kind, &scanned.session) != (item.kind, &item.session) {
                plan.seen.push((item.id, scanned));
                plan.back += usize::from(was_gone);
            }
            plan.unchanged += usize::from(!was_gone);
        } else {
            plan.work.push(Planned {
                scanned,
                target: Target::Append {
                    item: item.id,
                    version: current.id,
                },
                number: current.number,
                offset: current.length,
                base: current.hash,
                length: scanned.length - current.length,
                /* As last seen: nothing is writing it, so its unfinished last line is taken. */
                cut: scanned.lines && !as_seen,
                whole: !scanned.lines || as_seen,
                otherwise: Some(next(changed).target),
            });
        }
    }
    if whole {
        plan.gone = archived
            .into_values()
            .filter(|archived| archived.item.gone_since.is_none())
            .map(|archived| archived.item.id)
            .collect();
    }
    plan
}
