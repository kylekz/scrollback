//! A second reading of a session's transcripts, to check the parser against (the `check`
//! example). It reads each record as plain JSON, knowing as little of the parser as it can, and
//! counts what should come out the other end: tool calls, results, replies, thinking and tokens.
//! It also counts what the parser has no name for, which is how a release that added a record
//! type or an attachment shows up. Nothing a session says is kept: only the format's own names
//! and numbers.

use std::collections::{BTreeSet, HashSet};
use std::io::{BufRead as _, BufReader};

use serde_json::Value;

use super::parse::{BOOKKEEPING, BOUNDARY, CONTEXT, CONVERSATION, NOTICES, QUEUED, SILENT, TITLE};
use super::said::unknown_element;
use crate::provider::{Audit, SessionItems};
use crate::transcript::{Content, Conversation, Ending, Subagent, Tokens, Transcript};

/// A record in a session's own transcript that says it is a subagent's, as releases that kept
/// subagents' conversations there wrote them.
const SIDECHAIN: &str = "records marked isSidechain in a session's own transcript";

/// A subagent's conversation made of such records, and one of those that the parser put
/// under the call that started it.
const INLINE: &str = "subagents kept in a session's own transcript";
const ATTACHED: &str =
    "subagents kept in a session's own transcript, under the call that started them";

/// A record under a uuid that an earlier record of the transcript has.
const REWRITTEN: &str = "records written again under a uuid already used";

pub(in crate::provider) fn audit(items: &dyn SessionItems, transcript: &Transcript) -> Audit {
    let mut audit = Audit::default();
    for what in [SIDECHAIN, INLINE, ATTACHED, REWRITTEN] {
        audit.counts.insert(what.to_string(), 0);
    }
    let mut mismatches = BTreeSet::new();
    let own = transcript.conversation.as_ref();
    /* The subagents kept in the session's own transcript that are under no call are counted
    with it; the others have transcripts of their own. */
    let (inline, others): (Vec<&Conversation>, Vec<&Conversation>) = transcript
        .unattached
        .iter()
        .filter_map(Subagent::conversation)
        .partition(|conversation| own.is_some_and(|own| own.item == conversation.item));
    if let Some(own) = own {
        *audit.counts.entry(INLINE.to_string()).or_default() += inline.len() as u64;
        check(items, own, &inline, true, &mut audit, &mut mismatches);
    }
    for conversation in others {
        check(items, conversation, &[], false, &mut audit, &mut mismatches);
    }
    audit.mismatches = mismatches.into_iter().collect();
    audit
}

/// What one transcript holds, counted from its records or from its parsed conversation.
#[derive(Default, PartialEq, Eq)]
struct Counts {
    calls: usize,
    results: usize,
    replies: usize,
    thinking: usize,
    tokens: Tokens,
}

/// Checks `conversation`, with `beside`, the other conversations parsed from the same
/// transcript, against the transcript's records; and those of the subagents under their
/// calls against theirs.
fn check(
    items: &dyn SessionItems,
    conversation: &Conversation,
    beside: &[&Conversation],
    main: bool,
    audit: &mut Audit,
    mismatches: &mut BTreeSet<&'static str>,
) {
    let mut parsed = Counts::default();
    for conversation in std::iter::once(&conversation).chain(beside) {
        tally(items, conversation, &mut parsed, audit, mismatches);
    }
    /* A transcript that couldn't be read through can't be counted through either. */
    if matches!(conversation.ending, Ending::Unreadable(_)) {
        return;
    }
    let Some(recorded) = recorded(items, &conversation.item, main, audit) else {
        return;
    };
    for (name, differs) in [
        ("tool calls", recorded.calls != parsed.calls),
        ("tool results", recorded.results != parsed.results),
        ("replies", recorded.replies != parsed.replies),
        ("thinking", recorded.thinking != parsed.thinking),
        ("tokens", recorded.tokens != parsed.tokens),
    ] {
        if differs {
            mismatches.insert(name);
        }
    }
}

