//! The reader's rows (PLAN §9): an open session laid flat into the items of the virtualised
//! list, as the service holds it now.
//!
//! A row is one thing the column draws at one place: a message, a thought, one activity row, a
//! piece of a call's body, a line of a subagent's panel. A subagent's conversation is laid out
//! by the same code as the session's own, a level deeper, inside its panel, so a subagent with
//! thousands of blocks costs what its visible rows do. A call's body is cut into rows of a
//! bounded number of lines for the same reason.
//!
//! A [`Row`] is a value that says everything its height depends on, so the view can compare the
//! rows before and after a change and tell the list which ones to measure again.

use std::sync::Arc;

use sb_core::transcript::{BlockId, Content, Conversation, Ending, ToolKind, Turn};

use crate::services::reader::{BlockRef, CallBody, Load, OpenSession, Part, Toggle};

/// How many lines a call body shows of a part until "Show all" (DESIGN §4.2.4).
pub const FIRST_LINES: usize = 20;
/// How many lines one row of a body holds once it shows all of them.
pub const CHUNK_LINES: usize = 100;
/// How many of a subagent's calls its panel shows until "Show n more calls" (DESIGN §4.2.7).
pub const FIRST_CALLS: usize = 5;
/// The deepest a subagent's panel is drawn inside another's; one deeper shows only Open on its
/// own (DESIGN §4.2.7).
pub const MAX_DEPTH: usize = 3;

/// The space above a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
    Image(usize),
    /// A call that returned nothing, or was never answered.
    NoOutput,
    Footer,
}

