//! From a session's archived items to the transcript model (PLAN §7.3–§7.5).
//!
//! A transcript is read once, a line at a time, and nothing of a line is kept but what the
//! model holds: the text of prompts, replies and notes, each tool call's typed input, and for
//! everything large (tool input and output, images) where it is, as a `Stored`. So a session of
//! tens of megabytes parses into a small fraction of its size, and its tool output is read
//! from the archive when a row is opened (PLAN §9). Only the transcript asked for is read: the
//! session's own, or one subagent's.
//!
//! How the records become the model:
//!
//! - **The tree.** Conversation records (`user`, `assistant`, `system`, `attachment`) carry a
//!   `uuid` and the `parentUuid` of the record they follow. A record written again with a uuid
//!   already read (a resumed session repeats some) is passed over. A compaction boundary has a
//!   null `parentUuid` and its `logicalParentUuid` is followed instead. A subagent's transcript
//!   can hold a record before the one it follows, so parents are found once all are read, and
//!   a block is placed after the block it follows.
//! - **The branch shown** ends at the latest record. Walking `parentUuid` back from it is not
//!   the whole branch: a reply is written as one record per content block, and when it calls
//!   several tools at once the results hang off their own `tool_use` records, beside the chain
//!   and not on it. A rewound branch is set apart from those by how it starts: the user went
//!   back and asked again, so the first thing said on it is the user's prompt. Every side
//!   branch is shown except those.
//! - **Tool results** are `tool_result` blocks of `user` records, paired with their call by
//!   `tool_use_id` wherever in the transcript either is. Output Claude Code saved outside the
//!   transcript is recorded as a `<persisted-output>` note naming the file under
//!   `tool-results/`, and the result points at that item.
//! - **Text in the user's place** is the user's prompt only when nothing says otherwise
//!   ([`said`]): Claude Code puts context, notices and other sessions' messages there too,
//!   marked by `isMeta`, by `origin`, or by the markup it wraps them in.
//! - **Subagents.** Each has its own transcript. The result of the call that started one
//!   records its `agentId`, which names its file, and a workflow's records the run its
//!   subagents are filed under. Where a call that starts subagents has none by those, the
//!   `.meta.json` beside each remaining transcript is read for the `tool_use` it names.
//! - **Everything else.** Records, attachments and system notices of the kinds listed here as
//!   holding no conversation are passed over. One of a kind this parser doesn't know is kept
//!   as an `Unrecognised`: among the blocks when it has a `uuid`, beside the conversation
//!   otherwise. A known record whose fields aren't as this parser reads them is kept the same
//!   way, saying which part didn't fit (PLAN D5).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead as _, BufReader, Read as _};
use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use super::ID;
use super::record::{
    self, ApiError, Attachment, CompactMetadata, Head, Launched, Line, Message, Origin, Record,
    Shape, Source, SubagentMeta, Text, TitleFile, Usage, shape, string, typed,
};
use super::said::{Said, said};
use super::tools::{self, WORKFLOW};
use crate::provider::{ItemKind, SessionItem, SessionItems};
use crate::transcript::{
    BlockId, Builder, Content, Conversation, Encoding, Ending, Facts, Image, Output, Stored,
    Subagent, Tokens, ToolCall, ToolKind, ToolResult, ToolStatus, Transcript, Unreadable,
    Unrecognised,
};

/// The record types that carry the conversation.
pub(super) const CONVERSATION: [&str; 4] = ["user", "assistant", "system", "attachment"];

/// The record that holds the session's title.
pub(super) const TITLE: &str = "custom-title";

/// The record types that hold no conversation and are passed over.
pub(super) const BOOKKEEPING: [&str; 18] = [
    "agent-name",
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "artifact-autoreact-ledger",
    "artifact-comment-monitor",
    "atis-latch",
    "bridge-session",
    "cost-state",
    "file-history-delta",
    "file-history-snapshot",
    "frame-link",
    "last-prompt",
    "mode",
    "permission-mode",
    "pr-link",
    "queue-operation",
    "tools-used",
];

/// The attachment that is a prompt queued while the agent was working.
pub(super) const QUEUED: &str = "queued_command";

