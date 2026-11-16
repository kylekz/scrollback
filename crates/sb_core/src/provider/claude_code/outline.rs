//! The session's subagents outlined without reading their conversations (`Provider::outlines`):
//! what each says it is, from its `.meta.json`, and the subagents its calls started, found the
//! ways the parser finds them (`parse.rs`, Subagents) from only the records that can say so.
//!
//! A subagent's transcript is scanned a line at a time, and a line is parsed only when it holds
//! one of the fields that name what a call started: a result's `agentId` or `runId`, or a
//! `tool_use` whose id another subagent's `.meta.json` names.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead as _, BufReader};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::value::RawValue;

use super::parse::{file_name, meta_key, sidecar};
use super::record::{Launched, SubagentMeta, Text, typed};
use crate::provider::{ItemKind, SessionItems};
use crate::transcript::Outline;

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

#[derive(Deserialize)]
struct Use {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
}

pub(super) fn outlines(items: &dyn SessionItems) -> Vec<Outline> {
    let mut keys: Vec<&str> = items
        .list()
        .iter()
        .filter(|item| item.kind == ItemKind::Subagent)
        .map(|item| item.key.as_str())
        .collect();
    keys.sort_unstable();
    let metas: HashMap<&str, SubagentMeta> = keys
        .iter()
        .filter_map(|key| Some((*key, sidecar(items, &meta_key(key)).ok().flatten()?)))
        .collect();
    let by_call: HashMap<&str, &str> = metas
        .iter()
        .filter_map(|(key, meta)| Some((meta.tool_use_id.as_deref()?, *key)))
        .collect();
    let mut by_id: HashMap<&str, Vec<&str>> = HashMap::new();
    for key in &keys {
        let agent = file_name(key)
            .strip_prefix("agent-")
            .and_then(|name| name.strip_suffix(".jsonl"));
        let run = key
            .split_once("/subagents/workflows/")
            .and_then(|(_, rest)| rest.split('/').next());
        for id in [agent, run].into_iter().flatten() {
            by_id.entry(id).or_default().push(key);
        }
    }
    keys.iter()
        .map(|key| {
            let meta = metas.get(key);
            Outline {
                item: (*key).into(),
                kind: meta.and_then(|meta| meta.agent_type.as_ref().map(|Text(it)| it.clone())),
                description: meta
                    .and_then(|meta| meta.description.as_ref().map(|Text(it)| it.clone())),
                started: started(items, key, &by_id, &by_call),
            }
        })
        .collect()
}

/// The subagents the calls of the subagent `key` started, in the order its records name them.
fn started(
    items: &dyn SessionItems,
    key: &str,
    by_id: &HashMap<&str, Vec<&str>>,
    by_call: &HashMap<&str, &str>,
) -> Vec<Arc<str>> {
    let Ok(file) = items.open(key, 0) else {
        return Vec::new();
    };
    let mut found: Vec<Arc<str>> = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |child: &str| {
        if child != key && seen.insert(child.to_string()) {
            found.push(child.into());
        }
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    while reader
        .read_until(b'\n', &mut line)
        .is_ok_and(|read| read > 0)
    {
        let named = |field: &[u8]| line.windows(field.len()).any(|it| it == field);
        if named(b"\"agentId\"") || named(b"\"runId\"") || named(b"\"tool_use\"") {
            let text = String::from_utf8_lossy(&line);
            if let Ok(record) = serde_json::from_str::<Scanned>(&text) {
                let launched = record.tool_use_result.and_then(typed::<Launched>);
                for id in launched
                    .into_iter()
                    .flat_map(|it| it.agent_id.into_iter().chain(it.run_id))
                {
                    by_id
                        .get(id.as_str())
                        .into_iter()
                        .flatten()
                        .for_each(|it| add(it));
                }
                let uses = record
                    .message
                    .and_then(|message| message.content)
                    .and_then(typed::<Vec<&RawValue>>)
                    .unwrap_or_default();
                for id in uses
                    .into_iter()
                    .filter_map(typed::<Use>)
                    .filter(|it| it.kind == "tool_use")
                    .filter_map(|it| it.id)
                {
                    by_call.get(id.as_str()).into_iter().for_each(|it| add(it));
                }
            }
        }
        line.clear();
    }
    found
}
