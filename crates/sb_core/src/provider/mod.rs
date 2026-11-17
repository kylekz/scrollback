//! Providers (PLAN §7): the [`Provider`] trait, its helpers for the two common shapes of source
//! ([`FileTree`], [`DbSnapshot`]) and the list of providers. Each agent's provider is a module
//! under this one, and is the only code that knows that agent (PLAN D8).
//!
//! A provider is asked once per location whether its agent is there ([`Provider::detect`]).
//! Where it is, it hands back a [`Source`]: the agent's data at that location for one sync pass,
//! through which the engine scans, hashes and fetches without knowing what an item is.
//!
//! Parsing (PLAN §7.1) is on [`Provider`] itself: it reads a session's archived items
//! ([`SessionItems`]), never a location, and maps the agent's tools onto the common kinds as it
//! goes. What the index holds is derived from the parsed transcript, the same way for every
//! agent ([`crate::transcript::index`]).

mod claude_code;
mod db_snapshot;
mod file_tree;
mod session;

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use thiserror::Error;
use toolbox_store::rusqlite;

use crate::transcript::{CopyOf, Outline, Subagent, Transcript};

/**
 * What the records an app keeps beside its copies say of where one conversation ran (PLAN
 * §7.7), in the app's own words: nothing here is checked against a location.
 */
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyRecord {
    /// The agent's id for the conversation, as [`crate::transcript::Facts::conversation`].
    pub conversation: String,
    /// The host it ran on over SSH, as the app handed it to `ssh`.
    pub host: Option<String>,
    /// The path its transcript had on that machine.
    pub path: Option<String>,
    /// The key the provider gives the transcript at `path` where it ran: what the original's
    /// item is keyed by, if it is archived from there.
    pub key: Option<String>,
}
use crate::transport::{Halt, RangeSink, Transport, TransportError};

pub use crate::transport::{Hashed, RangeFailure, Unlisted};
pub use db_snapshot::DbSnapshot;
pub use file_tree::{FileClass, FileTree};
pub use session::{Audit, SessionItem, SessionItems};

/// The providers scrollback ships. Every location is asked by each of them (PLAN §7.1).
pub fn builtin() -> Vec<Box<dyn Provider>> {
    vec![Box::new(claude_code::Agent)]
}

/// A provider's fixed identity (PLAN §2.1). It names the provider in the catalogue and its
/// folder under each location in the archive, so it never changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProviderId(&'static str);

impl ProviderId {
    /**
     * The id `id`: lowercase ASCII letters, digits and `-`. It is a folder name, and two ids
     * that differed only in case would share a folder on Windows and macOS. Checked when the
     * constant is built, so a bad id fails the build: `const ID: ProviderId =
     * ProviderId::new("some-agent");`.
     */
    pub const fn new(id: &'static str) -> Self {
        let bytes = id.as_bytes();
        assert!(!bytes.is_empty(), "a provider id is not empty");
        let mut i = 0;
        while i < bytes.len() {
            assert!(
                bytes[i].is_ascii_lowercase() || bytes[i].is_ascii_digit() || bytes[i] == b'-',
                "a provider id is lowercase ASCII letters, digits and -"
            );
            i += 1;
        }
        Self(id)
    }

    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// How an agent is shown (PLAN §7.6): the views name no agent, and ask its provider for this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Display {
    /// The agent's name, as its makers write it.
    pub name: &'static str,
    /// The agent's mark; an agent without one is shown with a generic glyph.
    pub mark: Option<Mark>,
}

/// An agent's one-colour mark (DESIGN §1.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// The mark as an SVG document. It is drawn in one colour, whatever its own.
    pub svg: &'static str,
    /// The mark's colour as `0xRRGGBB`, on a dark surface and on a light one.
    pub dark: u32,
    pub light: u32,
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
    /// What an agent's app keeps about its sessions as a whole, of no one session: records
    /// the provider reads for where the copies the app keeps ran ([`Provider::copy_records`]).
    Register,
}

