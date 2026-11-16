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
//! task ([`Load`]): a prompt's or a reply's prose ([`Prose`]: cut into pieces, sanitised), the
//! highlights of a piece of fenced code ([`Code`]), a call's body when its row is open, a
//! subagent's conversation when its call is, a compaction's summary, an image ([`Picture`]: one
//! bounded frame). The first few prompts and replies are prepared with the parse, so the first
//! turns paint with it, while the prose's budget lasts. Loads read the items at the versions the
//! parse read ([`Pinned`]) through a [`SegmentCache`] of the last segments decoded, so opening
//! one row after another near the end of a long transcript decodes its last segment once.
//! Which subagent started which ([`Placement`]) is worked out once the subagents no call of the
//! session's own started come into view.
//!
//! **Jobs.** Prose and highlights take the most time, so they are jobs ([`Job`]), at most
//! [`JOBS`] running at once on gpui's background executor, whose other threads stay free for
//! sync and search. The rest wait in a queue, the rows drawn last first, nearest the top of the
//! column first ([`Rank`]). There is never more than one job for one thing. A row let go of
//! drops its job from the queue; one running is told to stop at its next piece of prose or
//! segment of code, and what it made is dropped (a row that came back meanwhile has it
//! prepared again). Highlights are kept by the code's text, up to [`highlights::KEPT_BYTES`],
//! so a block scrolled back to costs nothing; `TextView`'s fenced code asks for them too
//! ([`highlights`]).
//!
//! **Untrusted content.** Every job and load runs over a transcript's content, so each is
//! [`guarded`]: a panic in one is caught, logged, and shown as plain code or as text that
//! couldn't be read. It never reaches gpui's executor, which would abort the process.
//!
//! **What changed.** A landing that changes how a few rows are laid out (a block's prose
//! prepared, a call's body read, either let go of) is a [`Changed`] event, so the views lay
//! out those rows again and no others; one that changes only what rows draw (highlights, an
//! image) asks them to paint. Only what changes the rows' structure (a session opened or
//! closed, a row opened or closed, Full calls, a subagent read or let go of) notifies.
//!
//! **Letting go.** The views say what they have scrolled far from ([`ReaderService::release`]):
//! its prose, bodies and images are dropped, an image's texture with it, and prepared again if
//! it comes back. Closing the reader drops all of it, off the UI thread.
//!
//! **What is shown open.** The Activity / Full calls switch is the app's, not a session's
//! (DESIGN §4.2.1). Each row the owner opens or closes by hand is a [`Toggle`]: in Activity a
//! toggle opens a row; in Full calls, where every call, thought and subagent's call list is
//! open, it closes one until the switch is flipped again (DESIGN §4.2.3).

pub mod highlights;
pub mod picture;
pub mod prose;
#[cfg(test)]
mod tests;
pub mod text;

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use gpui::{AppContext as _, Context, Entity, EventEmitter, Subscription};
use sb_core::archive::{Archive, ArchivedSession, Pinned, SegmentCache};
use sb_core::catalogue::SessionId;
use sb_core::provider::{ItemKind, Provider, SessionItems};
use sb_core::transcript::{
    BlockId, Content, Conversation, Outline, Output, Stored, Subagent, ToolCall, place,
};

use super::{ArchiveService, LibraryService, Providers};
use crate::syntax::TokenKind;
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
/// with the parse, so that the first turns paint with it: those of at most a piece each, for at
/// most [`prose::PARSE_BUDGET`] all told.
const EAGER_PROSE: usize = 24;

/// How many jobs run at once (the module docs).
const JOBS: usize = 2;

/**
 * Where a row that asks for a job was drawn, to order the jobs waiting: those of rows drawn in
 * the newest frame first, of those the nearest the top of the column.
 */
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rank {
    /// The view's count of its frames.
    pub frame: u64,
    /// How many rows the row is from the one at the top of the column.
    pub distance: usize,
}

impl Rank {
    /// The least first.
    fn order(self) -> (Reverse<u64>, usize) {
        (Reverse(self.frame), self.distance)
    }
}

