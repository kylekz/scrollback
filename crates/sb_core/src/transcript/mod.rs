//! The transcript model (PLAN §7.4): what the reader renders, the same for every agent.
//!
//! A provider's parser (PLAN §7.1) turns a session's archived items into a [`Transcript`]. The
//! model names no agent and assumes nothing about how one keeps its sessions:
//!
//! - A [`Conversation`] is every [`Block`] the agent recorded, each linked to the block it
//!   followed. A block that two others follow is where the conversation was rewound; the branch
//!   to show is given as [`Turn`]s, and the blocks of the other branches stay in the list.
//! - A tool call carries its result, and the conversations of the subagents it started.
//! - What is large (tool input and output, images) is not in the model. A [`Stored`] says
//!   where it is archived, and it is loaded when it is shown (PLAN §9), so a first parse of a
//!   long session holds its structure and its text, not its tool output (PLAN §7.5).
//! - A record the provider doesn't know is kept as an [`Unrecognised`] (PLAN D5).
//!
//! [`index`] derives from a transcript what the session index and search hold (PLAN §4.3, §10).

mod index;

use std::io::{self, Read as _};
use std::ops::Index;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use thiserror::Error;

use crate::provider::SessionItems;

pub use index::{SessionIndex, Span, SpanKind, index};

/// A session as its provider parsed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transcript {
    pub facts: Facts,
    pub conversation: Conversation,
    /// Subagents whose conversations are archived with the session and that no call in it is
    /// known to have started.
    pub unattached: Vec<Subagent>,
}

/// What a session says about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    /// The id of the provider that parsed the session, which is its agent.
    pub agent: &'static str,
    /// The title the agent stored, if it stored one.
    pub title: Option<String>,
    /// The first working directory, as the agent wrote it (PLAN D9).
    pub project_path: Option<String>,
    /// The git branch the session started on.
    pub branch: Option<String>,
    /// How the agent was started (its command line, a desktop app), in its own word.
    pub entrypoint: Option<String>,
}

/// A block's place in its conversation's [`Conversation::blocks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(pub usize);

/// One conversation: a session's own, or a subagent's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    /// The key of the item the conversation was parsed from.
    pub item: Arc<str>,
    /// Every block, in the order the agent recorded them, whichever branch it is on.
    pub blocks: Vec<Block>,
    /// The branch that is shown, which is the one that ends latest, as turns.
    pub turns: Vec<Turn>,
    /// Records the provider doesn't know that have no place among the blocks.
    pub unplaced: Vec<Unrecognised>,
    /// The models that answered, in the order they first did.
    pub models: Vec<String>,
    /// What the conversation's own requests used. Its subagents' use is in theirs.
    pub tokens: Tokens,
    /// The item ends inside a record, as one still being written does. That record is not in
    /// the conversation.
    pub cut_short: bool,
}

impl Index<BlockId> for Conversation {
    type Output = Block;

    fn index(&self, id: BlockId) -> &Block {
        &self.blocks[id.0]
    }
}

impl Conversation {
    /// When the first and the last of its blocks were recorded, in Unix milliseconds.
    pub fn span(&self) -> Option<(i64, i64)> {
        let mut times = self.blocks.iter().filter_map(|block| block.at);
        let first = times.next()?;
        Some(times.fold((first, first), |(start, end), at| {
            (start.min(at), end.max(at))
        }))
    }

    /// The tool calls on the shown branch, in order.
    pub fn calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.turns
            .iter()
            .flat_map(|turn| &turn.blocks)
            .filter_map(|id| match &self[*id].content {
                Content::ToolCall(call) => Some(&**call),
                _ => None,
            })
    }
}

