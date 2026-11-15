//! Claude Code's provider (PLAN §7.3): the only code that knows Claude Code.
//!
//! Its data is a file tree (PLAN §7.2). The root is `$CLAUDE_CONFIG_DIR` when set, else
//! `~/.claude`, and every file under `projects/` is an item, keyed by its path relative to the
//! root, whether or not it is one of these. Under `projects/<project>/`, with `<session>` a
//! session id, which is a UUID:
//!
//! | Path | Item |
//! |---|---|
//! | `<session>.jsonl` | the session's main transcript |
//! | `<session>.<anything else>` | a sidecar of the session |
//! | `<session>/subagents/…/agent-<id>.jsonl` | a subagent's transcript |
//! | `<session>/tool-results/…` | tool output saved outside the transcript |
//! | `<session>/…` | a sidecar of the session: titles, subagent metadata, workflows |
//! | anything else (`memory/…`, an index file) | a sidecar of no session |
//!
//! A session's folder can be there without its transcript, and the transcript without the
//! folder. `<project>` is the starting directory with its punctuation replaced by `-`, so it
//! can't be turned back into a path and is not read here. Every `.jsonl` file is appended to
//! line by line.

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
        [file] => match session_of(file) {
            Some(session) if &file[session.len()..] == ".jsonl" => {
                (ItemKind::Transcript, Some(session))
            }
            session => (ItemKind::Sidecar, session),
        },
        [folder, ref rest @ ..] => match (session_of(folder), rest) {
            (Some(session), ["subagents", .., file])
                if session == folder && file.starts_with("agent-") && file.ends_with(".jsonl") =>
            {
                (ItemKind::Subagent, Some(session))
            }
            (Some(session), ["tool-results", ..]) if session == folder => {
                (ItemKind::ToolResult, Some(session))
            }
            (Some(session), _) if session == folder => (ItemKind::Sidecar, Some(session)),
            _ => (ItemKind::Sidecar, None),
        },
        [] => (ItemKind::Sidecar, None),
    };
    FileClass {
        kind,
        session: session.map(str::to_string),
        lines: key.ends_with(".jsonl"),
    }
}

/// The session id `name` starts with, when it is one alone or one followed by `.` and more: a
/// UUID, as in `01234567-89ab-cdef-0123-456789abcdef.jsonl`. Other names in a project folder
/// are not a session's.
fn session_of(name: &str) -> Option<&str> {
    let id = name.get(..36)?;
    let shaped = id.bytes().enumerate().all(|(at, byte)| match at {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    });
    (shaped && matches!(name.as_bytes().get(36), None | Some(b'.'))).then_some(id)
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
        const ID: &str = "01234567-89ab-cdef-0123-456789abcdef";
        let in_session = |path: &str| format!("{ID}{path}");
        for (inside, kind, session, lines) in [
            (in_session(".jsonl"), Transcript, true, true),
            (in_session(".released.json"), Sidecar, true, false),
            (
                in_session("/subagents/agent-a1.jsonl"),
                Subagent,
                true,
                true,
            ),
            (
                in_session("/subagents/agent-a1.meta.json"),
                Sidecar,
                true,
                false,
            ),
            (
                in_session("/subagents/agent-a1.prefix.json"),
                Sidecar,
                true,
                false,
            ),
            (
                in_session("/subagents/workflows/w1/agent-a2.jsonl"),
                Subagent,
                true,
                true,
            ),
            (
                in_session("/subagents/workflows/w1/journal.jsonl"),
                Sidecar,
                true,
                true,
            ),
            (in_session("/tool-results/r1.txt"), ToolResult, true, false),
            (in_session("/tool-results/r2.png"), ToolResult, true, false),
            (in_session("/tool-results/r3.html"), ToolResult, true, false),
            (in_session("/custom-title.json"), Sidecar, true, false),
            (in_session("/notes.txt"), Sidecar, true, false),
            (in_session("/workflows/w1.json"), Sidecar, true, false),
            (in_session("/workflows/w1/main.js"), Sidecar, true, false),
            /* Not a session's: no session id leads the name. */
            ("memory/notes.md".to_string(), Sidecar, false, false),
            ("sessions-index.json".to_string(), Sidecar, false, false),
            ("notes.jsonl".to_string(), Sidecar, false, true),
            (
                "other/tool-results/r1.txt".to_string(),
                Sidecar,
                false,
                false,
            ),
            (format!("{ID}x.jsonl"), Sidecar, false, true),
            (
                format!("{ID}x/subagents/agent-a1.jsonl"),
                Sidecar,
                false,
                true,
            ),
            (
                "01234567-89ab-cdef-0123-456789abcdeg.jsonl".to_string(),
                Sidecar,
                false,
                true,
            ),
        ] {
            assert_eq!(
                classify(&format!("projects/-home-user-code-app/{inside}")),
                class(kind, session.then_some(ID), lines),
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
