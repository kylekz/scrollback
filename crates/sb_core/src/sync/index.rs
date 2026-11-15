//! Index (PLAN §5, step 5; §7.5): the sessions whose items changed are parsed by their
//! provider and indexed by the one function that serves every agent
//! ([`crate::transcript::index`]), and each is then filed (PLAN §8).
//!
//! Which sessions those are is read from the catalogue, never remembered: a session is indexed
//! when it has no row, when its row was made by another version of its provider's parser or of
//! the index, and when its row was made from other versions of its items than are read now. So
//! a pass that stops before or while it indexes leaves sessions that the next pass, or
//! [`index`] at a start, takes up, and a new parser reaches everything already archived.
//!
//! Sessions are indexed one at a time, each from its start with every subagent read, and each
//! row is committed on its own. Nothing parsed is kept between two sessions, so the memory an
//! index takes is that of its largest session's transcript model (PLAN §7.5), and no catalogue
//! transaction is open while a session is parsed.
//!
//! This is also where a version is held back for how it parses (PLAN §4.2), since only here
//! are transcripts parsed: [`parses_worse`].

use std::any::Any;
use std::cmp::Reverse;
use std::panic::{self, AssertUnwindSafe};

use toolbox_process::CancelToken;

use super::{Progress, SyncError};
use crate::archive::{Archive, ArchivedSession};
use crate::catalogue::{Derived, Reading, SessionId, fingerprint};
use crate::location::LocationId;
use crate::provider::Provider;
use crate::transcript::{self, Conversation, Ending};

/// What an index did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexReport {
    /// The sessions parsed and indexed, those of `failed` included.
    pub sessions: usize,
    /// The sessions whose provider could not parse them, each with why. Their rows say so
    /// too, and they are tried again when their items or the parser change.
    pub failed: Vec<(SessionId, String)>,
    /// The sessions the rules moved into a folder.
    pub filed: usize,
    /// The versions newly held back because their transcript no longer parses where the copy
    /// being read does.
    pub held_back: usize,
}

/**
 * Indexes every session of `providers`' agents whose index is out of date (the module docs),
 * at `location` or, with `None`, at every location: the sessions not indexed yet first, then
 * the newest (PLAN §7.5). A pass ends with this for its location and provider; a start runs it
 * for all, which is how a new version of a parser or of the index reaches what is archived.
 *
 * `progress` hears how many sessions and bytes there are to index, and how far it is after
 * each session. `cancel` stops it before the next session, with [`SyncError::Cancelled`]; what
 * was indexed stays, and the rest is still out of date for the next run.
 *
 * Two indexes at once (a start's beside a pass's) do no harm, only the same work twice: a row
 * says what it was made from, so one made from items that have changed since is out of date
 * for the next run.
 */
pub fn index(
    archive: &Archive,
    location: Option<&LocationId>,
    providers: &[Box<dyn Provider + '_>],
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&Progress),
) -> Result<IndexReport, SyncError> {
    if cancel.is_cancelled() {
        return Err(SyncError::Cancelled);
    }
    let mut stale = Vec::new();
    for provider in providers {
        let sessions = archive.catalogue().stale(
            location,
            provider.id().as_str(),
            provider.parser_version(),
        )?;
        stale.extend(sessions.into_iter().map(|stale| (provider.as_ref(), stale)));
    }
    stale.sort_by_key(|(_, stale)| Reverse(stale.recency));
    progress(&Progress::Indexing {
        sessions: stale.len(),
        bytes: stale.iter().map(|(_, stale)| stale.bytes).sum(),
    });
    let mut report = IndexReport::default();
    let mut bytes = 0;
    for (provider, stale) in stale {
        if cancel.is_cancelled() {
            return Err(SyncError::Cancelled);
        }
        session(archive, provider, &stale.id, &mut report)?;
        report.sessions += 1;
        bytes += stale.bytes;
        progress(&Progress::Indexed {
            sessions: report.sessions,
            bytes,
        });
    }
    Ok(report)
}

