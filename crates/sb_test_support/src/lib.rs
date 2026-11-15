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
//! - [`Items`]: a session's items as a parser reads them, fetched from an [`Agent`].
//!
//! gpui: **never** (`script/check` step 3).

mod claude_code;
mod db_agent;

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;

use sb_core::provider::{Hashed, ItemRange, Provider, RangeFailure, SessionItem, SessionItems};
use sb_core::transport::{
    Env, FileStat, Halt, Launcher, Listing, LocalTransport, PrefixRequest, RangeRequest, RangeSink,
    ShellTransport, Transport, TransportError,
};
use toolbox_store::rusqlite::Connection;
use toolbox_store::rusqlite::types::ValueRef;

pub use claude_code::{Call, FileAgent, ProjectsTree, Records, Session, image_block};
pub use db_agent::{DbAgent, DbProvider};

/// The environment variable that names a shell (`sh`, `dash`) to read every fixture location
/// through, with the shell transport, instead of with `std::fs`. The sync tests run again
/// with it set to `sh` (PLAN §12).
pub const SHELL: &str = "SB_TEST_SHELL";

/// The environment variable that holds a command, as words, to run [`SHELL`]'s shell through:
/// one that starts a container whose userland the scripts are to be tried in, with the temp
/// dir mounted where it is on this machine.
pub const SHELL_VIA: &str = "SB_TEST_SHELL_VIA";

/**
 * The environment variable that names a WSL distro to read every fixture location through,
 * for tests run inside that distro: each call leaves the distro for `wsl.exe` on the
 * Windows side and comes back in through it, as a WSL location's does, with the fixture's
 * temp dir for a home. It needs the distro to be able to run Windows programs (the default).
 * Without it nothing goes through `wsl.exe`.
 */
pub const WSL: &str = "SB_TEST_WSL";

/// Whether every fixture location is read through the shell transport ([`SHELL`], [`WSL`]).
pub fn through_a_shell() -> bool {
    std::env::var_os(SHELL).is_some() || std::env::var_os(WSL).is_some()
}

/// The shells to run the shell transport's scripts in: the one [`SHELL`] names, `sh` alone
/// through [`WSL`], or else those of `sh`, `bash`, `dash`, `zsh` and `ksh` that this machine
/// has.
pub fn shells() -> Vec<String> {
    if let Ok(shell) = std::env::var(SHELL) {
        return vec![shell];
    }
    if std::env::var_os(WSL).is_some() {
        return vec!["sh".to_string()];
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
    /* As `Launcher::Wsl` starts a distro's shell, with `env` in the shell's place. */
    let wsl = std::env::var(WSL)
        .ok()
        .map(|distro| ["wsl.exe", "-d", &distro, "--cd", "/", "--exec"].map(str::to_string));
    let mut words = via
        .split_whitespace()
        .map(str::to_string)
        .chain(wsl.into_iter().flatten())
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
    /// The location, read with `std::fs`, or through the shell [`SHELL`] names, or through
    /// `sh` in the distro [`WSL`] names.
    pub fn new(home: &Path, vars: Vec<(String, String)>) -> Self {
        match std::env::var(SHELL) {
            Ok(shell) => Self::through(&shell, home, vars),
            Err(_) if std::env::var_os(WSL).is_some() => Self::through("sh", home, vars),
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

/// One session's items, held in memory: what a parser reads in place of the archive.
pub struct Items {
    list: Vec<SessionItem>,
    bytes: HashMap<String, Vec<u8>>,
}

impl Items {
    /// The items of the session whose transcript is `transcript`, scanned and fetched through
    /// the agent's provider as a sync does.
    pub fn of(agent: &impl Agent, transcript: &str) -> Self {
        let transport = agent.transport();
        let provider = agent.provider();
        let scratch = tempfile::tempdir().unwrap();
        let mut source = provider
            .detect(&transport, scratch.path())
            .unwrap()
            .unwrap();
        let scanned = source.scan().unwrap().items;
        let session = &scanned
            .iter()
            .find(|item| item.key == transcript)
            .unwrap()
            .session;
        let mut list: Vec<SessionItem> = scanned
            .iter()
            .filter(|item| item.session == *session)
            .map(|item| SessionItem {
                key: item.key.clone(),
                kind: item.kind,
                length: item.length,
            })
            .collect();
        list.sort_by(|a, b| a.key.cmp(&b.key));
        let ranges: Vec<ItemRange> = list
            .iter()
            .map(|item| ItemRange {
                key: item.key.clone(),
                offset: 0,
                length: item.length,
            })
            .collect();
        let mut fetched = Fetched(vec![Vec::new(); list.len()]);
        source.fetch(&ranges, &mut fetched).unwrap();
        let bytes = list
            .iter()
            .map(|item| item.key.clone())
            .zip(fetched.0)
            .collect();
        Self { list, bytes }
    }

    /// Takes the item `key` away, as if it had not been archived.
    pub fn remove(&mut self, key: &str) {
        self.list.retain(|item| item.key != key);
        self.bytes.remove(key);
    }
}

impl SessionItems for Items {
    fn list(&self) -> &[SessionItem] {
        &self.list
    }

    fn open(&self, key: &str, offset: u64) -> io::Result<Box<dyn Read + '_>> {
        let bytes = self.bytes.get(key).ok_or(io::ErrorKind::NotFound)?;
        Ok(Box::new(&bytes[bytes.len().min(offset as usize)..]))
    }
}

/// The bytes of each fetched range.
struct Fetched(Vec<Vec<u8>>);

impl RangeSink for Fetched {
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
        self.0[request].extend_from_slice(bytes);
        Ok(())
    }

    fn end(&mut self, _request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        result.unwrap();
        Ok(())
    }
}

/// What `sql` reads from the catalogue of the archive in `dir`, behind the archive's back: the
/// first column of each row, as text. For what the catalogue keeps and has no call to say.
pub fn read_catalogue(dir: &Path, sql: &str) -> Vec<String> {
    let connection = Connection::open(dir.join("scrollback.db")).unwrap();
    let mut select = connection.prepare(sql).unwrap();
    select
        .query_map([], |row| {
            row.get_ref(0).map(|value| match value {
                ValueRef::Integer(number) => number.to_string(),
                ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned(),
                other => format!("{other:?}"),
            })
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
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