/// A piece of fenced code to highlight: its language as its fence names it, and its text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Code {
    pub language: Arc<str>,
    pub text: Arc<str>,
}

/// The highlights of a piece of code: where each kind of token is. Empty when it is drawn plain.
pub type Highlights = Arc<[(Range<usize>, TokenKind)]>;

/// What a job is for (the module docs).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Job {
    Prose(BlockRef),
    Highlight(Code),
}

/// A job running: its flag, cleared when what it is for is let go of, and the rank it was
/// started at, to queue it again by.
struct Running {
    wanted: Arc<AtomicBool>,
    rank: Rank,
}

/// What a job made, and what for.
enum Done {
    Prose(BlockRef, Result<Arc<Prose>, String>),
    /// `None` when the code is drawn plain.
    Highlight(Code, Option<Vec<(Range<usize>, TokenKind)>>),
    /// It was let go of before it finished: what it made is dropped.
    Dropped(Job),
}

impl Done {
    fn job(&self) -> Job {
        match self {
            Self::Prose(at, _) => Job::Prose(at.clone()),
            Self::Highlight(code, _) => Job::Highlight(code.clone()),
            Self::Dropped(job) => job.clone(),
        }
    }

    /// What `job` gives when it fails, for the reason `why`: plain code, or prose that couldn't
    /// be read.
    fn failed(job: Job, why: String) -> Self {
        match job {
            Job::Prose(at) => Self::Prose(at, Err(why)),
            Job::Highlight(code) => Self::Highlight(code, None),
        }
    }
}

/**
 * Whether a job is to go on, as it asks it: `going`, and whether it ever said no, after which
 * what the job makes is dropped (part of it was made as if it didn't matter).
 */
struct Going<F> {
    going: F,
    stopped: AtomicBool,
}

impl<F: Fn() -> bool> Going<F> {
    fn new(going: F) -> Self {
        Self {
            going,
            stopped: AtomicBool::new(false),
        }
    }

    fn still(&self) -> bool {
        let going = (self.going)();
        if !going {
            self.stopped.store(true, Ordering::Relaxed);
        }
        going
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }
}

/**
 * What changed of the open session that only some rows show (the module docs): those rows are
 * laid out again, or painted again. The rest of what changes notifies.
 */
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Changed {
    /// A block's prose was prepared, failed, or was let go of.
    Prose(BlockRef),
    /// A call's body, or the whole of a part of it, was read, or let go of.
    Body(BlockRef),
    /// A record as recorded was read, or let go of.
    Raw(BlockRef),
    /// A piece of code's highlights were made.
    Highlight(Code),
    /// Nothing is laid out differently, but what rows draw may be: an image decoded, a
    /// decode's place freed for a row waiting for one, `TextView`'s highlights made.
    Paint,
}

impl EventEmitter<Changed> for ReaderService {}

/// The text of a block's prose, as it is prepared: Markdown, plain, or Markdown to read.
enum Source {
    Markdown(String),
    Plain(String),
    Stored(Stored),
}

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
    /// A piece of code's highlights, while they are waited for: the job waiting is dropped.
    /// Those made are kept by the code's text ([`HIGHLIGHTS_KEPT`]).
    Highlight(Code),
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
    /**
     * The subagents read with the session's own conversation, whose records are in its
     * transcript: kept for as long as the session is open, as reading one again would read the
     * session again.
     */
    read_with_session: HashSet<Arc<str>>,
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

    /// Which subagent started which, once it is asked for ([`ReaderService::want_placement`]).
    pub fn placement(&self) -> Option<&Load<Arc<Placement>>> {
        self.placement.as_ref()
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
    /// The jobs asked for and not started, with their rows' ranks (the module docs).
    queued: HashMap<Job, Rank>,
    /// The jobs of this generation running.
    running: HashMap<Job, Running>,
    /// How many jobs are running, of any generation: at most [`JOBS`].
    busy: usize,
    /// The highlights made lately, for the app ([`highlights`]).
    highlights: Arc<Mutex<highlights::Kept>>,
    /// The code `TextView` asked for whose highlights are not made yet: when they land, its
    /// highlighter is renewed so that it draws them.
    for_text_view: HashSet<Code>,
    _observers: [Subscription; 2],
}

