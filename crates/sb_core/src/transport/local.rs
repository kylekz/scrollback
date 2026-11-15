//! [`LocalTransport`]: this machine's file system through `std::fs` (PLAN §6.1). Every file is
//! opened read-only.

use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::{
    Env, FileEntry, FileStat, Halt, PrefixRequest, RangeFailure, RangeRequest, RangeSink,
    Transport, TransportError,
};
use crate::hash::{Hash, Hasher};

/// How much of a file is read at a time.
const CHUNK: usize = 256 * 1024;

/// This machine as a location: its user's home and environment, or stand-ins for them.
#[derive(Clone, Debug, Default)]
pub struct LocalTransport {
    /// `None`: the process's own home and environment.
    env: Option<Env>,
}

impl LocalTransport {
    /// This machine's user.
    pub fn new() -> Self {
        Self::default()
    }

    /// A location whose home is `home` and whose environment is `vars` alone: a location root in
    /// a temp dir, for tests (PLAN §12).
    pub fn with_env(
        home: impl Into<String>,
        vars: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            env: Some(Env {
                home: home.into(),
                vars: vars.into_iter().collect(),
            }),
        }
    }
}

impl Transport for LocalTransport {
    fn env(&self, names: &[&str]) -> Result<Env, TransportError> {
        let wanted = |name: &&str| names.contains(name);
        match &self.env {
            Some(env) => Ok(Env {
                home: env.home.clone(),
                vars: env
                    .vars
                    .iter()
                    .filter(|(name, _)| wanted(&name.as_str()))
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect(),
            }),
            None => Ok(Env {
                home: std::env::home_dir()
                    .and_then(|home| home.into_os_string().into_string().ok())
                    .ok_or(TransportError::NoHome)?,
                vars: names
                    .iter()
                    .filter_map(|name| Some(((*name).to_string(), std::env::var(name).ok()?)))
                    .collect::<HashMap<_, _>>(),
            }),
        }
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

    fn list(&self, root: &str) -> Result<Vec<FileEntry>, TransportError> {
        let mut files = Vec::new();
        /* Directories still to read, each with its path relative to `root`. */
        let mut pending = vec![(PathBuf::from(root), String::new())];
        while let Some((dir, relative)) = pending.pop() {
            let shown = dir.to_string_lossy().into_owned();
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                /* Missing, or removed since its parent was read. */
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error("list", &shown)(error)),
            };
            for entry in entries {
                let entry = entry.map_err(io_error("list", &shown))?;
                let Ok(name) = entry.file_name().into_string() else {
                    log::warn!(
                        "skipping {}: its name is not Unicode",
                        entry.path().display()
                    );
                    continue;
                };
                let path = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                let kind = entry.file_type().map_err(io_error("list", &shown))?;
                if kind.is_dir() {
                    pending.push((entry.path(), path));
                } else if kind.is_file() {
                    match listed_metadata(&entry) {
                        Ok(metadata) => files.push(FileEntry {
                            path,
                            stat: stat_of(&metadata).map_err(io_error("stat", &shown))?,
                        }),
                        /* Removed since the directory was read. */
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(io_error("stat", &shown)(error)),
                    }
                }
            }
        }
        Ok(files)
    }

    fn hash_prefixes(
        &self,
        requests: &[PrefixRequest],
    ) -> Result<Vec<Option<Hash>>, TransportError> {
        requests
            .iter()
            .map(|request| {
                let file = match File::open(&request.path) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(io_error("open", &request.path)(error)),
                };
                let mut hasher = Hasher::new();
                let mut chunks = Chunks::new(file, request.length);
                while let Some(bytes) = chunks.next().map_err(io_error("read", &request.path))? {
                    hasher.update(bytes);
                }
                Ok((chunks.left == 0).then(|| hasher.finish()))
            })
            .collect()
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
                        Ok(()) if chunks.left == 0 => Ok(()),
                        Ok(()) => Err(RangeFailure::Short {
                            expected: request.length,
                            got: request.length - chunks.left,
                        }),
                        Err(error) => Err(RangeFailure::Unreadable(error.to_string())),
                    }
                }
                Err(error) => Err(RangeFailure::Unreadable(error.to_string())),
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
    let mtime_ns = match metadata.modified()?.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_nanos()).map_or(i64::MIN, |ns| -ns),
    };
    Ok(FileStat {
        size: metadata.len(),
        mtime_ns,
    })
}

fn io_error(action: &'static str, path: &str) -> impl Fn(io::Error) -> TransportError {
    let path = Path::new(path).display().to_string();
    move |source| TransportError::Io {
        action,
        path: path.clone(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Collect {
        ranges: Vec<(Vec<u8>, Option<bool>)>,
    }

    impl RangeSink for Collect {
        fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
            self.ranges[request].0.extend_from_slice(bytes);
            Ok(())
        }

        fn end(&mut self, request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
            self.ranges[request].1 = Some(result.is_ok());
            Ok(())
        }
    }

    fn text(path: &Path) -> String {
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn lists_files_however_deep_and_skips_links() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("top.txt"), "1").unwrap();
        fs::write(dir.path().join("a/b/deep.txt"), "22").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("top.txt"), dir.path().join("a/link")).unwrap();

        let transport = LocalTransport::new();
        let mut files = transport.list(&text(dir.path())).unwrap();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let listed: Vec<_> = files
            .iter()
            .map(|file| (file.path.as_str(), file.stat.size))
            .collect();
        assert_eq!(listed, [("a/b/deep.txt", 2), ("top.txt", 1)]);
        assert!(
            transport
                .list(&text(&dir.path().join("missing")))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_listing_sees_what_a_writer_with_an_open_handle_has_appended() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("live.jsonl");
        fs::write(&file, "0123456789").unwrap();
        let transport = LocalTransport::new();
        let listed = || transport.list(&text(dir.path())).unwrap()[0].stat;
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
        let mut sink = Collect {
            ranges: vec![(Vec::new(), None); 4],
        };
        LocalTransport::new()
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
        assert_eq!(sink.ranges[0], (b"23456".to_vec(), Some(true)));
        assert_eq!(sink.ranges[1], (Vec::new(), Some(false)));
        assert_eq!(sink.ranges[2], (b"89".to_vec(), Some(false)));
        assert_eq!(sink.ranges[3], (b"0123456789".to_vec(), Some(true)));
    }

    #[test]
    fn hashes_a_prefix_and_refuses_one_longer_than_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "0123456789").unwrap();
        let request = |path: &Path, length| PrefixRequest {
            path: text(path),
            length,
        };
        let hashes = LocalTransport::new()
            .hash_prefixes(&[
                request(&file, 4),
                request(&file, 11),
                request(&dir.path().join("missing"), 1),
                request(&file, 0),
            ])
            .unwrap();
        assert_eq!(
            hashes,
            [Some(Hash::of(b"0123")), None, None, Some(Hash::of(b""))]
        );
    }

    #[test]
    fn a_stand_in_environment_hides_the_process_own() {
        let transport =
            LocalTransport::with_env("/home/user", [("SET".to_string(), "yes".to_string())]);
        let env = transport.env(&["SET", "PATH"]).unwrap();
        assert_eq!(env.home, "/home/user");
        assert_eq!(env.vars, HashMap::from([("SET".into(), "yes".into())]));
    }
}
