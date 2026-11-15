//! What scrollback's tests share (docs/PLAN.md §12): generated fixtures for each kind of agent,
//! in a temp dir that stands in for a location's home. Tests never read real transcripts.
//!
//! - [`ProjectsTree`]: a generated Claude Code `projects/` tree (PLAN §7.3), and [`FileAgent`],
//!   the same tree behind [`Agent`].
//! - [`DbAgent`] and [`DbProvider`]: a made-up agent that keeps its sessions in a SQLite
//!   database, with the test-only provider for it. It exists so the provider seam is tested
//!   with a second shape of source before a second real agent arrives (PLAN D8, §12).
//! - [`Agent`]: what the sync tests do to a source, so each test runs against both.
//! - [`Items`]: a session's items as a parser reads them, fetched from an [`Agent`].
//!
//! gpui: **never** (`script/check` step 3).

mod claude_code;
mod db_agent;

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;

use sb_core::provider::{ItemRange, Provider, SessionItem, SessionItems};
use sb_core::transport::{Halt, LocalTransport, RangeFailure, RangeSink};
use toolbox_store::rusqlite::Connection;

pub use claude_code::{Call, FileAgent, ProjectsTree, Records, Session, image_block};
pub use db_agent::{DbAgent, DbProvider};

/// An agent's data at a location in a temp dir, and the changes an agent makes to it. Sessions
/// are named by the key of their transcript item.
pub trait Agent {
    /// The provider that reads this agent.
    fn provider(&self) -> Box<dyn Provider>;

    /// The location the data is at.
    fn transport(&self) -> LocalTransport;

    /// A new session of `records` records. Returns its transcript's key.
    fn add_session(&mut self, records: usize) -> String;

    /// The agent goes on with the session: `records` more records at its end.
    fn append(&mut self, key: &str, records: usize);

    /// The session's first record changes, and it gains one: earlier bytes differ, and it is
    /// no shorter than before.
    fn rewrite(&mut self, key: &str);

    /// The session loses its last `records` records.
    fn truncate(&mut self, key: &str, records: usize);

    /// The session's stamp changes and its bytes don't.
    fn touch(&mut self, key: &str);

    /// The agent deletes the session.
    fn delete(&mut self, key: &str);

    /// A session of `records` records appears under the key of one that was deleted.
    fn recreate(&mut self, key: &str, records: usize);

    /// The bytes the item `key` holds at the source now.
    fn content(&self, key: &str) -> Vec<u8>;
}

/// One session's items, held in memory: what a parser reads in place of the archive.
pub struct Items {
    list: Vec<SessionItem>,
    bytes: HashMap<String, Vec<u8>>,
}

impl Items {
    /// The items of the session whose transcript is `transcript`, scanned and fetched through
    /// the agent's provider as a sync does.
    pub fn of(agent: &impl Agent, transcript: &str) -> Self {
        let transport = agent.transport();
        let provider = agent.provider();
        let mut source = provider.detect(&transport).unwrap().unwrap();
        let scanned = source.scan().unwrap();
        let session = &scanned
            .iter()
            .find(|item| item.key == transcript)
            .unwrap()
            .session;
        let mut list: Vec<SessionItem> = scanned
            .iter()
            .filter(|item| item.session == *session)
            .map(|item| SessionItem {
                key: item.key.clone(),
                kind: item.kind,
                length: item.length,
            })
            .collect();
        list.sort_by(|a, b| a.key.cmp(&b.key));
        let ranges: Vec<ItemRange> = list
            .iter()
            .map(|item| ItemRange {
                key: item.key.clone(),
                offset: 0,
                length: item.length,
            })
            .collect();
        let mut fetched = Fetched(vec![Vec::new(); list.len()]);
        source.fetch(&ranges, &mut fetched).unwrap();
        let bytes = list
            .iter()
            .map(|item| item.key.clone())
            .zip(fetched.0)
            .collect();
        Self { list, bytes }
    }
}

impl SessionItems for Items {
    fn list(&self) -> &[SessionItem] {
        &self.list
    }

    fn open(&self, key: &str, offset: u64) -> io::Result<Box<dyn Read + '_>> {
        let bytes = self.bytes.get(key).ok_or(io::ErrorKind::NotFound)?;
        Ok(Box::new(&bytes[bytes.len().min(offset as usize)..]))
    }
}

/// The bytes of each fetched range.
struct Fetched(Vec<Vec<u8>>);

impl RangeSink for Fetched {
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
        self.0[request].extend_from_slice(bytes);
        Ok(())
    }

    fn end(&mut self, _request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        result.unwrap();
        Ok(())
    }
}

const FAIL_COMMITS: &str = "fail_segment_commits";

/// Makes every commit of a segment to the catalogue of the archive in `dir` fail, as a crash
/// between a segment's rename into place and its commit would leave things (PLAN §4.1).
pub fn break_segment_commits(dir: &Path) {
    catalogue(dir)
        .execute_batch(&format!(
            "CREATE TRIGGER {FAIL_COMMITS} BEFORE INSERT ON segment
             BEGIN SELECT RAISE(ABORT, 'the test stops this commit'); END"
        ))
        .unwrap();
}

/// Undoes [`break_segment_commits`].
pub fn mend_segment_commits(dir: &Path) {
    catalogue(dir)
        .execute_batch(&format!("DROP TRIGGER {FAIL_COMMITS}"))
        .unwrap();
}

fn catalogue(dir: &Path) -> Connection {
    Connection::open(dir.join("scrollback.db")).unwrap()
}
