//! The reader's rows (PLAN §9): an open session laid flat into the items of the virtualised
//! list, as the service holds it now.
//!
//! A row is one thing the column draws at one place: a message's header, a piece of its prose,
//! a thought, one activity row, a piece of a call's body, a line of a subagent's panel. Nothing
//! one row draws is unbounded: prose comes in pieces ([`crate::services::reader::Prose`]), and
//! text shown as it is in rows of at most [`CHUNK_LINES`] lines. A subagent's conversation is
//! laid out by the same code as the session's own, a level deeper, inside its panel, so a
//! subagent with thousands of blocks costs what its visible rows do.
//!
//! A [`Row`] is a value that says everything its height depends on; [`Row::key`] says which
//! row it is whatever its state, so the view can match the rows before and after a change and
//! tell the list which ones were added, removed, or changed in place.

use std::collections::HashSet;
use std::sync::Arc;

use sb_core::transcript::{
    BlockId, Content, Conversation, Ending, ToolInput, ToolKind, ToolStatus, Turn,
};

use crate::services::reader::shape::diff::Diff;
use crate::services::reader::shape::{Renderer, renderer};
use crate::services::reader::{
    BlockRef, CallBody, Load, OpenSession, Part, Piece, Prose, Text, Toggle,
};

/// How many lines text shown as it is shows until "Show all" (DESIGN §4.2.4).
pub const FIRST_LINES: usize = 20;
/// How many lines one row holds once all are shown.
pub const CHUNK_LINES: usize = 100;
/// How many lines of an edit's diff, and of a write's, show until Show all (DESIGN §4.2.4).
pub const FIRST_DIFF_LINES: usize = 20;
pub const FIRST_WRITTEN_LINES: usize = 6;
/// How many of a web search's results show until Show all.
pub const FIRST_RESULTS: usize = 5;
/// How many of a subagent's calls its panel shows until "Show n more calls" (DESIGN §4.2.7).
pub const FIRST_CALLS: usize = 5;
/// The deepest a subagent's panel is drawn inside another's; one deeper shows only Open on its
/// own (DESIGN §4.2.7).
pub const MAX_DEPTH: usize = 3;

/// The space above a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Space {
    None,
    /// Between two activity rows of a group (1).
    Group,
    /// Between the parts of a message (8).
    Tight,
    /// Between blocks (16).
    Block,
    /// Above the column's first row (28).
    Top,
}

/// What a row of a call's body is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BodyPart {
    /// The body is being read: a skeleton of three lines.
    Loading,
    /// It couldn't be read, for this reason.
    Failed(String),
    /// The header over the input, and over the output.
    InputHead,
    OutputHead,
    /// Lines of the input (`part` `None`) or of an output part: the `chunk`th row of them.
    Lines {
        part: Option<usize>,
        chunk: usize,
    },
    /// The line under a part's lines when it has more: "+n more lines · Show all", or "Show
    /// fewer" once all are shown.
    More {
        part: Option<usize>,
        all: bool,
    },
    /// A part that couldn't be read.
    PartFailed {
        part: Option<usize>,
    },
    /// All of a part was asked for, of which the body has only the head: being read, or why
    /// it couldn't be.
    Whole {
        part: Option<usize>,
    },
    Image(usize),
    /// A call that returned nothing, or was never answered.
    NoOutput,
    Footer,
    /// The kind's own header (DESIGN §4.2.4): a command, a file and its lines, a search, a
    /// web fetch's prompt, an MCP tool's whole name.
    Head,
    /// The `chunk`th row of an edit's diff lines: its first [`FIRST_DIFF_LINES`] (a write's
    /// [`FIRST_WRITTEN_LINES`]) until all are shown, then [`CHUNK_LINES`] a row.
    Diff {
        chunk: usize,
    },
    /// Under a diff with more lines: how many more and Show all, or Show fewer.
    DiffMore {
        all: bool,
    },
    /// One of a list of todos.
    Todo(usize),
    /// One of a web search's results, and under them how many more and Show all.
    Result(usize),
    ResultsMore {
        all: bool,
    },
    /// A piece of a web fetch's result: Markdown, or the `chunk`th row of a piece shown as
    /// text, [`CHUNK_LINES`] lines a row.
    Fetched {
        piece: usize,
        chunk: usize,
    },
    /// The `chunk`th row of an MCP call's input and output, side by side.
    Pair {
        chunk: usize,
    },
}

