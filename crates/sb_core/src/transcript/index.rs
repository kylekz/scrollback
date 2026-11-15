//! What the session index and search hold about a session (PLAN §4.3, §10), derived from its
//! transcript. It is the same for every agent, and nothing here touches the catalogue.

use std::collections::BTreeMap;

use super::{
    BlockId, Content, Conversation, Ending, Subagent, Tokens, ToolInput, ToolKind, Transcript,
};

/// The version of what the index holds of a session: of [`index`], and of how the catalogue
/// derives and keeps a session's row from it. Rows made by another version are made again
/// (PLAN §7.5), so it goes up whenever either would give an archived session another row.
pub const INDEX_VERSION: u32 = 2;

/// How much of the first prompt stands in for a title the agent didn't store.
const TITLE_CHARS: usize = 80;
/// How much of a title the agent stored is kept.
const STORED_TITLE_CHARS: usize = 300;
/// How much of the first prompt a session's card and row show.
const PROMPT_CHARS: usize = 240;

/// A session's derived fields and search text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionIndex {
    /// The start of the title the agent stored, if it stored one that says anything; else the
    /// start of the first prompt.
    pub title: Option<String>,
    /// The start of the first prompt.
    pub first_prompt: Option<String>,
    pub project_path: Option<String>,
    pub branch: Option<String>,
    pub entrypoint: Option<String>,
    /// The session's items hold no main transcript: what is here is of its subagents alone.
    pub transcript_missing: bool,
    /// Some part of the session could not be read: one of its conversations to its end, or
    /// something that goes with one.
    pub unread: bool,
    /// From the first block recorded to the last block or result, in Unix milliseconds: of the
    /// session's own conversation, or without one, of its subagents'.
    pub started: Option<i64>,
    pub ended: Option<i64>,
    /// The models of the session's own conversation.
    pub models: Vec<String>,
    /// The session's own conversation alone.
    pub own: Totals,
    /// The session's own conversation and every subagent's that was read, unattached ones
    /// included.
    pub all: Totals,
    /// The text search indexes (PLAN §10), of the same conversations as `all`.
    pub spans: Vec<Span>,
}

/// What conversations used and did, on their shown branches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub tokens: Tokens,
    /// Tool calls by the kind of tool.
    pub calls: BTreeMap<ToolKind, u32>,
}

/// One piece of text search can match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub at: At,
    pub kind: SpanKind,
    /// The text is a subagent's, which opens the session at the call that started it (or, for
    /// one no call is known to have started, at [`At::Unattached`]).
    pub subagent: bool,
    pub text: String,
}

/// Where a match opens the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum At {
    /// A block of the session's own conversation, in its turn `turn` (counted from 0): the
    /// block the text is in, or for a subagent's text, the call under which it ran.
    Block { turn: usize, block: BlockId },
    /// The subagent at this place in [`Transcript::unattached`], or one under it.
    Unattached(usize),
}

/// What a span's text is. Of a tool call only what it was asked is indexed, never its output
/// (PLAN D10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Prompt,
    Reply,
    /// What a call was asked to do: a shell command, a search pattern or query, a subagent's
    /// task.
    Command,
    /// What a call was asked to work on: a file or folder path, or a web address.
    Path,
}

/**
 * The index of `transcript`, from what of it is read: a subagent whose conversation isn't adds
 * nothing, so the indexer reads them all first ([`Transcript::read_subagents`]).
 */
