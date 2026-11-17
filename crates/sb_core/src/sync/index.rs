//! Index (PLAN §5, step 5; §7.5): sessions are parsed by their provider and indexed by the one
//! function that serves every agent ([`crate::transcript::index`]), and each is then filed
//! (PLAN §8).
//!
//! Which sessions are to be indexed is read from the catalogue, never remembered. A session's
//! row says what it was made from (a fingerprint of the session's items as they were read) and
//! by which versions of the parser and of the index.
//!
//! - **A pass** ends by indexing the sessions of its location and provider whose items are no
//!   longer what their row was made from, or that have no row. So a pass that stops before or
//!   while it indexes leaves sessions that the next pass takes up.
//! - **[`index`]**, at a start and in the background, indexes those and every session whose
//!   row was made by another version of the parser or of the index, which is how a new parser
//!   reaches everything already archived. It first has each provider say again what every
//!   archived item is, when its parser's version is not the one that last said.
//! - **[`index_session`]** indexes one session at once: after the owner chose a version to
//!   read, say.
//!
//! An index is two steps. The first makes each session's row, so a first sync's sessions are
//! in the library as soon as they can be; the second parses each again and makes its text
//! searchable ([`crate::search`]), the sessions whose text is not of their row as it is now,
//! the newest first. How many are left is [`crate::archive::Archive::unsearchable`].
//!
//! Sessions are indexed one at a time, each from its start with every subagent read, and each
//! row is committed on its own. Nothing parsed is kept between two sessions, so the memory an
//! index takes is that of its largest session's transcript model (PLAN §7.5), and no catalogue
//! transaction is open while a session is parsed.
//!
//! This is also where a version is held back for how it parses (PLAN §4.2), since only here
//! are transcripts parsed: [`parses_worse`].

use std::any::Any;
use std::borrow::Cow;
use std::cmp::Reverse;
use std::panic::{self, AssertUnwindSafe};

use toolbox_process::CancelToken;

use super::{Progress, SyncError};
use crate::archive::{Archive, ArchivedSession};
use crate::catalogue::{Derived, Held, Indexed, Reading, SessionId, fingerprint};
use crate::location::LocationId;
use crate::provider::{ItemKind, Provider};
use crate::search::Text;
use crate::transcript::{self, Content, Conversation, Ending, SessionIndex, Subagent, Transcript};

/// What an index did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexReport {
    /// The sessions it tried to parse and index, those of `failed` included.
    pub sessions: usize,
    /// The sessions it could not parse, each with why: the parser stopped, or the archive
    /// could not be read. Their rows say so too.
    pub failed: Vec<(SessionId, String)>,
    /// The sessions the rules moved into a folder.
    pub filed: usize,
    /// The versions newly held back because their transcript no longer parses where the copy
    /// being read does.
    pub held_back: usize,
    /// The sessions whose text was made searchable.
    pub searchable: usize,
}

