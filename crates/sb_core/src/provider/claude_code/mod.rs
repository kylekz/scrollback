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
//! A file or folder belongs to a session when its name is the id or starts with it and a `.`.
//! A session's folder can be there without its transcript, and the transcript without the
//! folder. `<project>` is the starting directory with its punctuation replaced by `-`, so it
//! can't be turned back into a path and is not read here. Every `.jsonl` file is appended to
//! line by line.
//!
//! This provider's id for a session is `<project>/<session>`. The id alone is not enough: the
//! same one is found under two projects' folders, and those are two transcripts, each with
//! what goes with it.
//!
//! **The desktop app's copies** (PLAN §7.3, §7.7). A project folder named `ssh-<id>` is not a
//! project's: it is where the Claude desktop app keeps its copy of one session it ran over SSH
//! on another machine, and the folder is that session, its id the folder's name. Its
//! `<session>.jsonl` is the transcript, under the original's id, and the `agent-<id>.jsonl`
//! beside it are its subagents' transcripts, loose rather than under `<session>/subagents/`;
//! the rest is as in a project's folder. Only the first transcript of a folder is read, as of
//! any session; a folder with more has not been seen. [`Agent::copy_of`] says from the id alone
//! that the session is a copy, so the library shows its original in its place once that is
//! archived.
//!
//! **The desktop app's records** ([`desktop`]). The app keeps a JSON file per session in its
//! own data folder, outside the root: `Claude/claude-code-sessions/` under `$APPDATA` when
//! that is set, else under the home's `Library/Application Support`. Wherever a location has
//! that folder, the `.json` files directly in each of its session folders
//! (`claude-code-sessions/<session>/<name>.json`) are items of no session, of kind register,
//! keyed by their path relative to the app's data folder (which no key under the root starts
//! with); nothing else of the folder is archived. [`Agent::copy_records`] reads them for the
//! host and the transcript's path each conversation had where it ran (PLAN §7.7).
//!
//! [`parse`] turns a session's archived files into the transcript model, [`tools`] maps
//! Claude Code's tools onto the common kinds, and [`audit`] counts a session's records another
//! way to check the parse against.

mod audit;
mod desktop;
mod outline;
mod parse;
mod record;
mod said;
#[cfg(test)]
mod tests;
mod tools;

pub(super) use audit::audit;

use std::path::Path;

use serde_json::Value;

use super::{
    CopyRecord, Display, FileClass, FileTree, Hashed, ItemKind, ItemPrefix, ItemRange, Mark,
    Provider, ProviderError, ProviderId, RangeFailure, Scan, SessionItems, Source,
};
use crate::transcript::{CopyOf, Origin, Outline, Subagent, Transcript};
use crate::transport::{Env, Halt, RangeSink, Transport};

pub(super) const ID: ProviderId = ProviderId::new("claude-code");

/// The version of [`parse`].
const PARSER_VERSION: u32 = 3;

/// Where the items are, under the root.
const SUBTREE: &str = "projects";

/// What a project folder's name starts with when it holds the desktop app's copy of a session.
const COPY_FOLDER: &str = "ssh-";

/// How the desktop app's copies are shown (PLAN §7.7).
const COPY_LABEL: &str = "Claude Desktop's copy";
const COPY_ORIGIN: &str = "Ran over SSH; the copy doesn't record which machine.";

/// How long transcripts are kept when `settings.json` doesn't say (Claude Code's documented
/// default).
const DEFAULT_RETENTION_DAYS: u32 = 30;

/// The mark Claude's models are shown with (DESIGN §1.5): eight rays, on the icons' 24 grid
/// at their stroke.
const MARK: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v18M3 12h18M5.6 5.6l12.8 12.8M18.4 5.6 5.6 18.4"/></svg>"#;

/// Claude Code.
pub struct Agent;

impl Provider for Agent {
    fn id(&self) -> ProviderId {
        ID
    }

    fn display(&self) -> Display {
        Display {
            name: "Claude Code",
            mark: Some(Mark {
                svg: MARK,
                dark: 0xd97757,
                light: 0xc6613f,
            }),
        }
    }

    fn model_name(&self, model: &str) -> String {
        model_name(model)
    }

