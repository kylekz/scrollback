//! [`FileTree`]: the source of an agent that writes files (PLAN §7.2). Items are the files
//! under one directory of the agent's root, keys are their paths relative to the root, and the
//! stamp is the modification time. A provider for such an agent supplies where the root is and
//! how a path maps to a session.
//!
//! A file's size and modification time stand for its content only once the modification time
//! is safely in the past: a file written again within the same tick of the file system's clock
//! keeps its time. So a file modified as recently as the scan itself is not trusted (git's rule
//! for the same race). Its stamp is one no later scan repeats, which makes the next pass check
//! its bytes, and a fetch of it is never reported stable, which makes the engine prove them.

use std::collections::HashMap;

use super::{
    Hashed, ItemKind, ItemPrefix, ItemRange, ProviderError, RangeFailure, Scan, ScannedItem, Source,
};
use crate::transport::{FileStat, PrefixRequest, RangeRequest, RangeSink, Transport};

/// How long before the scan a file must have been modified last for its modification time to
/// be trusted. The coarsest timestamps in use tick every two seconds.
const RACY_NS: i64 = 2_000_000_000;

/// What a file is, from its key alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileClass {
    pub kind: ItemKind,
    pub session: Option<String>,
    /// See [`ScannedItem::lines`].
    pub lines: bool,
}

/// The files under `<root>/<subtree>` as a [`Source`].
pub struct FileTree<'a> {
    transport: &'a dyn Transport,
    root: String,
    subtree: &'static str,
    classify: fn(&str) -> FileClass,
    /// Each file the scan found with a trusted modification time, as it was then, by key.
    scanned: HashMap<String, FileStat>,
}

impl<'a> FileTree<'a> {
    /// The files under the directory `subtree` of the agent's root at `root`. `classify` is
    /// given each file's key, which starts with `subtree`.
    pub fn new(
        transport: &'a dyn Transport,
        root: String,
        subtree: &'static str,
        classify: fn(&str) -> FileClass,
    ) -> Self {
        Self {
            transport,
            root,
            subtree,
            classify,
            scanned: HashMap::new(),
        }
    }

    fn path(&self, key: &str) -> String {
        format!("{}/{key}", self.root)
    }
}

impl Source for FileTree<'_> {
    fn scan(&mut self) -> Result<Scan, ProviderError> {
        let listing = self.transport.list(&self.path(self.subtree))?;
        self.scanned.clear();
        let items = listing
            .files
            .into_iter()
            .map(|file| {
                let key = format!("{}/{}", self.subtree, file.path);
                let class = (self.classify)(&key);
                let mtime = file.stat.mtime_ns;
                let stamp = if mtime < listing.clock_ns - RACY_NS {
                    self.scanned.insert(key.clone(), file.stat);
                    mtime.to_string()
                } else {
                    format!("{mtime} seen at {}", listing.clock_ns)
                };
                ScannedItem {
                    key,
                    kind: class.kind,
                    session: class.session,
                    length: file.stat.size,
                    stamp,
                    lines: class.lines,
                    export_version: None,
                }
            })
            .collect();
        Ok(Scan {
            items,
            unlisted: listing.unlisted,
        })
    }

    fn hash_prefixes(
        &mut self,
        requests: &[ItemPrefix],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, ProviderError> {
        let requests: Vec<_> = requests
            .iter()
            .map(|request| PrefixRequest {
                path: self.path(&request.key),
                length: request.length,
            })
            .collect();
        Ok(self.transport.hash_prefixes(&requests)?)
    }

    fn fetch(
        &mut self,
        requests: &[ItemRange],
        sink: &mut dyn RangeSink,
    ) -> Result<(), ProviderError> {
        let requests: Vec<_> = requests
            .iter()
            .map(|request| RangeRequest {
                path: self.path(&request.key),
                offset: request.offset,
                length: request.length,
                expect: self.scanned.get(&request.key).copied(),
            })
            .collect();
        Ok(self.transport.read_ranges(&requests, sink)?)
    }
}
