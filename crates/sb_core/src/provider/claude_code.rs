//! Claude Code's provider (PLAN §7.3): the only code that knows Claude Code.
//!
//! Its data is a file tree (PLAN §7.2). The root is `$CLAUDE_CONFIG_DIR` when set, else
//! `~/.claude`, and every file under `projects/` is an item, keyed by its path relative to the
//! root. Under `projects/<project>/`:
//!
//! | Path | Item |
//! |---|---|
//! | `<session>.jsonl` | the session's main transcript |
//! | `<session>.<anything else>` | a sidecar of the session |
//! | `<session>/subagents/…/agent-<id>.jsonl` | a subagent's transcript |
//! | `<session>/tool-results/…` | tool output saved outside the transcript |
//! | `<session>/…` | a sidecar of the session: titles, subagent metadata, workflows |
//! | `memory/…` | the project's memory files: sidecars of no session |
//!
//! `<project>` is the starting directory with its punctuation replaced by `-`, so it can't be
//! turned back into a path and is not read here. Every `.jsonl` file is appended to line by line.

use std::path::Path;

use serde_json::Value;

use super::{
    FileClass, FileTree, Hashed, ItemKind, ItemPrefix, ItemRange, Provider, ProviderError,
    ProviderId, RangeFailure, Scan, Source,
};
use crate::transport::{Env, RangeSink, Transport};

const ID: ProviderId = ProviderId::new("claude-code");

/// Where the items are, under the root.
const SUBTREE: &str = "projects";

/// How long transcripts are kept when `settings.json` doesn't say (Claude Code's documented
/// default).
const DEFAULT_RETENTION_DAYS: u32 = 30;

/// Claude Code.
pub struct Agent;

impl Provider for Agent {
    fn id(&self) -> ProviderId {
        ID
    }

    fn detect<'a>(
        &'a self,
        transport: &'a dyn Transport,
        _scratch: &'a Path,
    ) -> Result<Option<Box<dyn Source + 'a>>, ProviderError> {
        let root = root(transport.env(&["CLAUDE_CONFIG_DIR"])?);
        let present = transport.stat(&[&format!("{root}/{SUBTREE}")])?[0].is_some();
        Ok(present.then(|| {
            Box::new(Data {
                tree: FileTree::new(transport, root.clone(), SUBTREE, classify),
                transport,
                root,
            }) as Box<dyn Source>
        }))
    }
}

/// Where Claude Code keeps its data at a location with the environment `env`.
fn root(mut env: Env) -> String {
    match env.vars.remove("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => dir,
        _ => format!("{}/.claude", env.home),
    }
}

/// Claude Code's data at one location.
struct Data<'a> {
    tree: FileTree<'a>,
    transport: &'a dyn Transport,
    root: String,
}

impl Source for Data<'_> {
    fn scan(&mut self) -> Result<Scan, ProviderError> {
        self.tree.scan()
    }

    fn hash_prefixes(
        &mut self,
        requests: &[ItemPrefix],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, ProviderError> {
        self.tree.hash_prefixes(requests)
    }

    fn fetch(
        &mut self,
        requests: &[ItemRange],
        sink: &mut dyn RangeSink,
    ) -> Result<(), ProviderError> {
        self.tree.fetch(requests, sink)
    }

    fn retention_days(&mut self) -> Result<Option<u32>, ProviderError> {
        Ok(retention_days(
            self.transport
                .read_small(&format!("{}/settings.json", self.root))?
                .as_deref(),
        ))
    }
}

/**
 * `cleanupPeriodDays` in the root's `settings.json`, given the file if there is one: Claude Code
 * deletes transcripts older than that many days, 30 unless set, at least 1. A settings file it
 * would reject (not JSON, or a period that isn't a whole number of days from 1 up) gives no
 * answer, as Claude Code itself then stops deleting. Settings from other scopes (a project's, a
 * managed policy) can override the value and are not read.
 */
fn retention_days(settings: Option<&[u8]>) -> Option<u32> {
    let Some(settings) = settings else {
        return Some(DEFAULT_RETENTION_DAYS);
    };
    match serde_json::from_slice(settings) {
        Ok(Value::Object(settings)) => match settings.get("cleanupPeriodDays") {
            None => Some(DEFAULT_RETENTION_DAYS),
            Some(days) => days
                .as_u64()
                .filter(|days| *days >= 1)
                .map(|days| u32::try_from(days).unwrap_or(u32::MAX)),
        },
        _ => None,
    }
}

