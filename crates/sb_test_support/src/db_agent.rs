//! A made-up agent that keeps its sessions in a SQLite database, and the test-only provider
//! for it (PLAN §7.2, §12). The provider copies the database through `DbSnapshot` and archives
//! one export per session: its messages, one JSON line each, in the order they were inserted.
//!
//! Its sessions are shaped unlike a file-keeping agent's, so that the transcript model is
//! tested against a second shape (PLAN §7.4): a message is a row with a role, the rows have no
//! links between them, a tool call and its result are one row, and where the session started
//! is a row of its own.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sb_core::hash::Hash;
use sb_core::provider::{
    DbSnapshot, Hashed, ItemKind, ItemPrefix, ItemRange, Provider, ProviderError, ProviderId,
    RangeFailure, Scan, ScannedItem, SessionItems, Source,
};
use sb_core::transcript::{
    Builder, Content, Conversation, Encoding, Ending, Facts, Output, Stored, ToolCall, ToolInput,
    ToolKind, ToolResult, ToolStatus, Transcript, Unrecognised,
};
use sb_core::transport::{RangeSink, Transport};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tempfile::TempDir;
use toolbox_store::rusqlite::{Connection, params};

use crate::{Agent, HomeTransport};

const ID: ProviderId = ProviderId::new("db-agent");

/// Where the agent keeps its database, under the location's home.
const DATABASE: &str = ".dbagent/sessions.db";

/// How much of an export is handed to the sink at a time: little, so that lines straddle the
/// pieces.
const CHUNK: usize = 1024;

/// One message as a line of a session's export. `call` is a tool call's row: the tool, its
/// arguments, its output and whether it worked. A row whose `call` is not JSON is exported
/// with what it holds, as text: an export loses nothing, whatever the agent wrote.
fn export_line(id: i64, role: &str, body: &str, call: Option<&str>) -> String {
    let call: Option<Value> = call
        .map(|call| serde_json::from_str(call).unwrap_or_else(|_| Value::String(call.to_string())));
    json!({ "id": id, "role": role, "body": body, "call": call }).to_string() + "\n"
}

/// A line of an export, as the parser reads it.
#[derive(Deserialize)]
struct Row<'a> {
    role: String,
    body: String,
    #[serde(borrow)]
    call: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct CallRow<'a> {
    tool: String,
    #[serde(borrow)]
    args: &'a RawValue,
    #[serde(borrow)]
    output: &'a RawValue,
    ok: bool,
}

/// The arguments of the tools the agent has: `exec` runs a command, `open` reads a file.
#[derive(Deserialize)]
struct Args {
    cmd: Option<String>,
    file: Option<String>,
}

/// The provider for [`DbAgent`].
pub struct DbProvider {
    /// The version of the export format, recorded with each archived version (PLAN §7.2).
    pub export_version: u32,
}

impl Default for DbProvider {
    fn default() -> Self {
        Self { export_version: 1 }
    }
}

impl Provider for DbProvider {
    fn id(&self) -> ProviderId {
        ID
    }

    fn detect<'a>(
        &'a self,
        transport: &'a dyn Transport,
        scratch: &'a Path,
    ) -> Result<Option<Box<dyn Source + 'a>>, ProviderError> {
        let database = format!("{}/{DATABASE}", transport.env(&[])?.home);
        let present = transport.stat(&[&database])?[0].is_some();
        Ok(present.then(|| {
            Box::new(Exports {
                transport,
                scratch,
                database,
                export_version: self.export_version,
                sessions: HashMap::new(),
            }) as Box<dyn Source>
        }))
    }

    fn parser_version(&self) -> u32 {
        1
    }

    fn classify(&self, key: &str) -> (ItemKind, Option<String>) {
        let session = key.strip_prefix("sessions/").map(str::to_string);
        (ItemKind::Transcript, session)
    }

    fn parse(&self, items: &dyn SessionItems) -> Transcript {
        let mut facts = Facts {
            agent: ID,
            title: None,
            project_path: None,
            branch: None,
            entrypoint: None,
        };
        let export = items
            .list()
            .iter()
            .find(|item| item.kind == ItemKind::Transcript);
        Transcript {
            conversation: export.map(|item| self.conversation(items, &item.key, &mut facts)),
            facts,
            unattached: Vec::new(),
        }
    }
}

