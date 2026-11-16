//! The transcript model (PLAN §7.4): what the reader renders, the same for every agent.
//!
//! A provider's parser (PLAN §7.1) turns a session's archived items into a [`Transcript`]. The
//! model names no agent and assumes nothing about how one keeps its sessions:
//!
//! - A [`Conversation`] is every [`Block`] the agent recorded, each linked to the block it
//!   followed. A block that two others follow is where the conversation was rewound; the branch
//!   to show is given as [`Turn`]s, and the blocks of the other branches stay in the list. A
//!   provider makes one through a [`Builder`], which keeps what the model promises of it.
//! - A tool call carries its result and the subagents it started. A [`Subagent`] is named when
//!   the conversation of the call that started it is read, and its own conversation is read
//!   when it is wanted: the reader opens on the session's own conversation and reads a
//!   subagent's when its call is opened; the index reads them all
//!   ([`Transcript::read_subagents`]).
//! - What is large (tool input and output, images) is not in the model. A [`Stored`] says
//!   where it is archived, and it is loaded when it is shown (PLAN §9), so a parse of a long
//!   session holds its structure and its text, not its tool output (PLAN §7.5).
//! - A record the provider doesn't know is kept as an [`Unrecognised`] (PLAN D5), and what
//!   couldn't be read is said ([`Ending`], [`Unreadable`]), not failed on.
//!
//! [`index`] derives from a transcript what the session index and search hold (PLAN §4.3, §10).

mod build;
mod index;

use std::collections::HashSet;
use std::io::{self, Read as _};
use std::ops::Index;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use thiserror::Error;

use crate::provider::{Provider, ProviderId, SessionItems};

pub use build::Builder;
pub use index::{At, INDEX_VERSION, SessionIndex, Span, SpanKind, Totals, index};

/// A session as its provider parsed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transcript {
    pub facts: Facts,
    /// The session's own conversation; `None` when the session's items hold no main
    /// transcript, only what went with one.
    pub conversation: Option<Conversation>,
    /// The subagents whose conversations are archived with the session and that no call read
    /// so far is known to have started. Until every subagent is read
    /// ([`Transcript::read_subagents`]), those started by another subagent's call are here too.
    pub unattached: Vec<Subagent>,
}

impl Transcript {
    /**
     * Reads every subagent's conversation that isn't read yet, and those of the subagents
     * they started in turn. Under each call its subagents end in the order they started, and
     * a subagent found under a call leaves [`Transcript::unattached`].
     */
    pub fn read_subagents(&mut self, provider: &dyn Provider, items: &dyn SessionItems) {
        let mut placed = HashSet::new();
        if let Some(conversation) = &mut self.conversation {
            read_under(conversation, provider, items, &mut placed, &mut Vec::new());
        }
        for index in 0..self.unattached.len() {
            let item = self.unattached[index].item().clone();
            if placed.contains(&item) {
                continue;
            }
            if matches!(self.unattached[index], Subagent::Unread { .. }) {
                self.unattached[index] = provider.subagent(items, &item);
            }
            if let Subagent::Read { conversation, .. } = &mut self.unattached[index] {
                read_under(conversation, provider, items, &mut placed, &mut vec![item]);
            }
        }
        self.unattached
            .retain(|subagent| !placed.contains(subagent.item()));
    }
}

/**
 * Reads the subagents the calls of `conversation` started, and theirs. `placed` gains every
 * item that is read under a call; an item already there stays unread under a second call that
 * claims it, as does one of `above`, the subagents this conversation is itself under. A
 * subagent its provider's parse read with the session (one whose records are in the
 * session's own transcript) is where the parse put it.
 */
