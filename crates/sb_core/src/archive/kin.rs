//! How the copies of a session kept elsewhere compare with it (PLAN §7.7): what the reader's
//! facts say of each.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::Read as _;
use std::sync::{Mutex, PoisonError};

use super::{Archive, Result};
use crate::catalogue::{Compared, Kin, SessionId};
use crate::hash::{Hash, Hasher};

/// A main transcript's version that is read: its id, length and hash.
type Read = (Option<i64>, u64, Option<String>);

/// How many comparisons a [`KinMemo`] keeps before it starts again.
const REMEMBERED: usize = 4096;

/**
 * The comparisons [`Archive::kin_remembered`] made, by the two versions compared as they were
 * read (each its id, length and hash): a version's bytes up to a length never change, so a
 * session parsed again while it grows hashes only what it has not compared before.
 */
#[derive(Debug, Default)]
pub struct KinMemo(Mutex<HashMap<(Read, Read), Compared>>);

impl Archive {
    /**
     * The other sessions of the session `id`'s conversation, originals first, each with how
     * its main transcript compares with `id`'s. A shorter one is behind only when it is the
     * start of the longer, which is hashed as far as the shorter goes; one that can't be read
     * that far is compared by its length.
     */
    pub fn kin(&self, id: &SessionId) -> Result<Vec<Kin>> {
        self.kin_remembered(id, &KinMemo::default())
    }

    /// [`Archive::kin`], with each comparison asked of `memo` first and kept in it after.
    pub fn kin_remembered(&self, id: &SessionId, memo: &KinMemo) -> Result<Vec<Kin>> {
        Ok(self
            .catalogue
            .kin_read(id)?
            .into_iter()
            .map(|read| {
                let pair = (read.theirs, read.ours);
                let known = (memo.0.lock().unwrap_or_else(PoisonError::into_inner))
                    .get(&pair)
                    .copied();
                let compared = known.unwrap_or_else(|| {
                    let compared = self.compare(&pair.0, &pair.1);
                    let mut memo = memo.0.lock().unwrap_or_else(PoisonError::into_inner);
                    if memo.len() >= REMEMBERED {
                        memo.clear();
                    }
                    memo.insert(pair.clone(), compared);
                    compared
                });
                Kin {
                    compared,
                    id: read.id,
                    copy: read.copy,
                }
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
