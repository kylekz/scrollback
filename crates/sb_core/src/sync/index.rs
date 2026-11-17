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
//! - **[`index`]**, at a start and in the background, indexes those and the backlog: every
//!   session whose row was made by another version of the parser or of the index, which is how
//!   a new parser reaches everything already archived, and the text not yet of its rows. It
//!   first has each provider say again what every archived item is, when its parser's version
//!   is not the one that last said. One index takes the backlog at a time
//!   ([`Archive::backlog`]), so passes beside a start's index don't parse it again.
//! - **[`index_session`]** indexes one session at once: after the owner chose a version to
//!   read, say.
//!
//! An index is two steps. The first makes each session's row, so a first sync's sessions are
//! in the library as soon as they can be, and from the same parse its text, which is written
//! to search ([`crate::search`]) some sessions at a time. The second, the backlog's, makes
//! searchable the sessions whose text is still not of their row as it is now, the newest
//! first: those the first step had no text of (the search index made anew, or text an index
//! could not write), each parsed again. How many are left is
//! [`crate::archive::Archive::unsearchable`]. The library never waits on search: a search
//! index that fails is let go of for the rest of the step ([`without_search`]).
//!
//! Each step parses several sessions at once, on threads of its own ([`in_order`]), each
//! session from its start with every subagent read; what came of each is recorded on the
//! index's own thread in the order the sessions are taken in, each row committed on its own,
//! so the library has it at once. A parse keeps nothing but what its row and its text are made
//! of (of each span, what search keeps), and at most twice as many sessions as there are
//! threads are parsed and not yet recorded: the memory an index takes is that of its
//! [`workers`] largest sessions' transcript models and a batch of text (PLAN §7.5). However
//! many indexes run, no more than [`workers`] sessions are parsed at once ([`PARSING`]). No
//! catalogue transaction is open while a session is parsed.
//!
//! This is also where a version is held back for how it parses (PLAN §4.2), since only here
//! are transcripts parsed: [`parses_worse`].

use std::any::Any;
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock, PoisonError, mpsc};
use std::thread;

use toolbox_process::CancelToken;

use super::{Progress, SyncError};
use crate::archive::{Archive, ArchivedSession};
use crate::catalogue::{Derived, Held, Indexed, Reading, RowMade, SessionId, fingerprint, made};
use crate::location::LocationId;
use crate::provider::{ItemKind, Provider};
use crate::search::{self, Kept, Recorded, SearchError, SearchIndex, Text};
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
    /// The sessions it deleted for good, of conversations the owner deleted for good (PLAN
    /// §4.5): arriving from another location, or made again, they are not archived again.
    pub deleted: Vec<SessionId>,
    /// It left the backlog (PLAN §7.5), text not yet of its rows, to the index of every
    /// location: it was a pass's, or another index had the backlog.
    pub backlog_left: bool,
}

/**
 * Indexes every session of `providers`' agents whose index is out of date (the module docs),
 * at `location` or, with `None`, at every location: the sessions not indexed yet first, then
 * the newest (PLAN §7.5). This is the index of a start, and the one that takes a new version
 * of a parser or of the index to what is archived: the backlog, which one index takes at a
 * time ([`Archive::backlog`]). The index of every location waits for it; one of a location
 * leaves it to the index that has it, and indexes what changed, as a pass does.
 *
 * `progress` hears how many sessions and bytes there are to index, and how far it is after
 * each session; then the same of the sessions its second step makes searchable. `cancel` stops
 * it before the next session, with [`SyncError::Cancelled`]: what was indexed stays, what was
 * parsed and not yet recorded is dropped, and the rest is still out of date for the next run.
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
    let backlog = match (archive.backlog(false), location) {
        (Some(held), _) => Some(held),
        (None, Some(_)) => None,
        /* Another index has it: this one waits, and says so. */
        (None, None) => {
            progress(&Progress::Waiting);
            archive.backlog(true)
        }
    };
    run(
        archive,
        location,
        &providers,
        backlog.is_some(),
        cancel,
        progress,
    )
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
    let parsed = parse(archive, provider, id, false)?;
    let text = match record_parse(archive, provider, id, parsed, &mut report)? {
        Some(text) => Some(text),
        /* Of a row made without a parse, the text is read as the second step reads it. */
        None => match archive.catalogue().made_of(id)? {
            Some(made) => text(archive, provider, id.clone(), made)?,
            None => None,
        },
    };
    let search = archive.search_index()?;
    /* One session's, in the background: it waits for a compaction rather than leave it. */
    if let Recorded::Written(written) = record(archive, &search, &Vec::from_iter(text), true)? {
        report.searchable += written;
    }
    /* A session of a conversation the owner deleted is deleted for good by its index. */
    if archive.catalogue().pending_removals()? > 0 {
        archive.start_removals(false);
    }
    Ok(report)
}