/// A prompt and everything up to the next one. A conversation that doesn't open with a prompt
/// has a first turn without one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    /// In the order they are shown. Never empty.
    pub blocks: Vec<BlockId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The block this one followed; `None` where a conversation starts. A block's parent comes
    /// before it in [`Conversation::blocks`].
    pub parent: Option<BlockId>,
    /// When it was recorded, in Unix milliseconds.
    pub at: Option<i64>,
    /// Where its record starts in the conversation's item. Opening a session at a turn starts
    /// reading there (PLAN §7.5).
    pub offset: u64,
    /// The model that produced it, for what the agent wrote.
    pub model: Option<Arc<str>>,
    pub content: Content,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    /// What the agent was asked. A turn starts at each.
    Prompt {
        text: String,
        images: Vec<Image>,
        /// Who asked, in the agent's own word, when it wasn't whoever the conversation is with
        /// (the user; for a subagent, the agent that started it): another session, say.
        from: Option<String>,
    },
    /// The agent's answer, as markdown.
    Reply {
        text: String,
    },
    Thinking {
        text: String,
        duration: Option<Duration>,
    },
    ToolCall(Box<ToolCall>),
    /// The agent replaced the conversation so far with a summary to make room.
    Compaction {
        tokens_before: Option<u64>,
        tokens_after: Option<u64>,
        /// The summary the agent went on with.
        summary: Option<Stored>,
    },
    /// Text that neither the user nor the model wrote: a notice from the agent, or context it
    /// put into the conversation. `kind` is the agent's own word for which.
    Note {
        kind: String,
        text: String,
    },
    Unrecognised(Unrecognised),
}

/// A record, or a part of one, that its provider couldn't turn into anything else. It is kept
/// so the reader can show it as it was recorded (PLAN D5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unrecognised {
    /// The agent's name for the kind of record; empty when the record couldn't be read at all.
    pub kind: String,
    /// What about it wasn't as its provider reads that kind; `None` when the provider doesn't
    /// know the kind.
    pub misfit: Option<&'static str>,
    pub raw: Stored,
}

/// The kinds of tool, common to all agents (PLAN §7.4). The reader has one renderer per kind;
/// each provider maps its agent's tools onto them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ToolKind {
    Shell,
    Read,
    Edit,
    Write,
    ContentSearch,
    FileSearch,
    Todos,
    Subagent,
    WebSearch,
    WebFetch,
    Mcp,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    /// The tool's name as its agent calls it.
    pub name: String,
    pub input: ToolInput,
    /// The input as the agent recorded it, for the tools shown as sent (PLAN §7.4).
    pub raw_input: Stored,
    /// `None` when the call was never answered: the session stopped first, or is still running.
    pub result: Option<ToolResult>,
    /// The subagents the call started, with their conversations.
    pub subagents: Vec<Subagent>,
}

/// A tool call's input, typed where its kind is known. A call of a mapped tool whose input
/// isn't in the shape its provider reads is [`ToolInput::Other`], like a tool that isn't
/// mapped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolInput {
    Shell {
        command: String,
        description: Option<String>,
    },
    Read {
        path: String,
        /// From 1; `None` reads from the start.
        first_line: Option<u64>,
        /// How many lines; `None` reads to the end.
        lines: Option<u64>,
    },
    /// `old` is replaced by `new` in the file at `path`.
    Edit {
        path: String,
        old: Stored,
        new: Stored,
        /// Every occurrence, not only the first.
        all: bool,
    },
    Write {
        path: String,
        content: Stored,
    },
    ContentSearch {
        pattern: String,
        path: Option<String>,
    },
    FileSearch {
        pattern: String,
        path: Option<String>,
    },
    Todos(Vec<Todo>),
    Subagent {
        /// The agent's name for the kind of subagent.
        kind: Option<String>,
        description: Option<String>,
        prompt: Stored,
    },
    WebSearch {
        query: String,
    },
    WebFetch {
        url: String,
        prompt: Option<String>,
    },
    Mcp {
        server: String,
        tool: String,
    },
    Other,
}

