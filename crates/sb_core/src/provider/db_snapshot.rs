//! [`DbSnapshot`]: a scratch copy of an agent's SQLite database (PLAN §7.2). The database file
//! and its journals (the write-ahead log, or a rollback journal) are read through the
//! transport, so nothing is written at the location and the agent's own tools are not run
//! there.
//!
//! The agent may be writing while the copy is made, and SQLite writes in place: a file's size
//! and modification time need not show it. So once all three files are copied, the location
//! hashes each of them, and the copy is kept only if every hash is that of what was copied
//! and no file has appeared, gone or changed length. Otherwise it is made again. The
//! write-ahead log alone may have grown: SQLite only adds to it between checkpoints, and a
//! checkpoint changes the database file, so the copied start of the log is a state the
//! database was in.
//!
//! The provider reads the copy and defines its items as exports of the rows. Only those are
//! archived; the copy is deleted when the snapshot is dropped.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::Path;
use std::thread;
use std::time::Duration;

use tempfile::TempDir;
use toolbox_store::rusqlite::{Connection, OpenFlags};

use super::ProviderError;
use crate::hash::{Hash, Hasher};
use crate::transport::{
    Halt, Hashed, PrefixRequest, RangeFailure, RangeRequest, RangeSink, Transport, TransportError,
};

/// How many times the copy is made before a database that keeps changing is given up on.
const ATTEMPTS: u32 = 5;

/// How long to wait after the first copy that didn't hold; twice as long after each next one.
const PAUSE: Duration = Duration::from_millis(100);

/// The copy's file name, and what SQLite adds to a database's name for its write-ahead log and
/// its rollback journal: it finds them by the database's name.
const NAME: &str = "snapshot.db";
const SUFFIXES: [&str; 3] = ["", "-wal", "-journal"];

/// A consistent copy of a database, on this machine, for one pass.
pub struct DbSnapshot {
    dir: TempDir,
}

impl DbSnapshot {
    /// Copies the database at `database` into a folder of its own under `scratch`; `None` when
    /// there is no database.
    pub fn take(
        transport: &dyn Transport,
        database: &str,
        scratch: &Path,
    ) -> Result<Option<Self>, ProviderError> {
        let sources = SUFFIXES.map(|suffix| format!("{database}{suffix}"));
        let paths = sources.each_ref().map(String::as_str);
        let failed = |action| {
            move |source| ProviderError::Scratch {
                action,
                database: database.to_string(),
                source,
            }
        };
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                thread::sleep(PAUSE * 2u32.pow(attempt - 1));
            }
            /* How long each file is, 0 for one that isn't there: SQLite treats an empty journal
            as none. */
            let sizes: Vec<u64> = transport
                .stat(&paths)?
                .iter()
                .map(|stat| stat.map_or(0, |stat| stat.size))
                .collect();
            if sizes[0] == 0 {
                return Ok(None);
            }
            let dir = tempfile::tempdir_in(scratch).map_err(failed("create"))?;
            let mut requests = Vec::new();
            let mut copies = Vec::new();
            for ((path, suffix), size) in paths.iter().zip(SUFFIXES).zip(&sizes) {
                if *size > 0 {
                    requests.push(RangeRequest {
                        path: (*path).to_string(),
                        offset: 0,
                        length: *size,
                    });
                    let file = File::create(dir.path().join(format!("{NAME}{suffix}")))
                        .map_err(failed("create"))?;
                    copies.push((file, Hasher::new()));
                }
            }
            let mut sink = Scratch {
                copies,
                whole: true,
                failed: None,
            };
            match transport.read_ranges(&requests, &mut sink) {
                Err(TransportError::Halted) => {}
                read => read?,
            }
            if let Some(error) = sink.failed {
                return Err(failed("write")(error));
            }
            if !sink.whole {
                continue;
            }
            let mut copied = sink.copies.into_iter().map(|(_, hasher)| hasher.finish());
            let expected: Vec<Option<Hash>> = sizes
                .iter()
                .map(|size| (*size > 0).then(|| copied.next()).flatten())
                .collect();
            let proofs: Vec<_> = paths
                .iter()
                .zip(&sizes)
                .map(|(path, size)| PrefixRequest {
                    path: (*path).to_string(),
                    length: *size,
                })
                .collect();
            let held = transport
                .hash_prefixes(&proofs)?
                .into_iter()
                .zip(expected.iter().zip(&sizes))
                .zip(SUFFIXES)
                .all(
                    |((hashed, (expected, size)), suffix)| match (hashed, expected) {
                        (Ok(Hashed { hash, size: now }), Some(copied)) => {
                            hash == *copied && (now == *size || (suffix == "-wal" && now > *size))
                        }
                        (Ok(Hashed { size: now, .. }), None) => now == 0,
                        (Err(RangeFailure::Missing), None) => true,
                        _ => false,
                    },
                );
            if held {
                return Ok(Some(Self { dir }));
            }
        }
        Err(ProviderError::Unsettled {
            database: database.to_string(),
            attempts: ATTEMPTS,
        })
    }

    /**
     * Opens the copy for reading, with what its journal holds applied: the write-ahead log's
     * committed transactions, or the roll-back of a transaction that was under way. SQLite does
     * that to the copy as it opens it, so the connection is not read-only; it is set to refuse
     * writes.
     */
    pub fn open(&self) -> Result<Connection, ProviderError> {
        let connection = Connection::open_with_flags(
            self.dir.path().join(NAME),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.pragma_update(None, "query_only", true)?;
        Ok(connection)
    }
}

/// Writes each range to its scratch file, hashing it on the way.
struct Scratch {
    copies: Vec<(File, Hasher)>,
    /// Whether every file so far arrived whole.
    whole: bool,
    failed: Option<io::Error>,
}

impl RangeSink for Scratch {
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
        let (file, hasher) = &mut self.copies[request];
        hasher.update(bytes);
        file.write_all(bytes).map_err(|error| {
            self.failed = Some(error);
            Halt
        })
    }

    fn end(&mut self, _request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        self.whole &= result.is_ok();
        Ok(())
    }
}
