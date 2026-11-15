//! A made-up agent that keeps its sessions in a SQLite database, and the test-only provider
//! for it (PLAN §7.2, §12). The provider copies the database through `DbSnapshot` and archives
//! one export per session: its messages, one JSON line each, in the order they were inserted.

use std::collections::HashMap;
use std::fs;

use sb_core::hash::Hash;
use sb_core::provider::{
    DbSnapshot, ItemKind, ItemPrefix, ItemRange, Provider, ProviderError, ScannedItem, Source,
};
use sb_core::transport::{
    Halt, LocalTransport, RangeFailure, RangeSink, Transport, TransportError,
};
use serde_json::json;
use tempfile::TempDir;
use toolbox_store::rusqlite::{Connection, params};

use crate::Agent;

/// Where the agent keeps its database, under the location's home.
const DATABASE: &str = ".dbagent/sessions.db";

/// One message as a line of a session's export.
fn export_line(id: i64, role: &str, body: &str) -> String {
    json!({ "id": id, "role": role, "body": body }).to_string() + "\n"
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
    fn id(&self) -> &'static str {
        "db-agent"
    }

    fn detect<'a>(
        &self,
        transport: &'a dyn Transport,
    ) -> Result<Option<Box<dyn Source + 'a>>, ProviderError> {
        let database = format!("{}/{DATABASE}", transport.env(&[])?.home);
        let present = transport.stat(&[&database])?[0].is_some();
        Ok(present.then(|| {
            Box::new(Exports {
                transport,
                database,
                export_version: self.export_version,
                sessions: HashMap::new(),
            }) as Box<dyn Source>
        }))
    }
}

/// The agent's sessions at one location, as exports.
struct Exports<'a> {
    transport: &'a dyn Transport,
    database: String,
    export_version: u32,
    /// Each session's export by its item key, as the scan's snapshot had it.
    sessions: HashMap<String, Vec<u8>>,
}

impl Exports<'_> {
    fn range(&self, key: &str, offset: u64, length: u64) -> Option<&[u8]> {
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(usize::try_from(length).ok()?)?;
        self.sessions.get(key)?.get(start..end)
    }
}

impl Source for Exports<'_> {
    fn scan(&mut self) -> Result<Vec<ScannedItem>, ProviderError> {
        self.sessions.clear();
        let Some(snapshot) = DbSnapshot::take(self.transport, &self.database)? else {
            return Ok(Vec::new());
        };
        let connection = snapshot.open()?;
        let sessions: Vec<(String, i64)> = connection
            .prepare("SELECT id, updated FROM session ORDER BY id")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut messages = connection
            .prepare("SELECT id, role, body FROM message WHERE session_id = ?1 ORDER BY id")?;
        let mut items = Vec::new();
        for (session, updated) in sessions {
            let export: String = messages
                .query_map([&session], |row| {
                    Ok(export_line(
                        row.get(0)?,
                        &row.get::<_, String>(1)?,
                        &row.get::<_, String>(2)?,
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
                lines: false,
                export_version: Some(self.export_version),
            });
            self.sessions.insert(key, export.into_bytes());
        }
        Ok(items)
    }

    fn hash_prefixes(
        &mut self,
        requests: &[ItemPrefix],
    ) -> Result<Vec<Option<Hash>>, ProviderError> {
        Ok(requests
            .iter()
            .map(|request| self.range(&request.key, 0, request.length).map(Hash::of))
            .collect())
    }

    fn fetch(
        &mut self,
        requests: &[ItemRange],
        sink: &mut dyn RangeSink,
    ) -> Result<(), ProviderError> {
        let halted = |Halt| ProviderError::Transport(TransportError::Halted);
        for (index, request) in requests.iter().enumerate() {
            let result = match self.range(&request.key, request.offset, request.length) {
                Some(range) => {
                    for chunk in range.chunks(1024) {
                        sink.data(index, chunk).map_err(halted)?;
                    }
                    Ok(())
                }
                None => Err(RangeFailure::Unreadable(
                    "the session has no such range".to_string(),
                )),
            };
            sink.end(index, result).map_err(halted)?;
        }
        Ok(())
    }
}

/// The agent's database in a temp dir that stands in for a location's home.
pub struct DbAgent {
    home: TempDir,
    /// Kept open, as a running agent's is: what it wrote last is only in the write-ahead log.
    connection: Connection,
    /// What each session's export should be, by item key: its messages' lines.
    expected: HashMap<String, Vec<(i64, String)>>,
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
    pub fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let database = home.path().join(DATABASE);
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
                     body TEXT NOT NULL
                 );",
            )
            .unwrap();
        Self {
            home,
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
            self.connection
                .execute(
                    "INSERT INTO message (session_id, role, body) VALUES (?1, ?2, ?3)",
                    params![Self::session_id(key), role, body],
                )
                .unwrap();
            let id = self.connection.last_insert_rowid();
            self.expected
                .get_mut(key)
                .unwrap()
                .push((id, export_line(id, role, &body)));
        }
    }
}

impl Agent for DbAgent {
    fn provider(&self) -> Box<dyn Provider> {
        Box::new(DbProvider {
            export_version: self.export_version,
        })
    }

    fn transport(&self) -> LocalTransport {
        LocalTransport::with_env(self.home.path().to_str().unwrap(), [])
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
        let first = &mut self.expected.get_mut(key).unwrap()[0];
        let body = "an edited message";
        self.connection
            .execute(
                "UPDATE message SET role = 'user', body = ?2 WHERE id = ?1",
                params![first.0, body],
            )
            .unwrap();
        first.1 = export_line(first.0, "user", body);
        self.insert(key, 1);
        self.bump(key);
    }

    fn truncate(&mut self, key: &str, records: usize) {
        let expected = self.expected.get_mut(key).unwrap();
        for (id, _) in expected.split_off(expected.len() - records) {
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
            .flat_map(|(_, line)| line.bytes())
            .collect()
    }
}
