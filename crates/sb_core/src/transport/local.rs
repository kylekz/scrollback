//! [`LocalTransport`]: this machine's file system through `std::fs` (PLAN §6.1). Every file is
//! opened read-only.

use std::fs::{self, File, Metadata};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    Env, FileEntry, FileStat, Halt, Hashed, Listing, NOT_A_FILE, NOT_UNICODE, PrefixRequest,
    RangeFailure, RangeRequest, RangeSink, SYMBOLIC_LINK, Transport, TransportError, Unlisted,
};
use crate::hash::Hasher;

/// How much of a file is read at a time.
const CHUNK: usize = 256 * 1024;

/// This machine as a location: its user's home, environment and files.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalTransport;

impl Transport for LocalTransport {
    fn env(&self, names: &[&str]) -> Result<Env, TransportError> {
        Ok(Env {
            home: std::env::home_dir()
                .and_then(|home| home.into_os_string().into_string().ok())
                .ok_or(TransportError::NoHome)?,
            vars: names
                .iter()
                .filter_map(|name| Some(((*name).to_string(), std::env::var(name).ok()?)))
                .collect(),
        })
    }

    fn stat(&self, paths: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        paths
            .iter()
            .map(|path| match fs::metadata(path) {
                Ok(metadata) => Ok(Some(stat_of(&metadata).map_err(io_error("stat", path))?)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(io_error("stat", path)(error)),
            })
            .collect()
    }

    fn list(&self, root: &str) -> Result<Listing, TransportError> {
        let mut listing = Listing {
            files: Vec::new(),
            unlisted: Vec::new(),
            skipped: Vec::new(),
            clock_ns: ns_since_epoch(SystemTime::now()),
        };
        let entry_of = |path: PathBuf, reason: String| Unlisted {
            path: path.to_string_lossy().into_owned(),
            reason,
        };
        /* Directories still to read, each with its path relative to `root`. */
        let mut pending = vec![(PathBuf::from(root), String::new())];
        while let Some((dir, relative)) = pending.pop() {
            let mut unlisted = |path: PathBuf, error: io::Error| {
                listing.unlisted.push(entry_of(path, error.to_string()));
            };
            /* A folder that is gone was there a moment ago, when it was found: what it held
            can't be told from here, any more than what an unreadable one holds. */
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) => {
                    unlisted(dir, error);
                    continue;
                }
            };
            for entry in entries {
                /* A directory that stops listing part-way may hold more than was seen. */
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        unlisted(dir, error);
                        break;
                    }
                };
                let Ok(name) = entry.file_name().into_string() else {
                    unlisted(entry.path(), io::Error::other(NOT_UNICODE));
                    continue;
                };
                let path = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                let kind = match entry.file_type() {
                    Ok(kind) => kind,
                    /* Removed since the directory was read. */
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        unlisted(entry.path(), error);
                        continue;
                    }
                };
                if kind.is_dir() {
                    pending.push((entry.path(), path));
                } else if kind.is_file() {
                    match listed_metadata(&entry).and_then(|metadata| stat_of(&metadata)) {
                        Ok(stat) => listing.files.push(FileEntry { path, stat }),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => unlisted(entry.path(), error),
                    }
                } else {
                    let what = if kind.is_symlink() {
                        SYMBOLIC_LINK
                    } else {
                        NOT_A_FILE
                    };
                    listing
                        .skipped
                        .push(entry_of(entry.path(), what.to_string()));
                }
            }
        }
        Ok(listing)
    }

    fn hash_prefixes(
        &self,
        requests: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError> {
        Ok(requests
            .iter()
            .map(|request| {
                let file = File::open(&request.path).map_err(failure)?;
                let mut hasher = Hasher::new();
                let mut chunks = Chunks::new(file, request.length);
                while let Some(bytes) = chunks.next().map_err(failure)? {
                    hasher.update(bytes);
                }
                if chunks.left > 0 {
                    return Err(RangeFailure::Short {
                        expected: request.length,
                        got: request.length - chunks.left,
                    });
                }
                Ok(Hashed {
                    hash: hasher.finish(),
                    size: chunks.file.metadata().map_err(failure)?.len(),
                })
            })
            .collect())
    }

    fn read_ranges(
        &self,
        requests: &[RangeRequest],
        sink: &mut dyn RangeSink,
    ) -> Result<(), TransportError> {
        for (index, request) in requests.iter().enumerate() {
            let opened = File::open(&request.path).and_then(|mut file| {
                file.seek(SeekFrom::Start(request.offset))?;
                Ok(file)
            });
            let result = match opened {
                Ok(file) => {
                    let mut chunks = Chunks::new(file, request.length);
                    let read = loop {
                        match chunks.next() {
                            Ok(Some(bytes)) => sink
                                .data(index, bytes)
                                .map_err(|Halt| TransportError::Halted)?,
                            Ok(None) => break Ok(()),
                            Err(error) => break Err(error),
                        }
                    };
                    match read {
                        Ok(()) if chunks.left > 0 => Err(RangeFailure::Short {
                            expected: request.length,
                            got: request.length - chunks.left,
                        }),
                        Ok(()) => Ok(()),
                        Err(error) => Err(failure(error)),
                    }
                }
                Err(error) => Err(failure(error)),
            };
            sink.end(index, result)
                .map_err(|Halt| TransportError::Halted)?;
        }
        Ok(())
    }

    fn read_small(&self, path: &str) -> Result<Option<Vec<u8>>, TransportError> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error("read", path)(error)),
        }
    }
}

