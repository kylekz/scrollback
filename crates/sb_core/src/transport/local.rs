//! [`LocalTransport`]: this machine's file system through `std::fs` (PLAN §6.1). Every file is
//! opened read-only.

use std::fs::{self, File, Metadata};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    Env, Fetched, FileEntry, FileStat, Halt, Hashed, Listing, PrefixRequest, RangeFailure,
    RangeRequest, RangeSink, Transport, TransportError, Unlisted,
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
            clock_ns: ns_since_epoch(SystemTime::now()),
        };
        /* Directories still to read, each with its path relative to `root`. */
        let mut pending = vec![(PathBuf::from(root), String::new())];
        while let Some((dir, relative)) = pending.pop() {
            let mut unlisted = |path: PathBuf, error: io::Error, folder| {
                listing.unlisted.push(Unlisted {
                    path: path.to_string_lossy().into_owned(),
                    reason: error.to_string(),
                    folder,
                });
            };
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                /* Missing, or removed since its parent was read. */
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    unlisted(dir, error, true);
                    continue;
                }
            };
            for entry in entries {
                /* A directory that stops listing part-way may hold more than was seen. */
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        unlisted(dir, error, true);
                        break;
                    }
                };
                let Ok(name) = entry.file_name().into_string() else {
                    let error = io::Error::other("its name is not Unicode");
                    /* Whether it is a file or a folder of files, none of it is listed. */
                    unlisted(entry.path(), error, true);
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
                        unlisted(entry.path(), error, true);
                        continue;
                    }
                };
                if kind.is_dir() {
                    pending.push((entry.path(), path));
                } else if kind.is_file() {
                    match entry.metadata().and_then(|metadata| stat_of(&metadata)) {
                        Ok(stat) => listing.files.push(FileEntry { path, stat }),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => unlisted(entry.path(), error, false),
                    }
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
            /* The file is compared with what the caller expects through the one open handle,
            before the read and after it: a file replaced by another of the same name is not
            mistaken for this one. */
            let opened = File::open(&request.path).and_then(|mut file| {
                let before = stat_of(&file.metadata()?)?;
                file.seek(SeekFrom::Start(request.offset))?;
                Ok((file, before))
            });
            let result = match opened {
                Ok((file, before)) => {
                    let mut chunks = Chunks::new(file, request.length);
                    let read = loop {
                        match chunks.next() {
                            Ok(Some(bytes)) => sink
                                .data(index, bytes)
                                .map_err(|Halt| TransportError::Halted)?,
                            Ok(None) => break chunks.file.metadata().and_then(|m| stat_of(&m)),
                            Err(error) => break Err(error),
                        }
                    };
                    match read {
                        Ok(_) if chunks.left > 0 => Err(RangeFailure::Short {
                            expected: request.length,
                            got: request.length - chunks.left,
                        }),
                        Ok(after) if Some(before) == request.expect && before == after => {
                            Ok(Fetched::Stable)
                        }
                        Ok(_) => Ok(Fetched::Unstable),
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

fn stat_of(metadata: &Metadata) -> io::Result<FileStat> {
    Ok(FileStat {
        size: metadata.len(),
        mtime_ns: ns_since_epoch(metadata.modified()?),
    })
}

fn ns_since_epoch(time: SystemTime) -> i64 {
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
        ends: Vec<Result<Fetched, String>>,
    }

    impl RangeSink for Collect {
        fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
            self.bytes.resize(request + 1, Vec::new());
            self.bytes[request].extend_from_slice(bytes);
            Ok(())
        }

        fn end(
            &mut self,
            request: usize,
            result: Result<Fetched, RangeFailure>,
        ) -> Result<(), Halt> {
            self.bytes.resize(request + 1, Vec::new());
            self.ends
                .push(result.map_err(|failure| failure.to_string()));
            Ok(())
        }
    }

    fn text(path: &Path) -> String {
        path.to_str().unwrap().to_string()
    }

    fn stat(path: &Path) -> Option<FileStat> {
        LocalTransport.stat(&[&text(path)]).unwrap()[0]
    }

    #[test]
    fn lists_files_however_deep_and_skips_links() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("top.txt"), "1").unwrap();
        fs::write(dir.path().join("a/b/deep.txt"), "22").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("top.txt"), dir.path().join("a/link")).unwrap();

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
        assert!(listing.clock_ns >= before);

        let missing = LocalTransport
            .list(&text(&dir.path().join("missing")))
            .unwrap();
        assert!(missing.files.is_empty() && missing.unlisted.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_read_is_reported_and_the_rest_is_listed() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("inside.txt"), "1").unwrap();
        fs::write(dir.path().join("open.txt"), "1").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let listing = LocalTransport.list(&text(dir.path())).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let listed: Vec<_> = listing.files.iter().map(|file| &file.path).collect();
        assert_eq!(listed, ["open.txt"]);
        assert_eq!(listing.unlisted.len(), 1);
        assert_eq!(
            (
                listing.unlisted[0].path.as_str(),
                listing.unlisted[0].folder
            ),
            (text(&locked).as_str(), true)
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
    fn a_missing_or_short_file_fails_its_range_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "0123456789").unwrap();
        let request = |path: &Path, offset, length| RangeRequest {
            path: text(path),
            offset,
            length,
            expect: stat(path),
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
                Ok(Fetched::Stable),
                Err("it is no longer there".to_string()),
                Err("the source ended after 2 of 5 bytes".to_string()),
                Ok(Fetched::Stable),
            ]
        );
    }

    #[test]
    fn a_read_is_stable_only_when_the_file_is_as_expected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "0123456789").unwrap();
        let seen = stat(&file).unwrap();
        let request = |expect| RangeRequest {
            path: text(&file),
            offset: 0,
            length: 10,
            expect,
        };
        let moved = FileStat {
            mtime_ns: seen.mtime_ns - 1,
            ..seen
        };
        let mut sink = Collect::default();
        LocalTransport
            .read_ranges(
                &[request(Some(seen)), request(Some(moved)), request(None)],
                &mut sink,
            )
            .unwrap();
        assert_eq!(
            sink.ends,
            [
                Ok(Fetched::Stable),
                Ok(Fetched::Unstable),
                Ok(Fetched::Unstable)
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
