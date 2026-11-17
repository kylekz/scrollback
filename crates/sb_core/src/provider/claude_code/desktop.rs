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

use crate::provider::{CopyRecord, FileClass, ItemKind, Joined, SessionItems};

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

/**
 * Whether the file at `key` (under [`SUBTREE`]) is one of the app's records, which are all
 * that is archived of its folder: a `.json` file one or two folders down,
 * `claude-code-sessions/<a>/<name>.json` or `claude-code-sessions/<a>/<b>/<name>.json`, as the
 * app has kept them (on Windows, the second: what seem an account's and an organisation's
 * folders, with every session's record in the inner one). The folders' names are not read: a
 * record says whose it is in its fields. Nothing at the top or deeper, and nothing else the
 * app keeps there, is a record.
 */
pub(super) fn is_record(key: &str) -> bool {
    let Some(inside) = key
        .strip_prefix(SUBTREE)
        .and_then(|rest| rest.strip_prefix('/'))
    else {
        return false;
    };
    let parts: Vec<&str> = inside.split('/').collect();
    let Some((name, folders)) = parts.split_last() else {
        return false;
    };
    (1..=2).contains(&folders.len())
        && folders.iter().all(|folder| !folder.is_empty())
        && name.len() > ".json".len()
        && name.ends_with(".json")
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
    cwd: Option<String>,
}

/**
 * What the records among `register` say of where each conversation ran (PLAN §7.7). A
 * conversation is joined to the records that name it by precedence, and the first that has any
 * is taken alone:
 *
 * 1. its own: the records whose `cliSessionId` is its id;
 * 2. the records whose `sshRemoteTranscriptPath` is its transcript, `<id>.jsonl`;
 * 3. the records that name it among the session's earlier or last ids.
 *
 * Other ids a record holds (a published artifact's path, the subagents left out of a copy)
 * are mentions, not identity, and are never read. Of several records at one level, what they
 * say alike is said and what they say differently is not; a path is said only of the id it
 * names. A conversation of which nothing is said is left out. A file that can't be read or
 * isn't JSON says nothing.
 */
pub(super) fn copy_records(register: &dyn SessionItems) -> Vec<CopyRecord> {
    /* Of each conversation, at each level, the host, path and cwd the records agree on (each
    `None` while none has said it, `Some(None)` once two disagreed), and the records. */
    #[derive(Default)]
    struct Agreed {
        host: Option<Option<String>>,
        path: Option<Option<String>>,
        cwd: Option<Option<String>>,
        sources: Vec<String>,
    }
    let mut levels: [BTreeMap<String, Agreed>; 3] = Default::default();
    for item in register.list() {
        if item.kind != ItemKind::Register {
            continue;
        }
        let Some(record) = read(register, &item.key) else {
            continue;
        };
        let said = said(&record);
        if said.host.is_none() && said.path.is_none() {
            continue;
        }
        let own = cli_id(&record);
        let by_path = (said.path.as_deref()).and_then(named);
        let mut joined: Vec<(usize, String)> = Vec::new();
        joined.extend(own.clone().map(|id| (0, id)));
        joined.extend(by_path.clone().map(|id| (1, id)));
        joined.extend((ids(&record).into_iter()).map(|id| (2, id)));
        for (level, id) in joined {
            let agreed = levels[level].entry(id.clone()).or_default();
            agree(&mut agreed.host, said.host.as_ref());
            agree(
                &mut agreed.path,
                (said.path.as_ref()).filter(|_| by_path.as_deref() == Some(id.as_str())),
            );
            agree(&mut agreed.cwd, said.cwd.as_ref());
            if !agreed.sources.contains(&item.key) {
                agreed.sources.push(item.key.clone());
            }
        }
    }
    let [own, by_path, mentioned] = levels;
    let mut taken: BTreeMap<String, (Joined, Agreed)> = BTreeMap::new();
    for (joined, level) in [
        (Joined::Own, own),
        (Joined::Path, by_path),
        (Joined::Mentioned, mentioned),
    ] {
        for (id, agreed) in level {
            taken.entry(id).or_insert((joined, agreed));
        }
    }
    taken
        .into_iter()
        .filter_map(|(conversation, (joined, agreed))| {
            let (host, path) = (agreed.host.flatten(), agreed.path.flatten());
            (host.is_some() || path.is_some()).then(|| CopyRecord {
                key: path.as_deref().and_then(key_of),
                conversation,
                host,
                path,
                cwd: agreed.cwd.flatten(),
                joined,
                sources: agreed.sources,
            })
        })
        .collect()
}