/// Adds what `conversation` holds to `counts`, and with it what the subagents under its calls
/// hold that were parsed from the same transcript. Those with a transcript of their own are
/// checked against it.
fn tally(
    items: &dyn SessionItems,
    conversation: &Conversation,
    counts: &mut Counts,
    audit: &mut Audit,
    mismatches: &mut BTreeSet<&'static str>,
) {
    counts.tokens.add(conversation.tokens);
    for block in conversation.blocks() {
        match &block.content {
            Content::Reply { .. } => counts.replies += 1,
            Content::Thinking { .. } => counts.thinking += 1,
            Content::Prompt {
                text, from: None, ..
            } => {
                if let Some(tag) = unknown_element(text) {
                    /* A tag of Claude Code's own making has a `-` in it. Any other may be
                    something the user wrote, and is not named. */
                    let named = if tag.contains('-') { tag } else { "(another)" };
                    *audit
                        .unknown
                        .entry(format!("a prompt that starts with the element {named}"))
                        .or_default() += 1;
                }
            }
            Content::ToolCall(call) => {
                counts.calls += 1;
                counts.results += usize::from(call.result.is_some());
                for inner in call.subagents.iter().filter_map(Subagent::conversation) {
                    if inner.item == conversation.item {
                        for what in [INLINE, ATTACHED] {
                            *audit.counts.entry(what.to_string()).or_default() += 1;
                        }
                        tally(items, inner, counts, audit, mismatches);
                    } else {
                        check(items, inner, &[], false, audit, mismatches);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Counts the records of the transcript `key`, and adds what kinds they are to `audit`.
fn recorded(items: &dyn SessionItems, key: &str, main: bool, audit: &mut Audit) -> Option<Counts> {
    let mut counts = Counts::default();
    let mut seen = HashSet::new();
    let mut messages = HashSet::new();
    let mut calls = HashSet::new();
    let mut answered = HashSet::new();
    let mut count = |what: &str| *audit.counts.entry(what.to_string()).or_default() += 1;
    for line in BufReader::new(items.open(key, 0).ok()?).split(b'\n') {
        let Ok(Value::Object(record)) = serde_json::from_slice::<Value>(&line.ok()?) else {
            continue;
        };
        let text = |name: &str| record.get(name).and_then(Value::as_str);
        let Some(kind) = text("type") else {
            continue;
        };
        let known = kind == TITLE || CONVERSATION.contains(&kind) || BOOKKEEPING.contains(&kind);
        let seen_before = audit.records.entry(kind.to_string()).or_insert((0, known));
        seen_before.0 += 1;
        if main && record.get("isSidechain") == Some(&Value::Bool(true)) {
            count(SIDECHAIN);
        }
        if !CONVERSATION.contains(&kind) {
            continue;
        }
        /* A record written again under the same uuid counts once. */
        if text("uuid").is_some_and(|uuid| !seen.insert(uuid.to_string())) {
            count(REWRITTEN);
            continue;
        }
        let content = record
            .get("message")
            .and_then(|message| message.get("content"));
        let blocks = content.and_then(Value::as_array).into_iter().flatten();
        match kind {
            "assistant" => {
                let message = record.get("message");
                let id = message.and_then(|message| message.get("id")?.as_str());
                if id.is_none_or(|id| messages.insert(id.to_string())) {
                    let used = |name: &str| {
                        message
                            .and_then(|message| message.get("usage")?.get(name)?.as_u64())
                            .unwrap_or(0)
                    };
                    counts.tokens.add(Tokens {
                        input: used("input_tokens"),
                        output: used("output_tokens"),
                        cache_read: used("cache_read_input_tokens"),
                        cache_write: used("cache_creation_input_tokens"),
                    });
                }
                counts.replies += usize::from(content.is_some_and(Value::is_string));
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let said = block.get("text").and_then(Value::as_str);
                            counts.replies +=
                                usize::from(said.is_some_and(|text| !text.trim().is_empty()));
                        }
                        Some("thinking") => counts.thinking += 1,
                        Some("tool_use") => {
                            if let Some(id) = block.get("id").and_then(Value::as_str) {
                                calls.insert(id.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            "user" => {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result")
                        && let Some(id) = block.get("tool_use_id").and_then(Value::as_str)
                    {
                        answered.insert(id.to_string());
                    }
                }
            }
            "system" => {
                let subtype = text("subtype").unwrap_or_default();
                if subtype != BOUNDARY && !NOTICES.contains(&subtype) && !SILENT.contains(&subtype)
                {
                    *audit
                        .unknown
                        .entry(format!("system {subtype}"))
                        .or_default() += 1;
                }
            }
            _ => {
                let attachment = record
                    .get("attachment")
                    .and_then(|attachment| attachment.get("type")?.as_str())
                    .unwrap_or_default();
                if attachment != QUEUED && !CONTEXT.contains(&attachment) {
                    *audit
                        .unknown
                        .entry(format!("attachment {attachment}"))
                        .or_default() += 1;
                }
            }
        }
    }
    counts.calls = calls.len();
    counts.results = calls.intersection(&answered).count();
    Some(counts)
}