/// Indexes what is out of date; with `backlog`, which is to have leave to take it
/// ([`Archive::backlog`]), also what another version of a parser or of the index made, and
/// the text not yet of its rows.
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
    let outdated = stale.iter().filter(|(_, stale)| stale.outdated).count();
    progress(&Progress::Indexing {
        sessions: stale.len(),
        bytes: stale.iter().map(|(_, stale)| stale.bytes).sum(),
        outdated,
    });
    let mut report = IndexReport::default();
    /* Opened first: one made anew says no row's text is searchable before any is made. The
    library never waits on search: without it, the text is left to the second step, or to the
    next index. */
    let search = archive.search_index().ok();
    let texts = Texts::new(archive, search.as_deref());
    let sessions = stale.len();
    let (mut done, mut bytes, mut batch) = (0, 0, Batch::default());
    let indexed = thread::scope(|scope| {
        let mut writer = texts.writer(scope);
        let indexed = in_order(
            stale,
            cancel,
            |(provider, stale)| {
                let parsed = parse(archive, provider, &stale.id, true);
                (provider, stale, parsed)
            },
            |(provider, stale, parsed)| {
                let text = record_parse(archive, provider, &stale.id, parsed?, &mut report)?;
                if let (true, Some(text)) = (texts.wanted(), text) {
                    batch.add(text);
                }
                done += 1;
                if batch.is_full() || done == sessions {
                    writer.write(std::mem::take(&mut batch))?;
                }
                bytes += stale.bytes;
                /* How far it is counts every session gone through, those found to be none
                any more too; the text written so far is as the writer says. */
                progress(&Progress::Indexed {
                    sessions: done,
                    bytes,
                    searchable: texts.written(),
                });
                Ok(())
            },
        );
        /* What was made is written, the index stopped or not. */
        if matches!(indexed, Ok(()) | Err(SyncError::Cancelled)) {
            writer.write(std::mem::take(&mut batch))?;
        }
        writer.end()?;
        indexed
    });
    report.searchable += texts.written();
    /* The text written after the last session was, said once it is (unless it was stopped,
    which is said next). */
    if sessions > 0 && !cancel.is_cancelled() {
        progress(&Progress::Indexed {
            sessions: done,
            bytes,
            searchable: report.searchable,
        });
    }
    let searched = texts.wanted();
    let search = search.filter(|_| searched);
    indexed?;
    if cancel.is_cancelled() {
        return Err(SyncError::Cancelled);
    }
    let made = if backlog {
        searchable(archive, location, providers, cancel, progress, &mut report)
    } else {
        /* The text of the rows it made is written; the rest is the backlog's, which is left
        when there is text an index could make: not when search is not to be had, nor for a
        row the archive could not be read for, nor of an agent not in hand. */
        let ids: Vec<&str> = providers.iter().map(|it| it.id().as_str()).collect();
        report.backlog_left = search.is_some() && catalogue.count_backlog(&ids)? > 0;
        progress(&Progress::Searching { sessions: 0 });
        forget_gone(archive)
    };
    if let Err(error) = made.and_then(|()| compact(archive, backlog, cancel, progress)) {
        without_search(error)?;
    }
    /* A log a read held when a delete for good was to empty it is emptied now: first, so the
    background removal started next doesn't find it being emptied. */
    catalogue.retry_empty_log();
    if let Some(search) = archive.search_index_if_open() {
        search.retry_empty_log();
    }
    /* What the index deleted for good (PLAN §4.5): sessions of conversations the owner
    deleted, and what they left that is no session's; and deleted text still to be erased from
    the files, which the background removal tries again. */
    if catalogue.pending_removals()? > 0 || archive.scrub_wanted()? {
        archive.start_removals(false);
    }
    Ok(report)
}

/**
 * Makes again what `provider`'s records at `location` (or at every location) say of where
 * copies ran, from its archived register items (PLAN §7.7), unless they are what they were
 * last read whole from. A record the archive can't read is left out and the rest are taken;
 * the provider stopping on them leaves what was recorded before; neither fails the index.
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
    /* The WSL locations a copy that ran at localhost over SSH may be of, each with the home
    its provider's root there says. */
    let distros: Vec<(LocationId, Option<String>)> = catalogue
        .wsl_roots(id.as_str())?
        .into_iter()
        .map(|(location, root)| {
            let home = root.and_then(|root| provider.home_of(&root));
            (location, home)
        })
        .collect();
    for location in locations {
        let readings = catalogue.register_items(&location, id.as_str())?;
        /* Records read whole from these very versions, with these distros, are not read
        again. */
        let made = format!(
            "{} {} {distros:?}",
            fingerprint(&readings).to_hex(),
            provider.parser_version()
        );
        if catalogue
            .copy_records_made(&location, id.as_str())?
            .as_deref()
            == Some(&made)
        {
            continue;
        }
        let read = panic::catch_unwind(AssertUnwindSafe(|| {
            let items = ArchivedSession::of(archive, &readings);
            let records = provider.copy_records(&items);
            (records, items.fault().is_none())
        }));
        match read {
            /* A record that could not be read is left out; the rest are taken, and what it
            said before is kept until it is read. */
            Ok((records, whole)) => {
                if !whole {
                    log::warn!("some of {id}'s records of copies could not be read");
                }
                let wsl: Vec<Option<LocationId>> = (records.iter())
                    .map(|record| distro_of(&distros, record.cwd.as_deref()))
                    .collect();
                catalogue.record_copies(&location, id.as_str(), &records, &wsl, whole, &made)?;
            }
            Err(fault) => log::warn!(
                "{id} stopped on its records of copies: {}",
                said(fault.as_ref())
            ),
        }
    }
    Ok(())
}

