//! Search (PLAN §10, §11; DESIGN §4.9): runs the palette's queries over the archive, off the
//! UI thread, and keeps what the palette shows before anything is typed.
//!
//! **One search at a time.** Each query asked for supersedes the one before: that one is
//! cancelled ([`sb_core::archive::Archive::search`] checks between its steps), and whatever it
//! found is dropped when it comes back, so the answer shown is always the last query's. While
//! a search runs, the answer to the one before stays shown under the palette's bar. The answer
//! keeps what was typed for it, so what a row of it opens is what was asked, whatever has been
//! typed since.
//!
//! **What shows before anything is typed:** the sessions the owner opened last and the
//! searches they opened a result of, the latest first, for as long as the app runs. A session
//! deleted for good leaves the list at once; each time the palette opens, the others are read
//! again, so they show as they are now and those moved to Trash leave it.
//!
//! **Completions** for `model:` and `branch:` are read when the palette opens, with how many
//! sessions are not searchable yet (the palette's "Still indexing" banner).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{AppContext as _, Context, Entity, Subscription};
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::catalogue::{Session, SessionId};
use sb_core::search::{Limit, Query, Results, SearchError, Searched};

use super::library::LibraryEvent;
use super::{ArchiveService, LibraryService};

/// How many recent sessions and searches are kept.
const RECENT: usize = 5;

/// What a search answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The query's terms are a letter or two, which nearly every session holds.
    KeepTyping,
    Found(Results),
    /// Search can't be used now, for this reason (the index can't be opened).
    Unavailable(String),
}

/// An answer, and what it answers.
#[derive(Debug)]
pub struct Answered {
    pub query: Query,
    /// What was typed for it, as the recent searches keep it.
    pub text: String,
    /// Its terms as Find in session is filled with them.
    pub find: String,
    pub answer: Answer,
    /// How long the search took.
    pub took: Duration,
}

pub struct SearchService {
    archive: Entity<ArchiveService>,
    /// Counts the queries asked for: a search's answer is taken only while its number is the
    /// last one.
    asked: u64,
    /// Counts the reads of what the palette shows before anything is typed, likewise.
    refreshed: u64,
    /// The search running, while one is.
    cancel: Option<CancelToken>,
    /// Searches and reads of what the palette shows before anything is typed in flight, those
    /// superseded or cancelled included: what still reads the archive.
    reading: usize,
    /// The app has moved onto another archive: nothing more is read of this one.
    closed: bool,
    answer: Option<Arc<Answered>>,
    /// How many sessions' text is not searchable yet, as last read.
    unsearchable: usize,
    /// The models and the branches the archive's sessions have, the commonest first.
    values: (Arc<[String]>, Arc<[String]>),
    /// Shared, so that the palette draws them each frame without copying them.
    recent_sessions: VecDeque<Arc<Session>>,
    recent_searches: VecDeque<String>,
    _observers: [Subscription; 2],
}