/**
 * Indexes every session of `providers`' agents whose index is out of date (the module docs),
 * at `location` or, with `None`, at every location: the sessions not indexed yet first, then
 * the newest (PLAN §7.5). This is the index of a start, and the one that takes a new version
 * of a parser or of the index to what is archived.
 *
 * `progress` hears how many sessions and bytes there are to index, and how far it is after
 * each session. `cancel` stops it before the next session, with [`SyncError::Cancelled`]; what
 * was indexed stays, and the rest is still out of date for the next run.
 *
 * Two indexes at once (this one beside a pass's) do no harm, only the same work twice: a row
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
    let providers: Vec<&dyn Provider> = providers.iter().map(Box::as_ref).collect();
    run(archive, location, &providers, true, cancel, progress)
}

/// Indexes the sessions of `provider` at `location` whose items changed: what a pass ends with.
pub(super) fn changed(
    archive: &Archive,
    location: &LocationId,
    provider: &dyn Provider,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&Progress),
) -> Result<IndexReport, SyncError> {
    run(
        archive,
        Some(location),
        &[provider],
        false,
        cancel,
        progress,
    )
}

/// Indexes the one session `id`, which is `provider`'s, whatever its row says: at once, for
/// the row to follow what the owner chose to read ([`crate::catalogue::Catalogue`]).
pub fn index_session(
    archive: &Archive,
    provider: &dyn Provider,
    id: &SessionId,
) -> Result<IndexReport, SyncError> {
    if provider.id().as_str() != id.provider {
        return Err(SyncError::NotItsProvider {
            session: id.provider.clone(),
            provider: provider.id(),
        });
    }
    let mut report = IndexReport::default();
    session(archive, provider, id, &mut report)?;
    if let Some(made) = archive.catalogue().made_of(id)? {
        let texts: Vec<Text> = text(archive, provider, id.clone(), made)?
            .into_iter()
            .collect();
        report.searchable += record(archive, &texts)?;
    }
    Ok(report)
}

/// Indexes what is out of date; with `backlog`, also what another version of a parser or of
/// the index made.
fn run(
    archive: &Archive,
    location: Option<&LocationId>,
    providers: &[&dyn Provider],
    backlog: bool,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&Progress),
) -> Result<IndexReport, SyncError> {
    if cancel.is_cancelled() {
        return Err(SyncError::Cancelled);
    }
    let catalogue = archive.catalogue();
    let mut stale = Vec::new();
    for provider in providers {
        let (id, parser) = (provider.id(), provider.parser_version());
        if backlog {
            catalogue.reclassify(location, id.as_str(), parser, &|key| provider.classify(key))?;
        }
        copy_records(archive, location, *provider)?;
        let (sessions, gone) = catalogue.stale(location, id.as_str(), parser, backlog)?;
        catalogue.forget_sessions(&gone)?;
        stale.extend(sessions.into_iter().map(|stale| (*provider, stale)));
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
        bytes += stale.bytes;
        progress(&Progress::Indexed {
            sessions: report.sessions,
            bytes,
        });
    }
    searchable(archive, location, providers, cancel, progress, &mut report)?;
    /* What the index deleted for good (PLAN §4.5): sessions of conversations the owner
    deleted, and what they left that is no session's. */
    if catalogue.pending_removals()? > 0 {
        archive.start_removals(false);
    }
    /* A log a read held when a delete for good was to empty it is emptied now. */
    catalogue.retry_empty_log();
    if let Some(search) = archive.search_index_if_open() {
        search.retry_empty_log();
    }
    Ok(report)
}

/**
 * Makes again what `provider`'s records at `location` (or at every location) say of where
 * copies ran, from its archived register items (PLAN §7.7). The archive being unreadable, or
 * the provider stopping on them, leaves what was recorded before; neither fails the index.
 */
fn copy_records(
    archive: &Archive,
    location: Option<&LocationId>,
    provider: &dyn Provider,
) -> Result<(), SyncError> {
    let catalogue = archive.catalogue();
    let locations = match location {
        Some(location) => vec![location.clone()],
        None => catalogue
            .locations()?
            .into_iter()
            .map(|location| location.id)
            .collect(),
    };
    let id = provider.id();
    for location in locations {
        let readings = catalogue.register_items(&location, id.as_str())?;
        let read = panic::catch_unwind(AssertUnwindSafe(|| {
            let items = ArchivedSession::of(archive, &readings);
            let records = provider.copy_records(&items);
            items.fault().is_none().then_some(records)
        }));
        match read {
            Ok(Some(records)) => catalogue.record_copies(&location, id.as_str(), &records)?,
            Ok(None) => log::warn!("{id}'s records of copies could not be read from the archive"),
            Err(fault) => log::warn!(
                "{id} stopped on its records of copies: {}",
                said(fault.as_ref())
            ),
        }
    }
    Ok(())
}

/// How much text the second step of an index writes at a time: the sessions parsed for it,
/// up to this many bytes of their text, or [`TEXTS`] sessions.
const TEXT_BYTES: usize = 16 * 1024 * 1024;
const TEXTS: usize = 256;

/**
 * The second step of an index (the module docs): makes the text of each session of
 * `providers` at `location` (or at every location) searchable, where it is not of the
 * session's row as it is now, the session that did something last first, some sessions at a
 * time; and takes out the text of each whose row went. The search index is opened first: one
 * made anew (gone, unreadable, or of another layout, as an older release left it) marks every
 * session not searchable, so the first index after a start makes it all again rather than the
 * first search finding it empty. It stays open, so later indexes only read the catalogue's
 * index of what is to do when there is nothing.
 */
