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

/// A database with a rollback journal and one committed row, and its only connection.
fn journaled(dir: &Path) -> (String, Connection) {
    let path = dir.join("agent.db");
    let connection = Connection::open(&path).unwrap();
    /* A cache this small makes a transaction write its pages to the database file before it
    commits, with the pages they replace in the journal beside it. */
    connection
        .execute_batch(
            "PRAGMA journal_mode = DELETE;
             PRAGMA cache_size = 2;
             CREATE TABLE message (body TEXT NOT NULL);
             INSERT INTO message (body) VALUES ('a committed message');",
        )
        .unwrap();
    (path.to_str().unwrap().to_string(), connection)
}

/// Begins a transaction that adds 200 rows and leaves it under way, part of it in the
/// database file already.
fn begin_a_long_transaction(connection: &Connection, path: &str) {
    connection.execute_batch("BEGIN").unwrap();
    for _ in 0..200 {
        connection
            .execute(
                "INSERT INTO message (body) VALUES (?1)",
                ["an uncommitted message ".repeat(40)],
            )
            .unwrap();
    }
    assert!(std::fs::metadata(format!("{path}-journal")).unwrap().len() > 0);
    assert!(std::fs::metadata(path).unwrap().len() > 100_000);
}

/// Something the location has just done for a snapshot.
enum Step<'a> {
    /// Looked up the files' sizes.
    Stat,
    /// Read the files.
    Copy,
    /// Hashed the file at this path.
    Proof(&'a str),
}

/// What the agent does after a step: it is told the step, and how many attempts at the
/// snapshot there were before this one.
type After<'a> = Box<dyn FnMut(Step<'_>, usize) + Send + 'a>;

/// This machine, with an agent that does something to its database between the steps of a
/// snapshot.
struct BusyAgent<'a> {
    after: Mutex<After<'a>>,
    attempts: AtomicUsize,
}

impl<'a> BusyAgent<'a> {
    fn new(after: impl FnMut(Step<'_>, usize) + Send + 'a) -> Self {
        Self {
            after: Mutex::new(Box::new(after)),
            attempts: AtomicUsize::new(0),
        }
    }

    fn after(&self, step: Step<'_>) {
        (self.after.lock().unwrap())(step, self.attempts() - 1);
    }

    /// How many times the snapshot was begun.
    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Relaxed)
    }
}

impl Transport for BusyAgent<'_> {
    fn env(&self, names: &[&str]) -> Result<Env, TransportError> {
        LocalTransport.env(names)
    }

    fn stat(&self, paths: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        let stat = LocalTransport.stat(paths)?;
        self.attempts.fetch_add(1, Ordering::Relaxed);
        self.after(Step::Stat);
        Ok(stat)
    }

    fn list(&self, root: &str) -> Result<Listing, TransportError> {
        LocalTransport.list(root)
    }

    fn hash_prefixes(
        &self,
        requests: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError> {
        let mut hashed = Vec::new();
        for request in requests {
            hashed.extend(LocalTransport.hash_prefixes(std::slice::from_ref(request))?);
            self.after(Step::Proof(&request.path));
        }
        Ok(hashed)
    }

    fn read_ranges(
        &self,
        requests: &[RangeRequest],
        sink: &mut dyn RangeSink,
    ) -> Result<(), TransportError> {
        LocalTransport.read_ranges(requests, sink)?;
        self.after(Step::Copy);
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
    let (path, connection) = journaled(dir.path());
    begin_a_long_transaction(&connection, &path);

    let snapshot = DbSnapshot::take(&LocalTransport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(rows(&snapshot), 1);
}

#[test]
fn a_log_that_grew_after_it_was_copied_is_copied_again() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, connection) = database(dir.path(), 3);
    /* The copied start of the log is still what the log starts with, and that is not enough:
    a checkpoint could have put the new row in the database file. */
    let transport = BusyAgent::new(move |step, attempt| {
        if matches!(step, Step::Copy) && attempt == 0 {
            write(&connection);
        }
    });

    let snapshot = DbSnapshot::take(&transport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(transport.attempts(), 2);
    assert_eq!(rows(&snapshot), 4);
}

/// What the tables `a` and `b` hold.
fn state(connection: &Connection) -> (i64, i64) {
    connection
        .query_row("SELECT (SELECT n FROM a), (SELECT n FROM b)", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
}

#[test]
fn a_commit_and_a_checkpoint_between_the_stat_and_the_copy_are_not_half_copied() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    let path = path.to_str().unwrap();
    let agent = Connection::open(path).unwrap();
    agent
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE a (n INTEGER NOT NULL);
             CREATE TABLE b (n INTEGER NOT NULL);
             INSERT INTO a VALUES (0);
             INSERT INTO b VALUES (0);
             PRAGMA wal_checkpoint(TRUNCATE);
             UPDATE a SET n = 1;",
        )
        .unwrap();
    /* The database has been (0, 0) and is (1, 0). Once its files' sizes are known, a
    transaction makes it (2, 2) and a checkpoint puts that in the database file: the log, as
    long as it was, no longer says what the file holds. */
    let writer = Connection::open(path).unwrap();
    let transport = BusyAgent::new(move |step, attempt| {
        if matches!(step, Step::Stat) && attempt == 0 {
            writer
                .execute_batch(
                    "PRAGMA wal_autocheckpoint = 0;
                     BEGIN; UPDATE a SET n = 2; UPDATE b SET n = 2; COMMIT;
                     PRAGMA wal_checkpoint(PASSIVE);",
                )
                .unwrap();
        }
    });

    let snapshot = DbSnapshot::take(&transport, path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(transport.attempts(), 2);
    assert_eq!(state(&snapshot.open().unwrap()), (2, 2));
    assert_eq!(state(&agent), (2, 2));
}

#[test]
fn a_log_that_appears_after_the_stat_is_not_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, connection) = database(dir.path(), 3);
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    assert_eq!(std::fs::metadata(format!("{path}-wal")).unwrap().len(), 0);
    let transport = BusyAgent::new(move |step, attempt| {
        if matches!(step, Step::Stat) && attempt == 0 {
            write(&connection);
        }
    });

    let snapshot = DbSnapshot::take(&transport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(transport.attempts(), 2);
    assert_eq!(rows(&snapshot), 4);
}

