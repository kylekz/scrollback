//! Transports (PLAN §6.1): a read-only view of a location's file system, and nothing more. A
//! transport knows nothing about agents; providers are its only callers (PLAN §7).
//!
//! No operation writes, and none runs an agent's own tools (PLAN §2.3, invariant 5). Paths are
//! the location's own, absolute, as text; a relative path a transport returns uses `/`.
//!
//! A location is read while its agent writes to it, and nothing is locked, so what a read
//! returns may be a mixture of two states of a file. A file's size and modification time
//! can't rule that out (a writer can leave both as they were), so the caller proves what it
//! read: [`Transport::hash_prefixes`] has the location hash the file's own start, and gives
//! the file's size with the hash ([`Hashed`]).
//!
//! [`LocalTransport`] reads this machine with `std::fs`. The shell transport for WSL and SSH
//! comes with phase 5 (PLAN §13).

mod local;

use std::collections::HashMap;
use std::io;

use thiserror::Error;

use crate::hash::Hash;

pub use local::LocalTransport;

/// A location's home directory and the environment variables that were asked for and are set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Env {
    pub home: String,
    pub vars: HashMap<String, String>,
}

/// A file's size and modification time, the latter in nanoseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStat {
    pub size: u64,
    pub mtime_ns: i64,
}

/// A file under a listed directory, by its path relative to that directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub stat: FileStat,
}

/// Something under a listed directory that is not among the listing's files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unlisted {
    /// The location's path of it, as far as it can be shown.
    pub path: String,
    pub reason: String,
}

/// What [`Transport::list`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    pub files: Vec<FileEntry>,
    /// What could not be listed: a folder or a file that could not be read, or was gone when
    /// it was looked at. While there is any, the files are not all there is.
    pub unlisted: Vec<Unlisted>,
    /// What was left out on purpose: symbolic links, and whatever is neither a file nor a
    /// folder.
    pub skipped: Vec<Unlisted>,
    /// The location's clock when the listing began, in nanoseconds since the Unix epoch: what
    /// the files' modification times are measured against.
    pub clock_ns: i64,
}

/// The first `length` bytes of the file at `path`, to be hashed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixRequest {
    pub path: String,
    pub length: u64,
}

/// A hashed prefix, and the whole size of the file it was read from once it was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hashed {
    pub hash: Hash,
    pub size: u64,
}

/// The bytes `[offset, offset + length)` of the file at `path`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeRequest {
    pub path: String,
    pub offset: u64,
    pub length: u64,
}

/// What a sink returns to stop the stream it is being fed: the pass was cancelled, or the sink
/// itself failed and keeps the reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Halt;

/// Why one requested range or prefix could not be read. It fails that request only.
#[derive(Debug, Error)]
pub enum RangeFailure {
    #[error("it is no longer there")]
    Missing,
    #[error("the source ended after {got} of {expected} bytes")]
    Short { expected: u64, got: u64 },
    #[error("{0}")]
    Unreadable(String),
}

/**
 * Where streamed ranges go. Ranges arrive one at a time, in the order they were requested:
 * `data` any number of times for a request, then `end` once. A range that ends with a failure
 * may have delivered some of its bytes first; the sink discards them.
 */
pub trait RangeSink {
    /// The next bytes of the request at index `request`.
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt>;

    /// The request at index `request` is over: all its bytes arrived, or it failed.
    fn end(&mut self, request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt>;
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("could not {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("this user has no home directory")]
    NoHome,
    /// A [`RangeSink`] stopped the stream. The sink knows why.
    #[error("the stream was stopped by its reader")]
    Halted,
}

/// A read-only view of a location's file system (the module docs).
pub trait Transport: Send + Sync {
    /// The location's home directory, and those of `names` that are set in its environment.
    fn env(&self, names: &[&str]) -> Result<Env, TransportError>;

    /// Each path's size and modification time, or `None` where nothing is there.
    fn stat(&self, paths: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError>;

    /// Every regular file under the directory `root`, however deep. Symbolic links are not
    /// followed. What can't be listed, `root` itself when it is missing, and what is left out
    /// are reported in the listing and do not fail it.
    fn list(&self, root: &str) -> Result<Listing, TransportError>;

    /// The SHA-256 of each requested prefix, hashed one after another in the order asked. A
    /// file that is missing, unreadable or shorter than its prefix fails that request only.
    fn hash_prefixes(
        &self,
        requests: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError>;

    /// Streams each requested range into `sink`. A file that is missing, unreadable or shorter
    /// than its range fails that range only.
    fn read_ranges(
        &self,
        requests: &[RangeRequest],
        sink: &mut dyn RangeSink,
    ) -> Result<(), TransportError>;

    /// One small file whole (a settings file), or `None` when it is missing.
    fn read_small(&self, path: &str) -> Result<Option<Vec<u8>>, TransportError>;
}
