//! Where things are in the archive's folder (PLAN §4.1):
//!
//! ```text
//! <archive>/
//!   archive.json                    format version, archive id, created
//!   scrollback.db                   the catalogue
//!   backups/                        checked backups of the catalogue
//!   lock                            held while the archive is open
//!   tmp/                            in-flight writes; emptied on start
//!   orphans/                        files the catalogue doesn't record, set aside
//!   files/<location-id>/<provider>/<ab>/<item-id>/
//!       source.json                 provider, location, the item's key
//!       v0001/0000000000000000.zst  bytes [0, a) of version 1
//!       v0001/0000000000184467.zst  bytes [a, b): an append
//!       v0002/0000000000000000.zst  the source was rewritten: version 2 starts at 0
//! ```
//!
//! Keys, which for files are source paths, are not mirrored on disk: they can be longer than
//! Windows allows a path to be and hold characters NTFS rejects. `source.json` keeps the key, so
//! the tree can be recovered without the catalogue.

use std::path::PathBuf;

use crate::hash::Hash;
use crate::location::LocationId;

pub(super) const MANIFEST: &str = "archive.json";
pub(super) const LOCK: &str = "lock";
pub(super) const TMP: &str = "tmp";
pub(super) const FILES: &str = "files";
pub(super) const ORPHANS: &str = "orphans";
pub(super) const SOURCE: &str = "source.json";

/// An item's folder name: the first 32 hex digits of the SHA-256 of its key.
pub(crate) fn item_folder(key: &str) -> String {
    let mut folder = Hash::of(key.as_bytes()).to_hex();
    folder.truncate(32);
    folder
}

/// An item's folder, relative to the archive.
pub(super) fn item_dir(location: &LocationId, provider: &str, folder: &str) -> PathBuf {
    [FILES, location.as_str(), provider, &folder[..2], folder]
        .iter()
        .collect()
}

pub(super) fn version_dir(number: u32) -> String {
    format!("v{number:04}")
}

pub(super) fn segment_file(offset: u64) -> String {
    format!("{offset:016}.zst")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_fixed_width() {
        assert_eq!(version_dir(1), "v0001");
        assert_eq!(version_dir(12345), "v12345");
        assert_eq!(segment_file(184_467), "0000000000184467.zst");
        let folder = item_folder("projects/-home-user-code-app/s1.jsonl");
        assert_eq!(folder.len(), 32);
        assert_eq!(folder, item_folder("projects/-home-user-code-app/s1.jsonl"));
    }
}