/// The attachments that are context Claude Code gave the model, not conversation, and are
/// passed over.
pub(super) const CONTEXT: [&str; 29] = [
    "agent_listing_delta",
    "auto_mode",
    "batching_reminder_sent",
    "command_permissions",
    "compact_file_reference",
    "credential_org",
    "critical_system_reminder",
    "date",
    "deferred_tools_delta",
    "deferred_tools_record",
    "edited_text_file",
    "environment",
    "file",
    "hook_additional_context",
    "hook_non_blocking_error",
    "instructions",
    "invoked_skills",
    "mcp_instructions_delta",
    "model",
    "prompt_snapshot",
    "read_truncation_notice",
    "remote_session_change",
    "session_context",
    "silent_turn_reminder",
    "skill_listing",
    "structured_output",
    "task_status",
    "thinking_drop",
    "total_tokens_reminder",
];

/// The `system` record that marks a compaction.
pub(super) const BOUNDARY: &str = "compact_boundary";

/// The `system` records that say something, in `content` or in `error`.
pub(super) const NOTICES: [&str; 2] = ["informational", "api_error"];

/// The `system` records that say nothing and are passed over.
pub(super) const SILENT: [&str; 1] = ["stop_hook_summary"];

/// The model named on replies Claude Code wrote itself (an interruption, an API error).
const SYNTHETIC: &str = "<synthetic>";

/// The session's saved tool output, by file name.
type Saved<'a> = HashMap<&'a str, &'a SessionItem>;

fn saved(list: &[SessionItem]) -> Saved<'_> {
    list.iter()
        .filter(|item| item.kind == ItemKind::ToolResult)
        .map(|item| (file_name(&item.key), item))
        .collect()
}

pub(super) fn parse(items: &dyn SessionItems) -> Transcript {
    let list = items.list();
    let read = list
        .iter()
        .find(|item| item.kind == ItemKind::Transcript)
        .map(|item| Reading::of(items, &item.key, &saved(list)).finish(items));
    let mut facts = Facts {
        agent: ID,
        title: None,
        project_path: None,
        branch: None,
        entrypoint: None,
    };
    let mut claimed = HashSet::new();
    let mut conversation = read.map(|read| {
        facts.title = read.title;
        facts.project_path = read.cwd;
        facts.branch = read.branch;
        facts.entrypoint = read.entrypoint;
        claimed = read.claimed;
        read.conversation
    });
    if facts.title.is_none()
        && let Some(file) = list
            .iter()
            .find(|item| item.key.ends_with("/custom-title.json"))
    {
        match sidecar::<TitleFile>(items, &file.key) {
            Ok(title) => facts.title = title.map(|title| title.custom_title),
            Err(reason) => {
                if let Some(conversation) = &mut conversation {
                    conversation.unreadable.push(Unreadable {
                        item: file.key.clone(),
                        reason,
                    });
                }
            }
        }
    }
    let mut unattached: Vec<&str> = list
        .iter()
        .filter(|item| item.kind == ItemKind::Subagent && !claimed.contains(&item.key))
        .map(|item| item.key.as_str())
        .collect();
    unattached.sort_unstable();
    Transcript {
        facts,
        conversation,
        unattached: unattached
            .into_iter()
            .map(|key| Subagent::Unread { item: key.into() })
            .collect(),
    }
}

pub(super) fn subagent(items: &dyn SessionItems, key: &str) -> Subagent {
    let mut conversation = Reading::of(items, key, &saved(items.list()))
        .finish(items)
        .conversation;
    let meta = meta_key(key);
    let meta = sidecar::<SubagentMeta>(items, &meta).unwrap_or_else(|reason| {
        conversation
            .unreadable
            .push(Unreadable { item: meta, reason });
        None
    });
    let meta = meta.unwrap_or_default();
    Subagent::Read {
        kind: meta.agent_type,
        description: meta.description,
        conversation: Box::new(conversation),
    }
}

fn file_name(key: &str) -> &str {
    key.rsplit_once('/').map_or(key, |(_, name)| name)
}

/// The key of the `.meta.json` beside the subagent transcript `key`.
fn meta_key(key: &str) -> String {
    format!("{}.meta.json", key.strip_suffix(".jsonl").unwrap_or(key))
}

/// The small JSON file `key` of the session, when it is there and is what `T` reads. An error
/// is why it couldn't be read.
fn sidecar<T: DeserializeOwned>(items: &dyn SessionItems, key: &str) -> Result<Option<T>, String> {
    if !items.list().iter().any(|item| item.key == key) {
        return Ok(None);
    }
    let mut json = Vec::new();
    items
        .open(key, 0)
        .and_then(|mut file| file.read_to_end(&mut json))
        .map_err(|error| error.to_string())?;
    Ok(serde_json::from_slice(&json).ok())
}

