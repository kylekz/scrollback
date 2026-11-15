//! [`FileTree`]: the source of an agent that writes files (PLAN §7.2). Items are the files
//! under one directory of the agent's root, keys are their paths relative to the root, and the
//! stamp is the modification time. A provider for such an agent supplies where the root is and
//! how a path maps to a session.

use super::{ItemKind, ItemPrefix, ItemRange, ProviderError, ScannedItem, Source};
use crate::hash::Hash;
use crate::transport::{PrefixRequest, RangeRequest, RangeSink, Transport};

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
        }
    }

    fn path(&self, key: &str) -> String {
        format!("{}/{key}", self.root)
    }
}

impl Source for FileTree<'_> {
    fn scan(&mut self) -> Result<Vec<ScannedItem>, ProviderError> {
        Ok(self
            .transport
            .list(&self.path(self.subtree))?
            .into_iter()
            .map(|file| {
                let key = format!("{}/{}", self.subtree, file.path);
                let class = (self.classify)(&key);
                ScannedItem {
                    key,
                    kind: class.kind,
                    session: class.session,
                    length: file.stat.size,
                    stamp: file.stat.mtime_ns.to_string(),
                    lines: class.lines,
                    export_version: None,
                }
            })
            .collect())
    }

    fn hash_prefixes(
        &mut self,
        requests: &[ItemPrefix],
    ) -> Result<Vec<Option<Hash>>, ProviderError> {
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
            })
            .collect();
        Ok(self.transport.read_ranges(&requests, sink)?)
    }
}