impl SearchService {
    pub fn new(
        archive: Entity<ArchiveService>,
        library: Entity<LibraryService>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            _observers: [
                // Every session opened, from the palette or a page, is a recent one.
                cx.observe(&library, |this, library, cx| {
                    let Some(open) = library.read(cx).open_session() else {
                        return;
                    };
                    if this.recent_sessions.front().map(|it| &it.id) != Some(&open.id) {
                        let open = open.clone();
                        this.recent_sessions.retain(|it| it.id != open.id);
                        this.recent_sessions.push_front(Arc::new(open));
                        this.recent_sessions.truncate(RECENT);
                        cx.notify();
                    }
                }),
                cx.subscribe(&library, |this, _, event, cx| {
                    if let LibraryEvent::DeletedForGood(id) = event {
                        this.forget(id, cx);
                    }
                }),
            ],
            archive,
            asked: 0,
            refreshed: 0,
            cancel: None,
            reading: 0,
            closed: false,
            answer: None,
            unsearchable: 0,
            values: (Arc::from([]), Arc::from([])),
            recent_sessions: VecDeque::new(),
            recent_searches: VecDeque::new(),
        }
    }

    /// Nothing is being searched or read, not even a search superseded or cancelled.
    pub fn is_settled(&self) -> bool {
        self.reading == 0
    }

    /// Cancels the search running, and reads nothing more of the archive: the app is moving
    /// onto another (`super::reopen`).
    pub(super) fn close(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        self.closed = true;
        cx.notify();
    }

    /// A search is running.
    pub fn is_searching(&self) -> bool {
        self.cancel.is_some()
    }

    /// The last answer, and what it answers.
    pub fn answer(&self) -> Option<&Arc<Answered>> {
        self.answer.as_ref()
    }

    /// How many sessions' text is not searchable yet (PLAN §7.5).
    pub fn unsearchable(&self) -> usize {
        self.unsearchable
    }

    /// The models and the branches the palette completes `model:` and `branch:` with.
    pub fn values(&self) -> (Arc<[String]>, Arc<[String]>) {
        self.values.clone()
    }

    /// The sessions opened last, the latest first.
    pub fn recent_sessions(&self) -> impl Iterator<Item = &Arc<Session>> {
        self.recent_sessions.iter()
    }

    /// The searches a result was opened from last, the latest first.
    pub fn recent_searches(&self) -> impl Iterator<Item = &String> {
        self.recent_searches.iter()
    }

    /// Keeps `text` among the recent searches: a result of it was opened.
    pub fn remember(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.recent_searches.retain(|it| it != text);
        self.recent_searches.push_front(text.to_string());
        self.recent_searches.truncate(RECENT);
        cx.notify();
    }

    /// Forgets a session deleted for good, which is no recent session any more.
    fn forget(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        self.recent_sessions.retain(|it| it.id != *id);
        cx.notify();
    }

    fn archive(&self, cx: &Context<Self>) -> Option<Arc<Archive>> {
        self.archive
            .read(cx)
            .archive()
            .filter(|_| !self.closed)
            .cloned()
    }

    /**
     * Reads what the palette shows before anything is typed, as it is now: the completions,
     * how many sessions are not searchable yet, and the recent sessions, which a rename or a
     * refiling shows and which leave the list once in Trash or deleted; those opened while it
     * reads stay. The palette is opening; a read asked for since supersedes this one.
     */
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive(cx) else {
            return;
        };
        self.refreshed += 1;
        self.reading += 1;
        let refreshed = self.refreshed;
        let recent: Vec<SessionId> = self
            .recent_sessions
            .iter()
            .map(|it| it.id.clone())
            .collect();
        let job = cx.background_spawn(async move {
            let catalogue = archive.catalogue();
            let sessions: Result<HashMap<SessionId, Option<Session>>, _> = (recent.into_iter())
                .map(|id| catalogue.session(&id).map(|read| (id, read)))
                .collect();
            (
                catalogue.models_and_branches(),
                archive.unsearchable(),
                sessions,
            )
        });
        cx.spawn(async move |this, cx| {
            let (values, unsearchable, sessions) = job.await;
            this.update(cx, |this, cx| {
                this.reading -= 1;
                if this.refreshed != refreshed {
                    return;
                }
                match values {
                    Ok((models, branches)) => this.values = (models.into(), branches.into()),
                    Err(error) => log::warn!("the palette's completions couldn't be read: {error}"),
                }
                if let Ok(unsearchable) = unsearchable {
                    this.unsearchable = unsearchable;
                }
                /* Merged into the list as it is now, which a session opened meanwhile has joined:
                each read is as it is now, or leaves the list when it is gone or in Trash. */
                match sessions {
                    Ok(read) => this
                        .recent_sessions
                        .retain_mut(|kept| match read.get(&kept.id) {
                            None => true,
                            Some(Some(now)) if now.trashed.is_none() => {
                                *kept = Arc::new(now.clone());
                                true
                            }
                            Some(_) => false,
                        }),
                    Err(error) => log::warn!("the recent sessions couldn't be read: {error}"),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /**
     * Searches for `query` off the UI thread, in place of the search running (module docs),
     * keeping `text`, what was typed for it, and `find`, its terms for Find in session. A
     * query with no terms, or with no archive to search, has no answer: the palette shows
     * the recent sessions and searches.
     */
    pub fn search(&mut self, query: Query, text: String, find: String, cx: &mut Context<Self>) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.asked += 1;
        let archive = self.archive(cx).filter(|_| !query.terms.is_empty());
        let Some(archive) = archive else {
            self.answer = None;
            cx.notify();
            return;
        };
        let (asked, cancel) = (self.asked, CancelToken::new());
        self.cancel = Some(cancel.clone());
        self.reading += 1;
        let job = cx.background_spawn(async move {
            let started = Instant::now();
            let searched = archive.search(&query, Limit::default(), &cancel);
            let took = started.elapsed();
            (query, searched, archive.unsearchable(), took)
        });
        cx.spawn(async move |this, cx| {
            let (query, searched, unsearchable, took) = job.await;
            this.update(cx, |this, cx| {
                this.reading -= 1;
                // A later query superseded it: its answer is the one to show.
                if this.asked != asked {
                    return;
                }
                this.cancel = None;
                if let Ok(unsearchable) = unsearchable {
                    this.unsearchable = unsearchable;
                }
                let answer = match searched {
                    Ok(Searched::KeepTyping) => Answer::KeepTyping,
                    Ok(Searched::Results(results)) => Answer::Found(results),
                    Err(SearchError::Unavailable(why)) => Answer::Unavailable(why),
                    Err(SearchError::Cancelled) => return,
                    Err(error) => {
                        log::warn!("a search failed: {error}");
                        Answer::Unavailable(error.to_string())
                    }
                };
                log::debug!("searched {:?} in {took:?}", query.terms);
                this.answer = Some(Arc::new(Answered {
                    query,
                    text,
                    find,
                    answer,
                    took,
                }));
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Takes `answer` as the last one: what a test shows the palette.
    #[cfg(test)]
    pub(crate) fn answered(&mut self, answer: Answered, cx: &mut Context<Self>) {
        self.answer = Some(Arc::new(answer));
        cx.notify();
    }
}