/**
 * The WSL location of `distros` (each with its home) a copy that started in `cwd` ran in, if
 * it ran in one over SSH at localhost (PLAN §7.7): the only one there is, else the one whose
 * home holds `cwd`, and none when none or several do.
 */
fn distro_of(distros: &[(LocationId, Option<String>)], cwd: Option<&str>) -> Option<LocationId> {
    if let [(only, _)] = distros {
        return Some(only.clone());
    }
    let cwd = cwd?;
    let mut holding = distros.iter().filter(|(_, home)| {
        home.as_deref()
            .is_some_and(|home| cwd == home || cwd.starts_with(&format!("{home}/")))
    });
    match (holding.next(), holding.next()) {
        (Some((location, _)), None) => Some(location.clone()),
        _ => None,
    }
}

/// How much text an index writes at a time: up to this many bytes of the sessions' text, or
/// [`TEXTS`] sessions. Small, so that what waits on a batch (rows, when text falls behind, and
/// search finding it) waits briefly: on a few thousand sessions whose text all changed, the
/// rows stood still for under a second at most, against nearly two with four times as much.
const TEXT_BYTES: usize = 4 * 1024 * 1024;
const TEXTS: usize = 64;

/// The text an index made and has yet to write, of sessions whose rows are as it was made of.
#[derive(Default)]
struct Batch {
    texts: Vec<Text>,
    bytes: usize,
}

impl Batch {
    fn add(&mut self, text: Text) {
        self.bytes += text.bytes();
        self.texts.push(text);
    }

    fn is_full(&self) -> bool {
        self.texts.len() >= TEXTS || self.bytes >= TEXT_BYTES
    }

    /// Writes what was added ([`record`]), waiting for a compaction, and empties it. Returns
    /// how many texts were written.
    fn write(&mut self, archive: &Archive, search: &SearchIndex) -> Result<usize, SyncError> {
        self.bytes = 0;
        match record(archive, search, &std::mem::take(&mut self.texts), true)? {
            Recorded::Written(written) => Ok(written),
            Recorded::Deferred => Ok(0),
        }
    }
}

/**
 * Where the first step's text goes ([`Texts::writer`]): written off the index's own thread,
 * so the rows never wait while a batch of text is, and the bar moves with the rows. Search
 * that fails is said in the log and let go of for the rest of the step, with what was made:
 * the backlog, or the next index, makes that text ([`without_search`]).
 */
struct Texts<'a> {
    archive: &'a Archive,
    search: Option<&'a SearchIndex>,
    /// Search is in hand, and has not failed.
    wanted: AtomicBool,
    /// How many sessions' text was written.
    written: AtomicUsize,
}

impl<'a> Texts<'a> {
    fn new(archive: &'a Archive, search: Option<&'a SearchIndex>) -> Self {
        Self {
            archive,
            search,
            wanted: AtomicBool::new(search.is_some()),
            written: AtomicUsize::new(0),
        }
    }

    /// Whether text is still to be made: there is search, and it has not failed.
    fn wanted(&self) -> bool {
        self.wanted.load(Ordering::Relaxed)
    }

    fn written(&self) -> usize {
        self.written.load(Ordering::Relaxed)
    }

    /// Writes `batch`, unless search failed: what is let go of then is said once, in the log.
    fn write(&self, batch: Batch) -> Result<(), SyncError> {
        let Some(search) = self.search.filter(|_| self.wanted()) else {
            return Ok(());
        };
        /* While the index is compacted the rows go on, and their text is the backlog's. */
        match record(self.archive, search, &batch.texts, false) {
            Ok(Recorded::Written(written)) => {
                self.written.fetch_add(written, Ordering::Relaxed);
                Ok(())
            }
            Ok(Recorded::Deferred) => Ok(()),
            Err(error) => {
                without_search(error)?;
                self.wanted.store(false, Ordering::Relaxed);
                Ok(())
            }
        }
    }

    /**
     * A thread of `scope` that writes what is handed to it, one batch at a time, with one
     * more waiting at most: past that, handing one over waits, so text that falls behind
     * holds no more than three batches. With no thread to be had, it is written where it is
     * handed over.
     */
    fn writer<'scope>(&'scope self, scope: &'scope thread::Scope<'scope, '_>) -> Writer<'scope> {
        let (send, receive) = mpsc::sync_channel::<Batch>(1);
        let thread = thread::Builder::new()
            .name("index-text".to_string())
            .spawn_scoped(scope, move || {
                for batch in receive {
                    self.write(batch)?;
                }
                Ok(())
            });
        match thread {
            Ok(thread) => Writer {
                texts: self,
                send: Some(send),
                thread: Some(thread),
            },
            Err(error) => {
                log::warn!("an index writes its text where it records: {error}");
                Writer {
                    texts: self,
                    send: None,
                    thread: None,
                }
            }
        }
    }
}

