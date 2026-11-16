//! [`ShellTransport`]: a WSL distro or a host over SSH, read by running POSIX `sh` scripts
//! there with the tools every such system has (PLAN D6, §6.2). Nothing is installed at the
//! location and nothing is written there.
//!
//! **One call, one process.** Each operation starts the [`Launcher`]'s `sh -s` and writes it
//! a script: `shell.sh`, which defines a function per operation and documents the answer's
//! records, then the requests as calls of those ([`script`]). A pass over a location where
//! nothing changed is four processes (the environment, whether the agent is there, its
//! settings, the listing), and a fifth for a WSL distro's `/etc/wsl.conf`; each batch it
//! fetches adds one for the stream and one for the proofs.
//!
//! **Requests are shell words.** A path is written between single quotes, where a POSIX shell
//! reads every character as itself, a quote in it as `'\''`. So a name with spaces, newlines,
//! quotes or a leading dash is one argument, whatever shell runs the script, and nothing but
//! the launcher's fixed arguments ever crosses a command line.
//!
//! **The answer is read without trusting names or file contents** ([`reply`], [`parse`]):
//! names end at a NUL, a file's bytes come in a block whose length is known before it, and the
//! tools' own output is taken apart here, not in the shell. Whether a file is missing is read
//! in what `stat` or the reading tool said of it: only "No such file or directory" is a file
//! that is gone, and anything else is an error, as it is on this machine.
//!
//! **A file that shrinks or goes while it is read** fills its block with zeros after a mark,
//! so the stream stays in step and the range is reported short. What a range held is never
//! taken on trust anyway: the engine has the source hash it afterwards (PLAN §4.2).
//!
//! **Many small files cost one process, not several each.** Whole files asked for one after
//! another are read with one `cat`, cut apart by the sizes one `stat` gave just before, and
//! whole files to be proven are hashed by one `sha256sum`, in the order asked. A group is
//! taken only when it holds together (the bytes add up; every hash line names its file, which
//! is still as long as its prefix); otherwise its files are asked for again one by one. The
//! engine's proof is as it was: a range is stored only when the location's own hash of the
//! file's start equals the hash of what was fetched.
//!
//! **Modification times** are in nanoseconds where the location's `stat` gives them (GNU
//! coreutils, BSD, macOS) and whole seconds otherwise (busybox). Whole seconds are safe: a
//! file's stamp is trusted only once it is two seconds old ([`crate::provider::FileTree`]).
//! The listing's clock is the location's own, to the nanosecond where its `date` can tell,
//! and otherwise in whole seconds, rounded down, which only makes a file look younger.
//!
//! **The environment** a command run by `wsl.exe --exec` or `ssh host command` gets is bare:
//! no profile is read. So the variables a provider asks about are asked of the user's login
//! shell (`$SHELL -lc`), which reads the profile, and of the bare environment only when the
//! login shell has no answer: there is none, it takes no `-lc` (csh and tcsh), or its profile
//! fails. One that is still running after a few seconds is killed and the call fails: the
//! bare environment could name another place than the profile would have. What only an
//! interactive shell's startup file sets is still not seen. The answer is kept for the life
//! of the [`ShellTransport`], which is one pass: the login shell is asked once in it.
//!
//! **A call that hears nothing** from its launcher for [`IDLE`] is given up on: a `find` on a
//! network mount that hangs, a `wsl.exe` that stalls. `ssh` itself notices a host that stops
//! answering much sooner ([`Launcher`]).
//!
//! **An answer whose tail is lost** ([`TransportError::Stopped`]: it began, the launcher ended
//! with exit status 0 and said nothing, and its end never arrived) has the operation run once
//! more, with a warning logged, where nothing of the answer reached the caller before its end
//! was read: `env`, `probe`, `stat`, `list`, `hash_prefixes` and `read_small` each keep what
//! they read to themselves until then ([`ShellTransport::whole`]). `read_ranges` hands each
//! range to its sink as it arrives and is never run again: the range it stopped in fails.
//!
//! **What the transport can't see.** A group's mark is one byte of evidence that its files
//! were as long as `stat` said. At a location whose `stat` and `read` disagree (a network
//! mount with a stale attribute cache), a grouped read can hand over bytes cut at the wrong
//! places; the engine's proof then rejects them, and nothing wrong is stored.

