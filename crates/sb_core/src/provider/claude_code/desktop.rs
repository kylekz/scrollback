//! The Claude desktop app's records of its sessions (PLAN §7.3, §7.7): a JSON file per session
//! in the app's own data folder, outside the agent's root, which is the one place that names
//! the host a session it ran over SSH ran on. They are archived as register items and read
//! here for what they say of each conversation.
//!
//! The files are the desktop app's own state, not a documented format, so every field is
//! taken only when it is there with the type expected, and a file that isn't JSON says
//! nothing. Of the fields seen on the owner's PC these are read:
//!
//! - `cliSessionId`, `priorCliSessionIds`, `priorRemoteCliSessionIds` and
//!   `lastAssistantCliSessionId`: the ids of the Claude Code sessions the desktop session ran
//!   as, any of which is the id a copy's transcript is named by;
//! - `sshConfig.sshHost`: the host as the app handed it to `ssh`;
//! - `sshRemoteTranscriptPath`: the transcript's path on that machine.
//!
//! `wslConfig` is not read: its shape has not been seen, so which distro it names can't be
//! taken from it unambiguously.

use std::collections::BTreeMap;
use std::io::Read as _;

use serde_json::Value;

use crate::provider::{CopyRecord, FileClass, ItemKind, SessionItems};

/// The folder of the desktop app's records, under the app's data folder. Keys of the items in
/// it start with it, and no key under the agent's root does.
pub(super) const SUBTREE: &str = "claude-code-sessions";

/// The app's data folder, under `%APPDATA%` on Windows and under the home's
/// `Library/Application Support` on macOS.
pub(super) const APP_FOLDER: &str = "Claude";
pub(super) const MAC_SUPPORT: &str = "Library/Application Support";

/// The most a record is read up to: one larger is not one of the app's.
const RECORD_BYTES: u64 = 1024 * 1024;

/// The variable that names Windows' roaming application data folder.
pub(super) const APPDATA: &str = "APPDATA";

/// Whether the file at `key` (under [`SUBTREE`]) is one of the app's records, which are all
/// that is archived of its folder: a `.json` file directly in one session's folder,
/// `claude-code-sessions/<session>/<name>.json`. Whatever else the app keeps there is its own.
pub(super) fn is_record(key: &str) -> bool {
    key.strip_prefix(SUBTREE)
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(folder, name)| {
            !folder.is_empty()
                && !name.contains('/')
                && name.len() > ".json".len()
                && name.ends_with(".json")
        })
}

/// What a file under [`SUBTREE`] is: a record of the app's ([`is_record`]), else something
/// else it keeps there, which is not archived. Neither is any session's.
pub(super) fn classify(key: &str) -> FileClass {
    FileClass {
        kind: if is_record(key) {
            ItemKind::Register
        } else {
            ItemKind::Sidecar
        },
        session: None,
        lines: false,
    }
}

/// What one record says.
#[derive(Default, PartialEq)]
struct Said {
    host: Option<String>,
    path: Option<String>,
}

/**
 * What the records among `register` say of where each conversation ran: one record for each
 * id a file names, with what the files that name it agree on, and those files. A path is said
 * only of the id it names (the transcript `<id>.jsonl`), not of the session's prior ones. Where
 * two files say different things of one conversation (another host, another path), that thing
 * is not said; a conversation of which nothing is said is left out. A file that can't be read
 * or isn't JSON says nothing.
 */
pub(super) fn copy_records(register: &dyn SessionItems) -> Vec<CopyRecord> {
    /* Each conversation's host and path, each `None` while no file has said it and
    `Some(None)` once two files disagreed, and the files that named it. */
    type Agreed = (Option<Option<String>>, Option<Option<String>>, Vec<String>);
    let mut agreed: BTreeMap<String, Agreed> = BTreeMap::new();
    for item in register.list() {
        if item.kind != ItemKind::Register {
            continue;
        }
        let Some(record) = read(register, &item.key) else {
            continue;
        };
        let said = said(&record);
        if said == Said::default() {
            continue;
        }
        for id in ids(&record) {
            let (host, path, sources) = agreed.entry(id.clone()).or_default();
            agree(host, said.host.as_ref());
            let named = (said.path.as_ref()).filter(|path| names(path, &id));
            agree(path, named);
            sources.push(item.key.clone());
        }
    }
    agreed
        .into_iter()
        .filter_map(|(conversation, (host, path, sources))| {
            let (host, path) = (host.flatten(), path.flatten());
            (host.is_some() || path.is_some()).then(|| CopyRecord {
                key: path.as_deref().and_then(key_of),
                conversation,
                host,
                path,
                sources,
            })
        })
        .collect()
}