impl DbProvider {
    /// The conversation in the export `key`. Its rows have no links: each follows the last.
    fn conversation(&self, items: &dyn SessionItems, key: &str, facts: &mut Facts) -> Conversation {
        let mut builder = Builder::new(Arc::from(key));
        let item = builder.item().clone();
        let rows = match items.open(key, 0) {
            Ok(export) => BufReader::new(export).split(b'\n'),
            Err(error) => return builder.finish(Ending::Unreadable(error.to_string())),
        };
        let mut last = None;
        let mut offset = 0;
        for line in rows {
            let line = match line {
                Ok(line) => line,
                Err(error) => return builder.finish(Ending::Unreadable(error.to_string())),
            };
            let whole = Stored {
                item: item.clone(),
                offset,
                length: line.len() as u64,
                encoding: Encoding::Plain,
            };
            let content = match serde_json::from_slice::<Row>(&line) {
                Ok(Row { role, body, call }) => match (role.as_str(), call) {
                    ("start", _) => {
                        facts.project_path = Some(body);
                        None
                    }
                    ("user", _) => Some(Content::Prompt {
                        text: body,
                        images: Vec::new(),
                        from: None,
                    }),
                    ("assistant", _) => Some(Content::Reply { text: body }),
                    ("tool", Some(call)) => Some(match serde_json::from_str(call.get()) {
                        Ok(call) => {
                            Content::ToolCall(Box::new(self.call(call, |raw, encoding| {
                                Stored::part(&item, offset, &line, raw.get(), encoding)
                            })))
                        }
                        Err(_) => Content::Unrecognised(Unrecognised {
                            kind: role,
                            misfit: Some("call"),
                            raw: whole,
                        }),
                    }),
                    _ => Some(Content::Unrecognised(Unrecognised {
                        kind: role,
                        misfit: None,
                        raw: whole,
                    })),
                },
                Err(_) => Some(Content::Unrecognised(Unrecognised {
                    kind: String::new(),
                    misfit: Some("not a row"),
                    raw: whole,
                })),
            };
            if let Some(content) = content {
                last = Some(builder.show(last, None, None, content));
            }
            offset += line.len() as u64 + 1;
        }
        builder.finish(Ending::Whole)
    }

    /// A `tool` row as a call with its result. `part` says where a piece of the row is archived.
    fn call(&self, call: CallRow, part: impl Fn(&RawValue, Encoding) -> Stored) -> ToolCall {
        let kind = match call.tool.as_str() {
            "exec" => ToolKind::Shell,
            "open" => ToolKind::Read,
            _ => ToolKind::Other,
        };
        ToolCall {
            kind,
            input: match (kind, serde_json::from_str(call.args.get())) {
                (
                    ToolKind::Shell,
                    Ok(Args {
                        cmd: Some(command), ..
                    }),
                ) => ToolInput::Shell {
                    command,
                    description: None,
                },
                (
                    ToolKind::Read,
                    Ok(Args {
                        file: Some(path), ..
                    }),
                ) => ToolInput::Read {
                    path,
                    first_line: None,
                    lines: None,
                },
                _ => ToolInput::Other,
            },
            name: call.tool,
            raw_input: part(call.args, Encoding::Plain),
            result: Some(ToolResult {
                at: None,
                status: if call.ok {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Failed
                },
                output: vec![Output::Text(part(call.output, Encoding::JsonString))],
                brief: None,
            }),
            subagents: Vec::new(),
        }
    }
}

/// The agent's sessions at one location, as exports.
struct Exports<'a> {
    transport: &'a dyn Transport,
    scratch: &'a Path,
    database: String,
    export_version: u32,
    /// Each session's export by its item key, as the scan's snapshot had it.
    sessions: HashMap<String, Vec<u8>>,
}

impl Exports<'_> {
    /// The bytes `[offset, offset + length)` of the export `key`.
    fn range(&self, key: &str, offset: u64, length: u64) -> Result<&[u8], RangeFailure> {
        let export = self.sessions.get(key).ok_or(RangeFailure::Missing)?;
        usize::try_from(offset)
            .ok()
            .zip(usize::try_from(offset + length).ok())
            .and_then(|(start, end)| export.get(start..end))
            .ok_or(RangeFailure::Short {
                expected: length,
                got: (export.len() as u64).saturating_sub(offset),
            })
    }
}