/// What hands the first step's text to its writer ([`Texts::writer`]).
struct Writer<'scope> {
    texts: &'scope Texts<'scope>,
    send: Option<mpsc::SyncSender<Batch>>,
    thread: Option<thread::ScopedJoinHandle<'scope, Result<(), SyncError>>>,
}

impl Writer<'_> {
    /// Hands `batch` over to be written, or writes it, with no thread. A writer that stopped
    /// is ended at once, and what stopped it is returned: nothing more is made for it.
    fn write(&mut self, batch: Batch) -> Result<(), SyncError> {
        if batch.texts.is_empty() {
            return Ok(());
        }
        match &self.send {
            Some(send) => match send.send(batch) {
                Ok(()) => Ok(()),
                Err(_) => self.join(),
            },
            None => self.texts.write(batch),
        }
    }

    /// Waits for what was handed over to be written. Returns what stopped the writer, if
    /// anything did; a panic in it is this thread's.
    fn end(mut self) -> Result<(), SyncError> {
        self.join()
    }

    /// [`Writer::end`], leaving it with no writer: what it is handed then is written here.
    fn join(&mut self) -> Result<(), SyncError> {
        drop(self.send.take());
        match self.thread.take().map(thread::ScopedJoinHandle::join) {
            None => Ok(()),
            Some(Ok(ended)) => ended,
            Some(Err(fault)) => panic::resume_unwind(fault),
        }
    }
}

/**
 * What a search that fails while an index makes its text comes to: nothing, said in the log;
 * the text is left to the next index, and the library never waits on search. What fails in
 * the catalogue is the index's failure, as is a cancel.
 */
fn without_search(error: SyncError) -> Result<(), SyncError> {
    match error {
        SyncError::Search(SearchError::Catalogue(_)) => Err(error),
        SyncError::Search(error) => {
            log::warn!("search text is left to the next index: {error}");
            Ok(())
        }
        error => Err(error),
    }
}

/**
 * Compacts the search index ([`SearchIndex::compact`]) when the index has the `backlog` and
 * the spans marked deleted there have come to a share of those there are
 * ([`SearchIndex::compaction_due`]): what is marked stays until the index's parts merge, and
 * after an update had the index write most of the text again it took the file to several
 * times its size. `progress` hears it; `cancel` stops it at once, with nothing changed.
 */
fn compact(
    archive: &Archive,
    backlog: bool,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&Progress),
) -> Result<(), SyncError> {
    if cancel.is_cancelled() {
        return Err(SyncError::Cancelled);
    }
    let Some(search) = archive.search_index_if_open().filter(|_| backlog) else {
        return Ok(());
    };
    if !search.compaction_due()? {
        return Ok(());
    }
    progress(&Progress::Compacting);
    match search.compact(false, &|| cancel.is_cancelled()) {
        Err(SearchError::Cancelled) => Err(SyncError::Cancelled),
        compacted => Ok(compacted?),
    }
}

/// Takes the text of the sessions whose rows went out of the search index, if it is open.
fn forget_gone(archive: &Archive) -> Result<(), SyncError> {
    let catalogue = archive.catalogue();
    let gone = catalogue.text_gone()?;
    if let (false, Some(search)) = (gone.is_empty(), archive.search_index_if_open()) {
        search.forget(&gone)?;
        catalogue.text_forgotten(&gone)?;
    }
    Ok(())
}

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
    if cancel.is_cancelled() {
        return Err(SyncError::Cancelled);
    }
    let catalogue = archive.catalogue();
    /* Search that is not available is left to the next index; the library is not. */
    let Ok(search) = archive.search_index() else {
        progress(&Progress::Searching { sessions: 0 });
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
    let sessions = rows.len();
    progress(&Progress::Searching { sessions });
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
    if cancel.is_cancelled() {
        return Err(SyncError::Cancelled);
    }
    /* Text made of a row as it is now, before its flag said so (an index stopped between the
    two, say): it stays, and is done at once. */
    let kept = search.keep(rows.iter().map(|(_, row)| row.clone()).collect(), |kept| {
        Ok(catalogue.set_searchable(kept)?)
    })?;
    let kept: HashSet<SessionId> = kept.into_iter().map(|(id, _)| id).collect();
    let rows: Vec<_> = (rows.into_iter())
        .filter(|(_, (id, _))| !kept.contains(id))
        .collect();
    /* What the first step wrote is not counted here: this step counts from its start. */
    let (mut done, mut searchable) = (kept.len(), 0);
    if !kept.is_empty() {
        progress(&Progress::Searched {
            sessions: done,
            searchable,
        });
    }
    let mut batch = Batch::default();
    let made = in_order(
        rows,
        cancel,
        |(provider, (id, made))| text(archive, provider, id, made),
        |text| {
            done += 1;
            if let Some(text) = text? {
                batch.add(text);
            }
            if batch.is_full() || done == sessions {
                let written = batch.write(archive, &search)?;
                searchable += written;
                report.searchable += written;
            }
            progress(&Progress::Searched {
                sessions: done,
                searchable,
            });
            Ok(())
        },
    );
    /* What was read is written, the step stopped or not. */
    if let Err(SyncError::Cancelled) = made {
        report.searchable += batch.write(archive, &search)?;
    }
    made
}