/// A record in the tree the `parentUuid`s make, with what it holds of the conversation.
struct Node {
    /// The uuid of the record it follows.
    follows: Option<String>,
    at: Option<i64>,
    /// The model of the reply the record is part of.
    model: Option<String>,
    contents: Vec<Content>,
}

/// Where a tool call is: its record's node, and its place among that node's contents.
type Place = (usize, usize);

/// A tool result as it was read, before its call is looked for.
struct Answer {
    call: Option<String>,
    result: ToolResult,
    /// The ids `toolUseResult` gives what the call started: a subagent's, a workflow run's.
    started: Vec<String>,
    /// The node of the record the result is in.
    node: usize,
    record: Stored,
}

/// One transcript, read.
struct Reading<'a> {
    saved: &'a Saved<'a>,
    item: Arc<str>,
    nodes: Vec<Node>,
    by_uuid: HashMap<String, usize>,
    /// Tool calls by their `tool_use` id.
    calls: HashMap<String, Place>,
    answers: Vec<Answer>,
    unplaced: Vec<Unrecognised>,
    tokens: Tokens,
    /// The replies whose usage is counted, by `message.id`.
    counted: HashSet<String>,
    /// The compaction boundary still waiting for its summary.
    compaction: Option<Place>,
    ending: Ending,
    title: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    entrypoint: Option<String>,
}

/// A transcript as a conversation, and what else it says.
struct Read {
    conversation: Conversation,
    /// The subagent transcripts its calls started, by key.
    claimed: HashSet<String>,
    title: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    entrypoint: Option<String>,
}

impl<'a> Reading<'a> {
    /// Reads the transcript `key`. What can't be read ends it there, and says so.
    fn of(items: &dyn SessionItems, key: &str, saved: &'a Saved<'a>) -> Self {
        let item: Arc<str> = Arc::from(key);
        let mut reading = Self {
            saved,
            item: item.clone(),
            nodes: Vec::new(),
            by_uuid: HashMap::new(),
            calls: HashMap::new(),
            answers: Vec::new(),
            unplaced: Vec::new(),
            tokens: Tokens::default(),
            counted: HashSet::new(),
            compaction: None,
            ending: Ending::Whole,
            title: None,
            cwd: None,
            branch: None,
            entrypoint: None,
        };
        let mut bytes = match items.open(key, 0) {
            Ok(bytes) => BufReader::with_capacity(1 << 16, bytes),
            Err(error) => {
                reading.ending = Ending::Unreadable(error.to_string());
                return reading;
            }
        };
        let mut buffer = Vec::new();
        let mut offset = 0;
        loop {
            buffer.clear();
            let length = match bytes.read_until(b'\n', &mut buffer) {
                Ok(0) => break,
                Ok(length) => length,
                Err(error) => {
                    reading.ending = Ending::Unreadable(error.to_string());
                    break;
                }
            };
            let ended = buffer.ends_with(b"\n");
            let text = buffer.strip_suffix(b"\n").unwrap_or(&buffer);
            let line = Line {
                item: &item,
                offset,
                bytes: text.strip_suffix(b"\r").unwrap_or(text),
            };
            if !line.bytes.is_empty() && !reading.record(&line) {
                if ended {
                    reading.unplaced.push(Unrecognised {
                        kind: String::new(),
                        misfit: Some("not a record"),
                        raw: line.whole(),
                    });
                } else {
                    /* The last line, still being written when it was archived. */
                    reading.ending = Ending::MidRecord;
                }
            }
            offset += length as u64;
        }
        reading
    }

