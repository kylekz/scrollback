//! The reader (PLAN §9, §11): the session open in the main area, parsed, and what has been
//! loaded of it.
//!
//! **Opening.** The library says which session is open ([`LibraryService::open_session`]);
//! this service follows it. Opening parses the session's own conversation on a background
//! task, with the provider of its agent, and keeps the transcript for that session only. It is
//! [`ReaderState::Opening`] until the parse lands, then [`ReaderState::Open`], or
//! [`ReaderState::Failed`] when the session's items could not be found at all; a parse itself
//! never fails (PLAN §7.1), and what it couldn't read is in the transcript (its ending) and in
//! [`OpenSession::fault`]. Each open is a generation: a job of an earlier one that lands late
//! (the owner went back, or on to another session) is dropped, so no view is ever handed
//! another session's content. A parse can't be stopped half-way; going back drops its result.
//!
//! **Loading.** What the views draw is prepared when they first draw it, each on a background
//! task ([`Load`]): a prompt's or a reply's prose ([`Prose`]: cut into pieces, sanitised), a
//! call's body when its row is open, a subagent's conversation when its call is, a
//! compaction's summary, an image ([`Picture`]: one bounded frame). The first few prompts and
//! replies are prepared with the parse, so the first turns paint with it. Loads read the items
//! at the versions the parse read ([`Pinned`]) through a [`SegmentCache`] of the last segments
//! decoded, so opening one row after another near the end of a long transcript decodes its
//! last segment once. Which subagent started which ([`Placement`]) is worked out once the
//! subagents no call of the session's own started come into view.
//!
//! **Letting go.** The views say what they have scrolled far from ([`ReaderService::release`]):
//! its prose, bodies and images are dropped, an image's texture with it, and prepared again if
//! it comes back. Closing the reader drops all of it, off the UI thread.
//!
//! **What is shown open.** The Activity / Full calls switch is the app's, not a session's
//! (DESIGN §4.2.1). Each row the owner opens or closes by hand is a [`Toggle`]: in Activity a
//! toggle opens a row; in Full calls, where every call, thought and subagent's call list is
//! open, it closes one until the switch is flipped again (DESIGN §4.2.3).

pub mod picture;
pub mod prose;
#[cfg(test)]
mod tests;
pub mod text;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use gpui::{AppContext as _, Context, Entity, Subscription};
use sb_core::archive::{Archive, ArchivedSession, Pinned, SegmentCache};
use sb_core::catalogue::SessionId;
use sb_core::provider::{ItemKind, Provider, SessionItems};
use sb_core::transcript::{
    BlockId, Content, Conversation, Outline, Output, Stored, Subagent, ToolCall,
};

use super::{ArchiveService, LibraryService, Providers};
pub use picture::Picture;
pub use prose::{Piece, Prose};
pub use text::Text;

/// How many decoded segments an open session keeps, and how many bytes of them: a segment is
/// up to 8 MiB.
const CACHED_SEGMENTS: usize = 4;
const CACHED_BYTES: usize = 32 * 1024 * 1024;

/**
 * How much of a call's input and of each part of its output is read for its body, until Show
 * all asks for the whole: past the first lines a body shows, for lines of a few hundred bytes.
 */
const HEAD_BYTES: u64 = 16 * 1024;

/// How many images are decoded at once: each may take [`picture::DECODE_BYTES`] while it is.
const DECODES: usize = 2;

/// How many of the first prompts and replies of the session's own conversation are prepared
/// with the parse, so that the first turns paint with it: those of at most a piece each.
const EAGER_PROSE: usize = 24;

/// A block of one of the session's conversations: the session's own, or a subagent's, named by
/// the item it was parsed from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlockRef {
    pub conversation: Arc<str>,
    pub block: BlockId,
}

/// Something read when it is wanted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Load<T> {
    Loading,
    Loaded(T),
    /// Why it couldn't be read.
    Failed(String),
}

/// Where an image is in its block: among a prompt's, or among a call's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    Prompt(usize),
    Output(usize),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ImageKey {
    pub at: BlockRef,
    pub slot: Slot,
}

/// What the views can let go of ([`ReaderService::release`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Held {
    /// A block's prose: a prompt's, a reply's, a thought's, a compaction's summary.
    Prose(BlockRef),
    Body(BlockRef),
    Image(ImageKey),
    /// An unrecognised record as it was recorded.
    Raw(BlockRef),
    /// A subagent's conversation, by its item.
    Subagent(Arc<str>),
    /// The whole of a call's input (`None`) or of a part of its output, read for Show all.
    Whole(BlockRef, Option<usize>),
}