#[test]
fn a_log_that_came_and_went_around_the_database_file_is_noticed() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, connection) = database(dir.path(), 3);
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    /* No log when the sizes are looked up, and none after the database file is hashed: one
    was there in between, and went into the file. */
    let database = path.clone();
    let transport = BusyAgent::new(move |step, attempt| match step {
        Step::Stat if attempt == 0 => write(&connection),
        Step::Proof(hashed) if attempt == 0 && hashed == database => connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap(),
        _ => {}
    });

    let snapshot = DbSnapshot::take(&transport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(transport.attempts(), 2);
    assert_eq!(rows(&snapshot), 4);
}

#[test]
fn a_journal_that_appears_after_the_stat_is_not_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, connection) = journaled(dir.path());
    let database = path.clone();
    let transport = BusyAgent::new(move |step, attempt| {
        if matches!(step, Step::Stat) && attempt == 0 {
            begin_a_long_transaction(&connection, &database);
        }
    });

    /* The first copy has pages of the transaction and no journal to take them back with. */
    let snapshot = DbSnapshot::take(&transport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(transport.attempts(), 2);
    assert_eq!(rows(&snapshot), 1);
}

#[test]
fn a_transaction_that_ends_right_after_the_database_file_is_hashed_is_noticed() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, connection) = journaled(dir.path());
    /* No journal when the sizes are looked up, and none after the database file is hashed,
    yet the file that was copied and hashed was part-way through a transaction. */
    let database = path.clone();
    let transport = BusyAgent::new(move |step, attempt| match step {
        Step::Stat if attempt == 0 => begin_a_long_transaction(&connection, &database),
        Step::Proof(hashed) if attempt == 0 && hashed == database => {
            connection.execute_batch("COMMIT").unwrap();
        }
        _ => {}
    });

    let snapshot = DbSnapshot::take(&transport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(transport.attempts(), 2);
    assert_eq!(rows(&snapshot), 201);
}

#[test]
fn a_database_that_changed_while_it_was_copied_is_copied_again() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, connection) = database(dir.path(), 3);
    /* After the first two copies the agent folds its log into the database file. */
    let transport = BusyAgent::new(move |step, attempt| {
        if matches!(step, Step::Copy) && attempt < 2 {
            write(&connection);
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
        }
    });

    let snapshot = DbSnapshot::take(&transport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(transport.attempts(), 3);
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
    let transport = BusyAgent::new(|step, attempt| {
        if matches!(step, Step::Copy) {
            let mut file = std::fs::File::options().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(40)).unwrap();
            file.write_all(&[if attempt == 0 { 0xff } else { 0 }])
                .unwrap();
            file.set_modified(before.modified().unwrap()).unwrap();
        }
    });

    let snapshot = DbSnapshot::take(&transport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before.len());
    assert_eq!(transport.attempts(), 3);
    assert_eq!(rows(&snapshot), 3);
}

#[test]
fn a_database_file_that_grew_after_it_was_copied_is_copied_again() {
    use std::io::Write as _;

    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, _agent) = database(dir.path(), 3);
    /* What was copied is still how the file starts. */
    let transport = BusyAgent::new(|step, attempt| {
        if matches!(step, Step::Copy) && attempt == 0 {
            let mut file = std::fs::File::options().append(true).open(&path).unwrap();
            file.write_all(&[0; 4096]).unwrap();
        }
    });

    let snapshot = DbSnapshot::take(&transport, &path, scratch.path())
        .unwrap()
        .unwrap();
    assert_eq!(transport.attempts(), 2);
    assert_eq!(rows(&snapshot), 3);
}

#[test]
fn a_database_that_never_settles_is_given_up_on() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (path, connection) = database(dir.path(), 1);
    let transport = BusyAgent::new(move |step, _| {
        if matches!(step, Step::Copy) {
            write(&connection);
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
        }
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