impl ReaderService {
    pub fn new(
        archive: Entity<ArchiveService>,
        library: Entity<LibraryService>,
        providers: Providers,
        cx: &mut Context<Self>,
    ) -> Self {
        let asked = highlights::asked(cx);
        cx.spawn(async move |this, cx| {
            while let Ok(code) = asked.recv().await {
                let wanted = this.update(cx, |this, cx| {
                    this.for_text_view.insert(code.clone());
                    this.want_highlight(code, Rank::default(), cx);
                });
                if wanted.is_err() {
                    break;
                }
            }
        })
        .detach();
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
            queued: HashMap::new(),
            running: HashMap::new(),
            busy: 0,
            highlights: highlights::kept(cx),
            for_text_view: HashSet::new(),
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
        !matches!(self.state, ReaderState::Opening(_))
            && self.loading == 0
            && self.queued.is_empty()
            && self.running.is_empty()
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
        let mut structure = false;
        for held in held {
            let changed = match held {
                Held::Prose(at) => {
                    let_go_of(&mut self.queued, &self.running, &Job::Prose(at.clone()));
                    open.prose.remove(&at).map(|_| Changed::Prose(at))
                }
                Held::Highlight(code) => {
                    let_go_of(&mut self.queued, &self.running, &Job::Highlight(code));
                    None
                }
                Held::Body(at) => open.bodies.remove(&at).map(|_| Changed::Body(at)),
                /* An image far from what is drawn: nothing that is drawn changes. */
                Held::Image(key) => {
                    if let Some(Load::Loaded(picture)) = open.images.remove(&key) {
                        cx.drop_image(picture.image, None);
                    }
                    None
                }
                Held::Raw(at) => open.raws.remove(&at).map(|_| Changed::Raw(at)),
                Held::Whole(at, part) => open
                    .wholes
                    .remove(&(at.clone(), part))
                    .map(|_| Changed::Body(at)),
                Held::Subagent(item) => {
                    structure |= !open.read_with_session.contains(&item)
                        && open.subagents.remove(&item).is_some();
                    None
                }
            };
            if let Some(changed) = changed {
                cx.emit(changed);
            }
        }
        if structure {
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
        /* A job running goes no further than its next piece; it still counts in `busy` until it
        ends. */
        for running in self.running.values() {
            running.wanted.store(false, Ordering::Relaxed);
        }
        self.running.clear();
        self.queued.clear();
        self.for_text_view.clear();
        let before = std::mem::replace(&mut self.state, ReaderState::Closed);
        if let ReaderState::Open(open) = &before {
            for image in open.images.values() {
                if let Load::Loaded(picture) = image {
                    cx.drop_image(picture.image.clone(), None);
                }
            }
        }
        let dropped = cx.background_spawn(async move { drop(before) });
        cx.spawn(async move |_, cx| {
            dropped.await;
            cx.update(|cx| crate::system::give_back_memory(cx));
        })
        .detach();
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
        /* Only the newest session asked for is parsed: one passed over (J held down) is
        closed, its flag cleared, before its turn on the background comes. */
        let alive = self.alive.clone();
        let job = cx.background_spawn({
            let id = id.clone();
            async move {
                if !alive.load(Ordering::Relaxed) {
                    return Err("passed over".to_string());
                }
                guarded(|| parse(&archive, &providers, &id, &alive)).and_then(|it| it)
            }
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
     * Runs `work` over the open session's items on a background task, [`guarded`], unless
     * `claim` says it is already loading or loaded: `claim` marks it loading, and `done` takes
     * in what `work` read, or why it failed, if the session is still the one open when it
     * lands. `done` says what changed ([`Changed`]).
     */
    fn load<T: Send + 'static>(
        &mut self,
        claim: impl FnOnce(&mut OpenSession) -> bool,
        work: impl FnOnce(&dyn Provider, &ArchivedSession) -> T + Send + 'static,
        done: impl FnOnce(&mut OpenSession, Result<T, String>, &mut Context<Self>) + 'static,
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
                guarded(|| {
                    let items = archive.pinned_session(&pinned, cache);
                    work(providers[provider].as_ref(), &items)
                })
            })
        });
        cx.spawn(async move |this, cx| {
            let value = job.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    /* Dropped; but its end may free a decode's place, which a row of the
                    session open now waits for (`want_image`). */
                    cx.emit(Changed::Paint);
                    return;
                }
                this.loading -= 1;
                if let (ReaderState::Open(open), Some(value)) = (&mut this.state, value) {
                    done(open, value, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /**
     * Prepares the prose of the block at `at`, a prompt, a reply, a thought or a compaction's
     * summary, as a job (the module docs) for a row drawn at `rank`; one waiting already takes
     * the rank.
     */
    pub fn want_prose(&mut self, at: &BlockRef, rank: Rank, cx: &mut Context<Self>) {
        let ReaderState::Open(open) = &mut self.state else {
            return;
        };
        let job = Job::Prose(at.clone());
        match open.prose.get(at) {
            Some(Load::Loading) => {
                if let Some(queued) = self.queued.get_mut(&job) {
                    *queued = rank;
                }
                return;
            }
            Some(_) => return,
            None => {}
        }
        open.prose.insert(at.clone(), Load::Loading);
        self.want(job, rank, cx);
    }

    /**
     * Queues `job` for a row drawn at `rank`; one waiting already takes the rank. One still
     * running for a row that was let go of and came back is wanted again: it goes on, or, if
     * it already stopped, is queued again when it ends.
     */
    fn want(&mut self, job: Job, rank: Rank, cx: &mut Context<Self>) {
        match self.running.get(&job) {
            Some(running) => running.wanted.store(true, Ordering::Relaxed),
            None => {
                self.queued.insert(job, rank);
                self.pump(cx);
            }
        }
    }

    /// The highlights of `code`, when they have been made; asked for, they are kept longer.
    pub fn highlighted(&self, code: &Code) -> Option<Highlights> {
        self.highlights
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(code)
    }

    /// Highlights `code` as a job (the module docs) for a row drawn at `rank`.
    pub fn want_highlight(&mut self, code: Code, rank: Rank, cx: &mut Context<Self>) {
        self.want(Job::Highlight(code), rank, cx);
    }

    /// Starts the jobs waiting, the first by their rank, while fewer than [`JOBS`] run.
    fn pump(&mut self, cx: &mut Context<Self>) {
        while self.busy < JOBS {
            let Some((job, rank)) = self
                .queued
                .iter()
                .min_by_key(|(_, rank)| rank.order())
                .map(|(job, rank)| (job.clone(), *rank))
            else {
                return;
            };
            self.queued.remove(&job);
            self.start(job, rank, cx);
        }
    }

    fn start(&mut self, job: Job, rank: Rank, cx: &mut Context<Self>) {
        let wanted = Arc::new(AtomicBool::new(true));
        /* A job of a row let go of, or of a session closed, meanwhile goes no further than its
        next piece of prose or segment of code. */
        let flag = wanted.clone();
        let going = Going::new(move || flag.load(Ordering::Relaxed));
        let work: Box<dyn FnOnce() -> Done + Send> = match &job {
            Job::Highlight(code) => {
                let code = code.clone();
                Box::new(move || {
                    #[cfg(test)]
                    if *code.language == *tests::PANICS {
                        panic!("the tests' content that a job panics on");
                    }
                    let highlights =
                        crate::syntax::highlight_while(&code.language, &code.text, &|| {
                            going.still()
                        });
                    if going.stopped() {
                        return Done::Dropped(Job::Highlight(code));
                    }
                    Done::Highlight(code, highlights)
                })
            }
            Job::Prose(at) => {
                let archive = self.archive.read(cx).archive().cloned();
                let ReaderState::Open(open) = &mut self.state else {
                    return;
                };
                let (Some(source), Some(archive)) = (source(open, at), archive) else {
                    open.prose.remove(at);
                    return;
                };
                let (at, pinned, cache) = (at.clone(), open.pinned.clone(), open.cache.clone());
                Box::new(move || {
                    let wanted = || going.still();
                    let prose = match source {
                        Source::Markdown(text) => {
                            Ok(Arc::new(Prose::markdown_while(&text, &wanted)))
                        }
                        Source::Plain(text) => Ok(Arc::new(Prose::plain(&text))),
                        Source::Stored(stored) => stored
                            .load_text(&archive.pinned_session(&pinned, cache))
                            .map(|text| Arc::new(Prose::markdown_while(&text, &wanted)))
                            .map_err(|error| error.to_string()),
                    };
                    if going.stopped() {
                        return Done::Dropped(Job::Prose(at));
                    }
                    Done::Prose(at, prose)
                })
            }
        };
        self.busy += 1;
        self.running.insert(job.clone(), Running { wanted, rank });
        let generation = self.generation;
        let task = cx.background_spawn(async move {
            guarded(work).unwrap_or_else(|why| Done::failed(job, why))
        });
        cx.spawn(async move |this, cx| {
            let done = task.await;
            this.update(cx, |this, cx| this.finish(generation, done, cx))
                .ok();
        })
        .detach();
    }

    /// Takes in what a job started in `generation` made, and starts the next.
    fn finish(&mut self, generation: u64, done: Done, cx: &mut Context<Self>) {
        self.busy -= 1;
        /* A job of a session closed since is no longer among those running. */
        let running = (generation == self.generation)
            .then(|| self.running.remove(&done.job()))
            .flatten();
        match done {
            /* Wanted again since it stopped: queued again, at the rank it had. */
            Done::Dropped(job) => {
                if let Some(running) = running
                    && running.wanted.load(Ordering::Relaxed)
                {
                    self.queued.insert(job, running.rank);
                }
            }
            /* Kept by the code's text, whatever has been opened since. */
            Done::Highlight(code, highlights) => {
                self.highlights
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(
                        code.clone(),
                        highlights.map(Highlights::from).unwrap_or_default(),
                    );
                if self.for_text_view.remove(&code) {
                    highlights::renew(cx);
                    cx.emit(Changed::Paint);
                }
                cx.emit(Changed::Highlight(code));
            }
            Done::Prose(at, prose) => {
                if let (Some(_), ReaderState::Open(open)) = (running, &mut self.state)
                    && land(&mut open.prose, at.clone(), loaded(prose))
                {
                    cx.emit(Changed::Prose(at));
                }
            }
        }
        self.pump(cx);
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
                move |open, body, cx| {
                    if land(&mut open.bodies, at.clone(), loaded(body)) {
                        cx.emit(Changed::Body(at));
                    }
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
                let at = at.clone();
                move |open, whole, cx| {
                    if land(
                        &mut open.wholes,
                        (at.clone(), part),
                        loaded(whole.and_then(|it| it)),
                    ) {
                        cx.emit(Changed::Body(at));
                    }
                }
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
            |_, parts, cx| {
                if let Ok(parts) = parts {
                    copy_parts(parts.into_iter(), cx);
                }
            },
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
                move |open, subagent, cx| {
                    let loaded = match subagent {
                        Ok(Subagent::Read {
                            kind,
                            description,
                            conversation,
                        }) => Load::Loaded(open.take_in(kind, description, *conversation)),
                        Ok(Subagent::Unread { .. }) => {
                            Load::Failed("its agent's provider reads no subagents".to_string())
                        }
                        Err(why) => Load::Failed(why),
                    };
                    if land(&mut open.subagents, item, loaded) {
                        cx.notify();
                    }
                }
            },
            cx,
        );
    }

    /**
     * Works out which subagent started which (the module docs), from the provider's outlines
     * of them: wanted once those no call of the session's own started come into view, or when a
     * search match in a subagent is gone to. They are placed by the rule the index places them
     * by ([`sb_core::transcript::place`]), so the subagents left unattached are the index's,
     * in its order.
     */
    pub fn want_placement(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open_session() else {
            return;
        };
        let roots = open.unattached.clone();
        let first: Vec<Arc<str>> = (open.main.iter())
            .flat_map(|main| main.claims().cloned())
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
                let placed = place(&first, &roots, |item| {
                    (outlines.get(item)).map_or_else(Vec::new, |outline| outline.started.clone())
                });
                let mut placement = Placement {
                    parents: placed.parents,
                    names: HashMap::new(),
                };
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
            |open, placement, cx| {
                open.placement = Some(loaded(placement));
                cx.notify();
            },
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
                move |open, raw, cx| {
                    if land(&mut open.raws, at.clone(), loaded(raw.and_then(|it| it))) {
                        cx.emit(Changed::Raw(at));
                    }
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
                /* Painted again either way: a decode's place is free for a row waiting. */
                move |open, picture, cx| {
                    land(&mut open.images, key, loaded(picture.and_then(|it| it)));
                    cx.emit(Changed::Paint);
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

/// Drops `job` from `queued`, and tells it to stop if it is `running`.
fn let_go_of(queued: &mut HashMap<Job, Rank>, running: &HashMap<Job, Running>, job: &Job) {
    queued.remove(job);
    if let Some(running) = running.get(job) {
        running.wanted.store(false, Ordering::Relaxed);
    }
}

/**
 * Runs `work`, a job or a load over a transcript's content, which is untrusted (the module
 * docs): a panic in it is caught, logged, and given back as why it failed, rather than
 * unwinding into gpui's executor, whose threads abort the process when one does.
 */
fn guarded<T>(work: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).map_err(|panic| {
        let why = panic
            .downcast_ref::<&str>()
            .map(|it| it.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        log::error!("the reader failed on a session's content: {why}");
        "scrollback failed on it".to_string()
    })
}

/// The text of the prose of the block at `at`, when it has any.
fn source(open: &OpenSession, at: &BlockRef) -> Option<Source> {
    match &open.block(at)?.content {
        Content::Prompt { text, .. } | Content::Reply { text } => {
            Some(Source::Markdown(text.clone()))
        }
        Content::Thinking { text, .. } => Some(Source::Plain(text.clone())),
        Content::Compaction {
            summary: Some(summary),
            ..
        } => Some(Source::Stored(summary.clone())),
        _ => None,
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
 * release it again. Whether it was put there.
 */
fn land<K: Eq + std::hash::Hash, T>(map: &mut HashMap<K, Load<T>>, key: K, value: Load<T>) -> bool {
    match map.get_mut(&key) {
        Some(slot @ Load::Loading) => {
            *slot = value;
            true
        }
        _ => false,
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
/// the UI thread, while `alive` says the session is still the one open.
fn parse(
    archive: &Archive,
    providers: &Providers,
    id: &SessionId,
    alive: &AtomicBool,
) -> Result<OpenSession, String> {
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
        read_with_session: HashSet::new(),
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
        let started = Instant::now();
        /* The budget is asked before each piece of each body: one it runs out in is dropped,
        and prepared as its row is drawn, as the rest are. */
        let going =
            Going::new(|| alive.load(Ordering::Relaxed) && started.elapsed() < prose::PARSE_BUDGET);
        let wanted = || going.still();
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
            .take_while(|_| wanted())
            .map_while(|(id, text)| {
                let prose = Prose::markdown_while(text, &wanted);
                (!going.stopped()).then(|| {
                    (
                        BlockRef {
                            conversation: item.clone(),
                            block: id,
                        },
                        Load::Loaded(Arc::new(prose)),
                    )
                })
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
        open.read_with_session.insert(item.clone());
        open.subagents.insert(item, Load::Loaded(read));
    }
    Ok(open)
}