mod launch;
mod parse;
mod reply;
mod script;

use std::collections::HashMap;
use std::io;
use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use toolbox_process::CancelToken;

use super::local::ns_since_epoch;
use super::{
    Env, FileStat, Halt, Hashed, Listing, MAY_NOT_BE_READ, PrefixRequest, RangeFailure,
    RangeRequest, RangeSink, Transport, TransportError, Unlisted, wsl,
};
use parse::{Trouble, clock, entry, not_unicode, number, parse_stat, range};
use reply::{END, Reply, unexpected};
use script::{Asked, Login, Script};

pub use launch::{Launcher, quoted};

/// How long a launcher may say nothing at all, on either stream, before its call is given
/// up on. Output comes per file, so the longest silence a healthy location has is one file
/// being hashed: ten minutes is a prefix of some tens of gigabytes on a slow disk.
const IDLE: Duration = Duration::from_secs(600);

/// How long the user's login shell may take to say what its profile sets, in seconds.
const LOGIN_SECONDS: u32 = 5;

/// How many names a listing asks about one by one in a single call, when `stat` answered for
/// fewer files than `find` named.
const ASK_AGAIN: usize = 256;

/// The most files read or hashed as one group, and the most bytes of their paths: a group's
/// paths are one command line at the location.
const GROUP_FILES: usize = 256;
const GROUP_PATHS: usize = 96 * 1024;

/// The largest file read in a group, and the most bytes a group asks for. A group's bytes are
/// held here until they are seen to add up, with [`GROUP_SLACK`] more for files that grew
/// since they were scanned.
const GROUP_FILE: u64 = 2 * 1024 * 1024;
const GROUP_BYTES: u64 = 16 * 1024 * 1024;
const GROUP_SLACK: u64 = 1024 * 1024;

/// The largest offset or length sent to the shell, whose arithmetic is signed: no file is as
/// long, so a request for more reads and ends as it would have.
const FAR: u64 = 1 << 62;

/// What a connection test found at a location whose shell answered (DESIGN §4.12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// The user's home directory there; empty when the shell could not tell.
    pub home: String,
    /// The location's clock, in nanoseconds since the Unix epoch.
    pub clock_ns: i64,
    /// How far that clock is ahead of this machine's, in nanoseconds: taken when its reading
    /// arrived, so the rest of the test is not counted in it.
    pub clock_ahead_ns: i64,
    /// The tools the transport needs that the location lacks. With any, it can't be synced.
    pub missing: Vec<String>,
    /// Whether files' modification times come in whole seconds only.
    pub whole_seconds: bool,
    /// Where the `sh` that answered is found at the location (`/bin/sh`).
    pub shell: String,
    /// The user's login shell, when it answered: the variables its profile sets are seen.
    /// `None` when it did not, and only the bare environment of a command is: always so
    /// for csh and tcsh, which take no `-lc`.
    pub login_shell: Option<String>,
    /// Where a WSL distro mounts the Windows drives, when its `/etc/wsl.conf` says it is not
    /// `/mnt/` (PLAN §6.3), with a slash at each end. `None` anywhere else. A sync reads
    /// the same file, and records what it says for the location.
    pub drive_mount: Option<String>,
    /// How long the test took, from starting the launcher to the end of the answer.
    pub took: Duration,
}

/// A location behind a [`Launcher`] (the module docs). Made anew for each pass: what it
/// learns of the location's environment it keeps.
#[derive(Clone, Debug)]
pub struct ShellTransport {
    launcher: Launcher,
    cancel: CancelToken,
    idle: Duration,
    environment: Arc<Mutex<Environment>>,
}

/// What the location's environment was found to be so far.
#[derive(Debug, Default)]
struct Environment {
    home: Option<String>,
    /// The login shell had no answer: the bare environment is all there is to ask.
    silent: bool,
    /// The variables asked about, and what each is set to.
    vars: HashMap<String, Option<String>>,
}