/// The id of the session a record is the app's own record of: its `cliSessionId`.
fn cli_id(record: &Value) -> Option<String> {
    (record.get("cliSessionId")?.as_str())
        .filter(|id| super::session_of(id) == Some(*id))
        .map(str::to_string)
}

/// The id of the session whose transcript the path `path` is: `…/<id>.jsonl`.
fn named(path: &str) -> Option<String> {
    let id = path.rsplit(['/', '\\']).next()?.strip_suffix(".jsonl")?;
    (super::session_of(id) == Some(id)).then(|| id.to_string())
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
        own: record.as_ref().and_then(cli_id),
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
        cwd: text(record.get("cwd")),
    }
}

/// The ids of the Claude Code sessions a record names as the session's: the current one, the
/// prior ones and the last one an assistant answered in. Only ids shaped as a session's are
/// taken.
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
        joined(conversation, Joined::Own, host, path, sources)
    }

    fn joined(
        conversation: &str,
        joined: Joined,
        host: Option<&str>,
        path: Option<&str>,
        sources: &[&str],
    ) -> CopyRecord {
        CopyRecord {
            conversation: conversation.to_string(),
            host: host.map(str::to_string),
            path: path.map(str::to_string),
            key: path.and_then(key_of),
            cwd: None,
            joined,
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
                joined(TWO, Joined::Mentioned, Some("buildbox"), None, &file),
                joined(THREE, Joined::Mentioned, Some("buildbox"), None, &file),
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

    /// A session's own record wins over another session's that names it among its earlier
    /// ids; a record naming its transcript wins over a mention; only mentions can disagree.
    #[test]
    fn a_session_s_own_record_wins_and_mentions_come_last() {
        const FOUR: &str = "00000000-0000-4000-8000-000000000004";
        const FIVE: &str = "00000000-0000-4000-8000-000000000005";
        let file = |fields: &str| format!("{{{fields}}}");
        let three = format!("/home/user/.claude/projects/-a/{THREE}.jsonl");
        let files = Files::of(&[
            (
                "a/a.json",
                &file(&format!(
                    r#""cliSessionId": "{ONE}", "sshConfig": {{"sshHost": "localhost"}}"#
                )),
            ),
            (
                "b/b.json",
                &file(&format!(
                    r#""cliSessionId": "{TWO}", "priorCliSessionIds": ["{ONE}"], "sshConfig": {{"sshHost": "macbook"}}"#
                )),
            ),
            (
                "c/c.json",
                &file(&format!(
                    r#""cliSessionId": "{FOUR}", "sshRemoteTranscriptPath": "{three}", "sshConfig": {{"sshHost": "buildbox"}}"#
                )),
            ),
            (
                "d/d.json",
                &file(&format!(
                    r#""lastAssistantCliSessionId": "{THREE}", "sshConfig": {{"sshHost": "elsewhere"}}"#
                )),
            ),
            (
                "e/e.json",
                &file(&format!(
                    r#""priorCliSessionIds": ["{FIVE}"], "sshConfig": {{"sshHost": "one"}},
                   "publishedArtifacts": [{{"sourcePath": "{FOUR}"}}]"#
                )),
            ),
            (
                "f/f.json",
                &file(&format!(
                    r#""priorRemoteCliSessionIds": ["{FIVE}"], "sshConfig": {{"sshHost": "two"}}"#
                )),
            ),
        ]);
        let by = |id: &str| {
            copy_records(&files)
                .into_iter()
                .find(|it| it.conversation == id)
                .map(|it| (it.joined, it.host))
        };
        assert_eq!(by(ONE), Some((Joined::Own, Some("localhost".to_string()))));
        assert_eq!(by(TWO), Some((Joined::Own, Some("macbook".to_string()))));
        assert_eq!(
            by(THREE),
            Some((Joined::Path, Some("buildbox".to_string())))
        );
        assert_eq!(by(FOUR), Some((Joined::Own, Some("buildbox".to_string()))));
        /* Two mentions that disagree say no host, and so nothing. */
        assert_eq!(by(FIVE), None);
    }
}