fn read_under(
    conversation: &mut Conversation,
    provider: &dyn Provider,
    items: &dyn SessionItems,
    placed: &mut HashSet<Arc<str>>,
    above: &mut Vec<Arc<str>>,
) {
    for block in &mut conversation.blocks {
        let Content::ToolCall(call) = &mut block.content else {
            continue;
        };
        for subagent in &mut call.subagents {
            let item = subagent.item().clone();
            if matches!(subagent, Subagent::Unread { .. }) {
                if above.contains(&item) || !placed.insert(item.clone()) {
                    continue;
                }
                *subagent = provider.subagent(items, &item);
            }
            if let Subagent::Read { conversation, .. } = subagent {
                above.push(item);
                read_under(conversation, provider, items, placed, above);
                above.pop();
            }
        }
        call.subagents.sort_by_key(|subagent| {
            let started = subagent
                .conversation()
                .and_then(Conversation::span)
                .map_or(i64::MAX, |(started, _)| started);
            (started, subagent.item().clone())
        });
    }
}

/// What a session says about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    /// The provider that parsed the session, which is its agent.
    pub agent: ProviderId,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub usize);

/// One conversation: a session's own, or a subagent's. Made through a [`Builder`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    /// The key of the item the conversation was parsed from.
    pub item: Arc<str>,
    blocks: Vec<Block>,
    turns: Vec<Turn>,
    /// Records the provider doesn't know that have no place among the blocks.
    pub unplaced: Vec<Unrecognised>,
    /// The models that answered, in the order they first did.
    pub models: Vec<String>,
    /// What the conversation's own requests used. Its subagents' use is in theirs.
    pub tokens: Tokens,
    /// Whether the item was read to its end.
    pub ending: Ending,
    /// What goes with the conversation and could not be read: the session's title, a
    /// subagent's description of itself.
    pub unreadable: Vec<Unreadable>,
}

impl Index<BlockId> for Conversation {
    type Output = Block;

    fn index(&self, id: BlockId) -> &Block {
        &self.blocks[id.0]
    }
}

impl Conversation {
    /// Every block, whichever branch it is on, each after the block it follows.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// The branch that is shown, which is the one that ends latest. A turn is never empty, and
    /// its blocks are in the order of [`Conversation::blocks`].
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// The call in the block `id`, to put a subagent's conversation under it once it is read.
    pub fn call_mut(&mut self, id: BlockId) -> Option<&mut ToolCall> {
        match &mut self.blocks.get_mut(id.0)?.content {
            Content::ToolCall(call) => Some(call),
            _ => None,
        }
    }

    /// From when its first block was recorded to when its last block or result was, in Unix
    /// milliseconds.
    pub fn span(&self) -> Option<(i64, i64)> {
        let mut times = self.blocks.iter().flat_map(|block| {
            let answered = match &block.content {
                Content::ToolCall(call) => call.result.as_ref().and_then(|result| result.at),
                _ => None,
            };
            block.at.into_iter().chain(answered)
        });
        let first = times.next()?;
        Some(times.fold((first, first), |(start, end), at| {
            (start.min(at), end.max(at))
        }))
    }

    /// The tool calls on the shown branch, in order, each with its block.
    pub fn calls(&self) -> impl Iterator<Item = (BlockId, &ToolCall)> {
        self.turns
            .iter()
            .flat_map(|turn| &turn.blocks)
            .filter_map(|id| match &self[*id].content {
                Content::ToolCall(call) => Some((*id, &**call)),
                _ => None,
            })
    }
}

/// Whether a conversation's item was read to its end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    Whole,
    /// The item ends inside a record, as one still being written does. That record is not in
    /// the conversation.
    MidRecord,
    /// Reading the item failed, for the reason given. What was read before that is in the
    /// conversation, which is nothing when the item couldn't be opened.
    Unreadable(String),
}

/// An item that goes with a conversation and could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreadable {
    /// The item's key.
    pub item: String,
    pub reason: String,
}

/// A prompt and everything up to the next one. A conversation that doesn't open with a prompt
/// has a first turn without one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub blocks: Vec<BlockId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The block this one followed; `None` where a conversation starts.
    pub parent: Option<BlockId>,
    /// When it was recorded, in Unix milliseconds.
    pub at: Option<i64>,
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
        /// Who asked, when it wasn't whoever the conversation is with (the user; for a
        /// subagent, the agent that started it): the name of another session, say.
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
    /// Text that nobody asked or answered with: a notice from the agent, or context it put
    /// into the conversation. `kind` is the agent's own word for which.
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
    /// Which kind of tool that is, from its name alone.
    pub kind: ToolKind,
    /// The input, typed where it is in the shape the tool's kind has.
    pub input: ToolInput,
    /// The input as the agent recorded it, for the tools shown as sent (PLAN §7.4).
    pub raw_input: Stored,
    /// `None` when the call was never answered: the session stopped first, or is still running.
    pub result: Option<ToolResult>,
    /// The subagents the call started.
    pub subagents: Vec<Subagent>,
}

