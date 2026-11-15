//! What scrollback's tests share (docs/PLAN.md §12): generated fixtures for each kind of agent,
//! in a temp dir that stands in for a location's home. Tests never read real transcripts.
//!
//! - [`ProjectsTree`]: a generated Claude Code `projects/` tree (PLAN §7.3), and [`FileAgent`],
//!   the same tree behind [`Agent`].
//! - [`DbAgent`] and [`DbProvider`]: a made-up agent that keeps its sessions in a SQLite
//!   database, with the test-only provider for it. It exists so the provider seam is tested
//!   with a second shape of source before a second real agent arrives (PLAN D8, §12).
//! - [`Agent`]: what the sync tests do to a source, so each test runs against both.
//! - [`HomeTransport`]: a fixture as a location, read with `std::fs` or through the shell
//!   transport by a local shell ([`loopback`]), so each test runs over both too.
//!
//! gpui: **never** (`script/check` step 3).

mod claude_code;
mod db_agent;

use std::path::Path;

use sb_core::provider::{Hashed, Provider, RangeFailure};
use sb_core::transport::{
    Env, FileStat, Launcher, Listing, LocalTransport, PrefixRequest, RangeRequest, RangeSink,
    ShellTransport, Transport, TransportError,
};
use toolbox_store::rusqlite::Connection;

pub use claude_code::{FileAgent, ProjectsTree, Session};
pub use db_agent::{DbAgent, DbProvider};

/// The environment variable that names a shell (`sh`, `dash`) to read every fixture location
/// through, with the shell transport, instead of with `std::fs`. The sync tests run again
/// with it set to `sh` (PLAN §12).
pub const SHELL: &str = "SB_TEST_SHELL";

/// The environment variable that holds a command, as words, to run [`SHELL`]'s shell through:
/// one that starts a container whose userland the scripts are to be tried in, with the temp
/// dir mounted where it is on this machine.
pub const SHELL_VIA: &str = "SB_TEST_SHELL_VIA";

