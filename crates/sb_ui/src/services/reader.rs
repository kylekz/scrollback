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
//! **Loading.** What a parse leaves where it is archived ([`sb_core::transcript::Stored`]) is
//! loaded when a view first draws it: a call's body when its row is expanded, a subagent's
//! conversation when its call is, a compaction's summary, a prompt's images. Each load is a
//! background job ([`Load`]), reading the items at the versions the parse read
//! ([`Pinned`]) through a [`SegmentCache`] of the last few segments decoded, so opening one
//! row after another near the end of a long transcript decodes its last segment once.
//!
//! **What is shown open.** The Activity / Full calls switch is the app's, not a session's
//! (DESIGN §4.2.1). Each row the owner opens or closes by hand is a [`Toggle`]: in Activity a
//! toggle opens a row; in Full calls, where every call, thought and subagent's call list is
//! open, it closes one until the switch is flipped again (DESIGN §4.2.3).
//!
//! Closing the reader drops all of it, off the UI thread.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::ops::Range;
use std::sync::Arc;

use gpui::{AppContext as _, Context, Entity, ImageFormat, Subscription};
use sb_core::archive::{Archive, ArchivedSession, Pinned, SegmentCache};
use sb_core::catalogue::SessionId;
use sb_core::provider::{ItemKind, Provider, SessionItems};
use sb_core::transcript::{BlockId, Content, Conversation, Image, Output, Subagent, ToolCall};

use super::{ArchiveService, LibraryService, Providers};
use crate::markdown::Sanitized;

/// How many decoded segments an open session keeps: each is up to 8 MiB.
const CACHED_SEGMENTS: usize = 4;

/// The most of a line a call body draws; the rest is clipped by the body anyway.
const LINE_BYTES: usize = 512;

/// A body over this is sanitised with the parse, off the UI thread; a smaller one is sanitised
/// when its row is first drawn, which costs well under a millisecond.
pub const SANITIZE_ON_DRAW_BYTES: usize = 4 * 1024;

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

/// A row the owner opened or closed by hand (the module docs).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Toggle {
    /// A call's body, or a subagent's panel.
    Call(BlockRef),
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

/// Text as a call body shows it: whole, and cut into lines, each at most [`LINE_BYTES`] long.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text {
    pub text: String,
    lines: Vec<Range<usize>>,
}

impl Text {
    pub fn new(text: String) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for line in text.split_inclusive('\n') {
            let shown = line.trim_end_matches(['\n', '\r']);
            let mut end = shown.len().min(LINE_BYTES);
            while !shown.is_char_boundary(end) {
                end -= 1;
            }
            lines.push(start..start + end);
            start += line.len();
        }
        Self { text, lines }
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// The lines `range`, joined: what a body draws of them.
    pub fn lines(&self, range: Range<usize>) -> String {
        self.lines[range.start.min(self.lines.len())..range.end.min(self.lines.len())]
            .iter()
            .map(|line| &self.text[line.clone()])
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// An image from the transcript, ready to draw.
#[derive(Clone, Debug)]
pub struct Picture {
    pub image: Arc<gpui::Image>,
    pub media_type: String,
    /// Its size in pixels, from its header.
    pub width: u32,
    pub height: u32,
    /// How many bytes it is.
    pub bytes: usize,
}

impl PartialEq for Picture {
    fn eq(&self, other: &Self) -> bool {
        self.image.id() == other.image.id()
    }
}

impl Eq for Picture {}

/// One part of what a tool returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Text(Result<Text, String>),
    Image(Result<Picture, String>),
}

/// A call's body: its input as it was sent, and its output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallBody {
    pub input: Result<Text, String>,
    pub output: Vec<Part>,
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
    /// The subagents no call is known to have started, by item.
    unattached: Vec<Arc<str>>,
    /// Why a read failed for a reason that is not the stored bytes (a file in use, a disk
    /// error): what was parsed around it is then not all the session holds, and opening it
    /// again may read more.
    fault: Option<String>,
    gone: Option<Gone>,
    /// Bodies over [`SANITIZE_ON_DRAW_BYTES`], sanitised with the parse or the subagent's read.
    prose: HashMap<BlockRef, Sanitized>,
    subagents: HashMap<Arc<str>, Load<Arc<SubagentRead>>>,
    bodies: HashMap<BlockRef, Load<Arc<CallBody>>>,
    summaries: HashMap<BlockRef, Load<Sanitized>>,
    raws: HashMap<BlockRef, Load<Arc<Text>>>,
    images: HashMap<(BlockRef, usize), Load<Picture>>,
    /// The rows opened by hand in Activity, and closed by hand in Full calls.
    opened: HashSet<Toggle>,
    closed: HashSet<Toggle>,
}