/// What the file at `key` is (the module docs).
fn classify(key: &str) -> FileClass {
    /* `projects/<project>/` and then the path inside the project. */
    let inside: Vec<&str> = key.split('/').skip(2).collect();
    let (kind, session) = match inside[..] {
        [] => (ItemKind::Sidecar, None),
        [file] => {
            let session = file.split('.').next().unwrap_or(file);
            let kind = if file.strip_prefix(session) == Some(".jsonl") {
                ItemKind::Transcript
            } else {
                ItemKind::Sidecar
            };
            (kind, Some(session))
        }
        ["memory", ..] => (ItemKind::Sidecar, None),
        [session, "subagents", .., file]
            if file.starts_with("agent-") && file.ends_with(".jsonl") =>
        {
            (ItemKind::Subagent, Some(session))
        }
        [session, "tool-results", ..] => (ItemKind::ToolResult, Some(session)),
        [session, ..] => (ItemKind::Sidecar, Some(session)),
    };
    FileClass {
        kind,
        session: session.map(str::to_string),
        lines: key.ends_with(".jsonl"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn class(kind: ItemKind, session: Option<&str>, lines: bool) -> FileClass {
        FileClass {
            kind,
            session: session.map(str::to_string),
            lines,
        }
    }

    #[test]
    fn files_are_classified_by_where_they_are() {
        use ItemKind::{Sidecar, Subagent, ToolResult, Transcript};
        for (inside, expected) in [
            ("s1.jsonl", class(Transcript, Some("s1"), true)),
            ("s1.released.json", class(Sidecar, Some("s1"), false)),
            (
                "s1/subagents/agent-a1.jsonl",
                class(Subagent, Some("s1"), true),
            ),
            (
                "s1/subagents/agent-a1.meta.json",
                class(Sidecar, Some("s1"), false),
            ),
            (
                "s1/subagents/workflows/w1/agent-a2.jsonl",
                class(Subagent, Some("s1"), true),
            ),
            (
                "s1/subagents/workflows/w1/journal.jsonl",
                class(Sidecar, Some("s1"), true),
            ),
            (
                "s1/tool-results/r1.txt",
                class(ToolResult, Some("s1"), false),
            ),
            (
                "s1/tool-results/r2.png",
                class(ToolResult, Some("s1"), false),
            ),
            ("s1/custom-title.json", class(Sidecar, Some("s1"), false)),
            ("s1/workflows/w1/main.js", class(Sidecar, Some("s1"), false)),
            ("memory/notes.md", class(Sidecar, None, false)),
        ] {
            assert_eq!(
                classify(&format!("projects/-home-user-code-app/{inside}")),
                expected,
                "{inside}"
            );
        }
        assert_eq!(classify("projects/stray.txt"), class(Sidecar, None, false));
    }

    #[test]
    fn the_root_is_the_config_dir_when_set_else_under_the_home_folder() {
        let env = |config_dir: Option<&str>| Env {
            home: "/home/user".to_string(),
            vars: config_dir
                .map(|dir| ("CLAUDE_CONFIG_DIR".to_string(), dir.to_string()))
                .into_iter()
                .collect::<HashMap<_, _>>(),
        };
        assert_eq!(root(env(None)), "/home/user/.claude");
        assert_eq!(root(env(Some(""))), "/home/user/.claude");
        assert_eq!(root(env(Some("/srv/agent"))), "/srv/agent");
    }

    #[test]
    fn retention_is_read_from_settings_with_the_documented_default() {
        let retention = |settings: &str| retention_days(Some(settings.as_bytes()));
        assert_eq!(retention_days(None), Some(30));
        assert_eq!(retention(r#"{"theme": "dark"}"#), Some(30));
        assert_eq!(retention(r#"{"cleanupPeriodDays": 365}"#), Some(365));
        assert_eq!(retention(r#"{"cleanupPeriodDays": 0}"#), None);
        assert_eq!(retention(r#"{"cleanupPeriodDays": "90"}"#), None);
        assert_eq!(retention("{ not json"), None);
    }
}