/// The record at `key` of `register`, read as JSON: `None` when it can't be read, is larger
/// than a record is, or isn't JSON.
fn read(register: &dyn SessionItems, key: &str) -> Option<Value> {
    let mut json = Vec::new();
    register
        .open(key, 0)
        .and_then(|file| file.take(RECORD_BYTES + 1).read_to_end(&mut json))
        .ok()?;
    if json.len() as u64 > RECORD_BYTES {
        return None;
    }
    serde_json::from_slice(&json).ok()
}

/// Whether the transcript path `path` is the transcript of the session `id`: `…/<id>.jsonl`.
fn names(path: &str, id: &str) -> bool {
    path.rsplit(['/', '\\'])
        .next()
        .and_then(|name| name.strip_suffix(".jsonl"))
        == Some(id)
}

/// What a check of the records says of one (the `check` example): the session ids it names,
/// whether it marks subagents left out (`subagentsTruncatedFor`), and whether it is larger
/// than a record is read up to. Nothing it says.
pub(in crate::provider) fn audit(bytes: &[u8]) -> crate::provider::RegisterAudit {
    let record: Option<Value> = serde_json::from_slice(bytes).ok();
    let truncated = match record
        .as_ref()
        .and_then(|it| it.get("subagentsTruncatedFor"))
    {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::Array(it)) => !it.is_empty(),
        Some(Value::String(it)) => !it.is_empty(),
        Some(Value::Object(it)) => !it.is_empty(),
        Some(_) => true,
    };
    crate::provider::RegisterAudit {
        json: record.is_some(),
        too_large: bytes.len() as u64 > RECORD_BYTES,
        ids: record.as_ref().map(ids).unwrap_or_default(),
        truncated,
    }
}

/**
 * The key a transcript at `path` on another machine has there: from the last `projects/` in
 * the path on, when that names a session's own transcript (the module docs of the provider).
 * The root above it can be anywhere (`$CLAUDE_CONFIG_DIR`), so it is not looked at here.
 */
fn key_of(path: &str) -> Option<String> {
    let at = path.rfind(&format!("/{}/", super::SUBTREE))?;
    let key = &path[at + 1..];
    let copy = key.split('/').nth(1).is_some_and(super::is_copy);
    (super::classify(key).kind == ItemKind::Transcript && !copy).then(|| key.to_string())
}

/// Takes `said` into what files agreed on so far, `agreed`: a file that says nothing of it
/// changes nothing, and one that says something else leaves it unsaid.
fn agree(agreed: &mut Option<Option<String>>, said: Option<&String>) {
    let Some(said) = said else {
        return;
    };
    match agreed {
        None => *agreed = Some(Some(said.clone())),
        Some(Some(was)) if was != said => *agreed = Some(None),
        Some(_) => {}
    }
}

/// The host and the transcript's path a record names, each when it is non-empty text.
fn said(record: &Value) -> Said {
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string)
    };
    Said {
        host: text(record.pointer("/sshConfig/sshHost")),
        path: text(record.get("sshRemoteTranscriptPath")),
    }
}