/// The login shell was killed before it answered.
fn late() -> TransportError {
    TransportError::LoginShellHung(LOGIN_SECONDS)
}

impl ShellTransport {
    pub fn new(launcher: Launcher) -> Self {
        Self {
            launcher,
            cancel: CancelToken::new(),
            idle: IDLE,
            environment: Arc::default(),
        }
    }

    /// Makes cancelling `cancel` kill whatever the transport is running, with everything it
    /// started; the call it was in fails with [`TransportError::Cancelled`].
    pub fn cancelled_by(mut self, cancel: &CancelToken) -> Self {
        self.cancel = cancel.clone();
        self
    }

    /// What starts the location's shell: to show the command that was run.
    pub fn launcher(&self) -> &Launcher {
        &self.launcher
    }

    /**
     * The connection test: whether the launcher reaches a shell, and what the transport would
     * find there. A location that can't be reached, or whose launcher fails, is an error; one
     * that lacks tools answers, and names them in [`Probe::missing`]. A login shell that had
     * to be stopped is an error, as it is to a pass.
     */
    pub fn probe(&self) -> Result<Probe, TransportError> {
        self.whole(|| self.probe_once())
    }

    /// [`Self::probe`], run once. What it read is its own until the answer's end.
    fn probe_once(&self) -> Result<Probe, TransportError> {
        let started = Instant::now();
        let login = Login::new();
        let script = Script::new().line("probe").login(&login, &[]);
        let (mut reply, missing) = self.start(&script)?;
        reply.field("H")?;
        let home = reply.words()?;
        let clock_ns = clock(&reply.field("C")?)?;
        let clock_ahead_ns = clock_ns.saturating_sub(ns_since_epoch(SystemTime::now()));
        let whole_seconds = reply.field("N")? == "0";
        reply.field("L")?;
        let shell = reply.words()?;
        let login_shell = reply.words()?;
        /* A file that is not there, or can't be read, sets nothing. */
        let drive_mount = small(&mut reply, wsl::CONF)
            .ok()
            .flatten()
            .and_then(|conf| wsl::automount_root(&String::from_utf8_lossy(&conf)));
        let answered = match login.read(&mut reply)? {
            Asked::Answered(_) => true,
            Asked::Silent => false,
            Asked::Late => return Err(late()),
        };
        reply.end()?;
        self.environment().silent = !answered;
        Ok(Probe {
            home,
            clock_ns,
            clock_ahead_ns,
            missing,
            whole_seconds,
            shell,
            login_shell: answered.then_some(login_shell),
            drive_mount,
            took: started.elapsed(),
        })
    }

    /**
     * Runs `call` once more when its answer stopped part-way (the module docs). Only for a
     * call that keeps everything it reads to itself until it has read its answer's end: run
     * again, it starts over from nothing, and nothing reaches its caller twice.
     */
    fn whole<T>(
        &self,
        mut call: impl FnMut() -> Result<T, TransportError>,
    ) -> Result<T, TransportError> {
        match call() {
            Err(TransportError::Stopped(first)) if !self.cancel.is_cancelled() => {
                log::warn!(
                    "{}: the answer stopped part-way ({first}); the call is run once more",
                    self.launcher.command_line()
                );
                call().map_err(|second| match second {
                    TransportError::Stopped(said) => TransportError::Stopped(format!(
                        "{said}\nit was run twice; the first time: {first}"
                    )),
                    other => other,
                })
            }
            other => other,
        }
    }

    fn environment(&self) -> MutexGuard<'_, Environment> {
        self.environment
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `script` at the location and reads its answer up to the first record.
    fn start(&self, script: &Script) -> Result<(Reply<'_>, Vec<String>), TransportError> {
        Reply::start(
            &self.launcher,
            script.text().into_bytes(),
            script.marker(),
            &self.cancel,
            self.idle,
        )
    }

