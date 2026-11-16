//! Search (PLAN §10, §11; DESIGN §4.9): runs the palette's queries over the archive, off the
//! UI thread, and keeps what the palette shows before anything is typed.
//!
//! **One search at a time.** Each query asked for supersedes the one before: that one is
//! cancelled ([`sb_core::archive::Archive::search`] checks between its steps), and whatever it
//! found is dropped when it comes back, so the answer shown is always the last query's. While
//! a search runs, the answer to the one before stays shown under the palette's bar.
//!
//! **What shows before anything is typed:** the sessions the owner opened last and the
//! searches they opened a result of, the latest first, for as long as the app runs.
//!
//! **Completions** for `model:` and `branch:` are read when the palette opens, with how many
//! sessions are not searchable yet (the palette's "Still indexing" banner).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{AppContext as _, Context, Entity, Subscription};
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::catalogue::{Session, SessionId};
use sb_core::search::{Limit, Query, Results, SearchError, Searched};

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

pub struct SearchService {
    archive: Entity<ArchiveService>,
    /// Counts the queries asked for: a search's answer is taken only while its number is the
    /// last one.
    asked: u64,
    /// The search running, while one is.
    cancel: Option<CancelToken>,
    /// The last answer, the query it answers, and how long it took.
    answer: Option<(Query, Answer, Duration)>,
    /// How many sessions' text is not searchable yet, as last read.
    unsearchable: usize,
    /// The models and the branches the archive's sessions have, the commonest first.
    values: (Arc<[String]>, Arc<[String]>),
    recent_sessions: VecDeque<Session>,
    recent_searches: VecDeque<String>,
    _observers: [Subscription; 1],
}

impl SearchService {
    pub fn new(
        archive: Entity<ArchiveService>,
        library: Entity<LibraryService>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            // Every session opened, from the palette or a page, is a recent one.
            _observers: [cx.observe(&library, |this, library, cx| {
                let Some(open) = library.read(cx).open_session() else {
                    return;
                };
                if this.recent_sessions.front().map(|it| &it.id) != Some(&open.id) {
                    let open = open.clone();
                    this.recent_sessions.retain(|it| it.id != open.id);
                    this.recent_sessions.push_front(open);
                    this.recent_sessions.truncate(RECENT);
                    cx.notify();
                }
            })],
            archive,
            asked: 0,
            cancel: None,
            answer: None,
            unsearchable: 0,
            values: (Arc::from([]), Arc::from([])),
            recent_sessions: VecDeque::new(),
            recent_searches: VecDeque::new(),
        }
    }

    /// Nothing is being searched.
    pub fn is_settled(&self) -> bool {
        self.cancel.is_none()
    }

    /// A search is running.
    pub fn is_searching(&self) -> bool {
        self.cancel.is_some()
    }

    /// The last answer, the query it is of, and how long the search took.
    pub fn answer(&self) -> Option<&(Query, Answer, Duration)> {
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
    pub fn recent_sessions(&self) -> impl Iterator<Item = &Session> {
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
    pub fn forget(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        self.recent_sessions.retain(|it| it.id != *id);
        cx.notify();
    }

    fn archive(&self, cx: &Context<Self>) -> Option<Arc<Archive>> {
        self.archive.read(cx).archive().cloned()
    }

    /// Reads the completions and how many sessions are not searchable yet: the palette is
    /// opening.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive(cx) else {
            return;
        };
        let job = cx.background_spawn(async move {
            let values = archive.catalogue().models_and_branches();
            (values, archive.unsearchable())
        });
        cx.spawn(async move |this, cx| {
            let (values, unsearchable) = job.await;
            this.update(cx, |this, cx| {
                match values {
                    Ok((models, branches)) => this.values = (models.into(), branches.into()),
                    Err(error) => log::warn!("the palette's completions couldn't be read: {error}"),
                }
                if let Ok(unsearchable) = unsearchable {
                    this.unsearchable = unsearchable;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /**
     * Searches for `query` off the UI thread, in place of the search running (module docs). A
     * query with no terms has no answer: the palette shows the recent sessions and searches.
     */
    pub fn search(&mut self, query: Query, cx: &mut Context<Self>) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.asked += 1;
        if query.terms.is_empty() {
            self.answer = None;
            cx.notify();
            return;
        }
        let Some(archive) = self.archive(cx) else {
            return;
        };
        let (asked, cancel) = (self.asked, CancelToken::new());
        self.cancel = Some(cancel.clone());
        let job = cx.background_spawn(async move {
            let started = Instant::now();
            let searched = archive.search(&query, Limit::default(), &cancel);
            let took = started.elapsed();
            (query, searched, archive.unsearchable(), took)
        });
        cx.spawn(async move |this, cx| {
            let (query, searched, unsearchable, took) = job.await;
            this.update(cx, |this, cx| {
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
                this.answer = Some((query, answer, took));
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}