    /// Takes in one line. `false` when it isn't a record: not a JSON object with a `type`.
    fn record(&mut self, line: &Line) -> bool {
        let mut record = match serde_json::from_slice::<Record>(line.bytes) {
            Ok(record) => record,
            Err(_) => {
                let Ok(head) = serde_json::from_slice::<Head>(line.bytes) else {
                    return false;
                };
                self.unplaced.push(Unrecognised {
                    kind: head.kind,
                    misfit: Some("record"),
                    raw: line.whole(),
                });
                return true;
            }
        };
        let text = |text: Option<Text>| text.map(|Text(text)| text);
        if self.cwd.is_none() {
            self.cwd = text(record.cwd.take());
        }
        if self.branch.is_none() {
            self.branch = text(record.git_branch.take()).filter(|branch| !branch.is_empty());
        }
        if self.entrypoint.is_none() {
            self.entrypoint = record.entrypoint.take();
        }
        match record.kind.as_str() {
            /* The last one is the title. */
            TITLE => self.title = text(record.custom_title.take()).or(self.title.take()),
            kind if BOOKKEEPING.contains(&kind) => {}
            _ if record
                .uuid
                .as_ref()
                .is_some_and(|uuid| self.by_uuid.contains_key(uuid)) => {}
            "assistant" => {
                self.node(&mut record);
                self.assistant(&record, line);
            }
            "user" => {
                self.node(&mut record);
                self.user(&record, line);
            }
            "system" => {
                self.node(&mut record);
                self.system(&record, line);
            }
            "attachment" => {
                self.node(&mut record);
                self.attachment(&record, line);
            }
            _ if record.uuid.is_some() => {
                self.node(&mut record);
                self.push(Content::Unrecognised(unknown(record.kind, line.whole())));
            }
            _ => self.unplaced.push(unknown(record.kind, line.whole())),
        }
        true
    }

    /// Starts the node of a record. What the record holds is pushed onto it next.
    fn node(&mut self, record: &mut Record) {
        if let Some(uuid) = record.uuid.take() {
            self.by_uuid.insert(uuid, self.nodes.len());
        }
        self.nodes.push(Node {
            follows: record
                .parent_uuid
                .take()
                .or(record.logical_parent_uuid.take()),
            at: record
                .timestamp
                .as_deref()
                .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                .map(|time| time.timestamp_millis()),
            model: None,
            contents: Vec::new(),
        });
    }

    /// Adds to the record whose node was started last, and says where.
    fn push(&mut self, content: Content) -> Place {
        let last = self.nodes.len() - 1;
        let contents = &mut self.nodes[last].contents;
        contents.push(content);
        (last, contents.len() - 1)
    }

    /// Adds what was said in the user's place: the prompt, then the notes that came with it.
    fn say(&mut self, said: Said) {
        if let Some((text, from)) = said.prompt {
            self.push(Content::Prompt {
                text,
                images: Vec::new(),
                from,
            });
        }
        for note in said.notes {
            self.push(note);
        }
    }

    /// Keeps a record of the known type `kind` whose `part` isn't as this parser reads it.
    fn misfit(&mut self, kind: &str, part: &'static str, raw: Stored) {
        self.push(Content::Unrecognised(Unrecognised {
            kind: kind.to_string(),
            misfit: Some(part),
            raw,
        }));
    }

    fn assistant(&mut self, record: &Record, line: &Line) {
        let Some(message) = record.message.and_then(typed::<Message>) else {
            return self.misfit("assistant", "message", line.whole());
        };
        if let Some(node) = self.nodes.last_mut() {
            node.model = message.model.filter(|model| model != SYNTHETIC);
        }
        if message.id.is_none_or(|id| self.counted.insert(id))
            && let Some(usage) = message.usage.and_then(typed::<Usage>)
        {
            self.tokens.add(Tokens {
                input: usage.input_tokens.unwrap_or(0),
                output: usage.output_tokens.unwrap_or(0),
                cache_read: usage.cache_read_input_tokens.unwrap_or(0),
                cache_write: usage.cache_creation_input_tokens.unwrap_or(0),
            });
        }
        match message.content.map(shape) {
            Some(Shape::Text(text)) => match string(text) {
                Some(text) => {
                    self.push(Content::Reply { text });
                }
                None => self.misfit("assistant", "content", line.whole()),
            },
            Some(Shape::Blocks(blocks)) => {
                for raw in blocks {
                    self.assistant_block(raw, record, line);
                }
            }
            _ => self.misfit("assistant", "content", line.whole()),
        }
    }

