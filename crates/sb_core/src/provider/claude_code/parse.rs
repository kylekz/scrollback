//! From a session's archived items to the transcript model (PLAN §7.3–§7.5).
//!
//! A transcript is read once, a line at a time, and nothing of a line is kept but what the
//! model holds: the text of prompts, replies and notes, each tool call's typed input, and for
//! everything large (tool input and output, images) where it is, as a `Stored`. So a session of
//! tens of megabytes parses into a small fraction of its size, and its tool output is read
//! from the archive when a row is opened (PLAN §9).
//!
//! How the records become the model:
//!
//! - **The tree.** Conversation records (`user`, `assistant`, `system`, `attachment`) carry a
//!   `uuid` and the `parentUuid` of the record they follow. A record written again with a uuid
//!   already read (a resumed session repeats some) is passed over. A compaction boundary has a
//!   null `parentUuid` and its `logicalParentUuid` is followed instead.
//! - **The branch shown** ends at the latest record. Walking `parentUuid` back from it is not
//!   the whole branch: a reply is written as one record per content block, and when it calls
//!   several tools at once the results hang off their own `tool_use` records, beside the chain
//!   and not on it. What sets a rewound branch apart from those is that it holds a prompt: the
//!   user went back and asked again. So everything is shown except the side branches with a
//!   prompt in them.
//! - **Tool results** are `tool_result` blocks of `user` records, paired with their call by
//!   `tool_use_id` wherever in the tree either is. Output Claude Code saved outside the
//!   transcript is recorded as a `<persisted-output>` note naming the file under
//!   `tool-results/`, and the result points at that item.
//! - **Subagents.** Each has its own transcript. Its `.meta.json` names the `tool_use` that
//!   started it, which can be in another subagent's transcript; without one, the call is found
//!   by the `agentId` its result recorded, or for a workflow's subagents by the run's id.
//! - **Everything else.** Bookkeeping records are passed over. A record of a type this parser
//!   doesn't know is kept as an `Unrecognised`: in the tree when it has a `uuid`, beside the
//!   conversation otherwise. A known record whose fields aren't as this parser reads them is
//!   kept the same way, saying which part didn't fit (PLAN D5).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead as _, BufReader, Read as _};
use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use super::record::{
    self, ApiError, Attachment, CompactMetadata, Head, Launched, Line, Message, Origin, Record,
    Shape, Source, SubagentMeta, TitleFile, Usage, shape, string, typed,
};
use super::{ID, tools};
use crate::provider::{ItemKind, ParseError, SessionItem, SessionItems};
use crate::transcript::{
    Block, BlockId, Content, Conversation, Encoding, Facts, Image, Output, Stored, Subagent,
    Tokens, ToolCall, ToolKind, ToolResult, ToolStatus, Transcript, Turn, Unrecognised,
};