fn searchable(
    archive: &Archive,
    location: Option<&LocationId>,
    providers: &[&dyn Provider],
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&Progress),
    report: &mut IndexReport,
) -> Result<(), SyncError> {
    let catalogue = archive.catalogue();
    /* Search that is not available is left to the next index; the library is not. */
    let Ok(search) = archive.search_index() else {
        return Ok(());
    };
    let gone = catalogue.text_gone()?;
    let mut rows = Vec::new();
    for provider in providers {
        rows.extend(
            catalogue
                .unsearchable(location, Some(provider.id().as_str()))?
                .into_iter()
                .map(|row| (*provider, row)),
        );
    }
    if gone.is_empty() && rows.is_empty() {
        return Ok(());
    }
    if !gone.is_empty() {
        search.forget(&gone)?;
        catalogue.text_forgotten(&gone)?;
    }
    if rows.is_empty() {
        return Ok(());
    }
    /* Text made of a row as it is now, before its flag said so (the index emptied and built
    again, or stopped between the two): it stays. */
    let docs = search.docs()?;
    let (kept, rows): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|(_, (id, made))| docs.get(id) == Some(made));
    catalogue.set_searchable(&kept.into_iter().map(|(_, row)| row).collect::<Vec<_>>())?;
    let mut batch: Vec<Text> = Vec::new();
    for (provider, (id, made)) in rows {
        if cancel.is_cancelled() {
            return Err(SyncError::Cancelled);
        }
        batch.extend(text(archive, provider, id, made)?);
        if batch.len() >= TEXTS || batch.iter().map(Text::bytes).sum::<usize>() >= TEXT_BYTES {
            report.searchable += record(archive, &std::mem::take(&mut batch))?;
            progress(&Progress::Searchable {
                sessions: report.searchable,
            });
        }
    }
    if !batch.is_empty() {
        report.searchable += record(archive, &batch)?;
        progress(&Progress::Searchable {
            sessions: report.searchable,
        });
    }
    Ok(())
}

/// Writes `texts`, of those sessions whose rows are still what the texts were made of, and
/// says so of their rows. Returns how many were written.
fn record(archive: &Archive, texts: &[Text]) -> Result<usize, SyncError> {
    if texts.is_empty() {
        return Ok(0);
    }
    let catalogue = archive.catalogue();
    let written = archive.search_index()?.record(texts, |text| {
        Ok(catalogue.made_of(&text.id)?.as_deref() == Some(&*text.made))
    })?;
    catalogue.set_searchable(&written)?;
    Ok(written.len())
}

/**
 * The text of the session `id`, as of its row, which was made from what `made` says
 * ([`crate::catalogue::Catalogue::made`]). There is none of a session whose items are no
 * longer what its row was made from (the next index makes its row again first), nor of one
 * the archive could not be read for (the next index tries again); a session its parser stops
 * on has its title alone.
 */
