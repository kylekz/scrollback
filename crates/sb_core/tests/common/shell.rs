//! What the shell transport's tests share: sinks that keep what they are given, a transport
//! over a local shell, and stand-ins for the tools a location has.

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use sb_core::transport::{Halt, Launcher, RangeFailure, RangeSink, ShellTransport};
use sb_test_support::loopback;
use tempfile::TempDir;

/// What a sink was given: each range's bytes, and how each ended.
#[derive(Default, Debug, PartialEq)]
pub struct Collect {
    pub bytes: Vec<Vec<u8>>,
    pub ends: Vec<Result<(), String>>,
}

impl RangeSink for Collect {
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
        self.bytes.resize(request + 1, Vec::new());
        self.bytes[request].extend_from_slice(bytes);
        Ok(())
    }

    fn end(&mut self, request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        self.bytes.resize(request + 1, Vec::new());
        self.ends.push(result.map_err(|failure| match failure {
            /* A tool's words for it differ from this machine's. */
            RangeFailure::Unreadable(_) => "unreadable".to_string(),
            failure => failure.to_string(),
        }));
        Ok(())
    }
}

pub fn text(path: &Path) -> String {
    path.to_str().unwrap().to_string()
}

/// A transport over `shell` whose home folder is `home`.
pub fn through(shell: &str, home: &Path) -> ShellTransport {
    ShellTransport::new(loopback(shell, home, &[]))
}

pub fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Bytes that are not text: every value, zeros in runs, and no pattern a shell would keep.
pub fn noise(length: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    (0..length)
        .map(|at| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            if at % 97 < 5 { 0 } else { state as u8 }
        })
        .collect()
}

/// So many megabytes of [`noise`].
pub fn megabytes(count: usize) -> Vec<u8> {
    noise(1024 * 1024).repeat(count)
}

/// Where this machine has `tool`.
pub fn tool(tool: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join(tool))
        .find(|found| found.is_file())
}

/// A folder to be a `PATH`: links to the tools the transport needs, but `without`.
pub fn tools(without: &[&str]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "sh",
        "find",
        "stat",
        "head",
        "tail",
        "dd",
        "cat",
        "date",
        "id",
        "sleep",
        "sha256sum",
        "shasum",
    ] {
        if without.contains(&name) {
            continue;
        }
        if let Some(found) = tool(name) {
            symlink(found, dir.path().join(name)).unwrap();
        }
    }
    dir
}

/// Puts a script in the place of `name` among `tools`: `body` runs with the real tool's path
/// in `$real` and the tool's arguments as its own.
pub fn stand_in(tools: &Path, name: &str, body: &str) {
    let real = tool(name).unwrap();
    let path = tools.join(name);
    let _ = fs::remove_file(&path);
    fs::write(
        &path,
        format!("#!/bin/sh\nreal='{}'\n{body}\n", text(&real)),
    )
    .unwrap();
    set_mode(&path, 0o755);
}

/// A transport over `sh` that finds only the tools in `path`.
pub fn with_tools(path: &Path) -> ShellTransport {
    ShellTransport::new(Launcher::Command {
        program: "env".into(),
        args: vec![
            "-i".to_string(),
            format!("PATH={}", text(path)),
            text(&path.join("sh")),
            "-s".to_string(),
        ],
    })
}

/// A transport whose launcher is `sh -c <line>`, with nothing of this process's environment
/// but its `PATH`, and a home folder that is not there: no profile of this machine's user is
/// read.
pub fn launched(line: &str) -> ShellTransport {
    ShellTransport::new(Launcher::Command {
        program: "env".into(),
        args: vec![
            "-i".to_string(),
            format!("PATH={}", std::env::var("PATH").unwrap()),
            "HOME=/nonexistent".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            line.to_string(),
        ],
    })
}

/// What a sink was told of each range's end, in the failure's own words.
#[derive(Default)]
pub struct Ends(pub Vec<String>);

impl RangeSink for Ends {
    fn data(&mut self, _request: usize, _bytes: &[u8]) -> Result<(), Halt> {
        Ok(())
    }

    fn end(&mut self, _request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        self.0
            .push(result.map_or_else(|failure| failure.to_string(), |()| "ok".to_string()));
        Ok(())
    }
}