    /// Runs `script`, which needs `tools`, and reads its answer up to the first record.
    fn ask(&self, tools: &str, script: Script) -> Result<Reply<'_>, TransportError> {
        let (reply, missing) = self.start(&script.needing(tools))?;
        if missing.is_empty() {
            Ok(reply)
        } else {
            Err(TransportError::MissingTools(missing))
        }
    }

    /// Asks the location for its home folder and the variables `names`, and adds them to
    /// what is `known` of its environment.
    fn learn(&self, known: &mut Environment, names: &[&str]) -> Result<(), TransportError> {
        let login = Login::new();
        let ask_login = !names.is_empty() && !known.silent;
        let mut script = Script::new().line("home");
        if ask_login {
            script = script.login(&login, names);
        }
        for name in names {
            script = script.line(&format!("var {name}"));
        }
        let mut reply = self.ask("", script)?;
        reply.field("H")?;
        let home = String::from_utf8(reply.text()?)
            .ok()
            .filter(|home| home.starts_with('/'))
            .ok_or(TransportError::NoHome)?;
        /* What the login shell has of them, when it answered; else what a command gets. */
        let logged_in = if ask_login {
            match login.read(&mut reply)? {
                Asked::Answered(vars) => Some(vars),
                Asked::Silent => None,
                Asked::Late => return Err(late()),
            }
        } else {
            None
        };
        let mut bare = HashMap::new();
        loop {
            let line = reply.line()?;
            let Some(name) = line.strip_prefix("V ") else {
                if line != END {
                    return Err(unexpected("V", &line));
                }
                break;
            };
            /* As on this machine, a value that is not Unicode counts as not set. */
            if let Ok(value) = String::from_utf8(reply.text()?) {
                bare.insert(name.to_string(), value);
            }
        }
        known.home = Some(home);
        known.silent |= ask_login && logged_in.is_none();
        let mut vars = logged_in.unwrap_or(bare);
        for name in names {
            known.vars.insert((*name).to_string(), vars.remove(*name));
        }
        Ok(())
    }

    /**
     * Asks about each of `names` by itself, without following a symbolic link: for names a
     * listing could not match to what `stat` said of them. One that is gone since `find` saw
     * it is left out; one that `stat` can't look at is unlisted, unless it is in one of the
     * `barred` folders, which the listing has reported.
     */
    fn list_again(
        &self,
        names: &[String],
        root: &str,
        barred: &[String],
        listing: &mut Listing,
    ) -> Result<(), TransportError> {
        for names in names.chunks(ASK_AGAIN) {
            let mut script = Script::new();
            for name in names {
                script = script.call("file P", &[], name)?;
            }
            let mut reply = self.ask("stat", script)?;
            for name in names {
                let code = reply.field("S")?;
                let said = reply.words()?;
                if code == "0" {
                    entry(listing, root, name.clone().into_bytes(), &said)?;
                    continue;
                }
                let covered = barred.iter().any(|folder| {
                    name.strip_prefix(folder)
                        .is_some_and(|rest| rest.starts_with('/'))
                });
                match Trouble::of(&said) {
                    Trouble::Gone => {}
                    _ if covered => {}
                    trouble => listing.unlisted.push(Unlisted {
                        path: name.clone(),
                        reason: trouble.reason(),
                    }),
                }
            }
            reply.end()?;
        }
        Ok(())
    }

    /**
     * Reads the ranges of `requests` from `from` on, the small whole files among them in
     * groups (the module docs), but those before `plain` one by one. `None` when all of them
     * arrived; otherwise the group that did not hold together, from whose first request the
     * caller starts again with its files asked for one by one. Nothing of that group was
     * passed on.
     */
    fn read_from(
        &self,
        requests: &[RangeRequest],
        from: usize,
        plain: usize,
        sink: &mut dyn RangeSink,
    ) -> Result<Option<Range<usize>>, TransportError> {
        let groups = groups(from..requests.len(), |run: &Range<usize>, next| {
            let request = &requests[next];
            let asked = |run: &Range<usize>| -> u64 {
                requests[run.clone()]
                    .iter()
                    .map(|request| request.length)
                    .sum()
            };
            next >= plain
                && request.offset == 0
                && (1..=GROUP_FILE).contains(&request.length)
                && asked(run) + request.length <= GROUP_BYTES
                && paths(requests[run.clone()].iter().map(|request| &request.path))
                    + request.path.len()
                    <= GROUP_PATHS
        });
        let mut script = Script::new();
        for group in &groups {
            let requests = &requests[group.clone()];
            script = match requests {
                [request] => script.call(
                    "range",
                    &[request.offset.min(FAR), request.length.min(FAR)],
                    &request.path,
                )?,
                _ => {
                    let asked: u64 = requests.iter().map(|request| request.length).sum();
                    script.group(
                        "bulk",
                        &[
                            (asked + GROUP_SLACK).to_string(),
                            lengths(requests.iter().map(|request| request.length)),
                        ],
                        requests.iter().map(|request| request.path.as_str()),
                    )?
                }
            };
        }
        let mut reply = self.ask("stat head tail dd cat /dev/zero", script)?;
        let halted = |Halt| TransportError::Halted;
        for group in groups {
            let mut one_by_one = |reply: &mut Reply<'_>| -> Result<(), TransportError> {
                for index in group.clone() {
                    let result = range(reply, requests[index].length, &mut |bytes| {
                        sink.data(index, bytes)
                    })?;
                    sink.end(index, result).map_err(halted)?;
                }
                Ok(())
            };
            if group.len() == 1 {
                one_by_one(&mut reply)?;
                continue;
            }
            let line = reply.line()?;
            let ["G", count, total] = line.split(' ').collect::<Vec<_>>()[..] else {
                return Err(unexpected("G", &line));
            };
            /* The location did not take them as a group: here they are one by one. */
            if count == "0" {
                one_by_one(&mut reply)?;
                continue;
            }
            let total = number::<u64>(total)?;
            let asked: u64 = requests[group.clone()]
                .iter()
                .map(|request| request.length)
                .sum();
            if number::<usize>(count)? != group.len() || total > asked + GROUP_SLACK {
                return Err(unexpected("G for the group asked for", &line));
            }
            let before = group
                .clone()
                .map(|_| reply.line())
                .collect::<Result<Vec<String>, TransportError>>()?;
            let mut block = Vec::new();
            reply.block(total + 1, &mut |bytes| {
                block.extend_from_slice(bytes);
                Ok(())
            })?;
            let mut later = Vec::new();
            loop {
                let line = reply.line()?;
                if line == "." {
                    break;
                }
                later.push(line);
            }
            let sizes = before
                .iter()
                .map(|line| Ok(parse_stat(line)?.1.size))
                .collect::<Result<Vec<u64>, TransportError>>()?;
            /* The files' bytes, the mark, and nothing more, from files that are afterwards
            as they were before: only then are the sizes where one file ends and the next
            begins. */
            if sizes.iter().sum::<u64>() != total
                || block.last() != Some(&parse::MARK)
                || later != before
            {
                return Ok(Some(group));
            }
            let mut rest = &block[..block.len() - 1];
            for (index, size) in group.zip(sizes) {
                let (file, after) = rest.split_at(size as usize);
                rest = after;
                let length = requests[index].length;
                /* A file that has grown since it was scanned gives the start asked for. */
                let wanted = &file[..file
                    .len()
                    .min(usize::try_from(length).unwrap_or(usize::MAX))];
                sink.data(index, wanted).map_err(halted)?;
                let result = if size < length {
                    Err(RangeFailure::Short {
                        expected: length,
                        got: size,
                    })
                } else {
                    Ok(())
                };
                sink.end(index, result).map_err(halted)?;
            }
        }
        reply.end()?;
        Ok(None)
    }

    /**
     * Hashes the prefixes of `requests` from the first that `hashes` lacks, whole files in
     * groups where the location finds them whole (the module docs), but those before `plain`
     * one by one. Returns the end of a group that did not hold together, from whose first
     * request the caller starts again, or the number of requests when all are hashed.
     */
    fn hash_from(
        &self,
        requests: &[PrefixRequest],
        hashes: &mut Vec<Result<Hashed, RangeFailure>>,
        plain: usize,
    ) -> Result<usize, TransportError> {
        /* The hashing tool prints a name with a newline or a backslash in it another way. */
        let plainly_named = |path: &str| !path.contains(['\n', '\\']);
        let groups = groups(hashes.len()..requests.len(), |run: &Range<usize>, next| {
            let request = &requests[next];
            next >= plain
                && request.length > 0
                && plainly_named(&request.path)
                && paths(requests[run.clone()].iter().map(|request| &request.path))
                    + request.path.len()
                    <= GROUP_PATHS
        });
        let mut script = Script::new();
        for group in groups {
            let requests = &requests[group];
            script = match requests {
                [request] => script.call("prefix", &[request.length.min(FAR)], &request.path)?,
                _ => script.group(
                    "wholes",
                    &[lengths(
                        requests.iter().map(|request| request.length.min(FAR)),
                    )],
                    requests.iter().map(|request| request.path.as_str()),
                )?,
            };
        }
        let mut reply = self.ask("head stat sha256", script)?;
        while hashes.len() < requests.len() {
            let line = reply.line()?;
            match line.split(' ').collect::<Vec<_>>()[..] {
                ["P", size] => {
                    let said = reply.words()?;
                    hashes.push(parse::prefix(&requests[hashes.len()], size, &said));
                }
                ["W", count] => {
                    let run = hashes.len()..hashes.len() + number::<usize>(count)?;
                    let sums = reply.words()?;
                    let sizes = reply.words()?;
                    if run.end > requests.len() {
                        return Err(unexpected("W for the files asked for", &line));
                    }
                    match parse::wholes(&requests[run.clone()], &sums, &sizes) {
                        Some(run) => hashes.extend(run.into_iter().map(Ok)),
                        None => return Ok(run.end),
                    }
                }
                _ => return Err(unexpected("P or W", &line)),
            }
        }
        reply.end()?;
        Ok(requests.len())
    }
}