/// The record types that hold no conversation and are passed over.
const BOOKKEEPING: [&str; 18] = [
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

/// The `origin` of the notice that a background task finished, which is also its tag.
const NOTICE: &str = "task-notification";

/// The tags Claude Code wraps text in that it puts in the user's place itself.
const WRAPPERS: [&str; 4] = [
    "system-reminder",
    "local-command-stdout",
    "local-command-caveat",
    NOTICE,
];

/// The model named on replies Claude Code wrote itself (an interruption, an API error).
const SYNTHETIC: &str = "<synthetic>";

/// The tool that runs a workflow, whose subagents are filed by the run's id.
const WORKFLOW: &str = "Workflow";

/// The session's saved tool output, by file name.
type Saved<'a> = HashMap<&'a str, &'a SessionItem>;

pub(super) fn parse(items: &dyn SessionItems) -> Result<Transcript, ParseError> {
    let list = items.list();
    let main = list
        .iter()
        .find(|item| item.kind == ItemKind::Transcript)
        .ok_or(ParseError::NoTranscript)?;
    let saved: Saved = list
        .iter()
        .filter(|item| item.kind == ItemKind::ToolResult)
        .map(|item| (file_name(&item.key), item))
        .collect();
    let main = Reader::of(items, main, &saved)?;

    let transcripts: Vec<&SessionItem> = list
        .iter()
        .filter(|item| item.kind == ItemKind::Subagent)
        .collect();
    let mut readers = Vec::new();
    let mut metas = Vec::new();
    for item in &transcripts {
        readers.push(Reader::of(items, item, &saved)?);
        let meta = item.key.strip_suffix(".jsonl").unwrap_or(&item.key);
        metas.push(
            sidecar::<SubagentMeta>(items, &format!("{meta}.meta.json"))?.unwrap_or_default(),
        );
    }

    /* Which call started each subagent: one in the main transcript (`None`) or in a
    subagent's. */
    let mut started_by_main = Vec::new();
    let mut started_by = vec![Vec::new(); readers.len()];
    {
        let calls: HashMap<&str, (Option<usize>, BlockId)> = std::iter::once((None, &main))
            .chain(
                readers
                    .iter()
                    .enumerate()
                    .map(|(n, reader)| (Some(n), reader)),
            )
            .flat_map(|(owner, reader)| {
                reader
                    .calls
                    .iter()
                    .map(move |(id, block)| (id.as_str(), (owner, *block)))
            })
            .collect();
        for (index, (item, meta)) in transcripts.iter().zip(&metas).enumerate() {
            let agent = file_name(&item.key)
                .strip_prefix("agent-")
                .and_then(|name| name.strip_suffix(".jsonl"));
            let run = item
                .key
                .split_once("/subagents/workflows/")
                .and_then(|(_, rest)| rest.split('/').next());
            match [meta.tool_use_id.as_deref(), run, agent]
                .into_iter()
                .flatten()
                .find_map(|id| calls.get(id))
            {
                Some((None, block)) => started_by_main.push((index, *block)),
                Some((Some(owner), block)) => started_by[*owner].push((index, *block)),
                None => {}
            }
        }
    }

    let title = match main.title {
        Some(title) => Some(title),
        None => match list
            .iter()
            .find(|item| item.key.ends_with("/custom-title.json"))
        {
            Some(item) => sidecar::<TitleFile>(items, &item.key)?.map(|file| file.custom_title),
            None => None,
        },
    };
    let mut subagents: Vec<Option<Subagent>> = readers
        .into_iter()
        .zip(metas)
        .map(|(reader, meta)| {
            Some(Subagent {
                kind: meta.agent_type,
                description: meta.description,
                conversation: reader.conversation,
            })
        })
        .collect();
    let mut conversation = main.conversation;
    attach(
        &mut conversation,
        &started_by_main,
        &mut subagents,
        &started_by,
    );
    let mut unattached = Vec::new();
    for index in 0..subagents.len() {
        if let Some(mut subagent) = subagents[index].take() {
            attach(
                &mut subagent.conversation,
                &started_by[index],
                &mut subagents,
                &started_by,
            );
            unattached.push(subagent);
        }
    }
    Ok(Transcript {
        facts: Facts {
            agent: ID,
            title,
            project_path: main.cwd,
            branch: main.branch,
            entrypoint: main.entrypoint,
        },
        conversation,
        unattached,
    })
}

/// Puts each subagent that a call of `conversation` started under that call, with the
/// subagents its own calls started under those. `started_by[n]` is what subagent `n` started.
fn attach(
    conversation: &mut Conversation,
    started: &[(usize, BlockId)],
    subagents: &mut [Option<Subagent>],
    started_by: &[Vec<(usize, BlockId)>],
) {
    for &(index, block) in started {
        let Some(mut subagent) = subagents[index].take() else {
            continue;
        };
        attach(
            &mut subagent.conversation,
            &started_by[index],
            subagents,
            started_by,
        );
        if let Content::ToolCall(call) = &mut conversation.blocks[block.0].content {
            call.subagents.push(subagent);
        }
    }
}

fn file_name(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/// The small JSON file `key` of the session, when it is there and is what `T` reads.
fn sidecar<T: DeserializeOwned>(
    items: &dyn SessionItems,
    key: &str,
) -> Result<Option<T>, ParseError> {
    if !items.list().iter().any(|item| item.key == key) {
        return Ok(None);
    }
    let mut json = Vec::new();
    items
        .open(key, 0)
        .and_then(|mut file| file.read_to_end(&mut json))
        .map_err(|source| ParseError::Read {
            key: key.to_string(),
            source,
        })?;
    Ok(serde_json::from_slice(&json).ok())
}

/// A record in the tree the `parentUuid`s make.
struct Node {
    parent: Option<usize>,
    /// Its blocks run from here to the next node's first.
    first: usize,
    /// The last block of the record, or of the nearest before it that has one.
    tail: Option<BlockId>,
    /// One of its blocks is a prompt.
    prompt: bool,
}

/// When a record was written and where it is: what each of its blocks is stamped with.
#[derive(Clone, Copy)]
struct Stamp {
    at: Option<i64>,
    offset: u64,
}

/// One transcript being read.
struct Reader<'a> {
    saved: &'a Saved<'a>,
    conversation: Conversation,
    nodes: Vec<Node>,
    by_uuid: HashMap<String, usize>,
    /// Tool calls by their `tool_use` id, and those that started a subagent or a workflow run
    /// also by the id their result gave it.
    calls: HashMap<String, BlockId>,
    /// The replies whose usage is counted, by `message.id`.
    counted: HashSet<String>,
    /// The latest reply's model, shared by its blocks.
    model: Option<Arc<str>>,
    /// The compaction boundary still waiting for its summary.
    compaction: Option<BlockId>,
    title: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    entrypoint: Option<String>,
}

impl<'a> Reader<'a> {
    fn of(
        items: &dyn SessionItems,
        item: &SessionItem,
        saved: &'a Saved<'a>,
    ) -> Result<Self, ParseError> {
        let failed = |source| ParseError::Read {
            key: item.key.clone(),
            source,
        };
        let key: Arc<str> = Arc::from(item.key.as_str());
        let mut reader = Self {
            saved,
            conversation: Conversation {
                item: key.clone(),
                blocks: Vec::new(),
                turns: Vec::new(),
                unplaced: Vec::new(),
                models: Vec::new(),
                tokens: Tokens::default(),
                cut_short: false,
            },
            nodes: Vec::new(),
            by_uuid: HashMap::new(),
            calls: HashMap::new(),
            counted: HashSet::new(),
            model: None,
            compaction: None,
            title: None,
            cwd: None,
            branch: None,
            entrypoint: None,
        };
        let mut bytes =
            BufReader::with_capacity(1 << 16, items.open(&item.key, 0).map_err(failed)?);
        let mut buffer = Vec::new();
        let mut offset = 0;
        loop {
            buffer.clear();
            let length = bytes.read_until(b'\n', &mut buffer).map_err(failed)?;
            if length == 0 {
                break;
            }
            let ended = buffer.ends_with(b"\n");
            let text = buffer.strip_suffix(b"\n").unwrap_or(&buffer);
            let line = Line {
                item: &key,
                offset,
                bytes: text.strip_suffix(b"\r").unwrap_or(text),
            };
            if !line.bytes.is_empty() && !reader.record(&line) {
                if ended {
                    reader.conversation.unplaced.push(Unrecognised {
                        kind: String::new(),
                        misfit: Some("not a record"),
                        raw: line.whole(),
                    });
                } else {
                    /* The last line, still being written when it was archived. */
                    reader.conversation.cut_short = true;
                }
            }
            offset += length as u64;
        }
        reader.conversation.turns = turns(&reader.nodes, &reader.conversation.blocks);
        Ok(reader)
    }

    /// Takes in one line. `false` when it isn't a record: not a JSON object with a `type`.
    fn record(&mut self, line: &Line) -> bool {
        let mut record = match serde_json::from_slice::<Record>(line.bytes) {
            Ok(record) => record,
            Err(_) => {
                let Ok(head) = serde_json::from_slice::<Head>(line.bytes) else {
                    return false;
                };
                self.conversation.unplaced.push(Unrecognised {
                    kind: head.kind,
                    misfit: Some("record"),
                    raw: line.whole(),
                });
                return true;
            }
        };
        if self.cwd.is_none() {
            self.cwd = record.cwd.take();
        }
        if self.branch.is_none() {
            self.branch = record.git_branch.take().filter(|branch| !branch.is_empty());
        }
        if self.entrypoint.is_none() {
            self.entrypoint = record.entrypoint.take();
        }
        let stamp = Stamp {
            at: record
                .timestamp
                .as_deref()
                .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                .map(|time| time.timestamp_millis()),
            offset: line.offset,
        };
        match record.kind.as_str() {
            /* The last one is the title. */
            "custom-title" => self.title = record.custom_title.take().or(self.title.take()),
            kind if BOOKKEEPING.contains(&kind) => {}
            _ if record
                .uuid
                .as_ref()
                .is_some_and(|uuid| self.by_uuid.contains_key(uuid)) => {}
            "assistant" => {
                self.node(&mut record);
                self.assistant(&record, line, stamp);
            }
            "user" => {
                self.node(&mut record);
                self.user(&record, line, stamp);
            }
            "system" => {
                self.node(&mut record);
                self.system(&record, stamp);
            }
            "attachment" => {
                self.node(&mut record);
                self.attachment(&record, line, stamp);
            }
            _ if record.uuid.is_some() => {
                self.node(&mut record);
                self.push(
                    stamp,
                    Content::Unrecognised(unknown(record.kind, line.whole())),
                );
            }
            _ => self
                .conversation
                .unplaced
                .push(unknown(record.kind, line.whole())),
        }
        true
    }

    /// Gives the record its place in the tree. Its blocks are pushed next.
    fn node(&mut self, record: &mut Record) {
        let parent = record
            .parent_uuid
            .as_ref()
            .or(record.logical_parent_uuid.as_ref())
            .and_then(|uuid| self.by_uuid.get(uuid).copied());
        if let Some(uuid) = record.uuid.take() {
            self.by_uuid.insert(uuid, self.nodes.len());
        }
        self.nodes.push(Node {
            parent,
            first: self.conversation.blocks.len(),
            tail: parent.and_then(|parent| self.nodes[parent].tail),
            prompt: false,
        });
    }

    /// Adds a block of the record whose node was made last.
    fn push(&mut self, stamp: Stamp, content: Content) -> BlockId {
        let id = BlockId(self.conversation.blocks.len());
        let node = self
            .nodes
            .last_mut()
            .expect("a record's node is made before its blocks are pushed");
        let model = match content {
            Content::Reply { .. } | Content::Thinking { .. } | Content::ToolCall(_) => {
                self.model.clone()
            }
            _ => None,
        };
        node.prompt |= matches!(content, Content::Prompt { from: None, .. });
        self.conversation.blocks.push(Block {
            parent: node.tail.replace(id),
            at: stamp.at,
            offset: stamp.offset,
            model,
            content,
        });
        id
    }

    /// Keeps a record of the known type `kind` whose `part` isn't as this parser reads it.
    fn misfit(&mut self, kind: &str, part: &'static str, raw: Stored, stamp: Stamp) {
        self.push(
            stamp,
            Content::Unrecognised(Unrecognised {
                kind: kind.to_string(),
                misfit: Some(part),
                raw,
            }),
        );
    }

    fn assistant(&mut self, record: &Record, line: &Line, stamp: Stamp) {
        let Some(message) = record.message.and_then(typed::<Message>) else {
            return self.misfit("assistant", "message", line.whole(), stamp);
        };
        self.model = match message.model.filter(|model| model != SYNTHETIC) {
            Some(model) if self.model.as_deref() == Some(model.as_str()) => self.model.take(),
            Some(model) => {
                if !self.conversation.models.contains(&model) {
                    self.conversation.models.push(model.clone());
                }
                Some(Arc::from(model))
            }
            None => None,
        };
        if message.id.is_none_or(|id| self.counted.insert(id))
            && let Some(usage) = message.usage.and_then(typed::<Usage>)
        {
            let tokens = &mut self.conversation.tokens;
            tokens.input += usage.input_tokens.unwrap_or(0);
            tokens.output += usage.output_tokens.unwrap_or(0);
            tokens.cache_read += usage.cache_read_input_tokens.unwrap_or(0);
            tokens.cache_write += usage.cache_creation_input_tokens.unwrap_or(0);
        }
        match message.content.map(shape) {
            Some(Shape::Text(text)) => {
                if let Some(text) = string(text) {
                    self.push(stamp, Content::Reply { text });
                }
            }
            Some(Shape::Blocks(blocks)) => {
                for raw in blocks {
                    self.assistant_block(raw, record, line, stamp);
                }
            }
            _ => self.misfit("assistant", "content", line.whole(), stamp),
        }
    }

    fn assistant_block(&mut self, raw: &RawValue, record: &Record, line: &Line, stamp: Stamp) {
        let whole = || line.stored(raw, Encoding::Plain);
        let Some(block) = typed::<record::Block>(raw) else {
            return self.misfit("assistant", "content block", whole(), stamp);
        };
        match block.kind.as_str() {
            "text" => match block.text.and_then(string) {
                /* A reply that is only a tool call starts with an empty text block. */
                Some(text) if text.trim().is_empty() => {}
                Some(text) => {
                    self.push(stamp, Content::Reply { text });
                }
                None => self.misfit("text", "text", whole(), stamp),
            },
            "thinking" => {
                self.push(
                    stamp,
                    Content::Thinking {
                        text: block.thinking.unwrap_or_default(),
                        duration: record.thinking_duration_ms.map(Duration::from_millis),
                    },
                );
            }
            "tool_use" => match (block.id, block.name, block.input) {
                (Some(id), Some(name), Some(input)) => {
                    let call = ToolCall {
                        input: tools::input(&name, input, line),
                        raw_input: line.stored(input, Encoding::Plain),
                        name,
                        result: None,
                        subagents: Vec::new(),
                    };
                    let block = self.push(stamp, Content::ToolCall(Box::new(call)));
                    self.calls.insert(id, block);
                }
                _ => self.misfit("tool_use", "id, name or input", whole(), stamp),
            },
            _ => {
                self.push(stamp, Content::Unrecognised(unknown(block.kind, whole())));
            }
        }
    }

    fn user(&mut self, record: &Record, line: &Line, stamp: Stamp) {
        let Some(content) = record
            .message
            .and_then(typed::<Message>)
            .and_then(|message| message.content)
        else {
            return self.misfit("user", "message", line.whole(), stamp);
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
            let boundary = self.compaction.take().unwrap_or_else(|| {
                self.push(
                    stamp,
                    Content::Compaction {
                        tokens_before: None,
                        tokens_after: None,
                        summary: None,
                    },
                )
            });
            if let Content::Compaction { summary: slot, .. } =
                &mut self.conversation.blocks[boundary.0].content
            {
                *slot = summary;
            }
            return;
        }
        let meta = record.is_meta == Some(true);
        let origin = record.origin.and_then(typed::<Origin>);
        let origin = origin.as_ref().map(|origin| origin.kind.as_str());
        match content {
            Shape::Text(text) => match string(text) {
                Some(text) => {
                    self.push(stamp, said(text, meta, origin));
                }
                None => self.misfit("user", "content", line.whole(), stamp),
            },
            Shape::Blocks(blocks) => {
                /* What was said comes first, its text blocks as one prompt. */
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
                        "tool_result" => self.result(block, record, line, stamp),
                        "text" => match block
                            .text
                            .and_then(string)
                            .map(|text| said(text, meta, origin))
                        {
                            Some(Content::Prompt { text, from, .. }) => {
                                let (prompt, _, sender) = prompt.get_or_insert_default();
                                if !prompt.is_empty() {
                                    prompt.push_str("\n\n");
                                }
                                prompt.push_str(&text);
                                *sender = from;
                            }
                            Some(note) => rest.push(note),
                            None => rest.push(misfit("text", "text")),
                        },
                        "image" => match block.source.and_then(|source| image(source, line)) {
                            Some(image) => prompt.get_or_insert_default().1.push(image),
                            None => rest.push(misfit("image", "source")),
                        },
                        _ => rest.push(Content::Unrecognised(unknown(block.kind, whole()))),
                    }
                }
                if let Some((text, images, from)) = prompt {
                    self.push(stamp, Content::Prompt { text, images, from });
                }
                for content in rest {
                    self.push(stamp, content);
                }
            }
            Shape::Other => self.misfit("user", "content", line.whole(), stamp),
        }
    }

    /// Pairs a `tool_result` block with its call.
    fn result(&mut self, block: record::Block, record: &Record, line: &Line, stamp: Stamp) {
        let output = match block.content.map(shape) {
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
        };
        let Some(id) = block
            .tool_use_id
            .and_then(|id| self.calls.get(&id).copied())
        else {
            return self.misfit("tool_result", "tool_use_id", line.whole(), stamp);
        };
        let Content::ToolCall(call) = &mut self.conversation.blocks[id.0].content else {
            return;
        };
        call.result = Some(ToolResult {
            at: stamp.at,
            status: if block.is_error == Some(true) {
                ToolStatus::Failed
            } else {
                ToolStatus::Ok
            },
            output,
        });
        if (tools::kind(&call.name) == ToolKind::Subagent || call.name == WORKFLOW)
            && let Some(launched) = record.tool_use_result.and_then(typed::<Launched>)
        {
            self.calls.extend(
                launched
                    .agent_id
                    .into_iter()
                    .chain(launched.run_id)
                    .map(|key| (key, id)),
            );
        }
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

    fn system(&mut self, record: &Record, stamp: Stamp) {
        let kind = record.subtype.as_deref().unwrap_or("system");
        if kind == "compact_boundary" {
            let tokens = record.compact_metadata.and_then(typed::<CompactMetadata>);
            let boundary = self.push(
                stamp,
                Content::Compaction {
                    tokens_before: tokens.as_ref().and_then(|tokens| tokens.pre_tokens),
                    tokens_after: tokens.as_ref().and_then(|tokens| tokens.post_tokens),
                    summary: None,
                },
            );
            self.compaction = Some(boundary);
            return;
        }
        let text = record.content.and_then(string).or_else(|| {
            let error = typed::<ApiError>(record.error?)?;
            string(error.formatted.or(error.message)?)
        });
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            self.push(
                stamp,
                Content::Note {
                    kind: kind.to_string(),
                    text,
                },
            );
        }
    }

    /// Of the attachments, only a prompt queued while the agent was working is conversation.
    fn attachment(&mut self, record: &Record, line: &Line, stamp: Stamp) {
        let Some(attachment) = record.attachment.and_then(typed::<Attachment>) else {
            return self.misfit("attachment", "attachment", line.whole(), stamp);
        };
        if attachment.kind == "queued_command"
            && let Some(text) = attachment.prompt.and_then(string)
        {
            let origin = attachment.origin.and_then(typed::<Origin>);
            self.push(
                stamp,
                said(
                    text,
                    attachment.is_meta == Some(true),
                    origin.as_ref().map(|origin| origin.kind.as_str()),
                ),
            );
        }
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

/**
 * Text in the user's place: a prompt, or a note when nobody asked the agent anything with it.
 *
 * - With an `origin` other than `human` it came from elsewhere. A finished background task's
 *   notice is a note; anything else (another session, a coordinator) is a prompt from that
 *   sender.
 * - Wrapped in one of the tags Claude Code uses for what it puts there itself, or on a record
 *   marked `isMeta`, it is a note, of the tag's kind or `meta`.
 */
fn said(text: String, meta: bool, origin: Option<&str>) -> Content {
    let note = match origin.filter(|origin| *origin != "human") {
        Some(NOTICE) => Some(NOTICE),
        Some(sender) => {
            return Content::Prompt {
                text,
                images: Vec::new(),
                from: Some(sender.to_string()),
            };
        }
        None => text
            .trim_start()
            .strip_prefix('<')
            .and_then(|rest| {
                WRAPPERS.into_iter().find(|tag| {
                    rest.strip_prefix(*tag)
                        .is_some_and(|rest| rest.starts_with('>'))
                })
            })
            .or(meta.then_some("meta")),
    };
    match note {
        Some(kind) => Content::Note {
            kind: kind.to_string(),
            text,
        },
        None => Content::Prompt {
            text,
            images: Vec::new(),
            from: None,
        },
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

/// The shown branch as turns (the module docs).
fn turns(nodes: &[Node], blocks: &[Block]) -> Vec<Turn> {
    /* A transcript can hold more than one tree: a record whose parent isn't in it starts
    another. Each tree's chain runs from its latest record back to its root. */
    let mut root = Vec::with_capacity(nodes.len());
    let mut latest: Vec<usize> = (0..nodes.len()).collect();
    for (index, node) in nodes.iter().enumerate() {
        let tree = node.parent.map_or(index, |parent| root[parent]);
        root.push(tree);
        latest[tree] = index;
    }
    let mut chain = vec![false; nodes.len()];
    for tree in (0..nodes.len()).filter(|index| root[*index] == *index) {
        let mut at = Some(latest[tree]);
        while let Some(index) = at {
            chain[index] = true;
            at = nodes[index].parent;
        }
    }
    /* A side branch is named by its first record, the one whose parent is on the chain. */
    let mut branch: Vec<usize> = (0..nodes.len()).collect();
    let mut rewound = vec![false; nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        if !chain[index] {
            if let Some(parent) = node.parent.filter(|parent| !chain[*parent]) {
                branch[index] = branch[parent];
            }
            rewound[branch[index]] |= node.prompt;
        }
    }
    let mut turns: Vec<Turn> = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        if !chain[index] && rewound[branch[index]] {
            continue;
        }
        let end = nodes.get(index + 1).map_or(blocks.len(), |next| next.first);
        for (id, block) in blocks.iter().enumerate().take(end).skip(node.first) {
            match turns.last_mut() {
                Some(turn) if !matches!(block.content, Content::Prompt { .. }) => {
                    turn.blocks.push(BlockId(id));
                }
                _ => turns.push(Turn {
                    blocks: vec![BlockId(id)],
                }),
            }
        }
    }
    turns
}