    fn assistant_block(&mut self, raw: &RawValue, record: &Record, line: &Line) {
        let whole = || line.stored(raw, Encoding::Plain);
        let Some(block) = typed::<record::Block>(raw) else {
            return self.misfit("assistant", "content block", whole());
        };
        match block.kind.as_str() {
            "text" => match block.text.and_then(string) {
                /* A reply that is only a tool call starts with an empty text block. */
                Some(text) if text.trim().is_empty() => {}
                Some(text) => {
                    self.push(Content::Reply { text });
                }
                None => self.misfit("text", "text", whole()),
            },
            "thinking" => {
                self.push(Content::Thinking {
                    text: block.thinking.map_or_else(String::new, |Text(text)| text),
                    duration: record.thinking_duration_ms.map(Duration::from_millis),
                });
            }
            "tool_use" => match (block.id, block.name, block.input) {
                (Some(id), Some(name), Some(input)) => {
                    let place = self.push(Content::ToolCall(Box::new(ToolCall {
                        kind: tools::kind(&name),
                        input: tools::input(&name, input, line),
                        raw_input: line.stored(input, Encoding::Plain),
                        name,
                        result: None,
                        subagents: Vec::new(),
                    })));
                    self.calls.entry(id).or_insert(place);
                }
                _ => self.misfit("tool_use", "id, name or input", whole()),
            },
            _ => {
                self.push(Content::Unrecognised(unknown(block.kind, whole())));
            }
        }
    }

    fn user(&mut self, record: &Record, line: &Line) {
        let Some(content) = record
            .message
            .and_then(typed::<Message>)
            .and_then(|message| message.content)
        else {
            return self.misfit("user", "message", line.whole());
        };
        let content = shape(content);
        if record.is_compact_summary == Some(true) {
            let summary = match content {
                Shape::Text(text) => Some(text),
                Shape::Blocks(blocks) => blocks
                    .first()
                    .and_then(|raw| typed::<record::Block>(raw))
                    .and_then(|block| block.text),
                Shape::Other => None,
            }
            .map(|text| line.text(text));
            /* It follows its boundary. One without a boundary still says the agent compacted. */
            let (node, at) = self.compaction.take().unwrap_or_else(|| {
                self.push(Content::Compaction {
                    tokens_before: None,
                    tokens_after: None,
                    summary: None,
                })
            });
            if let Content::Compaction { summary: slot, .. } = &mut self.nodes[node].contents[at] {
                *slot = summary;
            }
            return;
        }
        let meta = record.is_meta == Some(true);
        let origin = record.origin.and_then(typed::<Origin>);
        match content {
            Shape::Text(text) => match string(text) {
                Some(text) => self.say(said(&text, meta, origin.as_ref())),
                None => self.misfit("user", "content", line.whole()),
            },
            Shape::Blocks(blocks) => {
                /* What was asked comes first, its text blocks as one prompt. */
                let mut prompt: Option<(String, Vec<Image>, Option<String>)> = None;
                let mut rest = Vec::new();
                for raw in blocks {
                    let whole = || line.stored(raw, Encoding::Plain);
                    let misfit = |kind: &str, part| {
                        Content::Unrecognised(Unrecognised {
                            kind: kind.to_string(),
                            misfit: Some(part),
                            raw: whole(),
                        })
                    };
                    let Some(block) = typed::<record::Block>(raw) else {
                        rest.push(misfit("user", "content block"));
                        continue;
                    };
                    match block.kind.as_str() {
                        "tool_result" => self.answer(block, record, line),
                        "text" => match block.text.and_then(string) {
                            Some(text) => {
                                let said = said(&text, meta, origin.as_ref());
                                rest.extend(said.notes);
                                if let Some((text, from)) = said.prompt {
                                    let (prompt, _, sender) = prompt.get_or_insert_default();
                                    if !prompt.is_empty() {
                                        prompt.push_str("\n\n");
                                    }
                                    prompt.push_str(&text);
                                    *sender = from;
                                }
                            }
                            None => rest.push(misfit("text", "text")),
                        },
                        /* A picture the user didn't paste isn't part of a prompt. */
                        "image" if meta => rest.push(misfit("image", "isMeta")),
                        "image" => match block.source.and_then(|source| image(source, line)) {
                            Some(image) => prompt.get_or_insert_default().1.push(image),
                            None => rest.push(misfit("image", "source")),
                        },
                        _ => rest.push(Content::Unrecognised(unknown(block.kind, whole()))),
                    }
                }
                if let Some((text, images, from)) = prompt {
                    self.push(Content::Prompt { text, images, from });
                }
                for content in rest {
                    self.push(content);
                }
            }
            Shape::Other => self.misfit("user", "content", line.whole()),
        }
    }