impl ItemKind {
    /// Whether the item is a conversation: a session's own transcript or a subagent's. Those
    /// are what a parser reads as conversations, what makes a group of items a session, and
    /// the only items of which a version can be held back (PLAN §4.2).
    pub fn is_transcript(self) -> bool {
        matches!(self, Self::Transcript | Self::Subagent)
    }
}

/// One item the agent has at a location, as a scan finds it (PLAN §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedItem {
    /// The provider's name for the item, stable across passes: for a file, its path relative to
    /// the agent's root. A key names one item for good: once the owner deletes the session it
    /// was of, sync skips the key (PLAN §4.5), so a provider that gives it to other content
    /// later has that content skipped too, until the owner removes the session's tombstone.
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

/// What a scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub items: Vec<ScannedItem>,
    /// What the agent has here that could not be scanned. While there is any, the items are
    /// not all the agent has: an archived item that is not among them may still be there.
    pub unlisted: Vec<Unlisted>,
    /// What the provider leaves out on purpose, such as symbolic links.
    pub skipped: Vec<Unlisted>,
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
    /// The sink stopped the stream. The sink knows why.
    #[error("the fetch was stopped by its reader")]
    Halted,
    /// A snapshot of the agent's database could not be read.
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    /// The agent's database changed on every attempt to copy it.
    #[error("{database} kept changing while it was copied ({attempts} attempts)")]
    Unsettled { database: String, attempts: u32 },
    #[error("could not {action} the scratch copy of {database}: {source}")]
    Scratch {
        action: &'static str,
        database: String,
        #[source]
        source: std::io::Error,
    },
}

impl From<Halt> for ProviderError {
    fn from(_: Halt) -> Self {
        Self::Halted
    }
}

/// The code for one agent (PLAN §7.1). Everything specific to the agent is behind this trait.
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;

    /// How the agent is shown. One that brings nothing is shown by its id.
    fn display(&self) -> Display {
        Display {
            name: self.id().as_str(),
            mark: None,
        }
    }

    /// The name to show for the model `model`, which is as the agent recorded it.
    fn model_name(&self, model: &str) -> String {
        model.to_string()
    }

    /// The short label an activity row gives a call of the tool `name` (DESIGN §4.2.3: `bash`,
    /// `todos`, `fetch`); `None` for a tool the provider doesn't label. The call keeps the
    /// tool's own name either way, and the reader decides how to show one without a label.
    fn tool_label(&self, _name: &str) -> Option<&'static str> {
        None
    }

    /**
     * Whether the agent is present at the location behind `transport`; where it is, its data
     * there for one pass, which may borrow from the provider. `scratch` is a folder on this
     * machine for what the source has to keep during the pass (a copy of a database). What is
     * left in it is removed when the archive is next opened.
     */
    fn detect<'a>(
        &'a self,
        transport: &'a dyn Transport,
        scratch: &'a Path,
    ) -> Result<Option<Box<dyn Source + 'a>>, ProviderError>;

    /// The version of the provider's parser. It goes up whenever [`Provider::parse`] or
    /// [`Provider::subagent`] would read an archived session differently, and every session
    /// indexed by another version is then parsed and indexed again (PLAN §7.5).
    fn parser_version(&self) -> u32;

    /**
     * The transcript of the session whose archived items are `items` (PLAN §7.4), with its own
     * conversation read and the subagents its calls started named, not read. Nothing fails a
     * parse: records the provider doesn't know are kept (PLAN D5), and what can't be read is
     * said in the transcript.
     */
    fn parse(&self, items: &dyn SessionItems) -> Transcript;

    /// The subagent whose conversation is the item `item`, read, with the subagents its own
    /// calls started named. An agent without subagents has none to read.
    fn subagent(&self, _items: &dyn SessionItems, item: &str) -> Subagent {
        Subagent::Unread { item: item.into() }
    }

    /**
     * Every subagent of the session whose items are `items`, outlined: what it says it is and
     * the subagents its calls started (sorted by key), which is what places it among the
     * others. By default each is read whole ([`outlines_by_reading`]); a provider that can
     * find the calls without reading a conversation outlines them more cheaply, and must agree
     * with that.
     */
    fn outlines(&self, items: &dyn SessionItems) -> Vec<Outline> {
        outlines_by_reading(self, items)
    }

    /**
     * What the item `key` is: its kind, and the provider's id of the session it belongs to,
     * if it belongs to one. It is what the provider's scan says of an item at a location, said
     * of an archived one. The engine asks it again of everything archived when the parser's
     * version changes, so that a better answer reaches the items that are gone from their
     * location too.
     */
    fn classify(&self, key: &str) -> (ItemKind, Option<String>);

    /**
     * Whether the session `session` (the provider's id for it) is a copy another app keeps of
     * a session that ran on another machine (PLAN §7.7), and if so, the words it is shown with.
     * Like [`Provider::classify`], it is said from the id alone, so a copy is known for one
     * whether or not its transcript can be read.
     */
    fn copy_of(&self, _session: &str) -> Option<CopyOf> {
        None
    }

    /**
     * What the records an app keeps about its sessions, the location's items of kind
     * [`ItemKind::Register`] in `register`, say of where each conversation ran (PLAN §7.7): at
     * most one record per conversation. The records are another program's state, so one that
     * can't be read or understood says nothing, and nothing fails.
     */
    fn copy_records(&self, _register: &dyn SessionItems) -> Vec<CopyRecord> {
        Vec::new()
    }

    /**
     * The agent's id for the conversation of the session whose own transcript is the item
     * `transcript`, when its key says it: what a parse would say, for a session its parser
     * could not read (PLAN §4.5), so that a session of a conversation deleted for good is
     * known for one even then.
     */
    fn conversation_of(&self, _transcript: &str) -> Option<String> {
        None
    }
}