/// A row the owner opened or closed by hand (the module docs).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Toggle {
    /// A call's body, or the panels of the subagents it started.
    Call(BlockRef),
    /// A call that started subagents: its own input and output.
    CallIo(BlockRef),
    Thought(BlockRef),
    /// A compaction's summary.
    Summary(BlockRef),
    /// A record the agent's provider didn't know, as it was recorded.
    Raw(BlockRef),
    /// Every call of the subagent whose item this is, not only its first few.
    AllCalls(Arc<str>),
    /// The panel of a subagent no call is known to have started.
    Subagent(Arc<str>),
    /// Every line of a part of a call's body, not only its first: the input (`None`) or the
    /// output part at its place.
    AllLines(BlockRef, Option<usize>),
    /// Every line of a piece of prose shown as plain text.
    ProseLines(BlockRef, usize),
}

impl Toggle {
    /// Whether Full calls opens it.
    fn opened_by_full_calls(&self) -> bool {
        matches!(
            self,
            Self::Call(_) | Self::Thought(_) | Self::AllCalls(_) | Self::Subagent(_)
        )
    }
}

/// A subagent's conversation, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentRead {
    /// The agent's name for the kind of subagent.
    pub kind: Option<String>,
    pub description: Option<String>,
    pub conversation: Conversation,
}

/// One part of what a tool returned: text, or an image ([`Slot::Output`]) loaded on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Text(Result<Arc<Text>, String>),
    Image,
}

/// A call's body: its input as it was sent, and its output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallBody {
    pub input: Result<Arc<Text>, String>,
    pub output: Vec<Part>,
}

/// What a subagent says it is, from its own record of itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    pub kind: Option<String>,
    pub description: Option<String>,
}

/// Which subagent started which, from reading every subagent of the session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Placement {
    /// Each subagent started by another subagent's call, and that other subagent.
    pub parents: HashMap<Arc<str>, Arc<str>>,
    pub names: HashMap<Arc<str>, Names>,
}

/// The session's main transcript is gone at its location (DESIGN §4.14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gone {
    /// The location's name.
    pub location: String,
    /// Since when, in Unix milliseconds.
    pub since: i64,
}

/// An open session: what was parsed, and what has been loaded since.
pub struct OpenSession {
    pub id: SessionId,
    /// The provider of the session's agent, by its place among the app's providers.
    provider: usize,
    pinned: Pinned,
    cache: Arc<SegmentCache>,
    /// The session's own conversation; `None` when its items hold no main transcript.
    main: Option<Conversation>,
    /// The subagents no call of the session's own conversation started, by item. Until
    /// [`Placement`] is worked out, those started by another subagent are among them.
    unattached: Vec<Arc<str>>,
    /// Why a read failed for a reason that is not the stored bytes (a file in use, a disk
    /// error): what was parsed around it is then not all the session holds, and opening it
    /// again may read more.
    fault: Option<String>,
    gone: Option<Gone>,
    prose: HashMap<BlockRef, Load<Arc<Prose>>>,
    subagents: HashMap<Arc<str>, Load<Arc<SubagentRead>>>,
    bodies: HashMap<BlockRef, Load<Arc<CallBody>>>,
    raws: HashMap<BlockRef, Load<Arc<Text>>>,
    /// The whole of a call's input or output part, when its body has only its head.
    wholes: HashMap<(BlockRef, Option<usize>), Load<Arc<Text>>>,
    images: HashMap<ImageKey, Load<Picture>>,
    placement: Option<Load<Arc<Placement>>>,
    /// The rows opened by hand in Activity, and closed by hand in Full calls.
    opened: HashSet<Toggle>,
    closed: HashSet<Toggle>,
}

impl OpenSession {
    pub fn main(&self) -> Option<&Conversation> {
        self.main.as_ref()
    }

    /// The subagents no call is known to have started; `None` until that is known, while
    /// some of them may have been started by another subagent.
    pub fn unattached(&self) -> Option<Vec<Arc<str>>> {
        match &self.placement {
            _ if self.unattached.is_empty() => Some(Vec::new()),
            Some(Load::Loaded(placement)) => Some(
                self.unattached
                    .iter()
                    .filter(|item| match placement.parents.get(*item) {
                        None => true,
                        /* Placed under a subagent whose calls, once read, don't start it after
                        all: it is drawn here rather than nowhere. */
                        Some(parent) => match self.subagents.get(parent) {
                            Some(Load::Loaded(read)) => {
                                !read.conversation.calls().any(|(_, call)| {
                                    call.subagents.iter().any(|it| it.item() == *item)
                                })
                            }
                            _ => false,
                        },
                    })
                    .cloned()
                    .collect(),
            ),
            Some(Load::Failed(_)) => Some(self.unattached.clone()),
            _ => None,
        }
    }

