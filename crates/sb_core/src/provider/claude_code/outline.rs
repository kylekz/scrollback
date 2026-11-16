//! The session's subagents outlined without reading their conversations (`Provider::outlines`):
//! what each says it is, from its `.meta.json`, and the subagents its calls started, found by
//! the parser's rules (`parse.rs`: `Reading::pair` and `Reading::subagents`) from only the
//! records that can say so.
//!
//! A subagent's transcript is scanned a line at a time, and a line is parsed only when it holds
//! a `tool_use` or a `tool_result` (`"tool_use…`). From those: each call and whether it starts
//! subagents; each call's first result, and the ids its `toolUseResult` names when the call
//! starts subagents (or isn't known yet where the result is); then the subagent transcripts
//! those ids name; and, while a call that starts subagents has none, the transcripts whose
//! `.meta.json` names one of its calls.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead as _, BufReader};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::value::RawValue;

use super::parse::{file_name, meta_key, sidecar};
use super::record::{Launched, SubagentMeta, Text, typed};
use super::tools::{self, WORKFLOW};
use crate::provider::{ItemKind, SessionItems};
use crate::transcript::{Outline, ToolKind};

/// The parts of a record the scan reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Scanned<'a> {
    #[serde(borrow)]
    tool_use_result: Option<&'a RawValue>,
    #[serde(borrow)]
    message: Option<Message<'a>>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow)]
    content: Option<&'a RawValue>,
}

/// A content block, as far as the scan reads it.
#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    name: Option<String>,
    tool_use_id: Option<String>,
}

/// A call in the transcript: whether it starts subagents, and the line it is on.
struct Call {
    starts: bool,
    line: usize,
}

/// A subagent transcript of the session, and the ids that name it: its run's, its own.
struct Transcript<'a> {
    key: &'a str,
    ids: [Option<&'a str>; 2],
    meta: Option<SubagentMeta>,
}

pub(super) fn outlines(items: &dyn SessionItems) -> Vec<Outline> {
    let mut transcripts: Vec<Transcript> = items
        .list()
        .iter()
        .filter(|item| item.kind == ItemKind::Subagent)
        .map(|item| {
            let key = item.key.as_str();
            let agent = file_name(key)
                .strip_prefix("agent-")
                .and_then(|name| name.strip_suffix(".jsonl"));
            let run = key
                .split_once("/subagents/workflows/")
                .and_then(|(_, rest)| rest.split('/').next());
            Transcript {
                key,
                ids: [run, agent],
                meta: sidecar(items, &meta_key(key)).ok().flatten(),
            }
        })
        .collect();
    transcripts.sort_unstable_by_key(|it| it.key);
    transcripts
        .iter()
        .map(|transcript| {
            let meta = transcript.meta.as_ref();
            Outline {
                item: transcript.key.into(),
                kind: meta.and_then(|meta| meta.agent_type.as_ref().map(|Text(it)| it.clone())),
                description: meta
                    .and_then(|meta| meta.description.as_ref().map(|Text(it)| it.clone())),
                started: started(items, transcript.key, &transcripts),
            }
        })
        .collect()
}

/// The subagents the calls of the subagent `key` started, as the parser places them.
fn started(items: &dyn SessionItems, key: &str, transcripts: &[Transcript]) -> Vec<Arc<str>> {
    let Ok(file) = items.open(key, 0) else {
        return Vec::new();
    };
    let mut calls: HashMap<String, Call> = HashMap::new();
    /* Each result: its call, what its record says the call started, and its line. */
    let mut results: Vec<(String, Vec<String>, usize)> = Vec::new();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut at = 0;
    while reader
        .read_until(b'\n', &mut line)
        .is_ok_and(|read| read > 0)
    {
        at += 1;
        let marker = b"\"tool_use";
        if line.windows(marker.len()).any(|it| it == marker) {
            let text = String::from_utf8_lossy(&line);
            if let Ok(record) = serde_json::from_str::<Scanned>(&text) {
                let launched: Vec<String> = record
                    .tool_use_result
                    .and_then(typed::<Launched>)
                    .into_iter()
                    .flat_map(|it| it.agent_id.into_iter().chain(it.run_id))
                    .collect();
                let blocks = record
                    .message
                    .and_then(|message| message.content)
                    .and_then(typed::<Vec<&RawValue>>)
                    .unwrap_or_default();
                for block in blocks.into_iter().filter_map(typed::<Block>) {
                    match (block.kind.as_str(), block.id, block.tool_use_id) {
                        ("tool_use", Some(id), _) => {
                            let name = block.name.unwrap_or_default();
                            calls.entry(id).or_insert(Call {
                                starts: tools::kind(&name) == ToolKind::Subagent
                                    || name == WORKFLOW,
                                line: at,
                            });
                        }
                        ("tool_result", _, Some(call)) => {
                            results.push((call, launched.clone(), at));
                        }
                        _ => {}
                    }
                }
            }
        }
        line.clear();
    }
    /* A call's first result is its own; what it names, the call started. */
    let mut answered = HashSet::new();
    let mut starters: HashMap<String, &str> = HashMap::new();
    for (call, ids, line) in &results {
        let Some(known) = calls.get(call) else {
            continue;
        };
        if answered.insert(call.as_str()) && (known.starts || known.line > *line) {
            starters.extend(ids.iter().map(|id| (id.clone(), call.as_str())));
        }
    }
    let mut found: Vec<Arc<str>> = Vec::new();
    let mut started: HashSet<&str> = HashSet::new();
    for transcript in transcripts.iter().filter(|it| it.key != key) {
        if let Some(call) = transcript
            .ids
            .into_iter()
            .flatten()
            .find_map(|id| starters.get(id))
        {
            started.insert(call);
            found.push(transcript.key.into());
        }
    }
    let mut unresolved = calls
        .iter()
        .filter(|(id, call)| call.starts && !started.contains(id.as_str()))
        .count();
    for transcript in transcripts.iter().filter(|it| it.key != key) {
        if unresolved == 0 {
            break;
        }
        if found.iter().any(|it| **it == *transcript.key) {
            continue;
        }
        let named = transcript
            .meta
            .as_ref()
            .and_then(|meta| meta.tool_use_id.as_deref())
            .filter(|id| calls.contains_key(*id));
        if let Some(call) = named {
            if started.insert(call) {
                unresolved = unresolved.saturating_sub(1);
            }
            found.push(transcript.key.into());
        }
    }
    found.sort_unstable();
    found
}