impl Source for Exports<'_> {
    fn scan(&mut self) -> Result<Scan, ProviderError> {
        self.sessions.clear();
        let Some(snapshot) = DbSnapshot::take(self.transport, &self.database, self.scratch)? else {
            return Ok(Scan::default());
        };
        let connection = snapshot.open()?;
        let sessions: Vec<(String, i64)> = connection
            .prepare("SELECT id, updated FROM session ORDER BY id")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut messages = connection.prepare(
            "SELECT id, role, body, call FROM message WHERE session_id = ?1 ORDER BY id",
        )?;
        let mut items = Vec::new();
        for (session, updated) in sessions {
            let export: String = messages
                .query_map([&session], |row| {
                    Ok(export_line(
                        row.get(0)?,
                        &row.get::<_, String>(1)?,
                        &row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?.as_deref(),
                    ))
                })?
                .collect::<Result<_, _>>()?;
            let key = format!("sessions/{session}");
            items.push(ScannedItem {
                key: key.clone(),
                kind: ItemKind::Transcript,
                session: Some(session),
                length: export.len() as u64,
                stamp: updated.to_string(),
                lines: true,
                export_version: Some(self.export_version),
            });
            self.sessions.insert(key, export.into_bytes());
        }
        Ok(Scan {
            items,
            ..Scan::default()
        })
    }

    fn hash_prefixes(
        &mut self,
        requests: &[ItemPrefix],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, ProviderError> {
        Ok(requests
            .iter()
            .map(|request| {
                Ok(Hashed {
                    hash: Hash::of(self.range(&request.key, 0, request.length)?),
                    size: self.sessions[&request.key].len() as u64,
                })
            })
            .collect())
    }

    fn fetch(
        &mut self,
        requests: &[ItemRange],
        sink: &mut dyn RangeSink,
    ) -> Result<(), ProviderError> {
        for (index, request) in requests.iter().enumerate() {
            let range = self.range(&request.key, request.offset, request.length);
            for chunk in range.iter().flat_map(|range| range.chunks(CHUNK)) {
                sink.data(index, chunk)?;
            }
            sink.end(index, range.map(drop))?;
        }
        Ok(())
    }
}

/// A row of the agent's `message` table.
struct Message {
    id: i64,
    role: String,
    body: String,
    call: Option<String>,
}

/// The agent's database under a home folder.
pub struct DbAgent {
    home: PathBuf,
    /// The home folder, when this agent made it.
    _temp: Option<TempDir>,
    /// Kept open, as a running agent's is: what it wrote last is only in the write-ahead log.
    connection: Connection,
    /// What each session should hold, by item key.
    expected: HashMap<String, Vec<Message>>,
    sessions: usize,
    messages: usize,
    /// What [`Agent::provider`] gives the provider.
    pub export_version: u32,
}