    /// Takes in a `tool_result` block. Its call is looked for once the transcript is read.
    fn answer(&mut self, block: record::Block, record: &Record, line: &Line) {
        let content = block.content.map(shape);
        /* The call, when it was written before its result, as nearly all are. */
        let call = block
            .tool_use_id
            .as_ref()
            .and_then(|id| self.calls.get(id))
            .and_then(|(node, at)| match &self.nodes[*node].contents[*at] {
                Content::ToolCall(call) => Some((call.kind, call.name == WORKFLOW)),
                _ => None,
            });
        let first_text = match &content {
            Some(Shape::Text(text)) => Some(*text),
            Some(Shape::Blocks(parts)) => parts
                .first()
                .and_then(|raw| typed::<record::Block>(raw))
                .and_then(|part| part.text),
            _ => None,
        };
        let started = match call {
            Some((kind, workflow)) if kind != ToolKind::Subagent && !workflow => None,
            _ => record.tool_use_result.and_then(typed::<Launched>),
        };
        let result = ToolResult {
            at: self.nodes.last().and_then(|node| node.at),
            status: if block.is_error == Some(true) {
                ToolStatus::Failed
            } else {
                ToolStatus::Ok
            },
            brief: call
                .and_then(|(kind, _)| tools::brief(kind, first_text, record.tool_use_result)),
            output: match content {
                None => Vec::new(),
                Some(Shape::Text(text)) => vec![Output::Text(self.output_text(text, line))],
                Some(Shape::Blocks(parts)) => parts
                    .into_iter()
                    .map(|raw| {
                        let part = typed::<record::Block>(raw);
                        match part.as_ref().map(|part| (part.kind.as_str(), part)) {
                            Some((
                                "text",
                                record::Block {
                                    text: Some(text), ..
                                },
                            )) => Output::Text(self.output_text(text, line)),
                            Some((
                                "image",
                                record::Block {
                                    source: Some(source),
                                    ..
                                },
                            )) => match image(source, line) {
                                Some(image) => Output::Image(image),
                                None => Output::Text(line.stored(raw, Encoding::Plain)),
                            },
                            /* A part of another type is shown as it was recorded. */
                            _ => Output::Text(line.stored(raw, Encoding::Plain)),
                        }
                    })
                    .collect(),
                Some(Shape::Other) => block
                    .content
                    .map(|raw| Output::Text(line.stored(raw, Encoding::Plain)))
                    .into_iter()
                    .collect(),
            },
        };
        self.answers.push(Answer {
            call: block.tool_use_id,
            result,
            started: started
                .into_iter()
                .flat_map(|started| started.agent_id.into_iter().chain(started.run_id))
                .collect(),
            node: self.nodes.len() - 1,
            record: line.whole(),
        });
    }

    /**
     * Where a tool result's text is. Output too large for the transcript is saved under
     * `tool-results/` and the transcript holds a note in its place:
     *
     * ```text
     * <persisted-output>
     * Output too large (…). Full output saved to: <the file's path at the location>
     *
     * Preview (first …):
     * ```
     *
     * When that file is among the session's items the result is the file; otherwise it is the
     * note.
     */
    fn output_text(&self, raw: &RawValue, line: &Line) -> Stored {
        raw.get()
            .starts_with("\"<persisted-output>")
            .then(|| string(raw))
            .flatten()
            .and_then(|note| {
                let path = note
                    .split_once("Full output saved to: ")?
                    .1
                    .lines()
                    .next()?;
                let item = self.saved.get(path.trim().rsplit(['/', '\\']).next()?)?;
                Some(Stored {
                    item: Arc::from(item.key.as_str()),
                    offset: 0,
                    length: item.length,
                    encoding: Encoding::Plain,
                })
            })
            .unwrap_or_else(|| line.text(raw))
    }

    fn system(&mut self, record: &Record, line: &Line) {
        let kind = record.subtype.as_deref().unwrap_or_default();
        if kind == BOUNDARY {
            let tokens = record.compact_metadata.and_then(typed::<CompactMetadata>);
            let boundary = self.push(Content::Compaction {
                tokens_before: tokens.as_ref().and_then(|tokens| tokens.pre_tokens),
                tokens_after: tokens.as_ref().and_then(|tokens| tokens.post_tokens),
                summary: None,
            });
            self.compaction = Some(boundary);
            return;
        }
        let text = record.content.and_then(string).or_else(|| {
            let error = typed::<ApiError>(record.error?)?;
            string(error.formatted.or(error.message)?)
        });
        match text {
            Some(text) if !text.is_empty() => {
                self.push(Content::Note {
                    kind: kind.to_string(),
                    text,
                });
            }
            _ if SILENT.contains(&kind) || NOTICES.contains(&kind) => {}
            _ => {
                self.push(Content::Unrecognised(unknown(
                    format!("system/{kind}"),
                    line.whole(),
                )));
            }
        }
    }

