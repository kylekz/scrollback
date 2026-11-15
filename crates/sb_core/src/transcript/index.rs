//! What the session index and search hold about a session (PLAN §4.3, §7.5, §10), derived from
//! its transcript. It is the same for every agent, and nothing here touches the catalogue.

use std::collections::BTreeMap;

use super::{Content, Conversation, Tokens, ToolInput, ToolKind, Transcript};

/// How much of the first prompt stands in for a title the agent didn't store.
const TITLE_CHARS: usize = 80;
/// How much of the first prompt a session's card and row show.
const PROMPT_CHARS: usize = 240;

/// A session's derived fields and search text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionIndex {
    /// The title the agent stored, else the start of the first prompt.
    pub title: Option<String>,
    /// The start of the first prompt.
    pub first_prompt: Option<String>,
    pub project_path: Option<String>,
    pub branch: Option<String>,
    pub entrypoint: Option<String>,
    /// When its first and last blocks were recorded, in Unix milliseconds.
    pub started: Option<i64>,
    pub ended: Option<i64>,
    pub models: Vec<String>,
    pub tokens: Tokens,
    /// Where each turn of the shown branch starts in the session's transcript item, so the
    /// reader can open at a turn (PLAN §7.5).
    pub turns: Vec<u64>,
    /// The session's own tool calls on the shown branch, by kind. Its subagents' are not
    /// counted.
    pub calls: BTreeMap<ToolKind, u32>,
    /// The text search indexes (PLAN §10), subagents' included.
    pub spans: Vec<Span>,
}

/// One piece of text search can match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// The turn it is in, as an index into [`SessionIndex::turns`]. Text of a subagent is in
    /// the turn of the call that started it, which is where a match opens the session.
    pub turn: usize,
    pub kind: SpanKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Prompt,
    Reply,
    /// A tool call's command, path, pattern, query or address. Tool output is not indexed
    /// (PLAN D10).
    ToolCall,
}

pub fn index(transcript: &Transcript) -> SessionIndex {
    let conversation = &transcript.conversation;
    let first_prompt = conversation
        .turns
        .iter()
        .filter_map(|turn| match &conversation[*turn.blocks.first()?].content {
            Content::Prompt { text, .. } => Some(text),
            _ => None,
        })
        .find(|text| !text.trim().is_empty());
    let mut calls = BTreeMap::new();
    for call in conversation.calls() {
        *calls.entry(call.input.kind()).or_default() += 1;
    }
    let mut spans = Vec::new();
    collect(conversation, None, &mut spans);
    let span = conversation.span();
    SessionIndex {
        title: transcript
            .facts
            .title
            .clone()
            .or_else(|| first_prompt.map(|text| start(text, TITLE_CHARS))),
        first_prompt: first_prompt.map(|text| start(text, PROMPT_CHARS)),
        project_path: transcript.facts.project_path.clone(),
        branch: transcript.facts.branch.clone(),
        entrypoint: transcript.facts.entrypoint.clone(),
        started: span.map(|(started, _)| started),
        ended: span.map(|(_, ended)| ended),
        models: conversation.models.clone(),
        tokens: conversation.tokens,
        turns: conversation
            .turns
            .iter()
            .filter_map(|turn| Some(conversation[*turn.blocks.first()?].offset))
            .collect(),
        calls,
        spans,
    }
}

/// The search text of `conversation`'s shown branch. `within` is the session's turn that a
/// subagent's conversation belongs to; the session's own turns are their own.
fn collect(conversation: &Conversation, within: Option<usize>, spans: &mut Vec<Span>) {
    for (index, turn) in conversation.turns.iter().enumerate() {
        let turn_index = within.unwrap_or(index);
        let mut span = |kind, text: &str| {
            if !text.trim().is_empty() {
                spans.push(Span {
                    turn: turn_index,
                    kind,
                    text: text.to_string(),
                });
            }
        };
        let mut subagents = Vec::new();
        for id in &turn.blocks {
            match &conversation[*id].content {
                Content::Prompt { text, .. } => span(SpanKind::Prompt, text),
                Content::Reply { text } => span(SpanKind::Reply, text),
                Content::ToolCall(call) => {
                    match &call.input {
                        ToolInput::Shell { command, .. } => span(SpanKind::ToolCall, command),
                        ToolInput::Read { path, .. }
                        | ToolInput::Edit { path, .. }
                        | ToolInput::Write { path, .. } => span(SpanKind::ToolCall, path),
                        ToolInput::ContentSearch { pattern, path }
                        | ToolInput::FileSearch { pattern, path } => {
                            span(SpanKind::ToolCall, pattern);
                            span(SpanKind::ToolCall, path.as_deref().unwrap_or_default());
                        }
                        ToolInput::Subagent { description, .. } => {
                            span(
                                SpanKind::ToolCall,
                                description.as_deref().unwrap_or_default(),
                            );
                        }
                        ToolInput::WebSearch { query } => span(SpanKind::ToolCall, query),
                        ToolInput::WebFetch { url, .. } => span(SpanKind::ToolCall, url),
                        ToolInput::Todos(_) | ToolInput::Mcp { .. } | ToolInput::Other => {}
                    }
                    subagents.extend(&call.subagents);
                }
                Content::Thinking { .. }
                | Content::Compaction { .. }
                | Content::Note { .. }
                | Content::Unrecognised(_) => {}
            }
        }
        for subagent in subagents {
            collect(&subagent.conversation, Some(turn_index), spans);
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