/// How many sessions are parsed at once at most, over every index the app runs: indexing is
/// work in the background, beside the app and whatever else the owner runs.
const MOST_WORKERS: usize = 4;

/// How big a parsing thread's stack is: set, so it is the same on every platform.
const STACK: usize = 8 * 1024 * 1024;

/// How many sessions are parsed at once, over every index the app runs: one fewer than the
/// processors there are, so one is left for the rest, and at most [`MOST_WORKERS`].
fn workers() -> usize {
    static WORKERS: OnceLock<usize> = OnceLock::new();
    *WORKERS.get_or_init(|| {
        thread::available_parallelism()
            .map_or(1, |processors| processors.get() - 1)
            .clamp(1, MOST_WORKERS)
    })
}

/// What a session is parsed under ([`Permits`]): every index's parsing threads share them.
static PARSING: Permits = Permits::new();

/**
 * Leave to parse a session, of which there are [`workers`]: two indexes at once (a start's and
 * a pass's, or two passes') parse no more at once than one does. Only the threads that parse
 * take them; an index's own thread, which records, never waits for one.
 */
struct Permits {
    taken: Mutex<usize>,
    freed: Condvar,
}

impl Permits {
    const fn new() -> Self {
        Self {
            taken: Mutex::new(0),
            freed: Condvar::new(),
        }
    }

    /// Waits for leave, and takes it until the permit is dropped.
    fn take(&self) -> Permit<'_> {
        let mut taken = self.taken.lock().unwrap_or_else(PoisonError::into_inner);
        while *taken >= workers() {
            taken = self
                .freed
                .wait(taken)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *taken += 1;
        Permit(self)
    }
}

/// Leave to parse ([`Permits::take`]), given back when dropped.
struct Permit<'a>(&'a Permits);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0.taken.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        self.0.freed.notify_one();
    }
}

/**
 * Runs `work` on each of `jobs` on [`workers`] threads (fewer for fewer jobs), each job with
 * leave to parse ([`PARSING`]), and hands what came of each to `take` on this thread, in the
 * order of `jobs`. At most twice as many jobs as there are threads are begun and not yet
 * taken, so what was made waits for its turn without piling up. Threads that can't be started
 * are done without; with none, the jobs are run on this thread, one at a time.
 *
 * `cancel` stops it before it begins or takes the next job, with [`SyncError::Cancelled`];
 * `take` failing stops it with its error. Either way no job is begun after, and what was begun
 * and not taken is dropped once the threads have ended the jobs they were running.
 */
fn in_order<J: Send, R: Send>(
    jobs: Vec<J>,
    cancel: &CancelToken,
    work: impl Fn(J) -> R + Sync,
    take: impl FnMut(R) -> Result<(), SyncError>,
) -> Result<(), SyncError> {
    in_order_on(workers().min(jobs.len()), jobs, cancel, work, take)
}

/// What became of a job [`in_order`] began.
enum Ended<R> {
    Done(R),
    /// It was not run: the work had stopped before its turn came.
    Dropped,
    /// The work panicked, with this.
    Panicked(Box<dyn Any + Send>),
}

