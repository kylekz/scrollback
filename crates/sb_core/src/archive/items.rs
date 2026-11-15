//! A session's items as the archive holds them (PLAN §7.1): what a provider's parser reads.
//! Each item is at its reading copy, the version the owner chose, else the newest that is not
//! held back (PLAN §4.2).

use std::io::{self, Read};

use super::{Archive, Result};
use crate::catalogue::{Reading, SessionId, Version};
use crate::provider::{SessionItem, SessionItems};

/// The archived items of one session, by key, and the version each is read at.
pub struct ArchivedSession<'a> {
    archive: &'a Archive,
    list: Vec<SessionItem>,
    /// The version of each item of `list`.
    versions: Vec<i64>,
}

impl Archive {
    /// The items of the session `id`, each at its reading copy.
    pub fn session(&self, id: &SessionId) -> Result<ArchivedSession<'_>> {
        Ok(ArchivedSession::of(
            self,
            &self.catalogue.session_items(id)?,
        ))
    }
}

impl<'a> ArchivedSession<'a> {
    /// The items `readings`, which are one session's, by key.
    pub(crate) fn of(archive: &'a Archive, readings: &[Reading]) -> Self {
        Self {
            archive,
            list: readings
                .iter()
                .map(|reading| SessionItem {
                    key: reading.key.clone(),
                    kind: reading.kind,
                    length: reading.length,
                })
                .collect(),
            versions: readings.iter().map(|reading| reading.version).collect(),
        }
    }

    /// The same items, with the item `key` read at `version`.
    pub(crate) fn at(mut self, key: &str, version: &Version) -> Self {
        if let Ok(at) = self.find(key) {
            self.list[at].length = version.length;
            self.versions[at] = version.id;
        }
        self
    }

    /// Where the item `key` is in the list, which is in the order of the keys.
    fn find(&self, key: &str) -> std::result::Result<usize, usize> {
        self.list
            .binary_search_by(|item| item.key.as_str().cmp(key))
    }
}

impl SessionItems for ArchivedSession<'_> {
    fn list(&self) -> &[SessionItem] {
        &self.list
    }

    /// A segment that is not what the catalogue records fails the read where it is reached
    /// (PLAN §4.4), and the parser says so in the transcript.
    fn open(&self, key: &str, offset: u64) -> io::Result<Box<dyn Read + '_>> {
        let at = self.find(key).map_err(|_| io::ErrorKind::NotFound)?;
        self.archive
            .read_version_from(self.versions[at], offset)
            .map(|reader| Box::new(reader) as Box<dyn Read>)
            .map_err(io::Error::other)
    }
}