    /// What the subagent `item` says it is, once read or placed.
    pub fn names(&self, item: &str) -> Names {
        if let Some(Load::Loaded(read)) = self.subagents.get(item) {
            return Names {
                kind: read.kind.clone(),
                description: read.description.clone(),
            };
        }
        match &self.placement {
            Some(Load::Loaded(placement)) => placement.names.get(item).cloned().unwrap_or_default(),
            _ => Names::default(),
        }
    }

    pub fn fault(&self) -> Option<&str> {
        self.fault.as_deref()
    }

    pub fn gone(&self) -> Option<&Gone> {
        self.gone.as_ref()
    }

    /// The conversation parsed from the item `item`, once it is read.
    pub fn conversation(&self, item: &str) -> Option<&Conversation> {
        match &self.main {
            Some(main) if *main.item == *item => Some(main),
            _ => match self.subagents.get(item)? {
                Load::Loaded(read) => Some(&read.conversation),
                _ => None,
            },
        }
    }

    pub fn block(&self, at: &BlockRef) -> Option<&sb_core::transcript::Block> {
        self.conversation(&at.conversation)?
            .blocks()
            .get(at.block.0)
    }

    pub fn call(&self, at: &BlockRef) -> Option<&ToolCall> {
        match &self.block(at)?.content {
            Content::ToolCall(call) => Some(call),
            _ => None,
        }
    }

    pub fn subagent(&self, item: &str) -> Option<&Load<Arc<SubagentRead>>> {
        self.subagents.get(item)
    }

    pub fn prose(&self, at: &BlockRef) -> Option<&Load<Arc<Prose>>> {
        self.prose.get(at)
    }

    pub fn body(&self, at: &BlockRef) -> Option<&Load<Arc<CallBody>>> {
        self.bodies.get(at)
    }

    pub fn raw(&self, at: &BlockRef) -> Option<&Load<Arc<Text>>> {
        self.raws.get(at)
    }

    pub fn whole(&self, at: &BlockRef, part: Option<usize>) -> Option<&Load<Arc<Text>>> {
        self.wholes.get(&(at.clone(), part))
    }

    pub fn image(&self, key: &ImageKey) -> Option<&Load<Picture>> {
        self.images.get(key)
    }

    /// Whether `toggle` is open, with Full calls on or off.
    pub fn is_open(&self, toggle: &Toggle, full_calls: bool) -> bool {
        if full_calls && toggle.opened_by_full_calls() {
            !self.closed.contains(toggle)
        } else {
            self.opened.contains(toggle)
        }
    }

    /// What is loading or loaded, by kind, for automation's `state`.
    pub fn loads(&self) -> [(&'static str, usize, usize); 6] {
        fn count<K, T>(map: &HashMap<K, Load<T>>) -> (usize, usize) {
            let loaded = map
                .values()
                .filter(|load| !matches!(load, Load::Loading))
                .count();
            (map.len() - loaded, loaded)
        }
        let row = |name, (loading, loaded)| (name, loading, loaded);
        [
            row("bodies", count(&self.bodies)),
            row("subagents", count(&self.subagents)),
            row("prose", count(&self.prose)),
            row("images", count(&self.images)),
            row("raws", count(&self.raws)),
            row("wholes", count(&self.wholes)),
        ]
    }

    /// Keeps a subagent's conversation just read, with the subagents it read in place (whose
    /// records are in its own transcript) moved out of it.
    fn take_in(
        &mut self,
        kind: Option<String>,
        description: Option<String>,
        mut conversation: Conversation,
    ) -> Arc<SubagentRead> {
        let mut found = Vec::new();
        settle(&mut conversation, &mut found);
        for (item, read) in found {
            self.subagents.insert(item, Load::Loaded(read));
        }
        Arc::new(SubagentRead {
            kind,
            description,
            conversation,
        })
    }
}

/// Where the reader is.
pub enum ReaderState {
    /// No session is open.
    Closed,
    /// The session is being parsed.
    Opening(SessionId),
    /// Its items could not be found, for the reason given.
    Failed {
        id: SessionId,
        why: String,
    },
    Open(Box<OpenSession>),
}

impl ReaderState {
    /// `"closed"`, `"opening"`, `"failed"` or `"open"`, as automation's `state` spells it.
    pub fn key(&self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Opening(_) => "opening",
            Self::Failed { .. } => "failed",
            Self::Open(_) => "open",
        }
    }

    fn id(&self) -> Option<&SessionId> {
        match self {
            Self::Closed => None,
            Self::Opening(id) | Self::Failed { id, .. } => Some(id),
            Self::Open(open) => Some(&open.id),
        }
    }
}