/// [`in_order`] on `workers` threads.
fn in_order_on<J: Send, R: Send>(
    workers: usize,
    jobs: Vec<J>,
    cancel: &CancelToken,
    work: impl Fn(J) -> R + Sync,
    mut take: impl FnMut(R) -> Result<(), SyncError>,
) -> Result<(), SyncError> {
    if jobs.is_empty() {
        return Ok(());
    }
    let stopped = AtomicBool::new(false);
    let (begin, begun) = mpsc::channel::<(usize, J)>();
    let begun = Mutex::new(begun);
    let (end, ended) = mpsc::channel::<(usize, Ended<R>)>();
    let work = &work;
    thread::scope(|scope| {
        let mut spawned = 0;
        while spawned < workers {
            let (begun, end, stopped) = (&begun, end.clone(), &stopped);
            let worker = move || {
                loop {
                    let next = begun.lock().unwrap_or_else(PoisonError::into_inner).recv();
                    let Ok((at, job)) = next else {
                        break;
                    };
                    let stop = || stopped.load(Ordering::Relaxed) || cancel.is_cancelled();
                    let permit = (!stop()).then(|| PARSING.take());
                    let ended = if permit.is_none() || stop() {
                        Ended::Dropped
                    } else {
                        match panic::catch_unwind(AssertUnwindSafe(|| work(job))) {
                            Ok(done) => Ended::Done(done),
                            Err(fault) => Ended::Panicked(fault),
                        }
                    };
                    drop(permit);
                    if end.send((at, ended)).is_err() {
                        break;
                    }
                }
            };
            let thread = thread::Builder::new()
                .name("index-parse".to_string())
                .stack_size(STACK)
                .spawn_scoped(scope, worker);
            if let Err(error) = thread {
                log::warn!("an index parses on {spawned} threads, not {workers}: {error}");
                break;
            }
            spawned += 1;
        }
        drop(end);
        if spawned == 0 {
            drop(begin);
            return one_at_a_time(jobs, cancel, work, take);
        }
        let ahead = 2 * spawned;
        let mut jobs = jobs.into_iter().enumerate();
        let mut waiting = BTreeMap::new();
        let (mut started, mut taken) = (0, 0);
        let taking = loop {
            /* Nothing more is begun once it is cancelled. */
            if cancel.is_cancelled() {
                break Err(SyncError::Cancelled);
            }
            while started < taken + ahead
                && let Some(job) = jobs.next()
            {
                /* The workers hold the receiver until this scope ends. */
                let _ = begin.send(job);
                started += 1;
            }
            if taken == started {
                break Ok(());
            }
            let ended = loop {
                if let Some(ended) = waiting.remove(&taken) {
                    break ended;
                }
                let (at, ended) = ended
                    .recv()
                    .expect("the workers hold the sender until each job begun has ended");
                waiting.insert(at, ended);
            };
            taken += 1;
            match ended {
                Ended::Done(done) => {
                    if let Err(error) = take(done) {
                        break Err(error);
                    }
                }
                /* Only a cancel drops a job before this thread stops taking. */
                Ended::Dropped => break Err(SyncError::Cancelled),
                /* As it would on this thread, once the other threads have stopped. */
                Ended::Panicked(fault) => {
                    stopped.store(true, Ordering::Relaxed);
                    drop(begin);
                    panic::resume_unwind(fault);
                }
            }
        };
        stopped.store(true, Ordering::Relaxed);
        drop(begin);
        taking
    })
}

/// [`in_order`] on this thread alone, without leave to parse: what it does when it can start
/// no thread.
fn one_at_a_time<J, R>(
    jobs: Vec<J>,
    cancel: &CancelToken,
    work: impl Fn(J) -> R,
    mut take: impl FnMut(R) -> Result<(), SyncError>,
) -> Result<(), SyncError> {
    for job in jobs {
        if cancel.is_cancelled() {
            return Err(SyncError::Cancelled);
        }
        take(work(job))?;
    }
    Ok(())
}

/// Writes `texts` to `search`, of those sessions whose rows are still what the texts were
/// made of, and says so of their rows. Returns how many were written.
fn record(
    archive: &Archive,
    search: &SearchIndex,
    texts: &[Text],
    wait: bool,
) -> Result<Recorded, SyncError> {
    if texts.is_empty() {
        return Ok(Recorded::Written(0));
    }
    let catalogue = archive.catalogue();
    Ok(search.record(
        texts,
        wait,
        |text| Ok(catalogue.made_of(&text.id)?.as_deref() == Some(&*text.made)),
        |written| Ok(catalogue.set_searchable(written)?),
    )?)
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
        /* The archive could not be read for it, though its row was made: it is to be read
        again, by the next index of a start, and is no backlog meanwhile. */
        Ok(None) => {
            catalogue.read_again(&id, &made)?;
            return Ok(None);
        }
        Err(_) => SessionIndex {
            title: catalogue.session(&id)?.and_then(|session| session.title),
            ..SessionIndex::default()
        },
    };
    Ok(Some(Text {
        id,
        made,
        title: index.title,
        spans: search::kept(index.spans),
    }))
}

/// What a parse of a session came to, for its row to be made of.
struct Parsed {
    /// Its items, as they were read.
    readings: Vec<Reading>,
    /// How many versions the parse newly held back for how they parse.
    held_back: usize,
    outcome: Outcome,
}

/// What came of reading a session's items.
enum Outcome {
    Read {
        /// Which of the readings the session's own conversation was parsed from.
        main: Option<usize>,
        /// Its index, but for its spans.
        index: Box<SessionIndex>,
        /// Its spans, as search keeps them: only so much of each is kept until it is written.
        spans: Vec<Kept>,
        ending: Option<Ending>,
    },
    /// The archive could not be read, for the reason given.
    Unread(String),
    /// Its parser stopped, for the reason given.
    Faulted(String),
    /// Its row was made of its items as they are, by this parser and index, since it was
    /// found stale: it was not parsed, and there is nothing to record.
    Current,
}

