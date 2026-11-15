//! `DbSnapshot` (PLAN §7.2): a copy of a database its agent may be writing.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use sb_core::provider::{DbSnapshot, Hashed, ProviderError, RangeFailure};
use sb_core::transport::{
    Env, FileStat, Listing, LocalTransport, PrefixRequest, RangeRequest, RangeSink, Transport,
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

/// This machine, with an agent that does something to its database right after each copy of
/// it was read: it is told how many copies there were before.
struct BusyAgent<'a> {
    after_copy: Mutex<Box<dyn FnMut(usize) + Send + 'a>>,
    copies: AtomicUsize,
}

impl<'a> BusyAgent<'a> {
    fn new(after_copy: impl FnMut(usize) + Send + 'a) -> Self {
        Self {
            after_copy: Mutex::new(Box::new(after_copy)),
            copies: AtomicUsize::new(0),
        }
    }
}

impl Transport for BusyAgent<'_> {
    fn env(&self, names: &[&str]) -> Result<Env, TransportError> {
        LocalTransport.env(names)
    }

    fn stat(&self, paths: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        LocalTransport.stat(paths)
    }

    fn list(&self, root: &str) -> Result<Listing, TransportError> {
        LocalTransport.list(root)
    }

    fn hash_prefixes(
        &self,
        requests: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError> {
        LocalTransport.hash_prefixes(requests)
    }

    fn read_ranges(
        &self,
        requests: &[RangeRequest],
        sink: &mut dyn RangeSink,
    ) -> Result<(), TransportError> {
        LocalTransport.read_ranges(requests, sink)?;
        (self.after_copy.lock().unwrap())(self.copies.fetch_add(1, Ordering::Relaxed));
        Ok(())
    }

    fn read_small(&self, path: &str) -> Result<Option<Vec<u8>>, TransportError> {
        LocalTransport.read_small(path)
    }
}

#[test]
fn rows_still_in_the_write_ahead_log_are_in_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, _agent) = database(dir.path(), 3);
    assert!(std::fs::metadata(format!("{path}-wal")).unwrap().len() > 0);

    let snapshot = DbSnapshot::take(&LocalTransport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(rows(&snapshot), 3);

    /* The copy is in the scratch folder it was given, and goes with the snapshot. */
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 1);
    drop(snapshot);
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
}

#[test]
fn a_transaction_under_way_in_a_rollback_journal_is_not_in_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    let connection = Connection::open(&path).unwrap();
    /* A cache this small makes the transaction write its pages to the database file before it
    commits, with the pages they replace in the journal beside it. */
    connection
        .execute_batch(
            "PRAGMA journal_mode = DELETE;
             PRAGMA cache_size = 2;
             CREATE TABLE message (body TEXT NOT NULL);
             INSERT INTO message (body) VALUES ('a committed message');
             BEGIN;",
        )
        .unwrap();
    for _ in 0..200 {
        connection
            .execute(
                "INSERT INTO message (body) VALUES (?1)",
                ["an uncommitted message ".repeat(40)],
            )
            .unwrap();
    }
    let path = path.to_str().unwrap();
    assert!(std::fs::metadata(format!("{path}-journal")).unwrap().len() > 0);
    assert!(std::fs::metadata(path).unwrap().len() > 100_000);

    let snapshot = DbSnapshot::take(&LocalTransport, path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(rows(&snapshot), 1);
}

#[test]
fn rows_written_after_the_copy_wait_for_the_next_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, connection) = database(dir.path(), 3);
    /* The agent goes on adding to its log, which leaves what was copied a state it was in. */
    let transport = BusyAgent::new(move |_| write(&connection));

    let snapshot = DbSnapshot::take(&transport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(transport.copies.load(Ordering::Relaxed), 1);
    assert_eq!(rows(&snapshot), 3);
}

#[test]
fn a_database_that_changed_while_it_was_copied_is_copied_again() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, connection) = database(dir.path(), 3);
    /* After the first two copies the agent folds its log into the database file. */
    let transport = BusyAgent::new(move |copies| {
        if copies < 2 {
            write(&connection);
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
        }
    });

    let snapshot = DbSnapshot::take(&transport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(transport.copies.load(Ordering::Relaxed), 3);
    assert_eq!(rows(&snapshot), 5);
}

#[test]
fn a_change_that_leaves_size_and_time_alone_is_still_copied_again() {
    use std::io::{Seek as _, SeekFrom, Write as _};

    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, connection) = database(dir.path(), 3);
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let before = std::fs::metadata(&path).unwrap();
    /* A write in place, in the same tick of the clock as the copy: a byte of the file's
    header changes and nothing else about the file does. After the second copy it is put
    back, which is a change again; after the third nothing changes. */
    let transport = BusyAgent::new(|copies| {
        let mut file = std::fs::File::options().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(40)).unwrap();
        file.write_all(&[if copies == 0 { 0xff } else { 0 }])
            .unwrap();
        file.set_modified(before.modified().unwrap()).unwrap();
    });

    let snapshot = DbSnapshot::take(&transport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before.len());
    assert_eq!(transport.copies.load(Ordering::Relaxed), 3);
    assert_eq!(rows(&snapshot), 3);
}

#[test]
fn a_database_that_never_settles_is_given_up_on() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, connection) = database(dir.path(), 1);
    let transport = BusyAgent::new(move |_| {
        write(&connection);
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
    });

    assert!(matches!(
        DbSnapshot::take(&transport, &path, scratch.path()),
        Err(ProviderError::Unsettled { attempts: 5, .. })
    ));
    /* No attempt's copy is left behind. */
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
}

#[test]
fn a_missing_database_is_no_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("agent.db");
    assert!(
        DbSnapshot::take(&LocalTransport, missing.to_str().unwrap(), dir.path())
            .unwrap()
            .is_none()
    );
}

#[test]
fn the_source_database_is_left_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
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

    let snapshot = DbSnapshot::take(&LocalTransport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(rows(&snapshot), 3);
    drop(snapshot);
    let after = files();
    assert!(before == after);
}