impl OpenSession {
    pub fn main(&self) -> Option<&Conversation> {
        self.main.as_ref()
    }

    pub fn unattached(&self) -> &[Arc<str>] {
        &self.unattached
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

    pub fn body(&self, at: &BlockRef) -> Option<&Load<Arc<CallBody>>> {
        self.bodies.get(at)
    }

    pub fn summary(&self, at: &BlockRef) -> Option<&Load<Sanitized>> {
        self.summaries.get(at)
    }

    pub fn raw(&self, at: &BlockRef) -> Option<&Load<Arc<Text>>> {
        self.raws.get(at)
    }

    pub fn image(&self, at: &BlockRef, index: usize) -> Option<&Load<Picture>> {
        self.images.get(&(at.clone(), index))
    }

    /// A prompt's or a reply's text, sanitised: with the parse when it is large, else now.
    pub fn prose(&self, at: &BlockRef, text: &str) -> Sanitized {
        self.prose
            .get(at)
            .cloned()
            .unwrap_or_else(|| Sanitized::new(text))
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
    pub fn loads(&self) -> [(&'static str, usize, usize); 4] {
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
            row("summaries", count(&self.summaries)),
            row("images", count(&self.images)),
        ]
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
        let by_full_calls = full_calls && toggle.opened_by_full_calls();
        let set = if by_full_calls {
            &mut open.closed
        } else {
            &mut open.opened
        };
        if !set.remove(&toggle) {
            set.insert(toggle);
        }
        cx.notify();
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
        let before = std::mem::replace(&mut self.state, ReaderState::Closed);
        cx.background_spawn(async move { drop(before) }).detach();
        cx.notify();
    }

    fn open(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        self.close(cx);
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
        done: impl FnOnce(&mut OpenSession, T) + 'static,
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
        self.loading += 1;
        let job = cx.background_spawn(async move {
            let items = archive.pinned_session(&pinned, cache);
            work(providers[provider].as_ref(), &items)
        });
        cx.spawn(async move |this, cx| {
            let value = job.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading -= 1;
                if let ReaderState::Open(open) = &mut this.state {
                    done(open, value);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
                Arc::new(CallBody {
                    input: input
                        .load_text(items)
                        .map(|text| Text::new(pretty(text)))
                        .map_err(|error| error.to_string()),
                    output: output
                        .unwrap_or_default()
                        .iter()
                        .map(|part| match part {
                            Output::Text(text) => Part::Text(
                                text.load_text(items)
                                    .map(Text::new)
                                    .map_err(|error| error.to_string()),
                            ),
                            Output::Image(image) => Part::Image(picture(image, items)),
                        })
                        .collect(),
                })
            },
            {
                let at = at.clone();
                move |open, body| {
                    open.bodies.insert(at, Load::Loaded(body));
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
                move |open, subagent| {
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
                    open.subagents.insert(item, loaded);
                }
            },
            cx,
        );
    }

    /// Loads a compaction's summary.
    pub fn want_summary(&mut self, at: &BlockRef, cx: &mut Context<Self>) {
        let Some(Content::Compaction {
            summary: Some(summary),
            ..
        }) = self
            .open_session()
            .and_then(|open| open.block(at))
            .map(|block| &block.content)
        else {
            return;
        };
        let summary = summary.clone();
        let key = at.clone();
        self.load(
            |open| claim(&mut open.summaries, &key),
            move |_, items| {
                summary
                    .load_text(items)
                    .map(|text| Sanitized::new(&text))
                    .map_err(|error| error.to_string())
            },
            {
                let at = at.clone();
                move |open, summary| {
                    open.summaries.insert(at, loaded(summary));
                }
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
                move |open, raw| {
                    open.raws.insert(at, loaded(raw));
                }
            },
            cx,
        );
    }

    /// Loads the `index`th image of the prompt at `at`.
    pub fn want_image(&mut self, at: &BlockRef, index: usize, cx: &mut Context<Self>) {
        let Some(Content::Prompt { images, .. }) = self
            .open_session()
            .and_then(|open| open.block(at))
            .map(|block| &block.content)
        else {
            return;
        };
        let Some(image) = images.get(index).cloned() else {
            return;
        };
        let key = (at.clone(), index);
        self.load(
            |open| claim(&mut open.images, &key),
            move |_, items| picture(&image, items),
            {
                let key = (at.clone(), index);
                move |open, picture| {
                    open.images.insert(key, loaded(picture));
                }
            },
            cx,
        );
    }
}

impl OpenSession {
    /// Keeps a subagent's conversation just read: its large bodies sanitised, and the
    /// subagents it read in place (whose records are in its own transcript) moved out of it.
    fn take_in(
        &mut self,
        kind: Option<String>,
        description: Option<String>,
        mut conversation: Conversation,
    ) -> Arc<SubagentRead> {
        let mut found = Vec::new();
        settle(&mut conversation, &mut self.prose, &mut found);
        /* A subagent its calls started is placed under them now, not with those no call is
        known to have started. */
        let started: Vec<Arc<str>> = conversation
            .blocks()
            .iter()
            .filter_map(|block| match &block.content {
                Content::ToolCall(call) => Some(call),
                _ => None,
            })
            .flat_map(|call| {
                call.subagents
                    .iter()
                    .map(|subagent| subagent.item().clone())
            })
            .collect();
        self.unattached.retain(|item| !started.contains(item));
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

/// Marks `key` loading in `map`; `false` when it is loading or loaded already.
fn claim<K: Eq + std::hash::Hash + Clone, T>(map: &mut HashMap<K, Load<T>>, key: &K) -> bool {
    if map.contains_key(key) {
        return false;
    }
    map.insert(key.clone(), Load::Loading);
    true
}

fn loaded<T>(result: Result<T, String>) -> Load<T> {
    match result {
        Ok(value) => Load::Loaded(value),
        Err(why) => Load::Failed(why),
    }
}

/// `text` pretty-printed when it is JSON, else as it is.
fn pretty(text: String) -> String {
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or(text)
}

/// An image from the transcript, decoded far enough to know its size. Only raster formats are
/// drawn: an SVG can name files and addresses to load.
fn picture(image: &Image, items: &dyn SessionItems) -> Result<Picture, String> {
    let format = match image.media_type.as_str() {
        "image/png" => ImageFormat::Png,
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        other => return Err(format!("{other} images aren't shown")),
    };
    let bytes = image.data.load(items).map_err(|error| error.to_string())?;
    let (width, height) = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?
        .into_dimensions()
        .map_err(|error| format!("the image can't be read: {error}"))?;
    Ok(Picture {
        media_type: image.media_type.clone(),
        width,
        height,
        bytes: bytes.len(),
        image: Arc::new(gpui::Image::from_bytes(format, bytes)),
    })
}

/**
 * Prepares a conversation just parsed or read for the views: sanitises its large prompts and
 * replies into `prose`, and moves each subagent read with it (one whose records are in its
 * own transcript) out of its call into `found`, so that every subagent is looked up by item.
 */
fn settle(
    conversation: &mut Conversation,
    prose: &mut HashMap<BlockRef, Sanitized>,
    found: &mut Vec<(Arc<str>, Arc<SubagentRead>)>,
) {
    let item = conversation.item.clone();
    for at in 0..conversation.blocks().len() {
        let id = BlockId(at);
        match &conversation[id].content {
            Content::Prompt { text, .. } | Content::Reply { text }
                if text.len() > SANITIZE_ON_DRAW_BYTES =>
            {
                prose.insert(
                    BlockRef {
                        conversation: item.clone(),
                        block: id,
                    },
                    Sanitized::new(text),
                );
            }
            _ => {}
        }
        let Some(call) = conversation.call_mut(id) else {
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
                settle(&mut conversation, prose, found);
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
    let cache = Arc::new(SegmentCache::new(CACHED_SEGMENTS));
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
        summaries: HashMap::new(),
        raws: HashMap::new(),
        images: HashMap::new(),
        opened: HashSet::new(),
        closed: HashSet::new(),
    };
    let mut found = Vec::new();
    if let Some(main) = &mut open.main {
        settle(main, &mut open.prose, &mut found);
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

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::Text;

    #[test]
    fn a_body_is_cut_into_lines_each_clipped_at_a_character() {
        let long = "é".repeat(400);
        let text = Text::new(format!("one\r\ntwo\n{long}\n"));
        assert_eq!(text.line_count(), 3);
        assert_eq!(text.lines(0..2), "one\ntwo");
        let third = text.lines(2..3);
        assert_eq!(third.len(), 512);
        assert!(long.starts_with(&third));
        assert_eq!(text.lines(2..99), third);
        assert_eq!(Text::new(String::new()).line_count(), 0);
    }
}