/// A tool call's input, typed for the kinds that have a shape. It is [`ToolInput::Other`] for
/// a call whose input isn't in its kind's shape as its provider reads it, whatever the kind.
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
        /// Which files are searched, when not all: a file type, a glob, or both.
        files: Option<String>,
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
    /// What can be said of the result without loading its output, where the agent recorded it.
    pub brief: Option<Brief>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Ok,
    Failed,
}

/// The short form of a result that a collapsed row shows (DESIGN §4.2.3). A result without one
/// is not thereby a success: a command that timed out or was refused has failed and says no
/// more, so a row shows the result's status whether or not there is a brief.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Brief {
    /// The status a command that failed exited with.
    Exit(i32),
    /// How many lines were read.
    Lines(u64),
    /// How many lines an edit added and removed.
    Changed { added: u64, removed: u64 },
    /// A file was written: whether it is new, and its lines.
    Written { new: bool, lines: u64 },
    /// How many matches a search found.
    Matches(u64),
    /// A subagent ran: its model, how many calls it made and how long it took.
    Ran {
        model: Option<String>,
        calls: Option<u64>,
        duration: Option<Duration>,
    },
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

/**
 * A subagent of a session as far as placing it needs: what it says it is, and the subagents
 * its calls started, read without its conversation ([`Provider::outlines`]).
 *
 * [`Provider::outlines`]: crate::provider::Provider::outlines
 */
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outline {
    /// The key of the item its conversation is.
    pub item: Arc<str>,
    pub kind: Option<String>,
    pub description: Option<String>,
    pub started: Vec<Arc<str>>,
}

/// A subagent, under the call that started it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subagent {
    /// Its conversation is the item `item`, which hasn't been read:
    /// [`Provider::subagent`] reads it.
    Unread { item: Arc<str> },
    Read {
        /// The agent's name for the kind of subagent.
        kind: Option<String>,
        description: Option<String>,
        conversation: Box<Conversation>,
    },
}

impl Subagent {
    /// The key of the item its conversation is.
    pub fn item(&self) -> &Arc<str> {
        match self {
            Self::Unread { item } => item,
            Self::Read { conversation, .. } => &conversation.item,
        }
    }

