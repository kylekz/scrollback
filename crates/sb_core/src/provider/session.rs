//! A session's archived items, as its provider's parser reads them (PLAN §7.1). Parsing reads
//! the archive and never a location, so it works the same whatever the session was synced from.

use std::io::{self, Read};

use thiserror::Error;

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

#[derive(Debug, Error)]
pub enum ParseError {
    /// The session's items hold no main transcript.
    #[error("the session has no transcript")]
    NoTranscript,
    #[error("could not read {key}: {source}")]
    Read {
        key: String,
        #[source]
        source: io::Error,
    },
}