/**
 * Parses and indexes the session `id`; `None` when its items hold no transcript: they are
 * another session's now, or no conversation's. With `if_stale`, a session whose row is
 * already made of its items as they are, by this parser and index, is not parsed
 * ([`Outcome::Current`]): another index made it since this one found it stale, a pass beside
 * the index of a start, say. It writes nothing but the holds of its versions ([`hold`]), so
 * sessions are parsed several at once and their rows made in order.
 */
fn parse(
    archive: &Archive,
    provider: &dyn Provider,
    id: &SessionId,
    if_stale: bool,
) -> Result<Option<Parsed>, SyncError> {
    let catalogue = archive.catalogue();
    let mut readings = catalogue.session_items(id)?;
    if !readings.iter().any(|reading| reading.kind.is_transcript()) {
        return Ok(None);
    }
    let current = made(fingerprint(&readings), provider.parser_version());
    if if_stale && catalogue.is_made_of(id, &current)? {
        return Ok(Some(Parsed {
            readings,
            held_back: 0,
            outcome: Outcome::Current,
        }));
    }
    let mut held_back = 0;
    /* A provider's parser doesn't fail (PLAN §7.1), so what stops it on a session is a fault
    in it. That is caught here, for one session's fault to be that session's alone. */
    let parsed = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut items = ArchivedSession::of(archive, &readings);
        let mut transcript = provider.parse(&items);
        if items.fault().is_none() {
            let newly_held = hold(archive, provider, &readings, &transcript)?;
            if let Some(newly_held) = newly_held {
                /* Some transcript is read at another version now. */
                held_back += newly_held;
                readings = catalogue.session_items(id)?;
                items = ArchivedSession::of(archive, &readings);
                transcript = provider.parse(&items);
            }
        }
        transcript.read_subagents(provider, &items);
        let own = transcript.conversation.as_ref();
        Ok::<_, SyncError>(match items.fault() {
            Some(fault) => Outcome::Unread(format!("the archive could not be read: {fault}")),
            None => {
                let mut index = transcript::index(&transcript);
                let spans = search::kept(std::mem::take(&mut index.spans));
                Outcome::Read {
                    main: own.and_then(|own| {
                        readings
                            .iter()
                            .position(|reading| *reading.key == *own.item)
                    }),
                    index: Box::new(index),
                    spans,
                    ending: own.map(|own| own.ending.clone()),
                }
            }
        })
    }));
    let outcome = match parsed {
        Ok(outcome) => outcome?,
        Err(fault) => Outcome::Faulted(format!("its parser stopped: {}", said(fault.as_ref()))),
    };
    Ok(Some(Parsed {
        readings,
        held_back,
        outcome,
    }))
}

/**
 * Records what came of a parse of the session `id`: its row, and in `report` what became of
 * it. Returns the text of the row it made, when it made one: of what was parsed, or of a
 * session its parser stopped on, its title alone.
 */