impl Transport for ShellTransport {
    fn env(&self, names: &[&str]) -> Result<Env, TransportError> {
        /* Any other name is not a variable's, and is not set. */
        let names: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| script::is_identifier(name))
            .collect();
        /* Held across the call: whoever else asks meanwhile wants the same answer. */
        let mut known = self.environment();
        let new: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| !known.vars.contains_key(*name))
            .collect();
        if known.home.is_none() || !new.is_empty() {
            /* `known` is changed only once the answer's end is read. */
            self.whole(|| self.learn(&mut known, &new))?;
        }
        Ok(Env {
            home: known.home.clone().ok_or(TransportError::NoHome)?,
            vars: names
                .iter()
                .filter_map(|name| Some(((*name).to_string(), known.vars.get(*name)?.clone()?)))
                .collect(),
        })
    }

    fn stat(&self, paths: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        self.whole(|| self.stat_once(paths))
    }

    fn list(&self, root: &str) -> Result<Listing, TransportError> {
        /* With the names asked about again: the listing is handed over whole, or not. */
        self.whole(|| self.list_once(root))
    }

    fn hash_prefixes(
        &self,
        requests: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError> {
        self.whole(|| {
            let mut hashes = Vec::with_capacity(requests.len());
            let mut plain = 0;
            while hashes.len() < requests.len() {
                plain = self.hash_from(requests, &mut hashes, plain)?;
            }
            Ok(hashes)
        })
    }

    /// Never run again: each range goes to `sink` as it arrives, and one that stops part-way
    /// fails the call there.
    fn read_ranges(
        &self,
        requests: &[RangeRequest],
        sink: &mut dyn RangeSink,
    ) -> Result<(), TransportError> {
        let (mut from, mut plain) = (0, 0);
        while let Some(group) = self.read_from(requests, from, plain, sink)? {
            (from, plain) = (group.start, group.end);
        }
        Ok(())
    }

    fn read_small(&self, path: &str) -> Result<Option<Vec<u8>>, TransportError> {
        self.whole(|| {
            let script = Script::new().call("small", &[], path)?;
            let mut reply = self.ask("stat head /dev/zero", script)?;
            let content = small(&mut reply, path)?;
            reply.end()?;
            Ok(content)
        })
    }
}