/// [`Provider::outlines`] of every subagent of `items`, each read whole with
/// [`Provider::subagent`].
pub fn outlines_by_reading<P: Provider + ?Sized>(
    provider: &P,
    items: &dyn SessionItems,
) -> Vec<Outline> {
    items
        .list()
        .iter()
        .filter(|item| item.kind == ItemKind::Subagent)
        .filter_map(|item| match provider.subagent(items, &item.key) {
            Subagent::Read {
                kind,
                description,
                conversation,
            } => {
                let mut started: Vec<Arc<str>> = conversation
                    .calls()
                    .flat_map(|(_, call)| call.subagents.iter().map(|it| it.item().clone()))
                    .collect();
                started.sort_unstable();
                Some(Outline {
                    item: conversation.item.clone(),
                    kind,
                    description,
                    started,
                })
            }
            Subagent::Unread { .. } => None,
        })
        .collect()
}

/**
 * For development (the `check` example): checks `transcript`, which `provider` parsed from
 * `items` with every subagent read, against the session's records read another way. `None`
 * for a provider that has no such check.
 */
pub fn audit(
    provider: ProviderId,
    items: &dyn SessionItems,
    transcript: &Transcript,
) -> Option<Audit> {
    (provider == claude_code::ID).then(|| claude_code::audit(items, transcript))
}

/**
 * An agent's data at one location, for one sync pass (PLAN §5, §7.1). These are the only
 * operations of a provider that touch a location. Whatever the source keeps for the pass (a
 * scratch copy of a database) goes when it is dropped.
 *
 * The agent may be writing while the pass reads, so what [`Source::fetch`] delivers may mix
 * two states of an item. The engine stores nothing it has not proven with
 * [`Source::hash_prefixes`], asked after the fetch: the item's own hash of its start, with its
 * length.
 */
pub trait Source {
    /// Where the agent keeps what is archived from here, as the location names it: a folder,
    /// or a database file. It is only shown.
    fn root(&self) -> String;

    /// Every item the agent has here.
    fn scan(&mut self) -> Result<Scan, ProviderError>;

    /// The SHA-256 of each requested prefix, with the item's whole length. An item that is
    /// gone, unreadable or shorter than its prefix fails that request only.
    fn hash_prefixes(
        &mut self,
        requests: &[ItemPrefix],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, ProviderError>;

    /// Streams each requested range into `sink`. An item that is gone, unreadable or shorter
    /// than its range fails that range only.
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