pub struct ReaderService {
    archive: Entity<ArchiveService>,
    library: Entity<LibraryService>,
    providers: Providers,
    state: ReaderState,
    /// Whether every call is shown open (DESIGN §4.2.1), for the app.
    full_calls: bool,
    /// Counts the opens, so that what an earlier one asked for is dropped when it lands.
    generation: u64,
    /// The loads of this generation not landed yet.
    loading: usize,
    /**
     * Cleared when the session closes: a load of it that hasn't started by then never does,
     * so that a closed session's queue doesn't keep the background threads from the next.
     * (One already running runs on: every read and parse is bounded.)
     */
    alive: Arc<AtomicBool>,
    /// The images being decoded ([`Decoding`]): at most [`DECODES`].
    decoding: Arc<AtomicUsize>,
    _observers: [Subscription; 2],
}

impl ReaderService {
    pub fn new(
        archive: Entity<ArchiveService>,
        library: Entity<LibraryService>,
        providers: Providers,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            _observers: [
                cx.observe(&library, |this, _, cx| this.follow_library(cx)),
                cx.observe(&archive, |this, _, cx| this.follow_library(cx)),
            ],
            archive,
            library,
            providers,
            state: ReaderState::Closed,
            full_calls: false,
            generation: 0,
            loading: 0,
            alive: Arc::new(AtomicBool::new(true)),
            decoding: Arc::default(),
        }
    }

    pub fn state(&self) -> &ReaderState {
        &self.state
    }

    pub fn open_session(&self) -> Option<&OpenSession> {
        match &self.state {
            ReaderState::Open(open) => Some(open),
            _ => None,
        }
    }

    pub fn full_calls(&self) -> bool {
        self.full_calls
    }

    /// Nothing is being parsed or loaded.
    pub fn is_settled(&self) -> bool {
        !matches!(self.state, ReaderState::Opening(_)) && self.loading == 0
    }

    /// Shows every call open, or each as the owner left it. Rows closed by hand in Full calls
    /// open again with the next flip.
    pub fn set_full_calls(&mut self, full_calls: bool, cx: &mut Context<Self>) {
        if self.full_calls != full_calls {
            self.full_calls = full_calls;
            if let ReaderState::Open(open) = &mut self.state {
                open.closed.clear();
            }
            cx.notify();
        }
    }

    /// Opens a row that is closed, or closes one that is open.
    pub fn toggle(&mut self, toggle: Toggle, cx: &mut Context<Self>) {
        let full_calls = self.full_calls;
        let ReaderState::Open(open) = &mut self.state else {
            return;
        };
        let set = if full_calls && toggle.opened_by_full_calls() {
            &mut open.closed
        } else {
            &mut open.opened
        };
        if !set.remove(&toggle) {
            set.insert(toggle.clone());
        }
        /* Show fewer: the whole read for Show all goes, the head is enough again. */
        if let Toggle::AllLines(at, part) = &toggle
            && !open.is_open(&toggle, full_calls)
        {
            open.wholes.remove(&(at.clone(), *part));
        }
        cx.notify();
    }

    /// Lets go of what the views have scrolled far from (the module docs).
    pub fn release(&mut self, held: Vec<Held>, cx: &mut Context<Self>) {
        let ReaderState::Open(open) = &mut self.state else {
            return;
        };
        let mut released = false;
        for held in held {
            released |= match held {
                Held::Prose(at) => open.prose.remove(&at).is_some(),
                Held::Body(at) => open.bodies.remove(&at).is_some(),
                Held::Image(key) => match open.images.remove(&key) {
                    Some(Load::Loaded(picture)) => {
                        cx.drop_image(picture.image, None);
                        true
                    }
                    other => other.is_some(),
                },
                Held::Raw(at) => open.raws.remove(&at).is_some(),
                Held::Whole(at, part) => open.wholes.remove(&(at, part)).is_some(),
                Held::Subagent(item) => open.subagents.remove(&item).is_some(),
            };
        }
        if released {
            cx.notify();
        }
    }

    /// Opens the session the library has open, or closes the reader when it has none.
    fn follow_library(&mut self, cx: &mut Context<Self>) {
        let wanted = self
            .library
            .read(cx)
            .open_session()
            .map(|session| session.id.clone());
        if wanted.as_ref() == self.state.id() {
            return;
        }
        match wanted {
            Some(id) => self.open(id, cx),
            None => self.close(cx),
        }
    }

    /// Parses the session `id` again: after a failure, or to read past a read that failed.
    pub fn reopen(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.state.id().cloned() {
            self.open(id, cx);
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.loading = 0;
        self.alive.store(false, Ordering::Relaxed);
        self.alive = Arc::new(AtomicBool::new(true));
        let before = std::mem::replace(&mut self.state, ReaderState::Closed);
        if let ReaderState::Open(open) = &before {
            for image in open.images.values() {
                if let Load::Loaded(picture) = image {
                    cx.drop_image(picture.image.clone(), None);
                }
            }
        }
        cx.background_spawn(async move { drop(before) }).detach();
        cx.notify();
    }

    fn open(&mut self, id: SessionId, cx: &mut Context<Self>) {
        self.close(cx);
        /* Without the archive there is nothing to read: the reader stays closed rather than
        show the session it had. */
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        self.state = ReaderState::Opening(id.clone());
        let generation = self.generation;
        let providers = self.providers.clone();
        let job = cx.background_spawn({
            let id = id.clone();
            async move { parse(&archive, &providers, &id) }
        });
        cx.spawn(async move |this, cx| {
            let parsed = job.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    cx.background_spawn(async move { drop(parsed) }).detach();
                    return;
                }
                this.state = match parsed {
                    Ok(open) => ReaderState::Open(Box::new(open)),
                    Err(why) => {
                        log::warn!("a session could not be opened: {why}");
                        ReaderState::Failed { id, why }
                    }
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /**
     * Runs `work` over the open session's items on a background task, unless `claim` says it
     * is already loading or loaded: `claim` marks it loading, and `done` takes in what `work`
     * read, if the session is still the one open when it lands.
     */
    fn load<T: Send + 'static>(
        &mut self,
        claim: impl FnOnce(&mut OpenSession) -> bool,
        work: impl FnOnce(&dyn Provider, &ArchivedSession) -> T + Send + 'static,
        done: impl FnOnce(&mut OpenSession, T, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let ReaderState::Open(open) = &mut self.state else {
            return;
        };
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        if !claim(open) {
            return;
        }
        let (pinned, cache, provider) = (open.pinned.clone(), open.cache.clone(), open.provider);
        let providers = self.providers.clone();
        let generation = self.generation;
        let alive = self.alive.clone();
        self.loading += 1;
        let job = cx.background_spawn(async move {
            alive.load(Ordering::Relaxed).then(|| {
                let items = archive.pinned_session(&pinned, cache);
                work(providers[provider].as_ref(), &items)
            })
        });
        cx.spawn(async move |this, cx| {
            let value = job.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading -= 1;
                if let (ReaderState::Open(open), Some(value)) = (&mut this.state, value) {
                    done(open, value, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Prepares the prose of the block at `at`: a prompt, a reply, a thought, or a
    /// compaction's summary.
    pub fn want_prose(&mut self, at: &BlockRef, cx: &mut Context<Self>) {
        enum Source {
            Markdown(String),
            Plain(String),
            Stored(Stored),
        }
        let source = match self
            .open_session()
            .and_then(|open| open.block(at))
            .map(|block| &block.content)
        {
            Some(Content::Prompt { text, .. } | Content::Reply { text }) => {
                Source::Markdown(text.clone())
            }
            Some(Content::Thinking { text, .. }) => Source::Plain(text.clone()),
            Some(Content::Compaction {
                summary: Some(summary),
                ..
            }) => Source::Stored(summary.clone()),
            _ => return,
        };
        let key = at.clone();
        self.load(
            |open| claim(&mut open.prose, &key),
            move |_, items| match source {
                Source::Markdown(text) => Ok(Arc::new(Prose::markdown(&text))),
                Source::Plain(text) => Ok(Arc::new(Prose::plain(&text))),
                Source::Stored(stored) => stored
                    .load_text(items)
                    .map(|text| Arc::new(Prose::markdown(&text)))
                    .map_err(|error| error.to_string()),
            },
            {
                let at = at.clone();
                move |open, prose, _| {
                    land(&mut open.prose, at, loaded(prose));
                }
            },
            cx,
        );
    }

    /// Loads the body of the call at `at`, unless it is loading or loaded.
    pub fn want_body(&mut self, at: &BlockRef, cx: &mut Context<Self>) {
        let Some(call) = self.open_session().and_then(|open| open.call(at)) else {
            return;
        };
        let (input, output) = (
            call.raw_input.clone(),
            call.result.as_ref().map(|r| r.output.clone()),
        );
        let key = at.clone();
        self.load(
            |open| claim(&mut open.bodies, &key),
            move |_, items| {
                let head = |stored: &Stored, json: bool| {
                    stored
                        .load_head(items, HEAD_BYTES)
                        .map(|(text, rest)| {
                            Arc::new(match rest {
                                0 => Text::new(if json { pretty(text) } else { text }),
                                rest => Text::head(text, rest),
                            })
                        })
                        .map_err(|error| error.to_string())
                };
                Arc::new(CallBody {
                    input: head(&input, true),
                    output: output
                        .unwrap_or_default()
                        .iter()
                        .map(|part| match part {
                            Output::Text(stored) => Part::Text(head(stored, false)),
                            Output::Image(_) => Part::Image,
                        })
                        .collect(),
                })
            },
            {
                let at = at.clone();
                move |open, body, _| {
                    land(&mut open.bodies, at, Load::Loaded(body));
                }
            },
            cx,
        );
    }

    /**
     * Reads the whole of the input (`part` `None`) or of an output part of the call at `at`,
     * whose body has only its head: wanted when Show all is pressed under it.
     */
    pub fn want_whole(&mut self, at: &BlockRef, part: Option<usize>, cx: &mut Context<Self>) {
        let Some(call) = self.open_session().and_then(|open| open.call(at)) else {
            return;
        };
        let stored = match part {
            None => Some(call.raw_input.clone()),
            Some(index) => {
                call.result
                    .as_ref()
                    .and_then(|result| match result.output.get(index)? {
                        Output::Text(stored) => Some(stored.clone()),
                        Output::Image(_) => None,
                    })
            }
        };
        let Some(stored) = stored else {
            return;
        };
        let key = (at.clone(), part);
        self.load(
            |open| claim(&mut open.wholes, &key),
            move |_, items| {
                stored
                    .load_text(items)
                    /* The input as its head shows it when it is whole: pretty-printed. */
                    .map(|text| {
                        Arc::new(Text::new(if part.is_none() { pretty(text) } else { text }))
                    })
                    .map_err(|error| error.to_string())
            },
            {
                let key = (at.clone(), part);
                move |open, whole, _| land(&mut open.wholes, key, loaded(whole))
            },
            cx,
        );
    }

    /**
     * Copies the input of the call at `at`, or every text part of its output: from its body when
     * that has them whole, else read whole off the UI thread first.
     */
    pub fn copy_call(&mut self, at: &BlockRef, output: bool, cx: &mut Context<Self>) {
        let Some(open) = self.open_session() else {
            return;
        };
        /* What the body shows, when it shows all of it. */
        if let Some(Load::Loaded(body)) = open.body(at) {
            let parts: Vec<&Result<Arc<Text>, String>> = if output {
                body.output
                    .iter()
                    .filter_map(|part| match part {
                        Part::Text(text) => Some(text),
                        Part::Image => None,
                    })
                    .collect()
            } else {
                vec![&body.input]
            };
            if parts
                .iter()
                .all(|part| part.as_ref().is_ok_and(|it| it.rest.is_none()))
            {
                let parts = parts.into_iter().map(|part| match part {
                    Ok(text) => Ok(text.text.clone()),
                    Err(why) => Err(why.clone()),
                });
                copy_parts(parts, cx);
                return;
            }
        }
        let Some(call) = open.call(at) else {
            return;
        };
        let stored: Vec<Stored> = if output {
            call.result
                .iter()
                .flat_map(|result| &result.output)
                .filter_map(|part| match part {
                    Output::Text(stored) => Some(stored.clone()),
                    Output::Image(_) => None,
                })
                .collect()
        } else {
            vec![call.raw_input.clone()]
        };
        self.load(
            |_| true,
            move |_, items| {
                stored
                    .iter()
                    .map(|stored| {
                        let text = stored.load_text(items).map_err(|error| error.to_string())?;
                        Ok(if output { text } else { pretty(text) })
                    })
                    .collect::<Vec<_>>()
            },
            |_, parts, cx| copy_parts(parts.into_iter(), cx),
            cx,
        );
    }

    /// Reads the conversation of the subagent whose item is `item`, unless it is being read.
    pub fn want_subagent(&mut self, item: &Arc<str>, cx: &mut Context<Self>) {
        let key = item.clone();
        let read = item.clone();
        self.load(
            |open| claim(&mut open.subagents, &key),
            move |provider, items| provider.subagent(items, &read),
            {
                let item = item.clone();
                move |open, subagent, _| {
                    let loaded = match subagent {
                        Subagent::Read {
                            kind,
                            description,
                            conversation,
                        } => Load::Loaded(open.take_in(kind, description, *conversation)),
                        Subagent::Unread { .. } => {
                            Load::Failed("its agent's provider reads no subagents".to_string())
                        }
                    };
                    land(&mut open.subagents, item, loaded);
                }
            },
            cx,
        );
    }

    /**
     * Works out which subagent started which (the module docs), from the provider's outlines
     * of them: wanted once those no call of the session's own started come into view. Each is
     * placed under the first subagent found to start it, walking from those the session's own
     * calls started and then from the rest in turn, so that a subagent claimed by one it
     * started, or by itself, is still reached: such a claim is passed over.
     */
    pub fn want_placement(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open_session() else {
            return;
        };
        let roots = open.unattached.clone();
        let first: Vec<Arc<str>> = open
            .main
            .iter()
            .flat_map(|main| main.calls())
            .flat_map(|(_, call)| call.subagents.iter().map(|it| it.item().clone()))
            .collect();
        self.load(
            |open| {
                if open.placement.is_some() {
                    return false;
                }
                open.placement = Some(Load::Loading);
                true
            },
            move |provider, items| {
                let outlines: HashMap<Arc<str>, Outline> = provider
                    .outlines(items)
                    .into_iter()
                    .map(|outline| (outline.item.clone(), outline))
                    .collect();
                let mut placement = Placement::default();
                let mut seen: HashSet<Arc<str>> = first.iter().cloned().collect();
                let mut queue: VecDeque<Arc<str>> = first.into();
                let mut roots = roots.into_iter();
                while let Some(item) = queue.pop_front().or_else(|| {
                    let root = roots.find(|root| !seen.contains(root))?;
                    seen.insert(root.clone());
                    Some(root)
                }) {
                    let Some(outline) = outlines.get(&item) else {
                        continue;
                    };
                    for child in &outline.started {
                        if seen.insert(child.clone()) {
                            placement.parents.insert(child.clone(), item.clone());
                            queue.push_back(child.clone());
                        }
                    }
                }
                placement.names = outlines
                    .into_iter()
                    .map(|(item, outline)| {
                        let names = Names {
                            kind: outline.kind,
                            description: outline.description,
                        };
                        (item, names)
                    })
                    .collect();
                Arc::new(placement)
            },
            |open, placement, _| open.placement = Some(Load::Loaded(placement)),
            cx,
        );
    }

    /// Loads an unrecognised record as it was recorded.
    pub fn want_raw(&mut self, at: &BlockRef, cx: &mut Context<Self>) {
        let Some(Content::Unrecognised(record)) = self
            .open_session()
            .and_then(|open| open.block(at))
            .map(|block| &block.content)
        else {
            return;
        };
        let raw = record.raw.clone();
        let key = at.clone();
        self.load(
            |open| claim(&mut open.raws, &key),
            move |_, items| {
                raw.load_text(items)
                    .map(|text| Arc::new(Text::new(pretty(text))))
                    .map_err(|error| error.to_string())
            },
            {
                let at = at.clone();
                move |open, raw, _| {
                    land(&mut open.raws, at, loaded(raw));
                }
            },
            cx,
        );
    }

    /// Decodes an image: one of a prompt's, or one a call returned.
    pub fn want_image(&mut self, key: &ImageKey, cx: &mut Context<Self>) {
        let image = match (
            self.open_session()
                .and_then(|open| open.block(&key.at))
                .map(|block| &block.content),
            key.slot,
        ) {
            (Some(Content::Prompt { images, .. }), Slot::Prompt(index)) => images.get(index),
            (Some(Content::ToolCall(call)), Slot::Output(index)) => {
                call.result
                    .as_ref()
                    .and_then(|result| match result.output.get(index)? {
                        Output::Image(image) => Some(image),
                        Output::Text(_) => None,
                    })
            }
            _ => None,
        };
        let Some(image) = image.cloned() else {
            return;
        };
        if self.decoding.load(Ordering::Relaxed) >= DECODES {
            return;
        }
        let claimed = key.clone();
        /* Counted from now; dropped with the work if it isn't started after all. */
        let decoding = Decoding::start(&self.decoding);
        self.load(
            |open| claim(&mut open.images, &claimed),
            move |_, items| {
                let _decoding = decoding;
                image
                    .data
                    .load(items)
                    .map_err(|error| error.to_string())
                    .and_then(|bytes| picture::decode(&image.media_type, &bytes))
            },
            {
                let key = key.clone();
                move |open, picture, _| {
                    land(&mut open.images, key, loaded(picture));
                }
            },
            cx,
        );
    }
}

/**
 * A decode being run, counted in the service's `decoding` from when it is asked for until it
 * ends, or is dropped without running: not by its row's `Loading`, which a row that lets go
 * of it removes while it runs on.
 */
struct Decoding(Arc<AtomicUsize>);

impl Decoding {
    fn start(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        Self(count.clone())
    }
}

impl Drop for Decoding {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Marks `key` loading in `map`; `false` when it is loading or loaded already.
fn claim<K: Eq + std::hash::Hash + Clone, T>(map: &mut HashMap<K, Load<T>>, key: &K) -> bool {
    if map.contains_key(key) {
        return false;
    }
    map.insert(key.clone(), Load::Loading);
    true
}

/**
 * Puts what a load read for `key` in `map`, if `key` is still loading there: a row that let go
 * of it while it was being read (or a new read since) must not get it back, or nothing would
 * release it again.
 */
fn land<K: Eq + std::hash::Hash, T>(map: &mut HashMap<K, Load<T>>, key: K, value: Load<T>) {
    if let Some(slot @ Load::Loading) = map.get_mut(&key) {
        *slot = value;
    }
}

fn loaded<T>(result: Result<T, String>) -> Load<T> {
    match result {
        Ok(value) => Load::Loaded(value),
        Err(why) => Load::Failed(why),
    }
}

/**
 * Copies `parts` as a body shows them, joined: each as it reads, its bidirectional controls
 * shown (`⟦U+202E⟧`), and one that couldn't be read saying so.
 */
fn copy_parts(parts: impl Iterator<Item = Result<String, String>>, cx: &mut gpui::App) {
    let text = parts
        .map(|part| match part {
            Ok(text) => crate::hostile::bidi(&text).into_owned(),
            Err(why) => format!("[This part couldn't be read: {why}]"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        crate::system::copy_text(text, cx);
    }
}

/// The most of a call's input pretty-printed as JSON: past it, parsing it to print it again
/// takes long and holds several copies; it is shown as it was sent.
const PRETTY_BYTES: usize = 1024 * 1024;

/// `text` pretty-printed when it is JSON of at most [`PRETTY_BYTES`], else as it is.
fn pretty(text: String) -> String {
    if text.len() > PRETTY_BYTES {
        return text;
    }
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or(text)
}

/// Moves each subagent read with a conversation (one whose records are in its own transcript)
/// out of its call into `found`, so that every subagent is looked up by item.
fn settle(conversation: &mut Conversation, found: &mut Vec<(Arc<str>, Arc<SubagentRead>)>) {
    for at in 0..conversation.blocks().len() {
        let Some(call) = conversation.call_mut(BlockId(at)) else {
            continue;
        };
        for subagent in &mut call.subagents {
            let unread = Subagent::Unread {
                item: subagent.item().clone(),
            };
            if let Subagent::Read {
                kind,
                description,
                mut conversation,
            } = std::mem::replace(subagent, unread)
            {
                settle(&mut conversation, found);
                found.push((
                    conversation.item.clone(),
                    Arc::new(SubagentRead {
                        kind,
                        description,
                        conversation: *conversation,
                    }),
                ));
            }
        }
    }
}

/// Parses the session `id` with its agent's provider: what [`ReaderService::open`] runs off
/// the UI thread.
fn parse(archive: &Archive, providers: &Providers, id: &SessionId) -> Result<OpenSession, String> {
    let provider = providers
        .iter()
        .position(|provider| provider.id().as_str() == id.provider)
        .ok_or_else(|| format!("this build has no provider for {}", id.provider))?;
    let cache = Arc::new(SegmentCache::new(CACHED_SEGMENTS, CACHED_BYTES));
    let items = archive
        .session(id)
        .map_err(|error| error.to_string())?
        .with_cache(cache.clone());
    let transcript = providers[provider].parse(&items);
    let catalogue = archive.catalogue();
    let gone = items
        .list()
        .iter()
        .find(|item| item.kind == ItemKind::Transcript)
        .and_then(|main| {
            catalogue
                .item(&id.location, &id.provider, &main.key)
                .ok()
                .flatten()?
                .gone_since
        })
        .and_then(|since| {
            let location = catalogue.location(&id.location).ok().flatten()?;
            Some(Gone {
                location: location.name,
                since,
            })
        });
    let mut open = OpenSession {
        id: id.clone(),
        provider,
        pinned: items.pinned().clone(),
        cache,
        main: transcript.conversation,
        unattached: transcript
            .unattached
            .iter()
            .map(|subagent| subagent.item().clone())
            .collect(),
        fault: items.fault(),
        gone,
        prose: HashMap::new(),
        subagents: HashMap::new(),
        bodies: HashMap::new(),
        raws: HashMap::new(),
        wholes: HashMap::new(),
        images: HashMap::new(),
        placement: None,
        opened: HashSet::new(),
        closed: HashSet::new(),
    };
    let mut found = Vec::new();
    if let Some(main) = &mut open.main {
        settle(main, &mut found);
        let item = main.item.clone();
        let eager = main
            .turns()
            .iter()
            .flat_map(|turn| &turn.blocks)
            .filter_map(|&id| match &main[id].content {
                Content::Prompt { text, .. } | Content::Reply { text } => Some((id, text)),
                _ => None,
            })
            .take(EAGER_PROSE)
            .filter(|(_, text)| text.len() <= prose::PIECE_BYTES)
            .map(|(id, text)| {
                (
                    BlockRef {
                        conversation: item.clone(),
                        block: id,
                    },
                    Load::Loaded(Arc::new(Prose::markdown(text))),
                )
            });
        open.prose.extend(eager);
    }
    for subagent in transcript.unattached {
        if let Subagent::Read {
            kind,
            description,
            conversation,
        } = subagent
        {
            found.push((
                conversation.item.clone(),
                open.take_in(kind, description, *conversation),
            ));
        }
    }
    for (item, read) in found {
        open.subagents.insert(item, Load::Loaded(read));
    }
    Ok(open)
}