/// Parses and indexes one session, and records what came of it.
fn session(
    archive: &Archive,
    provider: &dyn Provider,
    id: &SessionId,
    report: &mut IndexReport,
) -> Result<(), SyncError> {
    let catalogue = archive.catalogue();
    let mut readings = catalogue.session_items(id)?;
    let mut held_back = false;
    /* A provider's parser doesn't fail (PLAN §7.1), so what stops it on a session is a fault
    in it. That is caught here, for one session's fault to be that session's alone. */
    let parsed = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut transcript = provider.parse(&ArchivedSession::of(archive, &readings));
        if let Some(held) = hold(
            archive,
            provider,
            &readings,
            transcript.conversation.as_ref(),
        )? {
            /* The main transcript is read at another version now. */
            held_back = held;
            readings = catalogue.session_items(id)?;
            transcript = provider.parse(&ArchivedSession::of(archive, &readings));
        }
        transcript.read_subagents(provider, &ArchivedSession::of(archive, &readings));
        let own = transcript.conversation.as_ref();
        let main_item = own
            .and_then(|own| main(&readings, own))
            .map(|main| main.item);
        Ok::<_, SyncError>((
            main_item,
            transcript::index(&transcript),
            own.map(|own| own.ending.clone()),
        ))
    }));
    let from = fingerprint(&readings);
    let parser = provider.parser_version();
    match parsed {
        Ok(derived) => {
            let (main_item, index, ending) = derived?;
            let derived = Derived {
                main_item,
                index: &index,
                ending: ending.as_ref(),
            };
            report.filed += usize::from(catalogue.record_session(id, parser, from, Ok(derived))?);
        }
        Err(fault) => {
            let reason = format!("its parser stopped: {}", said(fault.as_ref()));
            catalogue.record_session(id, parser, from, Err(&reason))?;
            report.failed.push((id.clone(), reason));
        }
    }
    report.held_back += usize::from(held_back);
    Ok(())
}

/// What a panic said.
fn said(fault: &(dyn Any + Send)) -> &str {
    fault
        .downcast_ref::<String>()
        .map(String::as_str)
        .or(fault.downcast_ref::<&str>().copied())
        .unwrap_or("it did not say why")
}

/// The item `own`, a session's own conversation, was parsed from.
fn main<'a>(readings: &'a [Reading], own: &Conversation) -> Option<&'a Reading> {
    readings.iter().find(|reading| *reading.key == *own.item)
}

/**
 * Decides whether the newest version of the session's main transcript is held back for how it
 * parses (PLAN §4.2), from `own`, the session's own conversation as `readings` give it. The
 * newest version is parsed beside the copy that is read without it, which is the newest
 * version before it that is not held back. A version held back only for being shorter is a
 * loss already and is not parsed for this. Returns whether the version is held back for how
 * it parses, when that is not as it was.
 */
fn hold(
    archive: &Archive,
    provider: &dyn Provider,
    readings: &[Reading],
    own: Option<&Conversation>,
) -> Result<Option<bool>, SyncError> {
    let Some(main) = own
        .and_then(|own| main(readings, own))
        .filter(|main| main.newest_number > 1)
    else {
        return Ok(None);
    };
    let catalogue = archive.catalogue();
    let versions = catalogue.versions(main.item)?;
    let Some((newest, reading)) = versions.split_last().and_then(|(newest, older)| {
        let reading = older.iter().rev().find(|version| !version.held_back)?;
        (!newest.held_back || newest.held_by_parse).then_some((newest, reading))
    }) else {
        return Ok(None);
    };
    let parsed = |version| {
        provider
            .parse(&ArchivedSession::of(archive, readings).at(&main.key, version))
            .conversation
    };
    let worse = parsed(newest)
        .zip(parsed(reading))
        .is_some_and(|(newest, reading)| parses_worse(&newest, &reading));
    if worse == newest.held_by_parse {
        return Ok(None);
    }
    catalogue.hold_for_parse(newest.id, worse)?;
    Ok(Some(worse))
}

/**
 * Whether a transcript "no longer parses" in its version `newer` where it did in `reading`,
 * the copy being read (PLAN §4.2): it can't be read to its end where the reading copy can, or
 * it shows fewer blocks than the reading copy does. What an agent appends or rewrites leaves
 * at least as many; a source that was emptied, cut short or replaced by something that is
 * not a transcript leaves fewer, however long it is.
 */
fn parses_worse(newer: &Conversation, reading: &Conversation) -> bool {
    let unreadable =
        |conversation: &Conversation| matches!(conversation.ending, Ending::Unreadable(_));
    let shown = |conversation: &Conversation| -> usize {
        conversation
            .turns()
            .iter()
            .map(|turn| turn.blocks.len())
            .sum()
    };
    (unreadable(newer) && !unreadable(reading)) || shown(newer) < shown(reading)
}