pub fn index(transcript: &Transcript) -> SessionIndex {
    let own = transcript.conversation.as_ref();
    let first_prompt = own
        .into_iter()
        .flat_map(Conversation::turns)
        .filter_map(|turn| match &own?[*turn.blocks.first()?].content {
            Content::Prompt { text, .. } => Some(text),
            _ => None,
        })
        .find(|text| !text.trim().is_empty());
    let mut index = SessionIndex {
        title: transcript
            .facts
            .title
            .as_deref()
            .map(|title| start(title, STORED_TITLE_CHARS))
            .filter(|title| !title.is_empty())
            .or_else(|| first_prompt.map(|text| start(text, TITLE_CHARS))),
        first_prompt: first_prompt.map(|text| start(text, PROMPT_CHARS)),
        project_path: transcript.facts.project_path.clone(),
        branch: transcript.facts.branch.clone(),
        entrypoint: transcript.facts.entrypoint.clone(),
        transcript_missing: own.is_none(),
        unread: false,
        started: None,
        ended: None,
        models: own.map_or_else(Vec::new, |own| own.models.clone()),
        own: Totals::default(),
        all: Totals::default(),
        spans: Vec::new(),
    };
    let mut span = own.and_then(Conversation::span);
    if let Some(own) = own {
        count(own, &mut index.own);
        collect(own, None, &mut index);
    }
    for (place, subagent) in transcript.unattached.iter().enumerate() {
        if let Subagent::Read { conversation, .. } = subagent {
            collect(conversation, Some(At::Unattached(place)), &mut index);
            if own.is_none() {
                span = match (span, conversation.span()) {
                    (Some((start, end)), Some((first, last))) => {
                        Some((start.min(first), end.max(last)))
                    }
                    (one, other) => one.or(other),
                };
            }
        }
    }
    (index.started, index.ended) = span.unzip();
    index
}

/// Adds what `conversation` used and the calls on its shown branch to `totals`.
fn count(conversation: &Conversation, totals: &mut Totals) {
    totals.tokens.add(conversation.tokens);
    for (_, call) in conversation.calls() {
        *totals.calls.entry(call.kind).or_default() += 1;
    }
}

/**
 * Adds `conversation` to the totals of all, and its shown branch's text to the spans, with
 * those of the subagents under its calls. `within` is where a subagent's conversation opens
 * the session; the session's own conversation opens at each block.
 */
fn collect(conversation: &Conversation, within: Option<At>, index: &mut SessionIndex) {
    count(conversation, &mut index.all);
    index.unread |=
        matches!(conversation.ending, Ending::Unreadable(_)) || !conversation.unreadable.is_empty();
    for (turn, blocks) in conversation.turns().iter().enumerate() {
        for block in &blocks.blocks {
            let at = within.unwrap_or(At::Block {
                turn,
                block: *block,
            });
            let mut span = |kind, text: &str| {
                if !text.trim().is_empty() {
                    index.spans.push(Span {
                        at,
                        kind,
                        subagent: within.is_some(),
                        text: text.to_string(),
                    });
                }
            };
            match &conversation[*block].content {
                Content::Prompt { text, .. } => span(SpanKind::Prompt, text),
                Content::Reply { text } => span(SpanKind::Reply, text),
                Content::ToolCall(call) => {
                    match &call.input {
                        ToolInput::Shell { command, .. } => span(SpanKind::Command, command),
                        ToolInput::Read { path, .. }
                        | ToolInput::Edit { path, .. }
                        | ToolInput::Write { path, .. } => span(SpanKind::Path, path),
                        ToolInput::ContentSearch { pattern, path }
                        | ToolInput::FileSearch { pattern, path } => {
                            span(SpanKind::Command, pattern);
                            span(SpanKind::Path, path.as_deref().unwrap_or_default());
                        }
                        ToolInput::Subagent { description, .. } => {
                            span(
                                SpanKind::Command,
                                description.as_deref().unwrap_or_default(),
                            );
                        }
                        ToolInput::WebSearch { query } => span(SpanKind::Command, query),
                        ToolInput::WebFetch { url, .. } => span(SpanKind::Path, url),
                        ToolInput::Todos(_) | ToolInput::Mcp { .. } | ToolInput::Other => {}
                    }
                    for subagent in &call.subagents {
                        if let Subagent::Read { conversation, .. } = subagent {
                            collect(conversation, Some(at), index);
                        }
                    }
                }
                Content::Thinking { .. }
                | Content::Compaction { .. }
                | Content::Note { .. }
                | Content::Unrecognised(_) => {}
            }
        }
    }
}

/// The first `chars` characters of `text`, its runs of white space as single spaces.
fn start(text: &str, chars: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(chars)
        .collect()
}