fn text(
    archive: &Archive,
    provider: &dyn Provider,
    id: SessionId,
    made: String,
) -> Result<Option<Text>, SyncError> {
    let catalogue = archive.catalogue();
    let readings = catalogue.session_items(&id)?;
    if made.split(' ').next() != Some(fingerprint(&readings).to_hex().as_str()) {
        return Ok(None);
    }
    let parsed = panic::catch_unwind(AssertUnwindSafe(|| {
        let items = ArchivedSession::of(archive, &readings);
        let mut transcript = provider.parse(&items);
        transcript.read_subagents(provider, &items);
        items
            .fault()
            .is_none()
            .then(|| transcript::index(&transcript))
    }));
    let index = match parsed {
        Ok(Some(index)) => index,
        Ok(None) => return Ok(None),
        Err(_) => SessionIndex {
            title: catalogue.session(&id)?.and_then(|session| session.title),
            ..SessionIndex::default()
        },
    };
    Ok(Some(Text {
        id,
        made,
        title: index.title,
        spans: index.spans,
    }))
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
    if !readings.iter().any(|reading| reading.kind.is_transcript()) {
        /* Its items are another session's now, or no conversation's. */
        return Ok(catalogue.forget_sessions(std::slice::from_ref(id))?);
    }
    report.sessions += 1;
    /* A provider's parser doesn't fail (PLAN §7.1), so what stops it on a session is a fault
    in it. That is caught here, for one session's fault to be that session's alone. */
    let parsed = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut items = ArchivedSession::of(archive, &readings);
        let mut transcript = provider.parse(&items);
        if items.fault().is_none() {
            let newly_held = hold(archive, provider, &readings, &transcript)?;
            if let Some(newly_held) = newly_held {
                /* Some transcript is read at another version now. */
                report.held_back += newly_held;
                readings = catalogue.session_items(id)?;
                items = ArchivedSession::of(archive, &readings);
                transcript = provider.parse(&items);
            }
        }
        transcript.read_subagents(provider, &items);
        let own = transcript.conversation.as_ref();
        Ok::<_, SyncError>(match items.fault() {
            Some(fault) => Err(fault),
            None => Ok((
                own.and_then(|own| {
                    readings
                        .iter()
                        .position(|reading| *reading.key == *own.item)
                }),
                transcript::index(&transcript),
                own.map(|own| own.ending.clone()),
            )),
        })
    }));
    let from = fingerprint(&readings);
    let parser = provider.parser_version();
    let copy = provider.copy_of(&id.key);
    let copy = copy.as_ref();
    let fault = match parsed {
        Ok(read) => match read? {
            Ok((main, index, ending)) => {
                let derived = Derived {
                    main: main.map(|main| &readings[main]),
                    index: &index,
                    ending: ending.as_ref(),
                    held_back: readings.iter().any(|reading| {
                        reading.kind.is_transcript() && reading.newest_held.is_some()
                    }),
                };
                let filed =
                    catalogue.record_session(id, parser, from, Indexed::Parsed(derived), copy)?;
                report.filed += usize::from(filed);
                return Ok(());
            }
            Err(fault) => {
                let reason = format!("the archive could not be read: {fault}");
                catalogue.record_session(id, parser, from, Indexed::NotRead(&reason), copy)?;
                reason
            }
        },
        Err(fault) => {
            let reason = format!("its parser stopped: {}", said(fault.as_ref()));
            catalogue.record_session(id, parser, from, Indexed::Faulted(&reason), copy)?;
            reason
        }
    };
    report.failed.push((id.clone(), fault));
    Ok(())
}

/// Whether reading a conversation's item failed part of the way.
fn unreadable(conversation: &Conversation) -> bool {
    matches!(conversation.ending, Ending::Unreadable(_))
}

/// What a panic said.
fn said(fault: &(dyn Any + Send)) -> &str {
    fault
        .downcast_ref::<String>()
        .map(String::as_str)
        .or(fault.downcast_ref::<&str>().copied())
        .unwrap_or("it did not say why")
}

/**
 * Decides, for each of the session's transcripts that has more than one version, whether its
 * newest is held back (PLAN §4.2). `transcript` is the session as `readings` give it, with no
 * subagent read.
 *
 * The newest version is measured beside the copy that is read without it, which is the newest
 * version before it that is not held back. It is held back when it is shorter than that copy;
 * and when it is not, when it parses worse ([`parses_worse`]), for which both are parsed. A
 * version the owner accepted is never held back. Nothing is decided from a read the
 * operating system failed, nor from stored bytes that are damaged: what can't be read of the
 * archive says nothing of what the source held, and is for verify to find and the next sync
 * to fetch again (PLAN §4.4). Returns how many versions are newly held back for how they
 * parse, when any hold is not as it was.
 */
fn hold(
    archive: &Archive,
    provider: &dyn Provider,
    readings: &[Reading],
    transcript: &Transcript,
) -> Result<Option<usize>, SyncError> {
    let catalogue = archive.catalogue();
    let own = transcript.conversation.as_ref();
    let mut newly_held = None;
    for reading in readings {
        if !reading.kind.is_transcript() || reading.newest_number == 1 || reading.newest_accepted {
            continue;
        }
        let Some(copy) = catalogue.copy_before(reading.item, reading.newest_number)? else {
            continue;
        };
        /* The item's conversation as it is in a version; `None` when the provider reads none
        from it, or a read failed for a reason that is not the stored bytes. */
        let conversation = |version: i64, length: u64| {
            let own = own.filter(|own| *own.item == *reading.key);
            if let Some(own) = own.filter(|_| reading.version == version) {
                return Some(Cow::Borrowed(own));
            }
            let items = ArchivedSession::of(archive, readings).at(&reading.key, version, length);
            let read = match (own, reading.kind) {
                (Some(_), _) => provider.parse(&items).conversation,
                (None, ItemKind::Subagent) => match provider.subagent(&items, &reading.key) {
                    Subagent::Read { conversation, .. } => Some(*conversation),
                    Subagent::Unread { .. } => None,
                },
                (None, _) => None,
            };
            read.filter(|_| items.fault().is_none()).map(Cow::Owned)
        };
        let held = if reading.newest_length < copy.length {
            Some(Held::Shorter)
        } else {
            let Some((newer, copy)) = conversation(reading.newest, reading.newest_length)
                .zip(conversation(copy.id, copy.length))
                .filter(|(newer, copy)| !unreadable(newer) && !unreadable(copy))
            else {
                continue;
            };
            parses_worse(&newer, &copy).then_some(Held::ParsesWorse)
        };
        if catalogue.set_hold(reading.newest, reading.newest_length, held)? {
            *newly_held.get_or_insert(0) += usize::from(held == Some(Held::ParsesWorse));
        }
    }
    Ok(newly_held)
}