/* What the operations that are run again whole read, each once: nothing reaches the caller
before the answer's end is read. */
impl ShellTransport {
    fn stat_once(&self, paths: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        let mut script = Script::new();
        for path in paths {
            script = script.call("file L", &[], path)?;
        }
        let mut reply = self.ask("stat", script)?;
        let mut stats = Vec::new();
        for path in paths {
            let code = reply.field("S")?;
            let said = reply.words()?;
            stats.push(match (code.as_str(), Trouble::of(&said)) {
                ("0", _) => Some(parse_stat(&said)?.1),
                /* A link to nothing; or stat said there is no such file. */
                ("1", _) | (_, Trouble::Gone) => None,
                (_, trouble) => return Err(remote("stat", path, trouble.reason())),
            });
        }
        reply.end()?;
        Ok(stats)
    }

    fn list_once(&self, root: &str) -> Result<Listing, TransportError> {
        /* find names what it finds as the folder, a slash and the rest. */
        let root = match root.trim_end_matches('/') {
            "" => "/",
            trimmed => trimmed,
        };
        let mut reply = self.ask("find stat date", Script::new().call("list", &[], root)?)?;
        let mut listing = Listing {
            files: Vec::new(),
            unlisted: Vec::new(),
            skipped: Vec::new(),
            clock_ns: clock(&reply.field("C")?)?,
        };
        /* Names of batches in which stat answered for fewer files than find named. */
        let mut again = Vec::new();
        /* The folders reported as ones this user may not read. */
        let mut barred = Vec::new();
        loop {
            let line = reply.line()?;
            let fields: Vec<&str> = line.split(' ').collect();
            match fields[..] {
                ["B", count] => {
                    let count = number::<usize>(count)?;
                    let names = (0..count)
                        .map(|_| reply.text())
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut stats = Vec::new();
                    loop {
                        let line = reply.line()?;
                        if line == "." {
                            break;
                        }
                        stats.push(line);
                    }
                    if stats.len() == count {
                        for (name, stat) in names.into_iter().zip(&stats) {
                            entry(&mut listing, root, name, stat)?;
                        }
                    } else {
                        for name in names {
                            match String::from_utf8(name) {
                                Ok(name) => again.push(name),
                                Err(error) => not_unicode(&mut listing, error.as_bytes()),
                            }
                        }
                    }
                }
                ["U"] => {
                    let folder = String::from_utf8_lossy(&reply.text()?).into_owned();
                    listing.unlisted.push(Unlisted {
                        path: folder.clone(),
                        reason: MAY_NOT_BE_READ.to_string(),
                    });
                    barred.push(folder);
                }
                ["F", status] => {
                    let said = reply.words()?;
                    /* With folders that may not be read named already, find's complaint is
                    of them. */
                    if status != "0" && barred.is_empty() {
                        listing.unlisted.push(Unlisted {
                            path: root.to_string(),
                            reason: match Trouble::of(&said) {
                                Trouble::Nothing => format!("find failed ({status})"),
                                trouble => trouble.reason(),
                            },
                        });
                    }
                    break;
                }
                _ => return Err(unexpected("B, U or F", &line)),
            }
        }
        reply.end()?;
        self.list_again(&again, root, &barred, &mut listing)?;
        Ok(listing)
    }
}