impl Default for DbAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl DbAgent {
    /// The agent in a home folder of its own, in a temp dir.
    pub fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let agent = Self::in_home(home.path());
        Self {
            _temp: Some(home),
            ..agent
        }
    }

    /// The agent in the home folder `home`, beside whatever other agents keep there.
    pub fn in_home(home: &Path) -> Self {
        let database = home.join(DATABASE);
        fs::create_dir_all(database.parent().unwrap()).unwrap();
        let connection = Connection::open(database).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA wal_autocheckpoint = 0;
                 CREATE TABLE session (id TEXT PRIMARY KEY, updated INTEGER NOT NULL);
                 CREATE TABLE message (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     session_id TEXT NOT NULL REFERENCES session (id),
                     role TEXT NOT NULL,
                     body TEXT NOT NULL,
                     call TEXT
                 );",
            )
            .unwrap();
        Self {
            home: home.to_path_buf(),
            _temp: None,
            connection,
            expected: HashMap::new(),
            sessions: 0,
            messages: 0,
            export_version: 1,
        }
    }

    fn session_id(key: &str) -> &str {
        key.strip_prefix("sessions/").unwrap()
    }

    /// Marks the session as changed, which is its stamp.
    fn bump(&self, key: &str) {
        self.connection
            .execute(
                "UPDATE session SET updated = updated + 1 WHERE id = ?1",
                [Self::session_id(key)],
            )
            .unwrap();
    }

    fn insert(&mut self, key: &str, records: usize) {
        for _ in 0..records {
            self.messages += 1;
            let role = if self.messages % 2 == 1 {
                "user"
            } else {
                "assistant"
            };
            let body = format!("message {} of the made-up agent", self.messages);
            self.row(key, role, &body, None);
        }
    }

    fn row(&mut self, key: &str, role: &str, body: &str, call: Option<&str>) {
        self.connection
            .execute(
                "INSERT INTO message (session_id, role, body, call) VALUES (?1, ?2, ?3, ?4)",
                params![Self::session_id(key), role, body, call],
            )
            .unwrap();
        let id = self.connection.last_insert_rowid();
        self.expected.get_mut(key).unwrap().push(Message {
            id,
            role: role.to_string(),
            body: body.to_string(),
            call: call.map(str::to_string),
        });
    }

    /// One more message of the session: `role` says `body`. The agent's roles are `start`
    /// (the body is the directory the session started in), `user` and `assistant`.
    pub fn say(&mut self, key: &str, role: &str, body: &str) {
        self.row(key, role, body, None);
        self.bump(key);
    }

    /// One more message of the session: a call of `tool` with the arguments `args`, and what
    /// came of it.
    pub fn call(&mut self, key: &str, tool: &str, args: Value, output: &str, ok: bool) {
        let call = json!({ "tool": tool, "args": args, "output": output, "ok": ok });
        self.row(key, "tool", "", Some(&call.to_string()));
        self.bump(key);
    }

    /// One more message of the session: a tool's row whose `call` holds `call` as it is,
    /// which need not be what the agent's own tools write there.
    pub fn call_raw(&mut self, key: &str, call: &str) {
        self.row(key, "tool", "", Some(call));
        self.bump(key);
    }

    /// Gives the session's first message the body `edit` makes of the one it has.
    fn edit_first(&mut self, key: &str, edit: impl FnOnce(&str) -> String) {
        let first = &mut self.expected.get_mut(key).unwrap()[0];
        first.body = edit(&first.body);
        self.connection
            .execute(
                "UPDATE message SET body = ?2 WHERE id = ?1",
                params![first.id, first.body],
            )
            .unwrap();
        self.bump(key);
    }
}

impl Agent for DbAgent {
    fn provider(&self) -> Box<dyn Provider> {
        Box::new(DbProvider {
            export_version: self.export_version,
        })
    }

    fn transport(&self) -> HomeTransport {
        HomeTransport::new(&self.home, Vec::new())
    }

    fn home(&self) -> &Path {
        &self.home
    }

    fn add_session(&mut self, records: usize) -> String {
        self.sessions += 1;
        let key = format!("sessions/s{:04}", self.sessions);
        self.recreate(&key, records);
        key
    }

    fn append(&mut self, key: &str, records: usize) {
        self.insert(key, records);
        self.bump(key);
    }

    fn rewrite(&mut self, key: &str) {
        self.insert(key, 1);
        self.edit_first(key, |_| "an edited message".to_string());
    }

    fn rewrite_same_size(&mut self, key: &str) {
        self.edit_first(key, |body| body.replacen("message", "MESSAGE", 1));
    }

    fn truncate(&mut self, key: &str, records: usize) {
        let expected = self.expected.get_mut(key).unwrap();
        for Message { id, .. } in expected.split_off(expected.len() - records) {
            self.connection
                .execute("DELETE FROM message WHERE id = ?1", [id])
                .unwrap();
        }
        self.bump(key);
    }

    fn touch(&mut self, key: &str) {
        self.bump(key);
    }

    fn delete(&mut self, key: &str) {
        let session = Self::session_id(key);
        self.connection
            .execute("DELETE FROM message WHERE session_id = ?1", [session])
            .unwrap();
        self.connection
            .execute("DELETE FROM session WHERE id = ?1", [session])
            .unwrap();
        self.expected.remove(key);
    }

    fn recreate(&mut self, key: &str, records: usize) {
        self.connection
            .execute(
                "INSERT INTO session (id, updated) VALUES (?1, 1)",
                [Self::session_id(key)],
            )
            .unwrap();
        self.expected.insert(key.to_string(), Vec::new());
        self.insert(key, records);
    }

    fn content(&self, key: &str) -> Vec<u8> {
        self.expected[key]
            .iter()
            .flat_map(|message| {
                export_line(
                    message.id,
                    &message.role,
                    &message.body,
                    message.call.as_deref(),
                )
                .into_bytes()
            })
            .collect()
    }

    fn exchange(&mut self, key: &str, prompt: &str, command: &str, output: &str, reply: &str) {
        self.say(key, "user", prompt);
        self.call(key, "exec", json!({ "cmd": command }), output, true);
        self.say(key, "assistant", reply);
    }
}
