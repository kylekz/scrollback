//! [`DbSnapshot`]: a scratch copy of an agent's SQLite database (PLAN §7.2). The database file
//! and its journals (the write-ahead log, or a rollback journal) are read through the
//! transport, so nothing is written or locked at the location and the agent's own tools are
//! not run there.
//!
//! The agent may be writing while the copy is made, and SQLite writes in place: a file's size
//! and modification time need not show it. So once the three files are copied, the location
//! hashes, one after another, the journals, the database file, and the journals again. The
//! copy is kept only if each hash and length is that of what was copied, a file that was not
//! there still being absent or empty. Otherwise it is made again.
//!
//! Why that is a state the database was in, at the instant its file was hashed:
//! - A log that reads the same on both sides of that instant held the same commits at it: it
//!   changes only by gaining frames or by being begun anew, under a header that was never
//!   used before, so it cannot change and change back. A checkpoint moves into the database
//!   file only pages the log holds, so however far one had got, the file with the log is the
//!   database as of the log's last commit.
//! - A rollback journal that reads the same on both sides is one transaction's, since each
//!   starts its journal under a new random number, and it gained nothing in between. So it
//!   holds what was in every page that transaction had written into the file by then, and
//!   rolling back gives the database as it was before the transaction.
//! - With neither on either side, no writer was part-way through the file, unless one began
//!   and ended within that one request. It then began after the copy was read, so the copy
//!   has nothing it wrote, and a file that has some of it hashes differently unless what was
//!   written is byte for byte what was there.
//!
//! No journal may have grown. A log that only gained commits looks harmless, but a checkpoint
//! may have moved them into the file that was hashed, and they are not in the copied log.
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

/// The files in the order they are hashed to prove the copy, by their place in [`SUFFIXES`]:
/// the journals on both sides of the database file.
const PROOFS: [usize; 5] = [1, 2, 0, 1, 2];

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
                arrived: true,
                failed: None,
            };
            match transport.read_ranges(&requests, &mut sink) {
                Err(TransportError::Halted) => {}
                read => read?,
            }
            if let Some(error) = sink.failed {
                return Err(failed("write")(error));
            }
            if !sink.arrived {
                continue;
            }
            let mut hashes = sink.copies.into_iter().map(|(_, hasher)| hasher.finish());
            /* The hash of each file's copy; `None` for a file there was nothing to copy of. */
            let copied: Vec<Option<Hash>> = sizes
                .iter()
                .map(|size| (*size > 0).then(|| hashes.next()).flatten())
                .collect();
            let proofs = PROOFS.map(|file| PrefixRequest {
                path: paths[file].to_string(),
                length: sizes[file],
            });
            let now = transport.hash_prefixes(&proofs)?;
            let held = now.len() == PROOFS.len()
                && PROOFS
                    .iter()
                    .zip(now)
                    .all(|(file, now)| match (now, copied[*file]) {
                        (Ok(now), Some(copied)) => now.hash == copied && now.size == sizes[*file],
                        (Ok(Hashed { size: 0, .. }) | Err(RangeFailure::Missing), None) => true,
                        _ => false,
                    });
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
    /// Whether every range so far arrived in full.
    arrived: bool,
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
        self.arrived &= result.is_ok();
        Ok(())
    }
}