/// Reads what `small` in `shell.sh` answered for the file at `path`: the file whole, or
/// `None` when it is not there.
fn small(reply: &mut Reply<'_>, path: &str) -> Result<Option<Vec<u8>>, TransportError> {
    let found = reply.line()?;
    match found.split(' ').collect::<Vec<_>>()[..] {
        ["M", "0", size] => {
            let mut content = Vec::new();
            let read = range(reply, number(size)?, &mut |bytes| {
                content.extend_from_slice(bytes);
                Ok(())
            })?;
            match read {
                /* Shorter than it was a moment ago: this is the whole of it now. */
                Ok(()) | Err(RangeFailure::Short { .. }) => Ok(Some(content)),
                Err(RangeFailure::Missing) => Ok(None),
                Err(RangeFailure::Unreadable(why)) => Err(remote("read", path, why)),
            }
        }
        ["M", "2"] => match Trouble::of(&reply.words()?) {
            Trouble::Gone => Ok(None),
            trouble => Err(remote("read", path, trouble.reason())),
        },
        _ => Err(unexpected("M", &found)),
    }
}

/**
 * Cuts `indexes` into runs of requests to ask the location for as one group: `joins` says
 * whether the request at an index may join the run before it (empty for a run's first). A
 * request that may not is a run of its own, as is one whose run found no second.
 */