    fn tool_label(&self, name: &str) -> Option<&'static str> {
        tools::label(name)
    }

    fn detect<'a>(
        &'a self,
        transport: &'a dyn Transport,
        _scratch: &'a Path,
    ) -> Result<Option<Box<dyn Source + 'a>>, ProviderError> {
        let env = transport.env(&["CLAUDE_CONFIG_DIR", desktop::APPDATA])?;
        let apps = app_folders(&env);
        let root = root(env);
        /* The desktop app's folder is looked for in the same call, so a location without it
        costs nothing more. */
        let mut paths = vec![format!("{root}/{SUBTREE}")];
        paths.extend(
            (apps.iter()).map(|app| format!("{app}/{}/{}", desktop::APP_FOLDER, desktop::SUBTREE)),
        );
        let found = transport.stat(&paths.iter().map(String::as_str).collect::<Vec<_>>())?;
        let app = (apps.into_iter().zip(&found[1..]))
            .find_map(|(app, found)| found.is_some().then_some(app));
        Ok(found[0].is_some().then(|| {
            Box::new(Data {
                tree: FileTree::new(transport, root.clone(), SUBTREE, classify),
                desktop: app.map(|app| {
                    FileTree::new(
                        transport,
                        format!("{app}/{}", desktop::APP_FOLDER),
                        desktop::SUBTREE,
                        desktop::classify,
                    )
                }),
                transport,
                root,
            }) as Box<dyn Source>
        }))
    }

    fn parser_version(&self) -> u32 {
        PARSER_VERSION
    }

    fn parse(&self, items: &dyn SessionItems) -> Transcript {
        parse::parse(items)
    }

    fn subagent(&self, items: &dyn SessionItems, item: &str) -> Subagent {
        parse::subagent(items, item)
    }

    fn outlines(&self, items: &dyn SessionItems) -> Vec<Outline> {
        outline::outlines(items)
    }

    fn classify(&self, key: &str) -> (ItemKind, Option<String>) {
        let class = if is_desktop(key) {
            desktop::classify(key)
        } else {
            classify(key)
        };
        (class.kind, class.session)
    }

    fn copy_of(&self, session: &str) -> Option<CopyOf> {
        is_copy(session).then(|| CopyOf {
            label: COPY_LABEL.to_string(),
            origin: Origin::Unsaid(COPY_ORIGIN.to_string()),
        })
    }

    fn copy_records(&self, register: &dyn SessionItems) -> Vec<CopyRecord> {
        desktop::copy_records(register)
    }

    /// The session id that names the transcript: the id its records carry (PLAN §7.3).
    fn conversation_of(&self, transcript: &str) -> Option<String> {
        let name = transcript.rsplit('/').next()?;
        let id = session_of(name).filter(|id| name.len() == id.len() + ".jsonl".len())?;
        (classify(transcript).kind == ItemKind::Transcript).then(|| id.to_string())
    }
}

/// Where the desktop app may keep its data folder at a location with the environment `env`:
/// under `$APPDATA` when it is set (Windows), and under the home's `Library/Application
/// Support` (macOS).
fn app_folders(env: &Env) -> Vec<String> {
    (env.vars
        .get(desktop::APPDATA)
        .filter(|dir| !dir.is_empty())
        .cloned())
    .into_iter()
    .chain([format!("{}/{}", env.home, desktop::MAC_SUPPORT)])
    .collect()
}

/// Whether `key` is an item of the desktop app's folder rather than of the root.
fn is_desktop(key: &str) -> bool {
    key.strip_prefix(desktop::SUBTREE)
        .is_some_and(|rest| rest.starts_with('/'))
}

/**
 * A model's id as people write the model: `claude-opus-4-6` is "Opus 4.6" and
 * `claude-3-5-sonnet-20241022` "3.5 Sonnet". The maker's name and a release date are left out,
 * and the numbers of a version are joined. An id that isn't one of Claude's stays as it is.
 */
