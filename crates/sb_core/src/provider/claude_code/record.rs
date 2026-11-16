//! The shapes of Claude Code's records, as far as the parser reads them (PLAN §7.3). Fields it
//! doesn't read are skipped, so a release that adds some changes nothing here. The parts that
//! are large or whose shape varies stay as raw JSON borrowed from the record's line: the parser
//! looks into the ones it needs, and for the rest keeps only where they are ([`Line::stored`]).

use std::sync::Arc;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

use crate::transcript::{Encoding, Stored, json_string};

/// One record while it is read: its bytes, without the newline, and where they are.
pub(super) struct Line<'a> {
    pub item: &'a Arc<str>,
    pub offset: u64,
    pub bytes: &'a [u8],
}

impl Line<'_> {
    /// Where `raw`, a part of this record, is archived.
    pub fn stored(&self, raw: &RawValue, encoding: Encoding) -> Stored {
        Stored::part(self.item, self.offset, self.bytes, raw.get(), encoding)
    }

    /// Where the text `raw` is archived: a JSON string, or whatever else was recorded in its
    /// place, as it was recorded.
    pub fn text(&self, raw: &RawValue) -> Stored {
        let encoding = if raw.get().starts_with('"') {
            Encoding::JsonString
        } else {
            Encoding::Plain
        };
        self.stored(raw, encoding)
    }

    /// The whole record.
    pub fn whole(&self) -> Stored {
        Stored {
            item: self.item.clone(),
            offset: self.offset,
            length: self.bytes.len() as u64,
            encoding: Encoding::Plain,
        }
    }
}

/// The text `raw` is, when it is a JSON string.
pub(super) fn string(raw: &RawValue) -> Option<String> {
    json_string(raw.get())
}

/// What `raw` holds, when it is in the shape `T` reads.
pub(super) fn typed<'a, T: Deserialize<'a>>(raw: &'a RawValue) -> Option<T> {
    serde_json::from_str(raw.get()).ok()
}

/**
 * Text that someone wrote or that was cut to fit: a JSON string read as [`json_string`] reads
 * one, so that half a surrogate pair left by a cut doesn't make its record unreadable.
 */
pub(super) struct Text(pub String);

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        string(<&RawValue>::deserialize(deserializer)?)
            .map(Self)
            .ok_or_else(|| D::Error::custom("not a string"))
    }
}

/// A record's type alone, for one whose other fields aren't as [`Record`] reads them.
#[derive(Deserialize)]
pub(super) struct Head {
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Record<'a> {
    #[serde(rename = "type")]
    pub kind: String,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    /// On a compaction boundary, whose `parentUuid` is null: the record it follows.
    pub logical_parent_uuid: Option<String>,
    pub timestamp: Option<String>,
    pub cwd: Option<Text>,
    pub git_branch: Option<Text>,
    pub entrypoint: Option<String>,
    pub subtype: Option<String>,
    pub custom_title: Option<Text>,
    /// The record was put into the conversation by Claude Code, not typed by the user.
    pub is_meta: Option<bool>,
    /// The record is of a subagent's conversation, not the session's own.
    pub is_sidechain: Option<bool>,
    pub is_compact_summary: Option<bool>,
    pub thinking_duration_ms: Option<u64>,
    #[serde(borrow)]
    pub message: Option<&'a RawValue>,
    /// A `system` record's text.
    #[serde(borrow)]
    pub content: Option<&'a RawValue>,
    #[serde(borrow)]
    pub error: Option<&'a RawValue>,
    #[serde(borrow)]
    pub tool_use_result: Option<&'a RawValue>,
    #[serde(borrow)]
    pub attachment: Option<&'a RawValue>,
    #[serde(borrow)]
    pub compact_metadata: Option<&'a RawValue>,
    #[serde(borrow)]
    pub origin: Option<&'a RawValue>,
}

#[derive(Deserialize)]
pub(super) struct Message<'a> {
    /// The API's id of the reply. One reply is written as a record per content block, all
    /// with this id and the same usage.
    pub id: Option<String>,
    pub model: Option<String>,
    #[serde(borrow)]
    pub usage: Option<&'a RawValue>,
    #[serde(borrow)]
    pub content: Option<&'a RawValue>,
}

#[derive(Deserialize)]
pub(super) struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

/// A message's content, or a tool result's: text, or a list of blocks.
pub(super) enum Shape<'a> {
    Text(&'a RawValue),
    Blocks(Vec<&'a RawValue>),
    Other,
}

pub(super) fn shape(raw: &RawValue) -> Shape<'_> {
    if raw.get().starts_with('"') {
        Shape::Text(raw)
    } else {
        typed(raw).map_or(Shape::Other, Shape::Blocks)
    }
}