impl ToolInput {
    pub fn kind(&self) -> ToolKind {
        match self {
            Self::Shell { .. } => ToolKind::Shell,
            Self::Read { .. } => ToolKind::Read,
            Self::Edit { .. } => ToolKind::Edit,
            Self::Write { .. } => ToolKind::Write,
            Self::ContentSearch { .. } => ToolKind::ContentSearch,
            Self::FileSearch { .. } => ToolKind::FileSearch,
            Self::Todos(_) => ToolKind::Todos,
            Self::Subagent { .. } => ToolKind::Subagent,
            Self::WebSearch { .. } => ToolKind::WebSearch,
            Self::WebFetch { .. } => ToolKind::WebFetch,
            Self::Mcp { .. } => ToolKind::Mcp,
            Self::Other => ToolKind::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Todo {
    pub text: String,
    pub state: TodoState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoState {
    Open,
    InProgress,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    /// When it was recorded, in Unix milliseconds. The call took from its block's time to this.
    pub at: Option<i64>,
    pub status: ToolStatus,
    /// What the tool returned, in order.
    pub output: Vec<Output>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Ok,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Text(Stored),
    Image(Image),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// Its MIME type, as `image/png`.
    pub media_type: String,
    pub data: Stored,
}

/// A subagent and its conversation, under the call that started it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subagent {
    /// The agent's name for the kind of subagent.
    pub kind: Option<String>,
    pub description: Option<String>,
    pub conversation: Conversation,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Input that was not read from a cache.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// Content left where it is archived: a range of one of the session's items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    /// The item's key.
    pub item: Arc<str>,
    pub offset: u64,
    pub length: u64,
    pub encoding: Encoding,
}

/// How a [`Stored`] range holds its content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// The bytes are the content.
    Plain,
    /// A JSON string, quotes included.
    JsonString,
    Base64,
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("could not read {item}: {source}")]
    Read {
        item: Arc<str>,
        #[source]
        source: io::Error,
    },
    /// The item is shorter than the range, as an earlier version of it can be.
    #[error("{item} ends before the content at {offset}")]
    Short { item: Arc<str>, offset: u64 },
    #[error("the content at {offset} of {item} is not {encoding:?}")]
    Encoding {
        item: Arc<str>,
        offset: u64,
        encoding: Encoding,
    },
}

impl Stored {
    /**
     * Where `part` is in the archive. `part` is a slice of `record`, and `record` is the bytes
     * from `offset` of the item `item`: a parser has both while it reads a record.
     */
    pub fn part(
        item: &Arc<str>,
        offset: u64,
        record: &[u8],
        part: &str,
        encoding: Encoding,
    ) -> Self {
        let within = (part.as_ptr() as usize)
            .checked_sub(record.as_ptr() as usize)
            .filter(|start| start + part.len() <= record.len())
            .expect("the part is a slice of the record it was parsed from");
        Self {
            item: item.clone(),
            offset: offset + within as u64,
            length: part.len() as u64,
            encoding,
        }
    }

    /// The content, read from the archive and decoded.
    pub fn load(&self, items: &dyn SessionItems) -> Result<Vec<u8>, LoadError> {
        let mut bytes = Vec::new();
        items
            .open(&self.item, self.offset)
            .and_then(|reader| reader.take(self.length).read_to_end(&mut bytes))
            .map_err(|source| LoadError::Read {
                item: self.item.clone(),
                source,
            })?;
        if bytes.len() as u64 != self.length {
            return Err(LoadError::Short {
                item: self.item.clone(),
                offset: self.offset,
            });
        }
        match self.encoding {
            Encoding::Plain => Some(bytes),
            Encoding::JsonString => serde_json::from_slice::<String>(&bytes)
                .ok()
                .map(String::into_bytes),
            Encoding::Base64 => STANDARD.decode(&bytes).ok(),
        }
        .ok_or_else(|| LoadError::Encoding {
            item: self.item.clone(),
            offset: self.offset,
            encoding: self.encoding,
        })
    }

    /// The content as text. Bytes that aren't UTF-8 show as replacement characters.
    pub fn load_text(&self, items: &dyn SessionItems) -> Result<String, LoadError> {
        Ok(String::from_utf8_lossy(&self.load(items)?).into_owned())
    }
}
