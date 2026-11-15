//! Providers (PLAN §7): the [`Provider`] trait, its helpers for the two common shapes of source
//! ([`FileTree`], [`DbSnapshot`]) and the list of providers. Each agent's provider is a module
//! under this one, and is the only code that knows that agent (PLAN D8).
//!
//! A provider is asked once per location whether its agent is there ([`Provider::detect`]).
//! Where it is, it hands back a [`Source`]: the agent's data at that location for one sync pass,
//! through which the engine scans, hashes and fetches without knowing what an item is.
//!
//! Indexing, parsing and the tool map (PLAN §7.1) join [`Provider`] with phase 2; they read the
//! archive, never a location.

pub mod claude_code;
mod db_snapshot;
mod file_tree;

use thiserror::Error;
use toolbox_store::rusqlite;

use crate::hash::Hash;
use crate::transport::{RangeSink, Transport, TransportError};

pub use db_snapshot::DbSnapshot;
pub use file_tree::{FileClass, FileTree};

/// The providers scrollback ships. Every location is asked by each of them (PLAN §7.1).
pub fn builtin() -> Vec<Box<dyn Provider>> {
    vec![Box::new(claude_code::Agent)]
}

/// What an item is to its session (PLAN §4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// A session's main transcript.
    Transcript,
    /// A subagent's transcript.
    Subagent,
    /// Tool output saved outside the transcript.
    ToolResult,
    /// Anything else the agent keeps: titles, metadata, memory.
    Sidecar,
}

/// One item the agent has at a location, as a scan finds it (PLAN §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedItem {
    /// The provider's name for the item, stable across passes: for a file, its path relative to
    /// the agent's root.
    pub key: String,
    pub kind: ItemKind,
    /// The provider's id of the session the item belongs to, if it belongs to one.
    pub session: Option<String>,
    pub length: u64,
    /// Changes whenever the item's bytes do. With `length` it is how an unchanged item is
    /// skipped; the engine only compares it.
    pub stamp: String,
    /// Whether the item is a sequence of lines being appended to. Such an item is stored up to
    /// its last newline while it is being written (PLAN §4.2).
    pub lines: bool,
    /// The version of the provider's export format, when the item is an export of a database's
    /// records (PLAN §7.2). A change of it starts a new version of the item.
    pub export_version: Option<u32>,
}

/// The first `length` bytes of the item `key`, to be hashed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemPrefix {
    pub key: String,
    pub length: u64,
}

/// The bytes `[offset, offset + length)` of the item `key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemRange {
    pub key: String,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// A snapshot of the agent's database could not be read.
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    /// The agent's database changed on every attempt to copy it.
    #[error("{database} kept changing while it was copied ({attempts} attempts)")]
    Unsettled { database: String, attempts: usize },
    #[error("could not {action} the scratch copy of {database}: {source}")]
    Scratch {
        action: &'static str,
        database: String,
        #[source]
        source: std::io::Error,
    },
}

/// The code for one agent (PLAN §7.1). Everything specific to the agent is behind this trait.
pub trait Provider: Send + Sync {
    /// The provider's fixed identity (PLAN §2.1). It names the provider in the catalogue and its
    /// folder under each location in the archive, so it never changes.
    fn id(&self) -> &'static str;

    /// Whether the agent is present at the location behind `transport`; where it is, its data
    /// there for one pass.
    fn detect<'a>(
        &self,
        transport: &'a dyn Transport,
    ) -> Result<Option<Box<dyn Source + 'a>>, ProviderError>;
}

/**
 * An agent's data at one location, for one sync pass (PLAN §5, §7.1). These are the only
 * operations of a provider that touch a location. Whatever the source keeps for the pass (a
 * scratch copy of a database) goes when it is dropped.
 */
pub trait Source {
    /// Every item the agent has here.
    fn scan(&mut self) -> Result<Vec<ScannedItem>, ProviderError>;

    /// The SHA-256 of each requested prefix, or `None` where the item is gone or shorter.
    fn hash_prefixes(
        &mut self,
        requests: &[ItemPrefix],
    ) -> Result<Vec<Option<Hash>>, ProviderError>;

    /// Streams each requested range into `sink`. An item that is gone or shorter than its range
    /// fails that range only.
    fn fetch(
        &mut self,
        requests: &[ItemRange],
        sink: &mut dyn RangeSink,
    ) -> Result<(), ProviderError>;

    /// How many days the agent keeps transcripts here before deleting them itself, if it says.
    fn retention_days(&mut self) -> Result<Option<u32>, ProviderError> {
        Ok(None)
    }
}
