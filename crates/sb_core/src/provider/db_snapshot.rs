//! [`DbSnapshot`]: a scratch copy of an agent's SQLite database (PLAN §7.2). The database file
//! and its write-ahead log are read through the transport, so nothing is written at the
//! location and the agent's own tools are not run there. The agent may be writing while the
//! copy is made, so both files are compared before and after, and copied again if either
//! changed.
//!
//! The provider reads the copy and defines its items as exports of the rows. Only those are
//! archived; the copy is deleted when the snapshot is dropped.

use std::fs::File;
use std::io::{self, Write as _};

use tempfile::TempDir;
use toolbox_store::rusqlite::{Connection, OpenFlags};

use super::ProviderError;
use crate::transport::{Halt, RangeFailure, RangeRequest, RangeSink, Transport, TransportError};

/// How many times the copy is made before a database that keeps changing is given up on.
const ATTEMPTS: usize = 5;

/// The copy's file names: SQLite finds the log by the database's name.
const NAMES: [&str; 2] = ["snapshot.db", "snapshot.db-wal"];

/// A consistent copy of a database, on this machine, for one pass.
pub struct DbSnapshot {
    dir: TempDir,
}

impl DbSnapshot {
    /// Copies the database at `database`; `None` when there is none.
    pub fn take(transport: &dyn Transport, database: &str) -> Result<Option<Self>, ProviderError> {
        let wal = format!("{database}-wal");
        let paths = [database, wal.as_str()];
        let scratch = |action| {
            move |source| ProviderError::Scratch {
                action,
                database: database.to_string(),
                source,
            }
        };
        for _ in 0..ATTEMPTS {
            let before = transport.stat(&paths)?;
            if before[0].is_none() {
                return Ok(None);
            }
            let dir = tempfile::tempdir().map_err(scratch("create"))?;
            let mut requests = Vec::new();
            let mut files = Vec::new();
            for ((path, name), stat) in paths.iter().zip(NAMES).zip(&before) {
                if let Some(stat) = stat.filter(|stat| stat.size > 0) {
                    requests.push(RangeRequest {
                        path: (*path).to_string(),
                        offset: 0,
                        length: stat.size,
                    });
                    files.push(File::create(dir.path().join(name)).map_err(scratch("create"))?);
                }
            }
            let mut sink = Copy {
                files,
                complete: true,
                failed: None,
            };
            match transport.read_ranges(&requests, &mut sink) {
                Err(TransportError::Halted) => {}
                read => read?,
            }
            if let Some(error) = sink.failed {
                return Err(scratch("write")(error));
            }
            if sink.complete && transport.stat(&paths)? == before {
                return Ok(Some(Self { dir }));
            }
        }
        Err(ProviderError::Unsettled {
            database: database.to_string(),
            attempts: ATTEMPTS,
        })
    }

    /// Opens the copy read-only, with what its log holds applied.
    pub fn open(&self) -> Result<Connection, ProviderError> {
        Ok(Connection::open_with_flags(
            self.dir.path().join(NAMES[0]),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?)
    }
}

/// Writes each range to its scratch file.
struct Copy {
    files: Vec<File>,
    /// Whether every range so far arrived whole. A file that shrank mid-copy did not.
    complete: bool,
    failed: Option<io::Error>,
}

impl RangeSink for Copy {
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
        self.files[request].write_all(bytes).map_err(|error| {
            self.failed = Some(error);
            Halt
        })
    }

    fn end(&mut self, _request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        self.complete &= result.is_ok();
        Ok(())
    }
}