    pub fn conversation(&self) -> Option<&Conversation> {
        match self {
            Self::Unread { .. } => None,
            Self::Read { conversation, .. } => Some(conversation),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Input that was not read from a cache.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Tokens {
    pub fn add(&mut self, more: Tokens) {
        self.input = self.input.saturating_add(more.input);
        self.output = self.output.saturating_add(more.output);
        self.cache_read = self.cache_read.saturating_add(more.cache_read);
        self.cache_write = self.cache_write.saturating_add(more.cache_write);
    }
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
    /// A JSON string, quotes included, read as [`json_string`] reads one.
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
            Encoding::JsonString => str::from_utf8(&bytes)
                .ok()
                .and_then(json_string)
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

    /**
     * The start of the content as text, from about its first `limit` bytes as stored, and how
     * many bytes as stored are after it (none when it is all of it): what shows the start of
     * something that may be large without reading it whole. Base64 content is read whole.
     */
    pub fn load_head(
        &self,
        items: &dyn SessionItems,
        limit: u64,
    ) -> Result<(String, u64), LoadError> {
        if self.length <= limit || self.encoding == Encoding::Base64 {
            return Ok((self.load_text(items)?, 0));
        }
        let mut bytes = Vec::new();
        items
            .open(&self.item, self.offset)
            .and_then(|reader| reader.take(limit).read_to_end(&mut bytes))
            .map_err(|source| LoadError::Read {
                item: self.item.clone(),
                source,
            })?;
        if (bytes.len() as u64) < limit {
            return Err(LoadError::Short {
                item: self.item.clone(),
                offset: self.offset,
            });
        }
        /* Not the part of a character the limit cut. */
        let whole = match str::from_utf8(&bytes) {
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            _ => bytes.len(),
        };
        let head = String::from_utf8_lossy(&bytes[..whole]);
        /* Closed where it was cut, less what may be half of an escape or of a surrogate pair:
        found in the text as written, not in what it decodes to. */
        let used = match self.encoding {
            Encoding::JsonString => whole_escapes(&head),
            _ => head.len(),
        };
        let text = match self.encoding {
            Encoding::JsonString => {
                json_string(&format!("{}\"", &head[..used])).ok_or_else(|| LoadError::Encoding {
                    item: self.item.clone(),
                    offset: self.offset,
                    encoding: self.encoding,
                })?
            }
            _ => head.into_owned(),
        };
        Ok((text, self.length.saturating_sub(used as u64)))
    }
}

/**
 * How much of `literal`, the start of a JSON string cut anywhere (its opening quote included),
 * ends at the end of a whole character: not inside an escape, nor between the two escapes of a
 * surrogate pair.
 */
fn whole_escapes(literal: &str) -> usize {
    let bytes = literal.as_bytes();
    let unit = |at: usize| {
        literal
            .get(at + 2..at + 6)
            .and_then(|hex| u16::from_str_radix(hex, 16).ok())
    };
    let (mut at, mut whole) = (1, 1);
    while at < bytes.len() {
        if bytes[at] != b'\\' {
            at += literal[at..].chars().next().map_or(1, char::len_utf8);
            whole = at;
            continue;
        }
        match bytes.get(at + 1) {
            None => break,
            Some(b'u') => {}
            Some(_) => {
                at += 2;
                whole = at;
                continue;
            }
        }
        if at + 6 > bytes.len() {
            break;
        }
        at += 6;
        if unit(at - 6).is_some_and(|it| (0xD800..0xDC00).contains(&it)) {
            /* A high surrogate: whole with its low one, or with what follows it when that is
            no escape; the cut may be inside the low one's escape. */
            let rest = &literal[at..];
            if rest.len() < 6 && b"\\u".starts_with(&rest.as_bytes()[..rest.len().min(2)]) {
                break;
            }
            if rest.starts_with("\\u") && unit(at).is_some_and(|it| (0xDC00..0xE000).contains(&it))
            {
                at += 6;
            }
        }
        whole = at;
    }
    whole
}

/**
 * The text of the JSON string `literal`, quotes included; `None` when it isn't one. Half of an
 * escaped surrogate pair on its own, which text cut in the middle of a character leaves behind,
 * reads as U+FFFD and doesn't make the rest unreadable.
 */
pub fn json_string(literal: &str) -> Option<String> {
    if let Ok(text) = serde_json::from_str(literal) {
        return Some(text);
    }
    /* The four hex digits of a `\u` escape. */
    fn unit(escaped: &mut std::str::Chars) -> Option<u32> {
        let digits = escaped.as_str().get(..4)?;
        let unit = u32::from_str_radix(digits, 16).ok()?;
        *escaped = escaped.as_str()[4..].chars();
        digits
            .bytes()
            .all(|digit| digit.is_ascii_hexdigit())
            .then_some(unit)
    }
    let mut rest = literal.strip_prefix('"')?.strip_suffix('"')?.chars();
    let mut text = String::with_capacity(literal.len());
    while let Some(next) = rest.next() {
        text.push(match next {
            '"' => return None,
            '\\' => match rest.next()? {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'u' => {
                    let first = unit(&mut rest)?;
                    let mut ahead = rest.clone();
                    let second = (ahead.next() == Some('\\') && ahead.next() == Some('u'))
                        .then(|| unit(&mut ahead))
                        .flatten();
                    match second {
                        Some(second)
                            if (0xD800..0xDC00).contains(&first)
                                && (0xDC00..0xE000).contains(&second) =>
                        {
                            rest = ahead;
                            char::from_u32(0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00))
                        }
                        _ => char::from_u32(first),
                    }
                    .unwrap_or(char::REPLACEMENT_CHARACTER)
                }
                escaped @ ('"' | '\\' | '/') => escaped,
                _ => return None,
            },
            plain => plain,
        });
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};

    use super::{Encoding, SessionItems, Stored, json_string};
    use crate::provider::SessionItem;

    /// One item, whose bytes are these.
    struct Bytes(Vec<u8>);

    impl SessionItems for Bytes {
        fn list(&self) -> &[SessionItem] {
            &[]
        }

        fn open(&self, _: &str, offset: u64) -> io::Result<Box<dyn Read + '_>> {
            Ok(Box::new(&self.0[offset as usize..]))
        }
    }

    #[test]
    fn the_head_of_content_is_read_without_the_rest_and_never_half_a_character() {
        let stored = |bytes: &Bytes, encoding| Stored {
            item: "item".into(),
            offset: 0,
            length: bytes.0.len() as u64,
            encoding,
        };
        let plain = Bytes("ab\u{e9}cd".as_bytes().to_vec());
        assert_eq!(
            stored(&plain, Encoding::Plain)
                .load_head(&plain, 3)
                .unwrap(),
            ("ab".to_string(), 4)
        );
        assert_eq!(
            stored(&plain, Encoding::Plain)
                .load_head(&plain, 64)
                .unwrap(),
            ("ab\u{e9}cd".to_string(), 0)
        );
        /* A JSON string cut inside an escape, wherever. */
        let json = Bytes(br#""a\n\u00e9\ud83c\udf89 b""#.to_vec());
        for limit in 2..json.0.len() as u64 {
            let (head, rest) = stored(&json, Encoding::JsonString)
                .load_head(&json, limit)
                .unwrap();
            assert!(rest > 0 && rest >= json.0.len() as u64 - limit);
            assert!(
                "a\n\u{e9}\u{1F389} b".starts_with(&head),
                "{limit}: {head:?}"
            );
        }
        /* Replacement characters the text holds, written or escaped, are text like any other;
        a high surrogate cut from its low one, before anything or before a wide character, is
        not kept. */
        for text in [
            format!("\"{}xyz\"", "\u{FFFD}".repeat(40)),
            format!("\"{}xyz\"", "\\ufffd".repeat(10)),
            "\"ab\\ud83c中文中文中文\"".to_string(),
        ] {
            let json = Bytes(text.into_bytes());
            for limit in 2..json.0.len() as u64 {
                let (head, _) = stored(&json, Encoding::JsonString)
                    .load_head(&json, limit)
                    .unwrap();
                assert!(head.len() as u64 <= limit, "{limit}: {head:?}");
            }
        }
    }

    #[test]
    fn a_json_string_reads_as_its_text() {
        for (literal, text) in [
            (r#""""#, Some("")),
            (r#""plain""#, Some("plain")),
            (
                r#""a \"b\" \\ \/ \n\t\r\b\f""#,
                Some("a \"b\" \\ / \n\t\r\u{8}\u{c}"),
            ),
            (r#""\u00e9 \u4e2d""#, Some("é 中")),
            (r#""\ud83c\udf89 done""#, Some("🎉 done")),
            /* Half a pair, wherever it is, is a replacement character. */
            (r#""cut \ud83c""#, Some("cut \u{fffd}")),
            (r#""\ud83c then""#, Some("\u{fffd} then")),
            (r#""\udf89 then""#, Some("\u{fffd} then")),
            (r#""\ud83c\ud83c\udf89""#, Some("\u{fffd}🎉")),
            (r#""\ud83c\u0041""#, Some("\u{fffd}A")),
            /* What isn't a string isn't read as one. */
            (r#""\ud83""#, None),
            (r#""\uzzzz""#, None),
            (r#""\x""#, None),
            (r#""un"quoted""#, None),
            (r#""open"#, None),
            ("17", None),
            ("null", None),
            ("", None),
        ] {
            assert_eq!(json_string(literal).as_deref(), text, "{literal}");
        }
    }
}