    /// Of the attachments, only a prompt queued while the agent was working is conversation.
    fn attachment(&mut self, record: &Record, line: &Line) {
        let Some(attachment) = record.attachment.and_then(typed::<Attachment>) else {
            return self.misfit("attachment", "attachment", line.whole());
        };
        match attachment.kind.as_str() {
            QUEUED => match attachment.prompt.and_then(string) {
                Some(text) => {
                    let origin = attachment.origin.and_then(typed::<Origin>);
                    self.say(said(
                        &text,
                        attachment.is_meta == Some(true),
                        origin.as_ref(),
                    ));
                }
                None => self.misfit("attachment/queued_command", "prompt", line.whole()),
            },
            kind if CONTEXT.contains(&kind) => {}
            kind => {
                self.push(Content::Unrecognised(unknown(
                    format!("attachment/{kind}"),
                    line.whole(),
                )));
            }
        }
    }

    /// Pairs results with calls, names the subagents the calls started, and puts the records
    /// in the model's order.
    fn finish(mut self, items: &dyn SessionItems) -> Read {
        /* The calls that started something, by the id their result gave it. */
        let mut starters: HashMap<String, Place> = HashMap::new();
        for answer in std::mem::take(&mut self.answers) {
            let place = answer
                .call
                .as_ref()
                .and_then(|id| self.calls.get(id).copied());
            if let Some((node, at)) = place
                && let Content::ToolCall(call) = &mut self.nodes[node].contents[at]
            {
                if call.result.is_none() {
                    call.result = Some(answer.result);
                }
                starters.extend(answer.started.into_iter().map(|id| (id, (node, at))));
            } else {
                self.nodes[answer.node]
                    .contents
                    .push(Content::Unrecognised(Unrecognised {
                        kind: "tool_result".to_string(),
                        misfit: Some("tool_use_id"),
                        raw: answer.record,
                    }));
            }
        }
        let claimed = self.subagents(items, &starters);

        /* Each record after the one it follows, otherwise in the order they were written. A
        record that follows itself, around a loop, starts a tree instead. */
        let mut parent: Vec<Option<usize>> = self
            .nodes
            .iter()
            .map(|node| self.by_uuid.get(node.follows.as_ref()?).copied())
            .collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        let mut placed = vec![false; self.nodes.len()];
        for node in 0..self.nodes.len() {
            let mut above = Vec::new();
            let mut at = node;
            while !placed[at] {
                placed[at] = true;
                above.push(at);
                match parent[at] {
                    Some(follows) if !placed[follows] => at = follows,
                    Some(follows) if above.contains(&follows) => parent[at] = None,
                    _ => {}
                }
            }
            order.extend(above.into_iter().rev());
        }

        let hidden = hidden(&self.nodes, &parent, &order);
        let mut builder = Builder::new(self.item);
        let mut tail: Vec<Option<BlockId>> = vec![None; self.nodes.len()];
        for node in order {
            let Node {
                at,
                model,
                contents,
                ..
            } = &mut self.nodes[node];
            let mut last = parent[node].and_then(|parent| tail[parent]);
            for content in contents.drain(..) {
                let model = match content {
                    Content::Reply { .. } | Content::Thinking { .. } | Content::ToolCall(_) => {
                        model.as_deref()
                    }
                    _ => None,
                };
                last = Some(if hidden[node] {
                    builder.hide(last, *at, model, content)
                } else {
                    builder.show(last, *at, model, content)
                });
            }
            tail[node] = last;
        }
        for record in self.unplaced {
            builder.unplaced(record);
        }
        builder.used(self.tokens);
        Read {
            conversation: builder.finish(self.ending),
            claimed,
            title: self.title,
            cwd: self.cwd,
            branch: self.branch,
            entrypoint: self.entrypoint,
        }
    }

