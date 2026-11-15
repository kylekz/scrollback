//! `DbSnapshot` (PLAN §7.2): a copy of a database its agent may be writing.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use sb_core::hash::Hash;
use sb_core::provider::{DbSnapshot, ProviderError};
use sb_core::transport::{
    Env, FileEntry, FileStat, LocalTransport, PrefixRequest, RangeRequest, RangeSink, Transport,
    TransportError,
};
use toolbox_store::rusqlite::Connection;

/// A database in WAL mode whose writer stays open and never checkpoints, so every row it wrote
/// is in the write-ahead log alone.
fn database(dir: &Path, rows: usize) -> (String, Connection) {
    let path = dir.join("agent.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE message (body TEXT NOT NULL);",
        )
        .unwrap();
    for _ in 0..rows {
        write(&connection);
    }
    (path.to_str().unwrap().to_string(), connection)
}

fn write(connection: &Connection) {
    connection
        .execute("INSERT INTO message (body) VALUES ('a message')", [])
        .unwrap();
}

fn rows(snapshot: &DbSnapshot) -> usize {
    snapshot
        .open()
        .unwrap()
        .query_row("SELECT count(*) FROM message", [], |row| row.get(0))
        .unwrap()
}

/// This machine, with an agent that writes a row right after each of the first `writes` copies
/// of its database were read.
struct WritingAgent {
    connection: Mutex<Connection>,
    writes: usize,
    copies: AtomicUsize,
}

impl Transport for WritingAgent {
    fn env(&self, names: &[&str]) -> Result<Env, TransportError> {
        LocalTransport::new().env(names)
    }

    fn stat(&self, paths: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        LocalTransport::new().stat(paths)
    }

    fn list(&self, root: &str) -> Result<Vec<FileEntry>, TransportError> {
        LocalTransport::new().list(root)
    }

    fn hash_prefixes(
        &self,
        requests: &[PrefixRequest],
    ) -> Result<Vec<Option<Hash>>, TransportError> {
        LocalTransport::new().hash_prefixes(requests)
    }

    fn read_ranges(
        &self,
        requests: &[RangeRequest],
        sink: &mut dyn RangeSink,
    ) -> Result<(), TransportError> {
        LocalTransport::new().read_ranges(requests, sink)?;
        if self.copies.fetch_add(1, Ordering::Relaxed) < self.writes {
            write(&self.connection.lock().unwrap());
        }
        Ok(())
    }

    fn read_small(&self, path: &str) -> Result<Option<Vec<u8>>, TransportError> {
        LocalTransport::new().read_small(path)
    }
}

#[test]
fn rows_still_in_the_write_ahead_log_are_in_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _agent) = database(dir.path(), 3);
    assert!(std::fs::metadata(format!("{path}-wal")).unwrap().len() > 0);

    let snapshot = DbSnapshot::take(&LocalTransport::new(), &path)
        .unwrap()
        .unwrap();
    assert_eq!(rows(&snapshot), 3);
}

#[test]
fn a_database_that_changed_while_it_was_copied_is_copied_again() {
    let dir = tempfile::tempdir().unwrap();
    let (path, connection) = database(dir.path(), 3);
    let transport = WritingAgent {
        connection: Mutex::new(connection),
        writes: 2,
        copies: AtomicUsize::new(0),
    };

    let snapshot = DbSnapshot::take(&transport, &path).unwrap().unwrap();
    assert_eq!(transport.copies.load(Ordering::Relaxed), 3);
    assert_eq!(rows(&snapshot), 5);
}

#[test]
fn a_database_that_never_settles_is_given_up_on() {
    let dir = tempfile::tempdir().unwrap();
    let (path, connection) = database(dir.path(), 1);
    let transport = WritingAgent {
        connection: Mutex::new(connection),
        writes: usize::MAX,
        copies: AtomicUsize::new(0),
    };

    assert!(matches!(
        DbSnapshot::take(&transport, &path),
        Err(ProviderError::Unsettled { attempts: 5, .. })
    ));
}

#[test]
fn a_missing_database_is_no_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("agent.db");
    assert!(
        DbSnapshot::take(&LocalTransport::new(), missing.to_str().unwrap())
            .unwrap()
            .is_none()
    );
}

#[test]
fn the_source_database_is_left_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _agent) = database(dir.path(), 3);
    let files = || -> BTreeMap<_, _> {
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), std::fs::read(entry.path()).unwrap())
            })
            .collect()
    };
    let before = files();

    let snapshot = DbSnapshot::take(&LocalTransport::new(), &path)
        .unwrap()
        .unwrap();
    assert_eq!(rows(&snapshot), 3);
    drop(snapshot);
    let after = files();
    assert!(before == after);
}