fn groups(
    indexes: Range<usize>,
    joins: impl Fn(&Range<usize>, usize) -> bool,
) -> Vec<Range<usize>> {
    let mut groups: Vec<Range<usize>> = Vec::new();
    /* Whether the last group is one that others may join. */
    let mut open = false;
    for index in indexes {
        match groups.last_mut() {
            Some(last) if open && last.len() < GROUP_FILES && joins(last, index) => {
                last.end += 1;
            }
            _ => {
                open = joins(&(index..index), index);
                groups.push(index..index + 1);
            }
        }
    }
    groups
}

/// How many bytes `paths` are, as arguments of one command.
fn paths<'a>(paths: impl Iterator<Item = &'a String>) -> usize {
    paths.map(|path| path.len() + 1).sum()
}

/// `lengths` as one word of a request: each with a space after it.
fn lengths(lengths: impl Iterator<Item = u64>) -> String {
    lengths.map(|length| format!("{length} ")).collect()
}

/// A tool at the location could not do `action` to `path`, and `said` why.
fn remote(action: &'static str, path: &str, said: String) -> TransportError {
    TransportError::Io {
        action,
        path: path.to_string(),
        source: io::Error::other(said),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn requests_are_grouped_in_order_and_a_group_has_its_limits() {
        /* Even numbers may join; a run is at most so many files. */
        let even = |_: &Range<usize>, next: usize| next.is_multiple_of(2) || next > 10;
        assert_eq!(
            groups(0..14, even),
            [
                0..1,
                1..2,
                2..3,
                3..4,
                4..5,
                5..6,
                6..7,
                7..8,
                8..9,
                9..10,
                10..14
            ]
        );
        assert_eq!(groups(3..3, even), []);
        let all = groups(0..GROUP_FILES * 2 + 1, |_, _| true);
        assert_eq!(
            all,
            [
                0..GROUP_FILES,
                GROUP_FILES..GROUP_FILES * 2,
                GROUP_FILES * 2..GROUP_FILES * 2 + 1
            ]
        );
        /* What may join depends on the run so far. */
        let pairs = groups(0..5, |run, _| run.len() < 2);
        assert_eq!(pairs, [0..2, 2..4, 4..5]);
    }

    /// A transport whose shell is given a moment to answer in.
    #[cfg(unix)]
    fn impatient(line: &str, launcher: fn(String) -> Launcher) -> ShellTransport {
        ShellTransport {
            idle: Duration::from_millis(300),
            ..ShellTransport::new(launcher(line.to_string()))
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_launcher_that_says_nothing_for_too_long_is_given_up_on() {
        let command = |line: String| Launcher::Command {
            program: "sh".into(),
            args: vec!["-c".to_string(), line],
        };
        /* It never answers; or it answers, and then the tool it runs hangs. */
        for line in ["sleep 30", "sh -s; sleep 30"] {
            let started = Instant::now();
            let hung = impatient(line, command).stat(&["/"]);
            match line {
                "sleep 30" => assert!(
                    matches!(&hung, Err(TransportError::Failed(said)) if said.contains("no answer")),
                    "{hung:?}"
                ),
                /* The answer was whole before the silence. */
                _ => assert!(hung.is_ok(), "{hung:?}"),
            }
            assert!(started.elapsed() < Duration::from_secs(10));
        }
        /* While it has things to say, on either stream, it is not given up on. */
        let chatty = "for n in 1 2 3 4 5 6; do sleep 0.1; echo waiting >&2; done; exec sh -s";
        assert!(impatient(chatty, command).stat(&["/"]).is_ok());
    }
}