/// The ids of the Claude Code sessions a record names: the current one, the prior ones and the
/// last one an assistant answered in. Only ids shaped as a session's are taken.
fn ids(record: &Value) -> Vec<String> {
    let one = ["cliSessionId", "lastAssistantCliSessionId"]
        .into_iter()
        .filter_map(|field| record.get(field));
    let many = ["priorCliSessionIds", "priorRemoteCliSessionIds"]
        .into_iter()
        .filter_map(|field| record.get(field)?.as_array())
        .flatten();
    let mut ids: Vec<String> = one
        .chain(many)
        .filter_map(Value::as_str)
        .filter(|id| super::session_of(id) == Some(*id))
        .map(str::to_string)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor, Read};

    use super::*;
    use crate::provider::SessionItem;

    const ONE: &str = "00000000-0000-4000-8000-000000000001";
    const TWO: &str = "00000000-0000-4000-8000-000000000002";
    const THREE: &str = "00000000-0000-4000-8000-000000000003";

    /// Register items, each a key with its bytes.
    struct Files(Vec<SessionItem>, Vec<Vec<u8>>);

    impl Files {
        fn of(files: &[(&str, &str)]) -> Self {
            Self(
                files
                    .iter()
                    .map(|(key, bytes)| SessionItem {
                        key: format!("{SUBTREE}/{key}"),
                        kind: classify(&format!("{SUBTREE}/{key}")).kind,
                        length: bytes.len() as u64,
                    })
                    .collect(),
                files
                    .iter()
                    .map(|(_, bytes)| bytes.as_bytes().to_vec())
                    .collect(),
            )
        }
    }

    impl SessionItems for Files {
        fn list(&self) -> &[SessionItem] {
            &self.0
        }

        fn open(&self, key: &str, offset: u64) -> io::Result<Box<dyn Read + '_>> {
            let at = (self.0.iter())
                .position(|item| item.key == key)
                .ok_or(io::ErrorKind::NotFound)?;
            Ok(Box::new(Cursor::new(&self.1[at][offset as usize..])))
        }
    }

    fn record(
        conversation: &str,
        host: Option<&str>,
        path: Option<&str>,
        sources: &[&str],
    ) -> CopyRecord {
        CopyRecord {
            conversation: conversation.to_string(),
            host: host.map(str::to_string),
            path: path.map(str::to_string),
            key: path.and_then(key_of),
            sources: (sources.iter())
                .map(|key| format!("{SUBTREE}/{key}"))
                .collect(),
        }
    }

    #[test]
    fn a_recorded_path_is_keyed_from_its_last_projects_folder() {
        assert_eq!(
            key_of(&format!(
                "/home/user/projects/.claude/projects/-a/{ONE}.jsonl"
            ))
            .as_deref(),
            Some(format!("projects/-a/{ONE}.jsonl").as_str())
        );
        assert_eq!(
            key_of(&format!(
                "/srv/agent/projects/-home-user-code-app/{ONE}.jsonl"
            ))
            .as_deref(),
            Some(format!("projects/-home-user-code-app/{ONE}.jsonl").as_str())
        );
        /* Only a session's own transcript, and never another app's copy of one. */
        for path in [
            format!("/home/user/.claude/projects/-a/{ONE}/subagents/agent-a1.jsonl"),
            format!("/home/user/.claude/projects/ssh-{TWO}/{ONE}.jsonl"),
            format!("~/.claude/history/{ONE}.jsonl"),
            format!("{ONE}.jsonl"),
        ] {
            assert_eq!(key_of(&path), None, "{path}");
        }
    }

    #[test]
    fn a_record_joins_every_id_it_names_to_its_host_and_path() {
        let files = Files::of(&[(
            "s1/local_s1.json",
            &format!(
                r#"{{"cliSessionId": "{ONE}", "priorCliSessionIds": ["{TWO}"],
                    "priorRemoteCliSessionIds": ["{THREE}", "not-an-id"],
                    "lastAssistantCliSessionId": "{ONE}",
                    "sshConfig": {{"sshHost": "buildbox", "name": "Build box"}},
                    "sshRemoteTranscriptPath": "/home/user/.claude/projects/-a/{ONE}.jsonl"}}"#
            ),
        )]);
        /* The path is the transcript of one of them, and said of that one alone. */
        let path = format!("/home/user/.claude/projects/-a/{ONE}.jsonl");
        let file = ["s1/local_s1.json"];
        assert_eq!(
            copy_records(&files),
            [
                record(ONE, Some("buildbox"), Some(&path), &file),
                record(TWO, Some("buildbox"), None, &file),
                record(THREE, Some("buildbox"), None, &file),
            ]
        );
    }

    #[test]
    fn what_can_t_be_read_says_nothing() {
        let files = Files::of(&[
            ("bad/a.json", "{ not json"),
            ("list/a.json", "[1, 2]"),
            (
                "typed/a.json",
                &format!(r#"{{"cliSessionId": "{ONE}", "sshConfig": {{"sshHost": 7}}}}"#),
            ),
            (
                "flat/a.json",
                &format!(r#"{{"cliSessionId": "{ONE}", "sshConfig": "buildbox"}}"#),
            ),
            (
                "blank/a.json",
                &format!(r#"{{"cliSessionId": "{ONE}", "sshConfig": {{"sshHost": " "}}}}"#),
            ),
            (
                "local/a.json",
                &format!(r#"{{"cliSessionId": "{TWO}", "cwd": "/Users/user/code"}}"#),
            ),
            (
                "noid/a.json",
                r#"{"cliSessionId": 5, "sshConfig": {"sshHost": "buildbox"}}"#,
            ),
            ("text/a.txt", "sshHost buildbox"),
        ]);
        assert_eq!(copy_records(&files), []);
    }

    #[test]
    fn files_that_disagree_leave_the_disagreement_unsaid() {
        let file = |id: &str, host: &str, path: &str| {
            format!(
                r#"{{"cliSessionId": "{id}", "sshConfig": {{"sshHost": "{host}"}},
                    "sshRemoteTranscriptPath": "{path}"}}"#
            )
        };
        let (one, two) = (format!("/p/{ONE}.jsonl"), format!("/p/{TWO}.jsonl"));
        let files = Files::of(&[
            ("a/a.json", &file(ONE, "buildbox", &one)),
            ("b/b.json", &file(ONE, "user@buildbox", &one)),
            ("c/c.json", &file(TWO, "buildbox", &two)),
            ("d/d.json", &file(TWO, "buildbox", &two)),
        ]);
        assert_eq!(
            copy_records(&files),
            [
                record(ONE, None, Some(&one), &["a/a.json", "b/b.json"]),
                record(TWO, Some("buildbox"), Some(&two), &["c/c.json", "d/d.json"]),
            ]
        );
    }
}