/// A content block of any type: the fields of them all.
#[derive(Deserialize)]
pub(super) struct Block<'a> {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(borrow)]
    pub text: Option<&'a RawValue>,
    pub thinking: Option<Text>,
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(borrow)]
    pub input: Option<&'a RawValue>,
    pub tool_use_id: Option<String>,
    #[serde(borrow)]
    pub content: Option<&'a RawValue>,
    pub is_error: Option<bool>,
    #[serde(borrow)]
    pub source: Option<&'a RawValue>,
}

impl Block<'_> {
    /// The call the block makes, its id and its tool's name: a `tool_use` with an id, a name
    /// and an input. What the parse keeps as a call, and the outline counts.
    pub fn call(&self) -> Option<(&str, &str)> {
        match (self.kind.as_str(), &self.id, &self.name, self.input) {
            ("tool_use", Some(id), Some(name), Some(_)) => Some((id, name)),
            _ => None,
        }
    }

    /// The call a `tool_result` block answers.
    pub fn answers(&self) -> Option<&str> {
        (self.kind == "tool_result")
            .then_some(self.tool_use_id.as_deref())
            .flatten()
    }
}

/// An image block's `source`.
#[derive(Deserialize)]
pub(super) struct Source<'a> {
    #[serde(rename = "type")]
    pub kind: String,
    pub media_type: Option<String>,
    #[serde(borrow)]
    pub data: Option<&'a RawValue>,
}

/// An `attachment` record's `attachment`, as far as a queued prompt has it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Attachment<'a> {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(borrow)]
    pub prompt: Option<&'a RawValue>,
    pub is_meta: Option<bool>,
    #[serde(borrow)]
    pub origin: Option<&'a RawValue>,
}

/// Who sent a prompt: `human`, or what stood in for one. A message from another session
/// (`peer`) carries the sender's name and, apart from the markup around it, what it said.
#[derive(Deserialize)]
pub(super) struct Origin {
    pub kind: String,
    pub name: Option<Text>,
    pub from: Option<Text>,
    pub body: Option<Text>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CompactMetadata {
    pub pre_tokens: Option<u64>,
    pub post_tokens: Option<u64>,
}

/// An `api_error` system record's `error`.
#[derive(Deserialize)]
pub(super) struct ApiError<'a> {
    #[serde(borrow)]
    pub formatted: Option<&'a RawValue>,
    #[serde(borrow)]
    pub message: Option<&'a RawValue>,
}

/*
 * A tool result's `toolUseResult` is Claude Code's structured copy of the result. Its fields
 * depend on the tool, and a subagent's transcript has none. The shapes below are what a short
 * result and a subagent's place are read from.
 */

/// The `toolUseResult` of an `Agent` or `Workflow` call.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Launched {
    /// The subagent an `Agent` call started.
    pub agent_id: Option<String>,
    /// The run a `Workflow` call's subagents are filed under.
    pub run_id: Option<String>,
    /// Of a subagent that ran to its end before the result was written.
    pub resolved_model: Option<String>,
    pub total_tool_use_count: Option<u64>,
    pub total_duration_ms: Option<u64>,
}

/// The `toolUseResult` of a `Read`.
#[derive(Deserialize)]
pub(super) struct ReadResult {
    pub file: ReadFile,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReadFile {
    pub num_lines: u64,
}

/// The `toolUseResult` of an `Edit`: the hunks of the change.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditResult<'a> {
    #[serde(borrow)]
    pub structured_patch: Vec<Hunk<'a>>,
}

/// The `toolUseResult` of a `Write`: `create` or `update`, and what was written.
#[derive(Deserialize)]
pub(super) struct WriteResult<'a> {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(borrow)]
    pub content: &'a RawValue,
}

/// The `toolUseResult` of a `Grep` or a `Glob`. A `Grep` says in `mode` what it was asked for:
/// the matching lines (`content`), the files that have one, or a count.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SearchResult {
    pub mode: Option<String>,
    pub num_matches: Option<u64>,
    pub num_lines: Option<u64>,
    pub num_files: Option<u64>,
}

/// A hunk of a `structuredPatch`: its lines, each starting with ` `, `+` or `-`.
#[derive(Deserialize)]
pub(super) struct Hunk<'a> {
    #[serde(borrow)]
    pub lines: Vec<&'a RawValue>,
}

/// A subagent's `.meta.json`.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SubagentMeta {
    pub agent_type: Option<Text>,
    pub description: Option<Text>,
    /// The call that started it. Subagents run by a workflow have none.
    pub tool_use_id: Option<String>,
}

/// `custom-title.json`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitleFile {
    pub custom_title: Text,
}