fn model_name(model: &str) -> String {
    let Some(rest) = model.strip_prefix("claude-") else {
        return model.to_string();
    };
    let mut words: Vec<String> = Vec::new();
    let mut after_number = false;
    for part in rest.split('-') {
        let number = !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
        if number && part.len() == 8 {
            continue;
        }
        match words.last_mut() {
            Some(version) if number && after_number => {
                version.push('.');
                version.push_str(part);
            }
            _ => {
                let mut letters = part.chars();
                words.push(match letters.next() {
                    Some(first) => first.to_uppercase().chain(letters).collect(),
                    None => String::new(),
                });
            }
        }
        after_number = number;
    }
    words.join(" ")
}

/// Where Claude Code keeps its data at a location with the environment `env`.
fn root(mut env: Env) -> String {
    match env.vars.remove("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => dir,
        _ => format!("{}/.claude", env.home),
    }
}

/// Claude Code's data at one location: the root's tree, and the desktop app's records where
/// the location has them.
struct Data<'a> {
    tree: FileTree<'a>,
    desktop: Option<FileTree<'a>>,
    transport: &'a dyn Transport,
    root: String,
}

impl<'a> Data<'a> {
    /// The tree the item `key` is in.
    fn tree_of(&mut self, key: &str) -> &mut FileTree<'a> {
        match &mut self.desktop {
            Some(desktop) if is_desktop(key) => desktop,
            _ => &mut self.tree,
        }
    }

    /**
     * `requests` split into the runs of them that are in one tree, in order, each with its
     * first request's index: a transport answers in the order asked (PLAN §4.1), so the runs
     * are asked one after another.
     */
    fn runs<T>(requests: &[T], key: impl Fn(&T) -> &str) -> Vec<(usize, &[T])> {
        let mut runs = Vec::new();
        let mut start = 0;
        for at in 1..=requests.len() {
            if at == requests.len()
                || is_desktop(key(&requests[at])) != is_desktop(key(&requests[start]))
            {
                runs.push((start, &requests[start..at]));
                start = at;
            }
        }
        runs
    }
}

impl Source for Data<'_> {
    fn root(&self) -> String {
        self.tree.root()
    }

    fn scan(&mut self) -> Result<Scan, ProviderError> {
        let mut scan = self.tree.scan()?;
        if let Some(desktop) = &mut self.desktop {
            let more = desktop.scan()?;
            /* Of the app's folder, only its records are archived (the module docs). */
            scan.items
                .extend((more.items.into_iter()).filter(|item| desktop::is_record(&item.key)));
            scan.unlisted.extend(more.unlisted);
            scan.skipped.extend(more.skipped);
        }
        Ok(scan)
    }

    fn hash_prefixes(
        &mut self,
        requests: &[ItemPrefix],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, ProviderError> {
        let mut hashed = Vec::with_capacity(requests.len());
        for (_, run) in Self::runs(requests, |request| &request.key) {
            hashed.extend(self.tree_of(&run[0].key).hash_prefixes(run)?);
        }
        Ok(hashed)
    }

    fn fetch(
        &mut self,
        requests: &[ItemRange],
        sink: &mut dyn RangeSink,
    ) -> Result<(), ProviderError> {
        for (first, run) in Self::runs(requests, |request| &request.key) {
            self.tree_of(&run[0].key)
                .fetch(run, &mut Shifted { sink, by: first })?;
        }
        Ok(())
    }

    fn retention_days(&mut self) -> Result<Option<u32>, ProviderError> {
        Ok(retention_days(
            self.transport
                .read_small(&format!("{}/settings.json", self.root))?
                .as_deref(),
        ))
    }
}

/// A sink fed a run of requests that starts at the index `by` of the whole fetch.
struct Shifted<'s> {
    sink: &'s mut dyn RangeSink,
    by: usize,
}

impl RangeSink for Shifted<'_> {
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
        self.sink.data(request + self.by, bytes)
    }

    fn end(&mut self, request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        self.sink.end(request + self.by, result)
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
    let mut parts = key.split('/').skip(1);
    let project = parts.next().unwrap_or_default();
    let inside: Vec<&str> = parts.collect();
    let copy = is_copy(project);
    let led = inside.first().and_then(|name| session_of(name));
    let subagent = |file: &str| file.starts_with("agent-") && file.ends_with(".jsonl");
    let kind = match (led, &inside[..]) {
        (Some(session), [file]) if &file[session.len()..] == ".jsonl" => ItemKind::Transcript,
        (Some(_), [_, "subagents", .., file]) if subagent(file) => ItemKind::Subagent,
        (Some(_), [_, "tool-results", _, ..]) => ItemKind::ToolResult,
        (None, [file]) if copy && subagent(file) => ItemKind::Subagent,
        _ => ItemKind::Sidecar,
    };
    let session = if copy {
        (!inside.is_empty()).then(|| project.to_string())
    } else {
        /* The id alone is not the session: the same one can be in two projects' folders. */
        led.map(|id| format!("{project}/{id}"))
    };
    FileClass {
        kind,
        session,
        lines: key.ends_with(".jsonl"),
    }
}