/**
 * Whether a transcript "no longer parses" in its version `newer` where it did in `reading`,
 * the copy being read (PLAN §4.2): it holds fewer blocks the provider recognised, over every
 * branch. What an agent appends only ever adds blocks, whichever branch they are on, so an
 * append is never held back for this; a source that was emptied, cut short or replaced by
 * something that is not a transcript holds fewer, however long it is and however many records
 * it has that the provider doesn't know. How either ends is not part of it.
 */
fn parses_worse(newer: &Conversation, reading: &Conversation) -> bool {
    let recognised = |conversation: &Conversation| {
        conversation
            .blocks()
            .iter()
            .filter(|block| !matches!(block.content, Content::Unrecognised(_)))
            .count()
    };
    recognised(newer) < recognised(reading)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{Builder, Encoding, Stored, Unrecognised};

    fn reply() -> Content {
        Content::Reply {
            text: "Said.".to_string(),
        }
    }

    fn unrecognised() -> Content {
        Content::Unrecognised(Unrecognised {
            kind: "hologram".to_string(),
            misfit: None,
            raw: Stored {
                item: "item".into(),
                offset: 0,
                length: 0,
                encoding: Encoding::Plain,
            },
        })
    }

    /// A conversation of `shown` blocks on the shown branch and `hidden` blocks off it.
    fn conversation(shown: Vec<Content>, hidden: Vec<Content>, ending: Ending) -> Conversation {
        let mut builder = Builder::new("item".into());
        for content in shown {
            builder.show(None, None, None, content);
        }
        for content in hidden {
            builder.hide(None, None, None, content);
        }
        builder.finish(ending)
    }

    fn of(blocks: usize) -> Conversation {
        conversation(vec![reply(); blocks], Vec::new(), Ending::Whole)
    }

    #[test]
    fn a_version_parses_worse_with_fewer_blocks_the_provider_recognised() {
        assert!(parses_worse(&of(2), &of(3)));
        assert!(!parses_worse(&of(3), &of(3)));
        assert!(!parses_worse(&of(4), &of(3)));
        assert!(!parses_worse(&of(0), &of(0)));

        /* Records the provider doesn't know are no conversation, however many there are. */
        let unknown = conversation(
            [vec![unrecognised(); 40], vec![reply()]].concat(),
            Vec::new(),
            Ending::Whole,
        );
        assert!(parses_worse(&unknown, &of(2)));
        assert!(!parses_worse(&unknown, &of(1)));

        /* A block on a branch that isn't shown is still there. */
        let rewound = conversation(vec![reply()], vec![reply(); 3], Ending::Whole);
        assert!(!parses_worse(&rewound, &of(4)));
        assert!(parses_worse(&rewound, &of(5)));
    }

    #[test]
    fn how_a_version_ends_is_not_how_it_parses() {
        let unreadable = |blocks| {
            conversation(
                vec![reply(); blocks],
                Vec::new(),
                Ending::Unreadable("damaged".to_string()),
            )
        };
        assert!(!parses_worse(&unreadable(5), &of(3)));
        assert!(!parses_worse(&unreadable(3), &of(3)));
        assert!(parses_worse(&unreadable(2), &of(3)));
        assert!(!parses_worse(&of(3), &unreadable(3)));
        /* A version that ends inside a record is one still being written. */
        let unfinished = conversation(vec![reply(); 3], Vec::new(), Ending::MidRecord);
        assert!(!parses_worse(&unfinished, &of(3)));
    }
}