fn record_parse(
    archive: &Archive,
    provider: &dyn Provider,
    id: &SessionId,
    parsed: Option<Parsed>,
    report: &mut IndexReport,
) -> Result<Option<Text>, SyncError> {
    let catalogue = archive.catalogue();
    let Some(Parsed {
        readings,
        held_back,
        outcome,
    }) = parsed
    else {
        /* Its items are another session's now, or no conversation's. */
        catalogue.forget_sessions(std::slice::from_ref(id))?;
        return Ok(None);
    };
    /* One made since by another index is that index's to count. */
    if !matches!(outcome, Outcome::Current) {
        report.sessions += 1;
        report.held_back += held_back;
    }
    let from = fingerprint(&readings);
    let parser = provider.parser_version();
    let made = made(from, parser);
    /* What a parse would say of its conversation, for a session it can't be parsed for. */
    let known = (readings.iter())
        .find(|reading| reading.kind == ItemKind::Transcript)
        .and_then(|reading| provider.conversation_of(&reading.key));
    let known = known.as_deref();
    let copy = provider.copy_of(&id.key);
    let copy = copy.as_ref();
    match outcome {
        Outcome::Read {
            main,
            index,
            spans,
            ending,
        } => {
            let derived = Derived {
                main: main.map(|main| &readings[main]),
                index: &index,
                ending: ending.as_ref(),
                held_back: readings
                    .iter()
                    .any(|reading| reading.kind.is_transcript() && reading.newest_held.is_some()),
            };
            match catalogue.record_session(
                id,
                parser,
                from,
                Indexed::Parsed(derived),
                copy,
                None,
            )? {
                RowMade::Made { filed } => report.filed += usize::from(filed),
                RowMade::Deleted => {
                    report.deleted.push(id.clone());
                    return Ok(None);
                }
            }
            Ok(Some(Text {
                id: id.clone(),
                made,
                title: index.title,
                spans,
            }))
        }
        /* Read again by the next index, which may find the text then. */
        Outcome::Unread(reason) => {
            catalogue.record_session(id, parser, from, Indexed::NotRead(&reason), copy, known)?;
            report.failed.push((id.clone(), reason));
            Ok(None)
        }
        Outcome::Current => Ok(None),
        Outcome::Faulted(reason) => {
            catalogue.record_session(id, parser, from, Indexed::Faulted(&reason), copy, known)?;
            report.failed.push((id.clone(), reason));
            Ok(Some(Text {
                id: id.clone(),
                made,
                title: catalogue.session(id)?.and_then(|session| session.title),
                spans: Vec::new(),
            }))
        }
    }
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
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

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

    /**
     * Takes the jobs `0..jobs` through [`in_order_on`] on `workers` threads, each job taking a
     * moment that differs from the next one's; `stop` says, of each job taken, whether to stop
     * there, by a cancel or with an error. Returns what came of it, the jobs taken, and how
     * many were begun.
     */
    fn run_in_order(
        workers: usize,
        jobs: usize,
        stop: impl Fn(usize) -> Option<SyncError>,
    ) -> (Result<(), SyncError>, Vec<usize>, usize) {
        let cancel = CancelToken::new();
        let (begun, made) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let mut taken = Vec::new();
        let ended = in_order_on(
            workers,
            (0..jobs).collect(),
            &cancel,
            |job: usize| {
                begun.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_micros((job as u64 * 7919) % 2000));
                made.fetch_add(1, Ordering::SeqCst);
                job
            },
            |job| {
                /* What was made and not taken never runs further ahead than the bound. */
                let waiting = made.load(Ordering::SeqCst) - taken.len();
                assert!(waiting <= 2 * workers, "{waiting} made and not taken");
                taken.push(job);
                match stop(job) {
                    Some(SyncError::Cancelled) => {
                        cancel.cancel();
                        Ok(())
                    }
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            },
        );
        (ended, taken, begun.load(Ordering::SeqCst))
    }

    #[test]
    fn work_done_on_several_threads_is_taken_in_order_and_runs_only_so_far_ahead() {
        for workers in [1, 2, 4] {
            let (ended, taken, begun) = run_in_order(workers, 200, |_| None);
            assert!(ended.is_ok(), "{ended:?}");
            assert_eq!(taken, (0..200).collect::<Vec<_>>());
            assert_eq!(begun, 200);
        }
        assert!(run_in_order(4, 0, |_| None).0.is_ok());
    }

    #[test]
    fn a_cancel_or_an_error_begins_nothing_more_and_drops_what_was_begun() {
        for workers in [1, 3] {
            let (ended, taken, begun) = run_in_order(workers, 200, |job| {
                (job == 9).then_some(SyncError::Cancelled)
            });
            assert!(matches!(ended, Err(SyncError::Cancelled)), "{ended:?}");
            assert_eq!(taken, (0..10).collect::<Vec<_>>());
            assert!(begun <= 10 + 2 * workers, "{begun} begun");

            let (ended, taken, begun) = run_in_order(workers, 200, |job| {
                (job == 4).then(|| SyncError::Protocol("stopped".to_string()))
            });
            assert!(matches!(ended, Err(SyncError::Protocol(_))), "{ended:?}");
            assert_eq!(taken, (0..5).collect::<Vec<_>>());
            assert!(begun <= 5 + 2 * workers, "{begun} begun");
        }
    }

    #[test]
    fn a_cancel_while_the_work_runs_ends_it_and_a_panic_in_the_work_is_this_threads() {
        /* Cancelled by what one of the threads does, while this one waits for another. */
        let cancel = CancelToken::new();
        let mut taken = Vec::new();
        let ended = in_order_on(
            3,
            (0..100).collect(),
            &cancel,
            |job: usize| {
                if job == 20 {
                    cancel.cancel();
                }
                thread::sleep(Duration::from_micros(200));
                job
            },
            |job| {
                taken.push(job);
                Ok(())
            },
        );
        assert!(matches!(ended, Err(SyncError::Cancelled)), "{ended:?}");
        assert!(taken.len() <= 21 && taken.iter().copied().eq(0..taken.len()));

        let mut taken = Vec::new();
        let ended = panic::catch_unwind(AssertUnwindSafe(|| {
            in_order_on(
                3,
                (0..100).collect(),
                &CancelToken::new(),
                |job: usize| {
                    assert_ne!(job, 30, "the work stops");
                    job
                },
                |job| {
                    taken.push(job);
                    Ok(())
                },
            )
        }));
        let fault = ended.expect_err("the panic reaches this thread");
        assert!(said(fault.as_ref()).contains("the work stops"));
        assert_eq!(taken, (0..30).collect::<Vec<_>>());
    }

    #[test]
    fn several_at_once_parse_no_more_at_once_than_one_does() {
        let (running, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let work = |job: usize| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_micros(300));
            running.fetch_sub(1, Ordering::SeqCst);
            job
        };
        thread::scope(|scope| {
            for _ in 0..3 {
                scope.spawn(|| {
                    let cancel = CancelToken::new();
                    in_order_on(MOST_WORKERS, (0..60).collect(), &cancel, work, |_| Ok(()))
                        .unwrap();
                });
            }
        });
        assert!(most.load(Ordering::SeqCst) <= workers(), "{most:?} at once");
    }
}