/// Whether the project folder `name` holds the desktop app's copy of a session (the module
/// docs).
fn is_copy(name: &str) -> bool {
    name.strip_prefix(COPY_FOLDER)
        .is_some_and(|rest| !rest.is_empty())
}

/// The session id that leads `name`, the name of a file or folder in a project: a UUID, alone
/// or followed by `.` and more, as in `01234567-89ab-cdef-0123-456789abcdef.jsonl`. Other
/// names there are not a session's.
fn session_of(name: &str) -> Option<&str> {
    let id = name.get(..36)?;
    let shaped = id.bytes().enumerate().all(|(at, byte)| match at {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    });
    (shaped && matches!(name.as_bytes().get(36), None | Some(b'.'))).then_some(id)
}

#[cfg(test)]
mod source_tests {
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
        /* A session is its id in its project's folder: the same id elsewhere is another. */
        const SESSION: &str = "-home-user-code-app/01234567-89ab-cdef-0123-456789abcdef";
        assert_eq!(
            classify(&format!("projects/-home-user-code-other/{ID}.jsonl")).session,
            Some(format!("-home-user-code-other/{ID}"))
        );
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
            /* A folder is a session's by the same rule as a file. */
            (in_session(".d/notes.txt"), Sidecar, true, false),
            (
                in_session(".d/tool-results/r1.txt"),
                ToolResult,
                true,
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
                class(kind, session.then_some(SESSION), lines),
                "{inside}"
            );
        }
        assert_eq!(classify("projects/stray.txt"), class(Sidecar, None, false));
    }

    /// The desktop app's copy is a folder that is one session: its transcript, its loose
    /// subagents and whatever else is in it.
    #[test]
    fn a_copy_folder_is_one_session_with_its_loose_subagents() {
        use ItemKind::{Sidecar, Subagent, ToolResult, Transcript};
        const ID: &str = "01234567-89ab-cdef-0123-456789abcdef";
        const FOLDER: &str = "ssh-89abcdef-0123-4567-89ab-cdef01234567";
        for (inside, kind, lines) in [
            (format!("{ID}.jsonl"), Transcript, true),
            ("agent-a1.jsonl".to_string(), Subagent, true),
            ("agent-a1.meta.json".to_string(), Sidecar, false),
            (format!("{ID}/subagents/agent-a2.jsonl"), Subagent, true),
            (format!("{ID}/tool-results/r1.txt"), ToolResult, false),
            ("notes.txt".to_string(), Sidecar, false),
        ] {
            assert_eq!(
                classify(&format!("projects/{FOLDER}/{inside}")),
                class(kind, Some(FOLDER), lines),
                "{inside}"
            );
        }
        /* Only under a copy's folder is a loose subagent a session's. */
        assert_eq!(
            classify("projects/-home-user-code-app/agent-a1.jsonl"),
            class(Sidecar, None, true)
        );
        assert_eq!(classify("projects/ssh-"), class(Sidecar, None, false));
        assert!(Agent.copy_of(FOLDER).is_some());
        assert!(
            Agent
                .copy_of(&format!("-home-user-code-app/{ID}"))
                .is_none()
        );
        assert!(Agent.copy_of("ssh-").is_none());
        /* A copy's conversation is its transcript's name, read or not. */
        assert_eq!(
            Agent
                .conversation_of(&format!("projects/{FOLDER}/{ID}.jsonl"))
                .as_deref(),
            Some(ID)
        );
        assert_eq!(
            Agent.conversation_of(&format!("projects/{FOLDER}/agent-a1.jsonl")),
            None
        );
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