/// What a row of a subagent's panel is, besides the blocks of its conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelPart {
    /// The meta line: "subagent", start, model, calls, duration; Open on its own.
    Meta,
    Loading,
    Failed(String),
    /// "Show n more calls".
    More(usize),
    /// Nested too deep to draw: only Open on its own.
    TooDeep,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// The hairline between two turns.
    Turn,
    /// A prompt. A subagent's first prompt is its task, from the agent that started it.
    Prompt(BlockRef),
    Task {
        at: BlockRef,
        from: Option<Arc<str>>,
    },
    /// The agent's mark, the model and the time, over what it wrote.
    Author(BlockRef),
    Reply(BlockRef),
    Thought {
        at: BlockRef,
        open: bool,
    },
    /// An activity row (DESIGN §4.2.3).
    Call {
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
    /// A compaction's summary, once open: loaded or not.
    Summary {
        at: BlockRef,
        ready: bool,
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
        ending: Ending,
    },
    /// The session holds no conversation of its own.
    NoConversation,
    /// Over the subagents no call is known to have started, and one of them.
    UnattachedHead(usize),
    Unattached {
        item: Arc<str>,
        open: bool,
    },
    /// Over a subagent read on its own: back to the call that started it.
    Alone(Arc<str>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// Whether the keyboard can rest on it (DESIGN §5: move between rows).
    pub fn is_focusable(&self) -> bool {
        matches!(
            self.kind,
            RowKind::Prompt(_)
                | RowKind::Task { .. }
                | RowKind::Reply(_)
                | RowKind::Thought { .. }
                | RowKind::Call { .. }
                | RowKind::Compaction { .. }
                | RowKind::Unrecognised { .. }
                | RowKind::Unattached { .. }
                | RowKind::Panel {
                    part: PanelPart::More(_),
                    ..
                }
        )
    }

    /// What `Enter` opens or closes on it.
    pub fn toggle(&self) -> Option<Toggle> {
        match &self.kind {
            RowKind::Thought { at, .. } => Some(Toggle::Thought(at.clone())),
            RowKind::Call { at, .. } => Some(Toggle::Call(at.clone())),
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
    let unattached = open.unattached();
    if !unattached.is_empty() {
        out.push(RowKind::UnattachedHead(unattached.len()), Space::Block);
        for item in unattached {
            let toggle = Toggle::Subagent(item.clone());
            let opened = open.is_open(&toggle, full_calls);
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
    out.finish()
}

struct Rows<'a> {
    open: &'a OpenSession,
    full_calls: bool,
    rows: Vec<Row>,
    /// How many panels the rows pushed now are inside.
    depth: usize,
    /// The model of the call that started the subagent being laid out, whose task it gave.
    task_from: Option<Arc<str>>,
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
                self.push(RowKind::Turn, Space::Block);
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
            self.block(conversation, id, at, space, task);
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
            Content::Prompt { .. } if task && id == conversation.turns()[0].blocks[0] => self.push(
                RowKind::Task {
                    at,
                    from: self.task_from.clone(),
                },
                space,
            ),
            Content::Prompt { .. } => self.push(RowKind::Prompt(at), space),
            Content::Reply { .. } => self.push(RowKind::Reply(at), space),
            Content::Thinking { .. } => {
                let opened = open.is_open(&Toggle::Thought(at.clone()), full_calls);
                self.push(RowKind::Thought { at, open: opened }, space);
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
                if opened {
                    if call.kind == ToolKind::Subagent && !call.subagents.is_empty() {
                        for subagent in &call.subagents {
                            self.panel(subagent.item(), Some(&at), Space::None, false);
                        }
                    } else {
                        self.body(&at);
                    }
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
                    let ready =
                        matches!(open.summary(&at), Some(Load::Loaded(_) | Load::Failed(_)));
                    self.push(RowKind::Summary { at, ready }, Space::Tight);
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
                    let ready = matches!(open.raw(&at), Some(Load::Loaded(_) | Load::Failed(_)));
                    self.push(RowKind::Raw { at, ready }, Space::Tight);
                }
            }
        }
    }

    fn ending(&mut self, conversation: &Conversation) {
        if conversation.ending != Ending::Whole {
            self.push(
                RowKind::Ending {
                    conversation: conversation.item.clone(),
                    ending: conversation.ending.clone(),
                },
                Space::Block,
            );
        }
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

    fn body_parts(&self, at: &BlockRef, body: &CallBody) -> Vec<BodyPart> {
        let mut parts = vec![BodyPart::InputHead];
        self.lines(
            at,
            None,
            body.input.as_ref().map(|text| text.line_count()),
            &mut parts,
        );
        parts.push(BodyPart::OutputHead);
        if body.output.is_empty() {
            parts.push(BodyPart::NoOutput);
        }
        for (index, part) in body.output.iter().enumerate() {
            match part {
                Part::Text(text) => self.lines(
                    at,
                    Some(index),
                    text.as_ref().map(|text| text.line_count()),
                    &mut parts,
                ),
                Part::Image(Ok(_)) => parts.push(BodyPart::Image(index)),
                Part::Image(Err(_)) => parts.push(BodyPart::PartFailed { part: Some(index) }),
            }
        }
        parts.push(BodyPart::Footer);
        parts
    }

    /// The rows of one part's lines: its first lines, or all of them a chunk a row.
    fn lines(
        &self,
        at: &BlockRef,
        part: Option<usize>,
        count: Result<usize, &String>,
        parts: &mut Vec<BodyPart>,
    ) {
        let Ok(count) = count else {
            parts.push(BodyPart::PartFailed { part });
            return;
        };
        if count == 0 {
            return;
        }
        let all = self
            .open
            .is_open(&Toggle::AllLines(at.clone(), part), self.full_calls);
        let chunks = if all { count.div_ceil(CHUNK_LINES) } else { 1 };
        parts.extend((0..chunks).map(|chunk| BodyPart::Lines { part, chunk }));
        if count > FIRST_LINES {
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
            let at = Self::at(conversation, first);
            self.push(
                RowKind::Task {
                    at,
                    from: self.task_from.clone(),
                },
                Space::Tight,
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
            self.push(RowKind::Reply(Self::at(conversation, reply)), Space::Tight);
        }
        self.ending(conversation);
    }
}
