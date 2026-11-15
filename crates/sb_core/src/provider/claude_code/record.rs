//! The shapes of Claude Code's records, as far as the parser reads them (PLAN §7.3). Fields it
//! doesn't read are skipped, so a release that adds some changes nothing here. The parts that
//! are large or whose shape varies stay as raw JSON borrowed from the record's line: the parser
//! looks into the ones it needs, and for the rest keeps only where they are ([`Line::stored`]).

use std::sync::Arc;

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::transcript::{Encoding, Stored};

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
    serde_json::from_str(raw.get()).ok()
}

/// What `raw` holds, when it is in the shape `T` reads.
pub(super) fn typed<'a, T: Deserialize<'a>>(raw: &'a RawValue) -> Option<T> {
    serde_json::from_str(raw.get()).ok()
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
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub entrypoint: Option<String>,
    pub subtype: Option<String>,
    pub custom_title: Option<String>,
    /// The record was put into the conversation by Claude Code, not typed by the user.
    pub is_meta: Option<bool>,
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
    pub thinking: Option<String>,
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

/// Who sent a prompt: `human`, or what stood in for one (another session, a finished task).
#[derive(Deserialize)]
pub(super) struct Origin {
    pub kind: String,
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

/// What a tool result's `toolUseResult` says the call started: a subagent, or a workflow run.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Launched {
    pub agent_id: Option<String>,
    pub run_id: Option<String>,
}

/// A subagent's `.meta.json`.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SubagentMeta {
    pub agent_type: Option<String>,
    pub description: Option<String>,
    /// The call that started it. Subagents run by a workflow have none.
    pub tool_use_id: Option<String>,
}

/// `custom-title.json`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitleFile {
    pub custom_title: String,
}
