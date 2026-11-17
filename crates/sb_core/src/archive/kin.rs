//! How the copies of a session kept elsewhere compare with it (PLAN §7.7): what the reader's
//! facts say of each.

use std::cmp::Ordering;
use std::io::Read as _;

use super::{Archive, Result};
use crate::catalogue::{Compared, Kin, SessionId};
use crate::hash::{Hash, Hasher};

/// A main transcript's version that is read: its id, length and hash.
type Read = (Option<i64>, u64, Option<String>);

impl Archive {
    /**
     * The other sessions of the session `id`'s conversation, originals first, each with how
     * its main transcript compares with `id`'s. A shorter one is behind only when it is the
     * start of the longer, which is hashed as far as the shorter goes; one that can't be read
     * that far is compared by its length.
     */
    pub fn kin(&self, id: &SessionId) -> Result<Vec<Kin>> {
        Ok(self
            .catalogue
            .kin_read(id)?
            .into_iter()
            .map(|read| Kin {
                compared: self.compare(&read.theirs, &read.ours),
                id: read.id,
                copy: read.copy,
            })
            .collect())
    }

    /// How `theirs` compares with `ours`.
    fn compare(&self, theirs: &Read, ours: &Read) -> Compared {
        let (longer, shorter, compared) = match theirs.1.cmp(&ours.1) {
            Ordering::Equal if theirs.2 == ours.2 => return Compared::Same,
            Ordering::Equal => return Compared::Differs,
            Ordering::Less => (ours, theirs, Compared::Shorter),
            Ordering::Greater => (theirs, ours, Compared::Longer),
        };
        match self.prefix_hash(longer.0, shorter.1) {
            Some(prefix) if Some(prefix.to_hex()) == shorter.2 => compared,
            Some(_) => Compared::Differs,
            None => compared,
        }
    }

    /// The hash of the first `length` bytes of the version `version`; `None` when there is none
    /// or it can't be read that far.
    fn prefix_hash(&self, version: Option<i64>, length: u64) -> Option<Hash> {
        let mut reader = self.read_version(version?).ok()?.take(length);
        let mut hasher = Hasher::new();
        let mut buffer = vec![0; 64 * 1024];
        let mut read = 0;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&buffer[..n]);
                    read += n as u64;
                }
                Err(error) => {
                    log::warn!("a copy could not be compared with its original: {error}");
                    return None;
                }
            }
        }
        (read == length).then(|| hasher.finish())
    }
}