/// What a row of a block's prose is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ProsePart {
    /// Being prepared: a skeleton.
    Pending,
    Failed(String),
    /// A piece laid out as Markdown.
    Markdown(usize),
    /// A piece of fenced code, in the reader's code view.
    Code(usize),
    /// Over a piece shown as plain text for its size: says so.
    Note(usize),
    /// The `chunk`th row of a piece's lines.
    Lines {
        piece: usize,
        chunk: usize,
    },
    More {
        piece: usize,
        all: bool,
    },
}

/// How a conversation that isn't whole ended (PLAN §7.4).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ended {
    /// Inside a record, as one still being written does.
    MidRecord,
    /// Reading it failed from some point, for this reason.
    Unreadable(String),
}

/// What a row of a subagent's panel is, besides the blocks of its conversation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PanelPart {
    /// The meta line: "subagent", start, model, calls, duration; Open on its own.
    Meta,
    Loading,
    Failed(String),
    /// "Show n more calls".
    More(usize),
    /// Nested too deep to draw: only Open on its own.
    TooDeep,
    /// Started again where it is already drawn (claimed by a second call, or by a subagent it
    /// started): drawn once, and here only named.
    Repeat,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowKind {
    /// The hairline before the turn that opens at this prompt.
    Turn(BlockRef),
    /// Over a prompt: who asked, and when. A subagent's first prompt is its task, from the
    /// agent that started it, drawn in a box its prose rows go on in.
    Prompt(BlockRef),
    Task {
        at: BlockRef,
        from: Option<Arc<str>>,
    },
    /// A row of a block's prose: a prompt's, a reply's, an open thought's or summary's. `boxed`
    /// when it is in a task's box, and `last` when it ends the box; `reply` on a reply's first
    /// row, where the keyboard rests on the reply and its Copy is.
    Prose {
        at: BlockRef,
        part: ProsePart,
        boxed: bool,
        last: bool,
        reply: bool,
    },
    /// A prompt's images as thumbnails.
    Images(BlockRef),
    /// The agent's mark, the model and the time, over what it wrote.
    Author(BlockRef),
    Thought {
        at: BlockRef,
        open: bool,
    },
    /// An activity row (DESIGN §4.2.3).
    Call {
        at: BlockRef,
        open: bool,
    },
    /// Under a call that started subagents: its own input and output, opened.
    CallIo {
        at: BlockRef,
        open: bool,
    },
    Body {
        at: BlockRef,
        part: BodyPart,
        first: bool,
        last: bool,
    },
    /// A line of the panel of the subagent `item`, started by the call `call` (none for one no
    /// call is known to have started).
    Panel {
        item: Arc<str>,
        call: Option<BlockRef>,
        part: PanelPart,
    },
    Compaction {
        at: BlockRef,
        open: bool,
    },
    Note(BlockRef),
    Unrecognised {
        at: BlockRef,
        open: bool,
    },
    Raw {
        at: BlockRef,
        ready: bool,
    },
    /// How the conversation of the item ended, when it isn't whole.
    Ending {
        conversation: Arc<str>,
        ending: Ended,
    },
    /// The session holds no conversation of its own.
    NoConversation,
    /// Over the subagents no call is known to have started: how many, once that is known.
    UnattachedHead(Option<usize>),
    Unattached {
        item: Arc<str>,
        open: bool,
    },
    /// Over a subagent read on its own: back to the call that started it.
    Alone(Arc<str>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Row {
    pub kind: RowKind,
    /// How many subagent panels it is inside.
    pub depth: usize,
    /// Whether the innermost panel starts at it, and how many of the panels it is inside end
    /// at it, from the innermost.
    pub opens: bool,
    pub closes: usize,
    pub space: Space,
}

impl Row {
    /// Which row it is, whatever its state: open or closed, loading or loaded. The rows before
    /// and after a change are matched by it.
    pub fn key(&self) -> RowKind {
        let mut key = self.kind.clone();
        match &mut key {
            RowKind::Thought { open, .. }
            | RowKind::Call { open, .. }
            | RowKind::CallIo { open, .. }
            | RowKind::Compaction { open, .. }
            | RowKind::Unrecognised { open, .. }
            | RowKind::Unattached { open, .. } => *open = false,
            RowKind::Raw { ready, .. } => *ready = false,
            RowKind::Body {
                part, first, last, ..
            } => {
                (*first, *last) = (false, false);
                match part {
                    BodyPart::Failed(_) => *part = BodyPart::Loading,
                    BodyPart::More { all, .. }
                    | BodyPart::DiffMore { all }
                    | BodyPart::ResultsMore { all } => *all = false,
                    _ => {}
                }
            }
            RowKind::Prose { part, last, .. } => {
                *last = false;
                match part {
                    ProsePart::Failed(_) => *part = ProsePart::Pending,
                    ProsePart::More { all, .. } => *all = false,
                    _ => {}
                }
            }
            RowKind::Panel { part, .. } => {
                if let PanelPart::Failed(_) = part {
                    *part = PanelPart::Loading;
                }
            }
            RowKind::UnattachedHead(count) => *count = None,
            _ => {}
        }
        key
    }

    /// Whether the keyboard can rest on it (DESIGN §5: move between rows).
    pub fn is_focusable(&self) -> bool {
        matches!(
            self.kind,
            RowKind::Prompt(_)
                | RowKind::Task { .. }
                | RowKind::Thought { .. }
                | RowKind::Call { .. }
                | RowKind::CallIo { .. }
                | RowKind::Compaction { .. }
                | RowKind::Unrecognised { .. }
                | RowKind::Unattached { .. }
                | RowKind::Panel {
                    part: PanelPart::More(_),
                    ..
                }
        ) || matches!(self.kind, RowKind::Prose { reply: true, .. })
    }

    /// What `Enter` opens or closes on it.
    pub fn toggle(&self) -> Option<Toggle> {
        match &self.kind {
            RowKind::Thought { at, .. } => Some(Toggle::Thought(at.clone())),
            RowKind::Call { at, .. } => Some(Toggle::Call(at.clone())),
            RowKind::CallIo { at, .. } => Some(Toggle::CallIo(at.clone())),
            RowKind::Compaction { at, .. } => Some(Toggle::Summary(at.clone())),
            RowKind::Unrecognised { at, .. } => Some(Toggle::Raw(at.clone())),
            RowKind::Unattached { item, .. } => Some(Toggle::Subagent(item.clone())),
            RowKind::Panel {
                item,
                part: PanelPart::More(_),
                ..
            } => Some(Toggle::AllCalls(item.clone())),
            _ => None,
        }
    }

    /// Whether it is open, for a row that opens.
    pub fn is_open(&self) -> bool {
        match self.kind {
            RowKind::Thought { open, .. }
            | RowKind::Call { open, .. }
            | RowKind::CallIo { open, .. }
            | RowKind::Compaction { open, .. }
            | RowKind::Unrecognised { open, .. }
            | RowKind::Unattached { open, .. } => open,
            _ => false,
        }
    }

    /// Whether a turn starts at it.
    pub fn starts_turn(&self) -> bool {
        self.depth == 0 && matches!(self.kind, RowKind::Prompt(_))
    }

    /// The block it is of, if it is one block's.
    pub fn block(&self) -> Option<&BlockRef> {
        match &self.kind {
            RowKind::Turn(at)
            | RowKind::Prompt(at)
            | RowKind::Task { at, .. }
            | RowKind::Prose { at, .. }
            | RowKind::Images(at)
            | RowKind::Author(at)
            | RowKind::Thought { at, .. }
            | RowKind::Call { at, .. }
            | RowKind::CallIo { at, .. }
            | RowKind::Body { at, .. }
            | RowKind::Compaction { at, .. }
            | RowKind::Note(at)
            | RowKind::Unrecognised { at, .. }
            | RowKind::Raw { at, .. } => Some(at),
            _ => None,
        }
    }
}

/// Lays out `open`, with Full calls on or off, or only the subagent `alone` when it is read on
/// its own, with the call that started it.
pub fn rows(
    open: &OpenSession,
    full_calls: bool,
    alone: Option<(&Arc<str>, Option<&BlockRef>)>,
) -> Vec<Row> {
    let mut out = Rows {
        open,
        full_calls,
        rows: Vec::new(),
        depth: 0,
        task_from: None,
        drawn: HashSet::new(),
    };
    if let Some((item, call)) = alone {
        out.push(RowKind::Alone(item.clone()), Space::Top);
        out.panel(item, call, Space::Block, true);
        return out.finish();
    }
    match open.main() {
        Some(main) => out.conversation(main, false),
        None => out.push(RowKind::NoConversation, Space::Top),
    }
    match open.unattached() {
        None => out.push(RowKind::UnattachedHead(None), Space::Block),
        Some(unattached) if !unattached.is_empty() => {
            out.push(
                RowKind::UnattachedHead(Some(unattached.len())),
                Space::Block,
            );
            for item in &unattached {
                let opened = open.is_open(&Toggle::Subagent(item.clone()), full_calls);
                out.push(
                    RowKind::Unattached {
                        item: item.clone(),
                        open: opened,
                    },
                    Space::Group,
                );
                if opened {
                    out.panel(item, None, Space::None, false);
                }
            }
        }
        Some(_) => {}
    }
    out.finish()
}

/**
 * The rows `old`, all of one block's prose, of one call's body or one record as recorded, laid
 * out again from `open` as it is now, where they are: as deep, the first with the space the
 * first had, the last closing the panels the last closed. What [`rows`] would lay out there,
 * without laying out the rest; so a body that loads costs its own rows, not the session's.
 */
pub fn relaid(open: &OpenSession, full_calls: bool, old: &[Row]) -> Vec<Row> {
    let (Some(first), Some(last)) = (old.first(), old.last()) else {
        return Vec::new();
    };
    let mut out = Rows {
        open,
        full_calls,
        rows: Vec::new(),
        depth: first.depth,
        task_from: None,
        drawn: HashSet::new(),
    };
    match &first.kind {
        RowKind::Prose {
            at, boxed, reply, ..
        } => out.prose(at, *boxed, *reply),
        RowKind::Body { at, .. } => out.body(at),
        RowKind::Raw { at, .. } => out.raw(at.clone()),
        _ => return old.to_vec(),
    }
    let mut rows = out.finish();
    if let Some(start) = rows.first_mut() {
        (start.space, start.opens) = (first.space, first.opens);
    }
    if let Some(end) = rows.last_mut() {
        end.closes = last.closes;
    }
    rows
}

struct Rows<'a> {
    open: &'a OpenSession,
    full_calls: bool,
    rows: Vec<Row>,
    /// How many panels the rows pushed now are inside.
    depth: usize,
    /// The model of the call that started the subagent being laid out, whose task it gave.
    task_from: Option<Arc<str>>,
    /// The subagents whose panels are laid out: each is laid out once, so that its rows' keys
    /// are its own.
    drawn: HashSet<Arc<str>>,
}

impl Rows<'_> {
    fn push(&mut self, kind: RowKind, space: Space) {
        let space = if self.rows.is_empty() {
            Space::Top
        } else {
            space
        };
        self.rows.push(Row {
            kind,
            depth: self.depth,
            opens: false,
            closes: 0,
            space,
        });
    }

    fn finish(self) -> Vec<Row> {
        self.rows
    }

    fn at(conversation: &Conversation, block: BlockId) -> BlockRef {
        BlockRef {
            conversation: conversation.item.clone(),
            block,
        }
    }

    /**
     * Every turn of `conversation`. A subagent's shows its first prompt as its task. A turn's
     * blocks follow its prompt: the agent's header before the first thing it wrote, then each
     * block.
     */
    fn conversation(&mut self, conversation: &Conversation, subagent: bool) {
        for (index, turn) in conversation.turns().iter().enumerate() {
            if index > 0 && self.depth == 0 {
                self.push(
                    RowKind::Turn(Self::at(conversation, turn.blocks[0])),
                    Space::Block,
                );
            }
            self.turn(conversation, turn, subagent && index == 0);
        }
        self.ending(conversation);
    }

    fn turn(&mut self, conversation: &Conversation, turn: &Turn, task: bool) {
        let mut authored = false;
        let mut last_call = false;
        for &id in &turn.blocks {
            let at = Self::at(conversation, id);
            let block = &conversation[id];
            let is_call = matches!(block.content, Content::ToolCall(_));
            let speaks = matches!(
                block.content,
                Content::Reply { .. } | Content::Thinking { .. } | Content::ToolCall(_)
            );
            if speaks && !authored {
                authored = true;
                self.push(RowKind::Author(at.clone()), Space::Block);
            }
            let space = match (is_call && last_call, speaks) {
                (true, _) => Space::Group,
                (false, true) => Space::Tight,
                (false, false) => Space::Block,
            };
            last_call = is_call;
            let first = task && id == conversation.turns()[0].blocks[0];
            self.block(conversation, id, at, space, first);
        }
    }

    fn block(
        &mut self,
        conversation: &Conversation,
        id: BlockId,
        at: BlockRef,
        space: Space,
        task: bool,
    ) {
        let full_calls = self.full_calls;
        let open = self.open;
        match &conversation[id].content {
            Content::Prompt { images, .. } => {
                if task {
                    self.push(
                        RowKind::Task {
                            at: at.clone(),
                            from: self.task_from.clone(),
                        },
                        space,
                    );
                } else {
                    self.push(RowKind::Prompt(at.clone()), space);
                }
                self.prose(&at, task, false);
                if !images.is_empty() {
                    self.push(RowKind::Images(at), Space::Tight);
                }
            }
            Content::Reply { .. } => {
                let start = self.rows.len();
                self.prose(&at, false, true);
                if let Some(first) = self.rows.get_mut(start) {
                    first.space = space;
                }
            }
            Content::Thinking { .. } => {
                let opened = open.is_open(&Toggle::Thought(at.clone()), full_calls);
                self.push(
                    RowKind::Thought {
                        at: at.clone(),
                        open: opened,
                    },
                    space,
                );
                if opened {
                    self.prose(&at, false, false);
                }
            }
            Content::ToolCall(call) => {
                let opened = open.is_open(&Toggle::Call(at.clone()), full_calls);
                self.push(
                    RowKind::Call {
                        at: at.clone(),
                        open: opened,
                    },
                    space,
                );
                if !opened {
                    return;
                }
                if call.subagents.is_empty() {
                    self.body(&at);
                    return;
                }
                let mut started = HashSet::new();
                for subagent in &call.subagents {
                    if started.insert(subagent.item()) {
                        self.panel(subagent.item(), Some(&at), Space::None, false);
                    }
                }
                let io = open.is_open(&Toggle::CallIo(at.clone()), full_calls);
                self.push(
                    RowKind::CallIo {
                        at: at.clone(),
                        open: io,
                    },
                    Space::None,
                );
                if io {
                    self.body(&at);
                }
            }
            Content::Compaction { summary, .. } => {
                let opened =
                    summary.is_some() && open.is_open(&Toggle::Summary(at.clone()), full_calls);
                self.push(
                    RowKind::Compaction {
                        at: at.clone(),
                        open: opened,
                    },
                    Space::Block,
                );
                if opened {
                    self.prose(&at, false, false);
                }
            }
            Content::Note { .. } => self.push(RowKind::Note(at), space),
            Content::Unrecognised(_) => {
                let opened = open.is_open(&Toggle::Raw(at.clone()), full_calls);
                self.push(
                    RowKind::Unrecognised {
                        at: at.clone(),
                        open: opened,
                    },
                    space,
                );
                if opened {
                    self.raw(at);
                }
            }
        }
    }

    /// A record as it was recorded, under its row.
    fn raw(&mut self, at: BlockRef) {
        let ready = matches!(self.open.raw(&at), Some(Load::Loaded(_) | Load::Failed(_)));
        self.push(RowKind::Raw { at, ready }, Space::Tight);
    }

    /// The rows of a block's prose, as far as it is prepared. In a task's box, the last closes
    /// it; a reply's first is where the keyboard rests on it.
    fn prose(&mut self, at: &BlockRef, boxed: bool, reply: bool) {
        let parts = match self.open.prose(at) {
            None | Some(Load::Loading) => vec![ProsePart::Pending],
            Some(Load::Failed(why)) => vec![ProsePart::Failed(why.clone())],
            Some(Load::Loaded(prose)) => self.prose_parts(at, prose),
        };
        let count = parts.len();
        for (index, part) in parts.into_iter().enumerate() {
            let space = if index == 0 {
                Space::Tight
            } else {
                Space::None
            };
            self.push(
                RowKind::Prose {
                    at: at.clone(),
                    part,
                    boxed,
                    last: boxed && index + 1 == count,
                    reply: reply && index == 0,
                },
                space,
            );
        }
    }

    fn prose_parts(&self, at: &BlockRef, prose: &Prose) -> Vec<ProsePart> {
        let mut parts = Vec::new();
        for (index, piece) in prose.pieces.iter().enumerate() {
            match piece {
                Piece::Markdown(_) => parts.push(ProsePart::Markdown(index)),
                Piece::Code { .. } => parts.push(ProsePart::Code(index)),
                Piece::Plain { text, oversized } => {
                    if *oversized {
                        parts.push(ProsePart::Note(index));
                    }
                    let count = text.line_count();
                    let all = !oversized
                        || self
                            .open
                            .is_open(&Toggle::ProseLines(at.clone(), index), self.full_calls);
                    let chunks = if all {
                        count.div_ceil(CHUNK_LINES)
                    } else {
                        count.min(1)
                    };
                    parts.extend((0..chunks).map(|chunk| ProsePart::Lines {
                        piece: index,
                        chunk,
                    }));
                    if *oversized && count > FIRST_LINES {
                        parts.push(ProsePart::More { piece: index, all });
                    }
                }
            }
        }
        parts
    }

    fn ending(&mut self, conversation: &Conversation) {
        let ending = match &conversation.ending {
            Ending::Whole => return,
            Ending::MidRecord => Ended::MidRecord,
            Ending::Unreadable(why) => Ended::Unreadable(why.clone()),
        };
        self.push(
            RowKind::Ending {
                conversation: conversation.item.clone(),
                ending,
            },
            Space::Block,
        );
    }

    /// The rows of a call's body, as far as it is loaded.
    fn body(&mut self, at: &BlockRef) {
        let parts = match self.open.body(at) {
            None | Some(Load::Loading) => vec![BodyPart::Loading],
            Some(Load::Failed(why)) => vec![BodyPart::Failed(why.clone())],
            Some(Load::Loaded(body)) => self.body_parts(at, body),
        };
        let last = parts.len() - 1;
        for (index, part) in parts.into_iter().enumerate() {
            self.push(
                RowKind::Body {
                    at: at.clone(),
                    part,
                    first: index == 0,
                    last: index == last,
                },
                Space::None,
            );
        }
    }

    /**
     * A call's body, by its kind (DESIGN §4.2.4): a command's header, its output and its
     * footer; a read's header and the file's lines; an edit's diff; a search's header and what
     * it found; a list of todos; a web search's results; a web fetch's prompt and its result
     * as Markdown; an MCP call's input and output side by side. A kind whose input isn't in
     * its shape, and a tool no provider knows, show the input as sent and each part of the
     * output. What failed shows what the tool said; a footer says which versions of the files
     * it saved to are read, when they have more than one.
     */
    fn body_parts(&self, at: &BlockRef, body: &CallBody) -> Vec<BodyPart> {
        let Some(call) = self.open.call(at) else {
            return vec![BodyPart::NoOutput];
        };
        let failed = call
            .result
            .as_ref()
            .is_some_and(|result| result.status == ToolStatus::Failed);
        let mut parts = Vec::new();
        match (renderer(call), &call.input) {
            (Renderer::Shell, _) => {
                parts.push(BodyPart::Head);
                self.outputs(at, body, &mut parts);
                parts.push(BodyPart::Footer);
            }
            (Renderer::Read | Renderer::Search, _) => {
                parts.push(BodyPart::Head);
                self.outputs(at, body, &mut parts);
            }
            (Renderer::Files, _) => self.outputs(at, body, &mut parts),
            (Renderer::Diff, _) => {
                match &body.diff {
                    Some(Ok(diff)) => self.diff(at, call.kind, diff, &mut parts),
                    _ => parts.push(BodyPart::PartFailed { part: None }),
                }
                if failed {
                    self.outputs(at, body, &mut parts);
                }
            }
            (Renderer::Todos, ToolInput::Todos(todos)) => {
                parts.extend((0..todos.len()).map(BodyPart::Todo));
                if failed {
                    self.outputs(at, body, &mut parts);
                }
            }
            (Renderer::WebSearch, _) if !body.results.is_empty() => {
                let all = self
                    .open
                    .is_open(&Toggle::AllLines(at.clone(), None), self.full_calls);
                let count = body.results.len();
                let shown = if all { count } else { count.min(FIRST_RESULTS) };
                parts.extend((0..shown).map(BodyPart::Result));
                if count > FIRST_RESULTS {
                    parts.push(BodyPart::ResultsMore { all });
                }
            }
            (Renderer::WebFetch, _) if body.fetched.is_some() => {
                parts.push(BodyPart::Head);
                for (piece, prose) in body
                    .fetched
                    .iter()
                    .flat_map(|prose| prose.pieces.iter())
                    .enumerate()
                {
                    let chunks = match prose {
                        Piece::Markdown(_) => 1,
                        Piece::Code { text, .. } | Piece::Plain { text, .. } => {
                            text.line_count().div_ceil(CHUNK_LINES).max(1)
                        }
                    };
                    parts.extend((0..chunks).map(|chunk| BodyPart::Fetched { piece, chunk }));
                }
            }
            (Renderer::Mcp, _) => {
                parts.push(BodyPart::Head);
                let lines = |text: Option<&Result<Arc<Text>, String>>| {
                    text.and_then(|text| text.as_ref().ok())
                        .map_or(0, |text| text.line_count())
                };
                let first = body.output.iter().find_map(|part| match part {
                    Part::Text(text) => Some(text),
                    Part::Image => None,
                });
                let count = lines(Some(&body.input)).max(lines(first));
                let all = self
                    .open
                    .is_open(&Toggle::AllLines(at.clone(), None), self.full_calls);
                let chunks = if all {
                    count.div_ceil(CHUNK_LINES)
                } else {
                    count.min(1)
                };
                parts.extend((0..chunks).map(|chunk| BodyPart::Pair { chunk }));
                if count > FIRST_LINES {
                    parts.push(BodyPart::More { part: None, all });
                }
                parts.extend(
                    (body.output.iter().enumerate())
                        .filter(|(_, part)| matches!(part, Part::Image))
                        .map(|(index, _)| BodyPart::Image(index)),
                );
            }
            _ => {
                parts.push(BodyPart::InputHead);
                self.lines(at, None, body.input.as_ref(), &mut parts);
                parts.push(BodyPart::OutputHead);
                self.outputs(at, body, &mut parts);
                parts.push(BodyPart::Footer);
            }
        }
        if !body.versions.is_empty() && !parts.contains(&BodyPart::Footer) {
            parts.push(BodyPart::Footer);
        }
        parts
    }

    /// The rows of each part of a call's output: its lines, or its image; or that it has none.
    fn outputs(&self, at: &BlockRef, body: &CallBody, parts: &mut Vec<BodyPart>) {
        if body.output.is_empty() {
            parts.push(BodyPart::NoOutput);
        }
        for (index, part) in body.output.iter().enumerate() {
            match part {
                Part::Text(text) => self.lines(at, Some(index), text.as_ref(), parts),
                Part::Image => parts.push(BodyPart::Image(index)),
            }
        }
    }

    /// The rows of an edit's diff: its first lines, or all, a bounded number of lines drawn a
    /// row ([`super::kinds::diff_chunk`]).
    fn diff(&self, at: &BlockRef, kind: ToolKind, diff: &Diff, parts: &mut Vec<BodyPart>) {
        let written = kind == ToolKind::Write;
        let all = self
            .open
            .is_open(&Toggle::AllLines(at.clone(), None), self.full_calls);
        let chunks = if all {
            diff.drawn.div_ceil(CHUNK_LINES)
        } else {
            diff.drawn.min(1)
        };
        parts.extend((0..chunks).map(|chunk| BodyPart::Diff { chunk }));
        let (lines, drawn) = super::kinds::diff_rest(diff, written);
        if lines > 0 || drawn {
            parts.push(BodyPart::DiffMore { all });
        }
    }

    /**
     * The rows of one part's lines: its first lines, or all of them a chunk a row. All of a
     * part the body has only the head of are its whole, read for them.
     */
    fn lines(
        &self,
        at: &BlockRef,
        part: Option<usize>,
        text: Result<&Arc<Text>, &String>,
        parts: &mut Vec<BodyPart>,
    ) {
        let Ok(text) = text else {
            parts.push(BodyPart::PartFailed { part });
            return;
        };
        let all = self
            .open
            .is_open(&Toggle::AllLines(at.clone(), part), self.full_calls);
        let count = match (all, text.rest) {
            (true, Some(_)) => match self.open.whole(at, part) {
                Some(Load::Loaded(whole)) => whole.line_count(),
                _ => {
                    parts.push(BodyPart::Whole { part });
                    parts.push(BodyPart::More { part, all });
                    return;
                }
            },
            _ => text.line_count(),
        };
        if count == 0 {
            return;
        }
        let chunks = if all { count.div_ceil(CHUNK_LINES) } else { 1 };
        parts.extend((0..chunks).map(|chunk| BodyPart::Lines { part, chunk }));
        if count > FIRST_LINES || text.rest.is_some() {
            parts.push(BodyPart::More { part, all });
        }
    }

    /**
     * The panel of the subagent `item`: its meta line, then its conversation as far as it is
     * read, a level deeper. Read on its own (`alone`), it is the column's whole content: not in
     * a panel, and every call shown.
     */
    fn panel(&mut self, item: &Arc<str>, call: Option<&BlockRef>, space: Space, alone: bool) {
        let panel = |part| RowKind::Panel {
            item: item.clone(),
            call: call.cloned(),
            part,
        };
        if !alone {
            self.depth += 1;
        }
        if !self.drawn.insert(item.clone()) {
            self.push(panel(PanelPart::Repeat), space);
            if !alone {
                if let Some(last) = self.rows.last_mut() {
                    last.opens = true;
                    last.closes += 1;
                }
                self.depth -= 1;
            }
            return;
        }
        let outer = std::mem::replace(
            &mut self.task_from,
            call.and_then(|call| self.open.block(call)?.model.clone()),
        );
        let start = self.rows.len();
        if self.depth > MAX_DEPTH {
            self.push(panel(PanelPart::TooDeep), space);
        } else {
            self.push(panel(PanelPart::Meta), space);
            match self.open.subagent(item) {
                None | Some(Load::Loading) => self.push(panel(PanelPart::Loading), Space::Tight),
                Some(Load::Failed(why)) => {
                    self.push(panel(PanelPart::Failed(why.clone())), Space::Tight)
                }
                Some(Load::Loaded(read)) => {
                    if alone
                        || self
                            .open
                            .is_open(&Toggle::AllCalls(item.clone()), self.full_calls)
                    {
                        self.conversation(&read.conversation, true);
                    } else {
                        self.summary(&read.conversation, item, call);
                    }
                }
            }
        }
        self.task_from = outer;
        if !alone {
            self.rows[start].opens = true;
            if let Some(last) = self.rows.last_mut() {
                last.closes += 1;
            }
            self.depth -= 1;
        }
    }

    /// A subagent's panel before its calls are all shown: its task, its first few calls, how
    /// many more there are, and its last reply (DESIGN §4.2.7).
    fn summary(&mut self, conversation: &Conversation, item: &Arc<str>, call: Option<&BlockRef>) {
        let shown: Vec<BlockId> = conversation
            .turns()
            .iter()
            .flat_map(|turn| &turn.blocks)
            .copied()
            .collect();
        if let Some(&first) = shown.first()
            && matches!(conversation[first].content, Content::Prompt { .. })
        {
            self.block(
                conversation,
                first,
                Self::at(conversation, first),
                Space::Tight,
                true,
            );
        }
        let calls: Vec<BlockId> = conversation.calls().map(|(id, _)| id).collect();
        for (index, &id) in calls.iter().take(FIRST_CALLS).enumerate() {
            let space = if index == 0 {
                Space::Tight
            } else {
                Space::Group
            };
            self.block(conversation, id, Self::at(conversation, id), space, false);
        }
        if calls.len() > FIRST_CALLS {
            self.push(
                RowKind::Panel {
                    item: item.clone(),
                    call: call.cloned(),
                    part: PanelPart::More(calls.len() - FIRST_CALLS),
                },
                Space::Group,
            );
        }
        if let Some(&reply) = shown
            .iter()
            .rev()
            .find(|id| matches!(conversation[**id].content, Content::Reply { .. }))
        {
            self.block(
                conversation,
                reply,
                Self::at(conversation, reply),
                Space::Tight,
                false,
            );
        }
        self.ending(conversation);
    }
}
