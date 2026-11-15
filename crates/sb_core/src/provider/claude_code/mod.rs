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
//!
//! [`parse`] turns a session's archived files into the transcript model, [`tools`] maps
//! Claude Code's tools onto the common kinds, and [`audit`] counts a session's records another
//! way to check the parse against.

mod audit;
mod parse;
mod record;
mod said;
#[cfg(test)]
mod tests;
mod tools;

use serde_json::Value;

use super::{
    Audit, FileClass, FileTree, ItemKind, ItemPrefix, ItemRange, Provider, ProviderError,
    ScannedItem, SessionItems, Source,
};
use crate::hash::Hash;
use crate::transcript::{Subagent, Transcript};
use crate::transport::{RangeSink, Transport};

/// The provider's identity (PLAN §2.1).
const ID: &str = "claude-code";

/// Where the items are, under the root.
const SUBTREE: &str = "projects";

/// How long transcripts are kept when `settings.json` doesn't say (Claude Code's documented
/// default).
const DEFAULT_RETENTION_DAYS: u32 = 30;

/// Claude Code.
pub struct Agent;

impl Provider for Agent {
    fn id(&self) -> &'static str {
        ID
    }

    fn detect<'a>(
        &self,
        transport: &'a dyn Transport,
    ) -> Result<Option<Box<dyn Source + 'a>>, ProviderError> {
        let mut env = transport.env(&["CLAUDE_CONFIG_DIR"])?;
        let root = match env.vars.remove("CLAUDE_CONFIG_DIR") {
            Some(dir) if !dir.is_empty() => dir,
            _ => format!("{}/.claude", env.home),
        };
        let present = transport.stat(&[&format!("{root}/{SUBTREE}")])?[0].is_some();
        Ok(present.then(|| {
            Box::new(Data {
                tree: FileTree::new(transport, root.clone(), SUBTREE, classify),
                transport,
                root,
            }) as Box<dyn Source>
        }))
    }

    fn parse(&self, items: &dyn SessionItems) -> Transcript {
        parse::parse(items)
    }

    fn subagent(&self, items: &dyn SessionItems, item: &str) -> Subagent {
        parse::subagent(items, item)
    }

    fn audit(&self, items: &dyn SessionItems, transcript: &Transcript) -> Option<Audit> {
        Some(audit::audit(items, transcript))
    }
}

/// Claude Code's data at one location.
struct Data<'a> {
    tree: FileTree<'a>,
    transport: &'a dyn Transport,
    root: String,
}

impl Source for Data<'_> {
    fn scan(&mut self) -> Result<Vec<ScannedItem>, ProviderError> {
        self.tree.scan()
    }

    fn hash_prefixes(
        &mut self,
        requests: &[ItemPrefix],
    ) -> Result<Vec<Option<Hash>>, ProviderError> {
        self.tree.hash_prefixes(requests)
    }

    fn fetch(
        &mut self,
        requests: &[ItemRange],
        sink: &mut dyn RangeSink,
    ) -> Result<(), ProviderError> {
        self.tree.fetch(requests, sink)
    }

    /**
     * `cleanupPeriodDays` in the root's `settings.json`: Claude Code deletes transcripts older
     * than that many days, 30 unless set, at least 1. A settings file it would reject (not
     * JSON, or a period that isn't a whole number of days from 1 up) gives no answer, as Claude
     * Code itself then stops deleting. Settings from other scopes (a project's, a managed
     * policy) can override the value and are not read.
     */
    fn retention_days(&mut self) -> Result<Option<u32>, ProviderError> {
        let Some(settings) = self
            .transport
            .read_small(&format!("{}/settings.json", self.root))?
        else {
            return Ok(Some(DEFAULT_RETENTION_DAYS));
        };
        Ok(match serde_json::from_slice(&settings) {
            Ok(Value::Object(settings)) => match settings.get("cleanupPeriodDays") {
                None => Some(DEFAULT_RETENTION_DAYS),
                Some(days) => days
                    .as_u64()
                    .filter(|days| *days >= 1)
                    .map(|days| u32::try_from(days).unwrap_or(u32::MAX)),
            },
            _ => None,
        })
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
mod source_tests {
    use std::fs;

    use super::*;
    use crate::transport::LocalTransport;

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
    fn the_root_is_the_config_dir_when_set_else_the_home_folder() {
        let home = tempfile::tempdir().unwrap();
        let home_text = home.path().to_str().unwrap().to_string();
        let transport = LocalTransport::with_env(home_text.clone(), []);
        assert!(Agent.detect(&transport).unwrap().is_none());

        let project = home.path().join(".claude/projects/-home-user-code-app");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("s1.jsonl"), "{}\n").unwrap();
        let mut found = Agent.detect(&transport).unwrap().unwrap();
        let keys: Vec<_> = found.scan().unwrap().into_iter().map(|i| i.key).collect();
        assert_eq!(keys, ["projects/-home-user-code-app/s1.jsonl"]);

        let elsewhere = home.path().join("config");
        fs::create_dir_all(elsewhere.join("projects/-home-user-code-other")).unwrap();
        fs::write(
            elsewhere.join("projects/-home-user-code-other/s2.jsonl"),
            "{}\n",
        )
        .unwrap();
        let transport = LocalTransport::with_env(
            home_text,
            [(
                "CLAUDE_CONFIG_DIR".to_string(),
                elsewhere.to_str().unwrap().to_string(),
            )],
        );
        let mut found = Agent.detect(&transport).unwrap().unwrap();
        let keys: Vec<_> = found.scan().unwrap().into_iter().map(|i| i.key).collect();
        assert_eq!(keys, ["projects/-home-user-code-other/s2.jsonl"]);
    }

    #[test]
    fn retention_is_read_from_settings_with_the_documented_default() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".claude");
        fs::create_dir_all(root.join("projects")).unwrap();
        let transport = LocalTransport::with_env(home.path().to_str().unwrap().to_string(), []);
        let retention = |settings: Option<&str>| {
            match settings {
                Some(settings) => fs::write(root.join("settings.json"), settings).unwrap(),
                None => drop(fs::remove_file(root.join("settings.json"))),
            }
            Agent
                .detect(&transport)
                .unwrap()
                .unwrap()
                .retention_days()
                .unwrap()
        };
        assert_eq!(retention(None), Some(30));
        assert_eq!(retention(Some(r#"{"theme": "dark"}"#)), Some(30));
        assert_eq!(retention(Some(r#"{"cleanupPeriodDays": 365}"#)), Some(365));
        assert_eq!(retention(Some(r#"{"cleanupPeriodDays": 0}"#)), None);
        assert_eq!(retention(Some(r#"{"cleanupPeriodDays": "90"}"#)), None);
        assert_eq!(retention(Some("{ not json")), None);
    }
}