/// The shells to run the shell transport's scripts in: the one [`SHELL`] names, or else those
/// of `sh`, `bash`, `dash`, `zsh` and `ksh` that this machine has.
pub fn shells() -> Vec<String> {
    if let Ok(shell) = std::env::var(SHELL) {
        return vec![shell];
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    ["sh", "bash", "dash", "zsh", "ksh"]
        .into_iter()
        .filter(|shell| std::env::split_paths(&path).any(|dir| dir.join(shell).is_file()))
        .map(str::to_string)
        .collect()
}

/// A launcher that runs `shell` on this machine with `home` as its home folder and `vars`,
/// and nothing else of this process's environment but its `PATH`.
pub fn loopback(shell: &str, home: &Path, vars: &[(String, String)]) -> Launcher {
    let via = std::env::var(SHELL_VIA).unwrap_or_default();
    let mut words = via
        .split_whitespace()
        .map(str::to_string)
        .chain([
            "env".to_string(),
            "-i".to_string(),
            format!("PATH={}", std::env::var("PATH").unwrap()),
            format!("HOME={}", home.to_str().unwrap()),
        ])
        .chain(vars.iter().map(|(name, value)| format!("{name}={value}")))
        .chain([shell.to_string(), "-s".to_string()]);
    Launcher::Command {
        program: words.next().unwrap().into(),
        args: words.collect(),
    }
}

/// A location in a temp dir: this machine's files, with a home folder and an environment of
/// the test's choosing in place of the real ones.
#[derive(Clone, Debug)]
pub struct HomeTransport {
    home: String,
    vars: Vec<(String, String)>,
    /// The same location through a shell, when it is read that way.
    shell: Option<ShellTransport>,
}

impl HomeTransport {
    /// The location, read with `std::fs`, or through the shell [`SHELL`] names.
    pub fn new(home: &Path, vars: Vec<(String, String)>) -> Self {
        match std::env::var(SHELL) {
            Ok(shell) => Self::through(&shell, home, vars),
            Err(_) => Self {
                home: home.to_str().unwrap().to_string(),
                vars,
                shell: None,
            },
        }
    }

    /// The location, read through the shell transport by `shell`.
    pub fn through(shell: &str, home: &Path, vars: Vec<(String, String)>) -> Self {
        Self {
            home: home.to_str().unwrap().to_string(),
            shell: Some(ShellTransport::new(loopback(shell, home, &vars))),
            vars,
        }
    }

    /// The transport every operation but `env` goes to.
    fn inner(&self) -> &dyn Transport {
        match &self.shell {
            Some(shell) => shell,
            None => &LocalTransport,
        }
    }
}

impl Transport for HomeTransport {
    fn env(&self, names: &[&str]) -> Result<Env, TransportError> {
        if let Some(shell) = &self.shell {
            return shell.env(names);
        }
        Ok(Env {
            home: self.home.clone(),
            vars: self
                .vars
                .iter()
                .filter(|(name, _)| names.contains(&name.as_str()))
                .cloned()
                .collect(),
        })
    }

    fn stat(&self, paths: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        self.inner().stat(paths)
    }

    fn list(&self, root: &str) -> Result<Listing, TransportError> {
        self.inner().list(root)
    }

    fn hash_prefixes(
        &self,
        requests: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError> {
        self.inner().hash_prefixes(requests)
    }

    fn read_ranges(
        &self,
        requests: &[RangeRequest],
        sink: &mut dyn RangeSink,
    ) -> Result<(), TransportError> {
        self.inner().read_ranges(requests, sink)
    }

    fn read_small(&self, path: &str) -> Result<Option<Vec<u8>>, TransportError> {
        self.inner().read_small(path)
    }
}

/// An agent's data at a location in a temp dir, and the changes an agent makes to it. Sessions
/// are named by the key of their transcript item.
pub trait Agent {
    /// The provider that reads this agent.
    fn provider(&self) -> Box<dyn Provider>;

    /// The location the data is at.
    fn transport(&self) -> HomeTransport;

    /// A new session of `records` records. Returns its transcript's key.
    fn add_session(&mut self, records: usize) -> String;

    /// The agent goes on with the session: `records` more records at its end.
    fn append(&mut self, key: &str, records: usize);

    /// The session's first record changes, and it gains one: earlier bytes differ, and it is
    /// no shorter than before.
    fn rewrite(&mut self, key: &str);

    /// The session's first record changes and its length does not.
    fn rewrite_same_size(&mut self, key: &str);

    /// The session loses its last `records` records.
    fn truncate(&mut self, key: &str, records: usize);

    /// The session's stamp changes and its bytes don't.
    fn touch(&mut self, key: &str);

    /// The agent deletes the session.
    fn delete(&mut self, key: &str);

    /// A session of `records` records appears under the key of one that was deleted.
    fn recreate(&mut self, key: &str, records: usize);

    /// The bytes the item `key` holds at the source now.
    fn content(&self, key: &str) -> Vec<u8>;
}

const FAIL_COMMITS: &str = "fail_segment_commits";

/// Makes every commit of a segment to the catalogue of the archive in `dir` fail, as a crash
/// between a segment's rename into place and its commit would leave things (PLAN §4.1).
pub fn break_segment_commits(dir: &Path) {
    edit_catalogue(
        dir,
        &format!(
            "CREATE TRIGGER {FAIL_COMMITS} BEFORE INSERT ON segment
             BEGIN SELECT RAISE(ABORT, 'the test stops this commit'); END"
        ),
    );
}

/// Undoes [`break_segment_commits`].
pub fn mend_segment_commits(dir: &Path) {
    edit_catalogue(dir, &format!("DROP TRIGGER {FAIL_COMMITS}"));
}

/// Runs `sql` on the catalogue of the archive in `dir`, behind the archive's back: how a test
/// puts the catalogue in a state the app never leaves it in.
pub fn edit_catalogue(dir: &Path, sql: &str) {
    Connection::open(dir.join("scrollback.db"))
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}
