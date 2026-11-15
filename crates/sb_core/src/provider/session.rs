//! A session's archived items, as its provider's parser reads them (PLAN §7.1). Parsing reads
//! the archive and never a location, so it works the same whatever the session was synced from.

use std::collections::BTreeMap;
use std::io::{self, Read};

use super::ItemKind;

/// One archived item of a session: the version of it that is being read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionItem {
    pub key: String,
    pub kind: ItemKind,
    pub length: u64,
}

/// The items of one session, and their bytes.
pub trait SessionItems {
    fn list(&self) -> &[SessionItem];

    /// The bytes of the item `key`, from `offset` on.
    fn open(&self, key: &str, offset: u64) -> io::Result<Box<dyn Read + '_>>;
}

/**
 * What a provider finds when it checks its parse of a session against the session's records
 * read another way (the `check` example). It holds the agent's names for things and counts of
 * them, and nothing a session says.
 */
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Audit {
    /// Each type of record seen: how many, and whether the parser knows the type.
    pub records: BTreeMap<String, (u64, bool)>,
    /// What the parser doesn't know inside the record types it does: each by its name, and how
    /// many.
    pub unknown: BTreeMap<String, u64>,
    /// Anything else worth counting about how the agent wrote the session, by what it is.
    pub counts: BTreeMap<String, u64>,
    /// The checks that found the parsed transcript to differ from the records, by name.
    pub mismatches: Vec<&'static str>,
}