/// A range of a file from where the file stands, read a chunk at a time.
struct Chunks {
    file: File,
    /// How many bytes of the range are still to be read.
    left: u64,
    buffer: Vec<u8>,
}

impl Chunks {
    fn new(file: File, length: u64) -> Self {
        Self {
            file,
            left: length,
            buffer: vec![0; CHUNK],
        }
    }

    /// The next bytes; `None` at the end of the range, or of the file if that comes first.
    fn next(&mut self) -> io::Result<Option<&[u8]>> {
        let want = usize::try_from(self.left).map_or(CHUNK, |left| left.min(CHUNK));
        loop {
            match self.file.read(&mut self.buffer[..want]) {
                Ok(0) => return Ok(None),
                Ok(read) => {
                    self.left -= read as u64;
                    return Ok(Some(&self.buffer[..read]));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

/// The metadata of a regular file found in a listing.
///
/// On Windows it is asked of the file, not taken from the directory's entry: NTFS brings an
/// entry's size and times up to date only when a handle to the file is closed or flushed, so a
/// file its writer keeps open would be listed as it was when the writer opened it, and a sync
/// would take it for unchanged for as long as it is being written.
#[cfg(windows)]
fn listed_metadata(entry: &fs::DirEntry) -> io::Result<Metadata> {
    fs::metadata(entry.path())
}

/// The metadata of a regular file found in a listing.
#[cfg(not(windows))]
fn listed_metadata(entry: &fs::DirEntry) -> io::Result<Metadata> {
    entry.metadata()
}

fn stat_of(metadata: &Metadata) -> io::Result<FileStat> {
    Ok(FileStat {
        size: metadata.len(),
        mtime_ns: ns_since_epoch(metadata.modified()?),
    })
}

pub(super) fn ns_since_epoch(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_nanos()).map_or(i64::MIN, |ns| -ns),
    }
}

/// Why one file's request failed.
fn failure(error: io::Error) -> RangeFailure {
    if error.kind() == io::ErrorKind::NotFound {
        RangeFailure::Missing
    } else {
        RangeFailure::Unreadable(error.to_string())
    }
}

fn io_error(action: &'static str, path: &str) -> impl Fn(io::Error) -> TransportError {
    move |source| TransportError::Io {
        action,
        path: path.to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::hash::Hash;

    #[derive(Default)]
    struct Collect {
        bytes: Vec<Vec<u8>>,
        ends: Vec<Result<(), String>>,
    }

    impl RangeSink for Collect {
        fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
            self.bytes.resize(request + 1, Vec::new());
            self.bytes[request].extend_from_slice(bytes);
            Ok(())
        }

        fn end(&mut self, request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
            self.bytes.resize(request + 1, Vec::new());
            self.ends
                .push(result.map_err(|failure| failure.to_string()));
            Ok(())
        }
    }

    fn text(path: &Path) -> String {
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn lists_files_however_deep_and_reports_links() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("top.txt"), "1").unwrap();
        fs::write(dir.path().join("a/b/deep.txt"), "22").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("a/b"), dir.path().join("a/link")).unwrap();

        let before = ns_since_epoch(SystemTime::now());
        let mut listing = LocalTransport.list(&text(dir.path())).unwrap();
        listing.files.sort_by(|a, b| a.path.cmp(&b.path));
        let listed: Vec<_> = listing
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.stat.size))
            .collect();
        assert_eq!(listed, [("a/b/deep.txt", 2), ("top.txt", 1)]);
        assert_eq!(listing.unlisted, []);
        assert!(before <= listing.clock_ns);
        #[cfg(unix)]
        assert_eq!(
            listing.skipped,
            [Unlisted {
                path: text(&dir.path().join("a/link")),
                reason: "it is a symbolic link".to_string(),
            }]
        );
    }

    #[test]
    fn a_folder_that_is_not_there_is_reported_not_listed_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = text(&dir.path().join("missing"));
        let listing = LocalTransport.list(&missing).unwrap();
        assert!(listing.files.is_empty());
        assert_eq!(listing.unlisted.len(), 1);
        assert_eq!(listing.unlisted[0].path, missing);
    }

    #[cfg(unix)]
    #[test]
    fn what_cannot_be_read_is_reported_and_the_rest_is_listed() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let set_mode = |path: &Path, mode| {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        };
        /* One folder that can't be listed, and one whose files can't be looked at. */
        let (locked, opaque) = (dir.path().join("locked"), dir.path().join("opaque"));
        for folder in [&locked, &opaque] {
            fs::create_dir_all(folder).unwrap();
            fs::write(folder.join("inside.txt"), "1").unwrap();
        }
        fs::write(dir.path().join("open.txt"), "1").unwrap();
        set_mode(&locked, 0o000);
        set_mode(&opaque, 0o444);

        let listing = LocalTransport.list(&text(dir.path())).unwrap();
        set_mode(&locked, 0o755);
        set_mode(&opaque, 0o755);
        let listed: Vec<_> = listing.files.iter().map(|file| &file.path).collect();
        assert_eq!(listed, ["open.txt"]);
        let mut unlisted: Vec<_> = listing.unlisted.iter().map(|entry| &entry.path).collect();
        unlisted.sort();
        assert_eq!(
            unlisted,
            [&text(&locked), &text(&opaque.join("inside.txt"))]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_whose_name_is_not_unicode_is_reported() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(OsStr::from_bytes(b"bad-\xff.txt")), "1").unwrap();
        fs::write(dir.path().join("good.txt"), "1").unwrap();

        let listing = LocalTransport.list(&text(dir.path())).unwrap();
        assert_eq!(listing.files.len(), 1);
        assert_eq!(listing.unlisted.len(), 1);
        assert_eq!(listing.unlisted[0].reason, "its name is not Unicode");
    }

    #[test]
    fn a_listing_sees_what_a_writer_with_an_open_handle_has_appended() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("live.jsonl");
        fs::write(&file, "0123456789").unwrap();
        let listed = || LocalTransport.list(&text(dir.path())).unwrap().files[0].stat;
        let before = listed();
        assert_eq!(before.size, 10);

        // The agent goes on writing and keeps its handle, without flushing to disk.
        let mut writer = File::options().append(true).open(&file).unwrap();
        writer.write_all(b"abcde").unwrap();
        assert_eq!(listed().size, 15);
        drop(writer);
        assert_eq!(listed().size, 15);
    }

    #[test]
    fn a_missing_or_short_file_fails_its_range_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "0123456789").unwrap();
        let request = |path: &Path, offset, length| RangeRequest {
            path: text(path),
            offset,
            length,
        };
        let mut sink = Collect::default();
        LocalTransport
            .read_ranges(
                &[
                    request(&file, 2, 5),
                    request(&dir.path().join("missing"), 0, 1),
                    request(&file, 8, 5),
                    request(&file, 0, 10),
                ],
                &mut sink,
            )
            .unwrap();
        assert_eq!(sink.bytes, [&b"23456"[..], b"", b"89", b"0123456789"]);
        assert_eq!(
            sink.ends,
            [
                Ok(()),
                Err("it is no longer there".to_string()),
                Err("the source ended after 2 of 5 bytes".to_string()),
                Ok(()),
            ]
        );
    }

    #[test]
    fn hashes_a_prefix_and_fails_one_longer_than_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "0123456789").unwrap();
        let request = |path: &Path, length| PrefixRequest {
            path: text(path),
            length,
        };
        let hashes: Vec<_> = LocalTransport
            .hash_prefixes(&[
                request(&file, 4),
                request(&file, 11),
                request(&dir.path().join("missing"), 1),
                request(&file, 0),
            ])
            .unwrap()
            .into_iter()
            .map(|hashed| hashed.map_err(|failure| failure.to_string()))
            .collect();
        let hashed = |bytes: &[u8]| Hashed {
            hash: Hash::of(bytes),
            size: 10,
        };
        assert_eq!(
            hashes,
            [
                Ok(hashed(b"0123")),
                Err("the source ended after 10 of 11 bytes".to_string()),
                Err("it is no longer there".to_string()),
                Ok(hashed(b"")),
            ]
        );
    }
}