    /**
     * Names, under each call, the subagents it started, and returns their transcripts' keys.
     * A subagent's file is named by the `agentId` the result of its call recorded, and a
     * workflow's subagents are in a folder named by the run. Only when a call that starts
     * subagents has none by those is each remaining transcript's `.meta.json` read, for the
     * `tool_use` it names.
     */
    fn subagents(
        &mut self,
        items: &dyn SessionItems,
        starters: &HashMap<String, Place>,
    ) -> HashSet<String> {
        let mut transcripts: Vec<&str> = items
            .list()
            .iter()
            .filter(|item| item.kind == ItemKind::Subagent && *item.key != *self.item)
            .map(|item| item.key.as_str())
            .collect();
        transcripts.sort_unstable();
        let mut claimed = HashSet::new();
        let mut start = |claimed: &mut HashSet<String>, (node, at): Place, key: &str| {
            if let Content::ToolCall(call) = &mut self.nodes[node].contents[at] {
                call.subagents.push(Subagent::Unread { item: key.into() });
                claimed.insert(key.to_string());
            }
        };
        for key in &transcripts {
            let agent = file_name(key)
                .strip_prefix("agent-")
                .and_then(|name| name.strip_suffix(".jsonl"));
            let run = key
                .split_once("/subagents/workflows/")
                .and_then(|(_, rest)| rest.split('/').next());
            if let Some(place) = [run, agent]
                .into_iter()
                .flatten()
                .find_map(|id| starters.get(id))
            {
                start(&mut claimed, *place, key);
            }
        }
        let unresolved = self
            .nodes
            .iter()
            .flat_map(|node| &node.contents)
            .any(|content| match content {
                Content::ToolCall(call) => {
                    (call.kind == ToolKind::Subagent || call.name == WORKFLOW)
                        && call.subagents.is_empty()
                }
                _ => false,
            });
        if unresolved {
            for key in transcripts {
                if claimed.contains(key) {
                    continue;
                }
                let named = sidecar::<SubagentMeta>(items, &meta_key(key))
                    .ok()
                    .flatten()
                    .and_then(|meta| self.calls.get(&meta.tool_use_id?).copied());
                if let Some(place) = named
                    && let Content::ToolCall(call) = &mut self.nodes[place.0].contents[place.1]
                {
                    call.subagents.push(Subagent::Unread { item: key.into() });
                    claimed.insert(key.to_string());
                }
            }
        }
        claimed
    }
}

/// A record or block of a type this parser doesn't know.
fn unknown(kind: String, raw: Stored) -> Unrecognised {
    Unrecognised {
        kind,
        misfit: None,
        raw,
    }
}

/// An image block's `source`, when it holds the image as base64.
fn image(source: &RawValue, line: &Line) -> Option<Image> {
    let source = typed::<Source>(source)?;
    let data = source
        .data
        .filter(|_| source.kind == "base64")?
        .get()
        .strip_prefix('"')?
        .strip_suffix('"')?;
    Some(Image {
        media_type: source.media_type.unwrap_or_default(),
        data: Stored::part(line.item, line.offset, line.bytes, data, Encoding::Base64),
    })
}

/**
 * Which records are on branches that aren't shown (the module docs). `parent` is each
 * record's parent, and `order` has every record after its parent.
 */
fn hidden(nodes: &[Node], parent: &[Option<usize>], order: &[usize]) -> Vec<bool> {
    /* A transcript can hold more than one tree: a record whose parent isn't in it starts
    another. Each tree's chain runs from the record written last back to its root. */
    let mut root: Vec<usize> = (0..nodes.len()).collect();
    let mut latest: Vec<usize> = (0..nodes.len()).collect();
    for &node in order {
        if let Some(parent) = parent[node] {
            root[node] = root[parent];
        }
        latest[root[node]] = latest[root[node]].max(node);
    }
    let mut chain = vec![false; nodes.len()];
    for tree in (0..nodes.len()).filter(|node| root[*node] == *node) {
        let mut at = Some(latest[tree]);
        while let Some(node) = at {
            chain[node] = true;
            at = parent[node];
        }
    }
    /* A side branch is named by its first record, the one whose parent is on the chain. It is
    a rewound one when the first thing said on it is the user's prompt. */
    let mut branch: Vec<usize> = (0..nodes.len()).collect();
    let mut rewound: Vec<Option<bool>> = vec![None; nodes.len()];
    for &node in order {
        if chain[node] {
            continue;
        }
        if let Some(parent) = parent[node].filter(|parent| !chain[*parent]) {
            branch[node] = branch[parent];
        }
        if rewound[branch[node]].is_none() {
            rewound[branch[node]] = nodes[node]
                .contents
                .iter()
                .find_map(|content| match content {
                    Content::Prompt { from: None, .. } => Some(true),
                    Content::Note { .. } | Content::Unrecognised(_) => None,
                    _ => Some(false),
                });
        }
    }
    (0..nodes.len())
        .map(|node| !chain[node] && rewound[branch[node]] == Some(true))
        .collect()
}
