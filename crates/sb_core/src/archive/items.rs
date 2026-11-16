//! A session's items as the archive holds them (PLAN §7.1): what a provider's parser reads.
//! Each item is at its reading copy (PLAN §4.2): the version the owner chose of it, else for a
//! transcript the newest that is not held back, else the newest.
//!
//! A read can fail in two ways, and a parser can't tell them apart: it says of either that
//! the conversation could not be read. A segment that is not what the catalogue records is
//! the archive's state, and stays so until the item is fetched again (PLAN §4.4). A read the
//! operating system failed (a file in use, a disk error) says nothing of what is stored, and
//! what was parsed around it is not what the session holds: [`ArchivedSession::fault`] keeps
//! the first such failure for whoever parsed.

use std::cell::RefCell;
use std::io::{self, Read};
use std::sync::Arc;

use super::{Archive, Problem, Result, SegmentCache, VersionReader};
use crate::catalogue::{Reading, SessionId};
use crate::provider::{SessionItem, SessionItems};

/// The archived items of one session, by key, and the version each is read at.
pub struct ArchivedSession<'a> {
    archive: &'a Archive,
    pinned: Pinned,
    fault: RefCell<Option<String>>,
    cache: Option<Arc<SegmentCache>>,
}

/**
 * A session's items as an [`ArchivedSession`] read them: each at the version it was read at.
 * The reader keeps it to load a part of the session long after parsing it
 * ([`Archive::pinned_session`]), and gets the bytes it parsed even when a sync has started a
 * newer version since.
 */
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pinned {
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

    /// A session's items at the versions `pinned` holds, read through `cache`.
    pub fn pinned_session(&self, pinned: &Pinned, cache: Arc<SegmentCache>) -> ArchivedSession<'_> {
        ArchivedSession {
            archive: self,
            pinned: pinned.clone(),
            fault: RefCell::default(),
            cache: Some(cache),
        }
    }
}

impl<'a> ArchivedSession<'a> {
    /// The items `readings`, which are one session's, by key.
    pub(crate) fn of(archive: &'a Archive, readings: &[Reading]) -> Self {
        Self {
            archive,
            pinned: Pinned {
                list: readings
                    .iter()
                    .map(|reading| SessionItem {
                        key: reading.key.clone(),
                        kind: reading.kind,
                        length: reading.length,
                    })
                    .collect(),
                versions: readings.iter().map(|reading| reading.version).collect(),
            },
            fault: RefCell::default(),
            cache: None,
        }
    }

    /// The same items, with the item `key` read at its version `version`, of `length` bytes.
    pub(crate) fn at(mut self, key: &str, version: i64, length: u64) -> Self {
        if let Ok(at) = self.find(key) {
            self.pinned.list[at].length = length;
            self.pinned.versions[at] = version;
        }
        self
    }

    /// Which version each item is read at, to read them so again later.
    pub fn pinned(&self) -> &Pinned {
        &self.pinned
    }

    /// The same items, read through `cache`.
    pub fn with_cache(self, cache: Arc<SegmentCache>) -> Self {
        Self {
            cache: Some(cache),
            ..self
        }
    }

    /// Why a read of one of the items failed, if one did for a reason that is not the stored
    /// bytes (the module docs): the first such reason.
    pub fn fault(&self) -> Option<String> {
        self.fault.borrow().clone()
    }

    /// Where the item `key` is in the list, which is in the order of the keys.
    fn find(&self, key: &str) -> std::result::Result<usize, usize> {
        self.pinned
            .list
            .binary_search_by(|item| item.key.as_str().cmp(key))
    }

    /// Keeps `error` as the fault, if it is one and the first.
    fn note(&self, error: &io::Error) {
        if Problem::of(error).is_none() && error.kind() != io::ErrorKind::Interrupted {
            self.fault
                .borrow_mut()
                .get_or_insert_with(|| error.to_string());
        }
    }
}

impl SessionItems for ArchivedSession<'_> {
    fn list(&self) -> &[SessionItem] {
        &self.pinned.list
    }

    /// A segment that is not what the catalogue records fails the read where it is reached
    /// (PLAN §4.4), and the parser says so in the transcript.
    fn open(&self, key: &str, offset: u64) -> io::Result<Box<dyn Read + '_>> {
        let at = self.find(key).map_err(|_| io::ErrorKind::NotFound)?;
        let version = self.pinned.versions[at];
        match self
            .archive
            .read_version_cached(version, offset, self.cache.clone())
        {
            Ok(reader) => Ok(Box::new(Watched {
                reader,
                session: self,
            })),
            Err(error) => {
                let error = io::Error::other(error);
                self.note(&error);
                Err(error)
            }
        }
    }
}

/// A version's reader that tells its session of a read that failed.
struct Watched<'a> {
    reader: VersionReader,
    session: &'a ArchivedSession<'a>,
}

impl Read for Watched<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reader
            .read(buffer)
            .inspect_err(|error| self.session.note(error))
    }
}
