//! [`Reply`]: the shell's answer to one call, read as it arrives from the launcher's process.
//!
//! The answer starts at a line only this call knows (whatever a login script printed before
//! it is passed over) and is a sequence of records: a line of ASCII fields, then, by its first
//! letter, text ended by a NUL or a block of as many bytes as the line says or the caller
//! asked for (`shell.sh`). Its last line is `Z`. An answer that ends without it is not one:
//! the launcher failed or the connection dropped, and the call fails with what was read
//! thrown away.
//!
//! The script is written to the launcher from a thread of its own while this one already
//! reads: a launcher that is slow to take it (`ssh` still connecting, `wsl.exe` starting a
//! distro), or that has a lot to say on stderr first, holds nothing up, and a cancel is seen
//! at once. Dropping the reply kills the launcher and everything it started, which also
//! releases a write that was still waiting. A launcher that ends before the answer began
//! says, in its error, whether the script was written to it whole.
//!
//! **A script that reaches the shell cut short is run once more.** The shell parses the whole
//! script before it runs any of it ([`super::script`]), so a script that arrived cut short
//! ran nothing, and the shell ends in one of two ways: with exit status 0 and not a word,
//! when nothing arrived at all, or with its own complaint of a script that stops in the
//! middle. A launcher that ends either way before the answer began has its call started over
//! once, and a warning is logged. Any other end is the call's failure, as it always was.

use std::fmt;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::time::{Duration, Instant};

use toolbox_process::{CancelToken, Event, Handle, ProcessError, Stop, spawn_streaming};

use super::Launcher;
use crate::transport::{Halt, TransportError};

/// How often a read that is waiting for the process looks at the cancel token and the clock.
const POLL: Duration = Duration::from_millis(50);

/// How many chunks of output wait here for the caller before the process is made to wait.
const QUEUE: usize = 8;

/// The longest line of fields a record starts with; a longer one is not a record.
const LINE_MAX: usize = 256;

/// The longest text a record carries: every complaint of a `find` over a large tree.
const TEXT_MAX: usize = 16 * 1024 * 1024;

/// How much is kept for an error message of what the launcher wrote besides the answer: the
/// start of its stdout, and the end of its stderr, where a launcher gives its verdict.
const KEPT: usize = 16 * 1024;

/// How much a launcher may print before the answer begins: a greeting, however long, is less.
const GREETING_MAX: usize = 1024 * 1024;

/// The answer's last line.
pub(super) const END: &str = "Z";

/// The script goes to the launcher in pieces this long, so that a write that fails can say
/// how far it got.
const PIECE: usize = 8 * 1024;

/// How long a launcher that has ended may keep its writer waiting before how the write went
/// is given up on: once the launcher is gone, a write fails at once.
const WRITTEN_WAIT: Duration = Duration::from_secs(1);

/// How much of what the launcher wrote to stderr goes in the warning of a call run again.
const LOGGED: usize = 1024;

/**
 * How a shell words a script that ends in the middle (lowercased): bash and dash say "syntax
 * error" or bash "unexpected EOF while looking for matching"; ksh "syntax error at line N:
 * `{' unmatched"; zsh "parse error near", "unmatched '" or "closing brace expected"; busybox
 * ash "syntax error: unexpected end of file (expecting "}")", "syntax error: unterminated
 * quoted string" or "syntax error: missing '}'". A script that arrived whole always parses.
 */
const UNFINISHED: [&str; 5] = [
    "syntax error",
    "unexpected eof",
    "parse error",
    "unmatched",
    "closing brace expected",
];

pub(super) struct Reply<'a> {
    /// Dropped before `process`, whose own thread may be waiting to send here.
    events: Receiver<Event>,
    /// Shared with the thread that writes the script. Killed when the reply is dropped.
    process: Arc<Handle>,
    launcher: &'a Launcher,
    cancel: &'a CancelToken,
    /// How long the process may say nothing at all before it is given up on.
    idle: Duration,
    /// Output not yet read, from `at` on.
    buffer: Vec<u8>,
    at: usize,
    /// The end of what it wrote to stderr.
    stderr: Vec<u8>,
    /// The start of what came before the answer began.
    before: Vec<u8>,
    /// Set once the process has ended: its exit status, if it could be waited for.
    exit: Option<Option<ExitStatus>>,
    /// How many bytes of the answer have arrived, and what the last line read was: for
    /// saying where an answer that stopped part-way stopped.
    answered: u64,
    last: String,
}

impl Drop for Reply<'_> {
    fn drop(&mut self) {
        /* The writing thread may still hold the process, waiting for it to read. */
        self.process.stop(Stop::Kill);
    }
}

impl<'a> Reply<'a> {
    /**
     * Starts the launcher, has `script` written to its stdin, and reads up to where the
     * records begin: past the line `marker`, and the script's word on the tools it needs.
     * Returns the tools that are missing with the reply. A launcher that ends as one does
     * when the script reached its shell cut short is started once more (the module docs).
     */
    pub(super) fn start(
        launcher: &'a Launcher,
        script: Vec<u8>,
        marker: &str,
        cancel: &'a CancelToken,
        idle: Duration,
    ) -> Result<(Self, Vec<String>), TransportError> {
        let script: Arc<[u8]> = script.into();
        /* Running a call again is sound only because nothing of it has reached the caller:
        the reply is handed over once its marker is read, and no sink is reachable from here
        before then. So nothing is delivered twice. */
        let first = match Self::attempt(launcher, &script, marker, cancel, idle)? {
            Ok(started) => return Ok(started),
            Err(unanswered) => unanswered,
        };
        let failure = first.failure(launcher);
        if !first.cut_short(&failure) {
            return Err(failure);
        }
        if cancel.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        log::warn!(
            "{} ended before it answered, as when the script reaches its shell cut short \
             ({}; {}), and is run once more; it said on stderr: {:?}",
            launcher.command_line(),
            first
                .exit
                .map_or("no exit status".to_string(), |exit| exit.to_string()),
            first.written,
            String::from_utf8_lossy(&first.stderr[first.stderr.len().saturating_sub(LOGGED)..]),
        );
        match Self::attempt(launcher, &script, marker, cancel, idle)? {
            Ok(started) => Ok(started),
            Err(second) => Err(match second.failure(launcher) {
                TransportError::Failed(said) => TransportError::Failed(format!(
                    "{said}\nit was run twice; the first time: {failure}"
                )),
                other => other,
            }),
        }
    }

    /// One run of the launcher with `script`: the reply, or how the launcher ended when it
    /// did so before the answer began.
    fn attempt(
        launcher: &'a Launcher,
        script: &Arc<[u8]>,
        marker: &str,
        cancel: &'a CancelToken,
        idle: Duration,
    ) -> Result<Result<(Self, Vec<String>), Unanswered>, TransportError> {
        /* Before anything is started: `wsl.exe` would start a distro that is stopped. */
        if cancel.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        launcher.check()?;
        let (tx, events) = sync_channel(QUEUE);
        let process = spawn_streaming(&launcher.command(), move |event| {
            let _ = tx.send(event);
        })
        .map(Arc::new)
        .map_err(|error| match error {
            ProcessError::Start { program, source } => TransportError::Launch {
                program: program.display().to_string(),
                source,
            },
            other => TransportError::Failed(other.to_string()),
        })?;
        let mut reply = Self {
            events,
            process: Arc::clone(&process),
            launcher,
            cancel,
            idle,
            buffer: Vec::new(),
            at: 0,
            stderr: Vec::new(),
            before: Vec::new(),
            exit: None,
            answered: 0,
            last: "the line it begins with".to_string(),
        };
        /* The script is all the shell is to read. A launcher that ends without reading it
        says why through its exit, and the writer says how far it got. */
        let (wrote, written) = sync_channel(1);
        let script = Arc::clone(script);
        std::thread::Builder::new()
            .name("sb-shell-in".into())
            .spawn(move || {
                let mut went = 0;
                let result = script.chunks(PIECE).try_for_each(|piece| {
                    process.write(piece)?;
                    went += piece.len();
                    Ok::<(), ProcessError>(())
                });
                process.close_stdin();
                let _ = wrote.send(match result {
                    Ok(()) => Written::Whole(went),
                    Err(error) => Written::Failed {
                        went,
                        of: script.len(),
                        error,
                    },
                });
            })
            .map_err(|error| TransportError::Failed(error.to_string()))?;

        let marker = format!("{marker}\n");
        loop {
            if let Some(found) = find(&reply.buffer, marker.as_bytes()) {
                reply.before = reply.buffer[..found.min(KEPT)].to_vec();
                reply.at = found + marker.len();
                reply.answered = (reply.buffer.len() - reply.at) as u64;
                break;
            }
            if reply.buffer.len() > GREETING_MAX {
                return Err(TransportError::Protocol(format!(
                    "it printed more than {GREETING_MAX} bytes without beginning its answer, \
                     starting {:?}",
                    String::from_utf8_lossy(&reply.buffer[..64])
                )));
            }
            if !reply.more()? {
                return Ok(Err(Unanswered {
                    exit: reply.exit.flatten(),
                    stderr: std::mem::take(&mut reply.stderr),
                    stdout: reply.buffer[..reply.buffer.len().min(KEPT)].to_vec(),
                    written: written
                        .recv_timeout(WRITTEN_WAIT)
                        .unwrap_or(Written::Unknown),
                }));
            }
        }
        let mut missing = Vec::new();
        loop {
            let line = reply.line()?;
            match line.strip_prefix("T ") {
                Some(tool) => missing.push(tool.to_string()),
                None if line == "K" || (line == END && !missing.is_empty()) => break,
                None => return Err(unexpected("K", &line)),
            }
        }
        Ok(Ok((reply, missing)))
    }

    /// Waits for more output and adds it to the buffer; `false` once the process has ended.
    fn more(&mut self) -> Result<bool, TransportError> {
        let mut silent_since = Instant::now();
        while self.exit.is_none() {
            if self.cancel.is_cancelled() {
                return Err(TransportError::Cancelled);
            }
            match self.events.recv_timeout(POLL) {
                Ok(Event::Stdout(bytes)) => {
                    self.answered += bytes.len() as u64;
                    if self.at == self.buffer.len() {
                        self.buffer = bytes;
                    } else {
                        self.buffer.drain(..self.at);
                        self.buffer.extend_from_slice(&bytes);
                    }
                    self.at = 0;
                    return Ok(true);
                }
                Ok(Event::Stderr(line)) => {
                    silent_since = Instant::now();
                    self.stderr.extend_from_slice(&line);
                    self.stderr.push(b'\n');
                    if self.stderr.len() > 2 * KEPT {
                        self.stderr.drain(..self.stderr.len() - KEPT);
                    }
                }
                Ok(Event::Exit(exit)) => self.exit = Some(exit),
                Err(RecvTimeoutError::Timeout) if silent_since.elapsed() >= self.idle => {
                    /* Not a host that can't be reached: `ssh` asks a silent host whether
                    it is there, and ends by itself when it is not. Something hangs. */
                    return Err(TransportError::Failed(format!(
                        "no answer for {} seconds",
                        self.idle.as_secs()
                    )));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.exit = Some(None),
            }
        }
        Ok(false)
    }

    /**
     * The process ended where there should have been more of the answer: why, by the
     * launcher's account. One that ended well and said nothing lost the rest of its answer on
     * the way ([`TransportError::Stopped`]): at a WSL location, under load, the tail of an
     * answer after the login shell's has been seen to go missing so.
     */
    fn broken(&self) -> TransportError {
        let exit = self.exit.flatten();
        match exit {
            Some(status) if status.success() && self.stderr.is_empty() => {
                TransportError::Stopped(format!(
                    "{} bytes of it arrived; the last line read was {} ({status})",
                    self.answered, self.last
                ))
            }
            _ => self.launcher.failure(exit, &self.stderr, &self.before),
        }
    }

    /// The bytes up to the next `end`, which is passed over; at most `limit` of them.
    fn until(&mut self, end: u8, limit: usize) -> Result<Vec<u8>, TransportError> {
        let mut searched = 0;
        loop {
            let unread = &self.buffer[self.at..];
            if let Some(found) = unread[searched..].iter().position(|byte| *byte == end) {
                let taken = unread[..searched + found].to_vec();
                self.at += taken.len() + 1;
                return Ok(taken);
            }
            searched = unread.len();
            if searched > limit {
                return Err(TransportError::Protocol(format!(
                    "{:?}… goes on for more than {limit} bytes",
                    String::from_utf8_lossy(&unread[..32])
                )));
            }
            if !self.more()? {
                return Err(self.broken());
            }
        }
    }

    /// The next record's line of fields, without its newline.
    pub(super) fn line(&mut self) -> Result<String, TransportError> {
        let line = String::from_utf8(self.until(b'\n', LINE_MAX)?).map_err(|error| {
            TransportError::Protocol(format!(
                "{:?} is not a record",
                String::from_utf8_lossy(error.as_bytes())
            ))
        })?;
        /* Its kind only: a record's first field is one letter, a line of the call's own
        (the login shell's bounds) is longer. */
        self.last = match line.split(' ').next() {
            Some(kind) if kind.len() == 1 => format!("a record of kind {kind}"),
            _ => "a line of the call's own".to_string(),
        };
        Ok(line)
    }

    /// The one field of the next record, empty when it has none. The record must be one
    /// that starts with `letter`.
    pub(super) fn field(&mut self, letter: &str) -> Result<String, TransportError> {
        let line = self.line()?;
        match line.split_once(' ') {
            Some((first, field)) if first == letter && !field.contains(' ') => {
                Ok(field.to_string())
            }
            None if line == letter => Ok(String::new()),
            _ => Err(unexpected(letter, &line)),
        }
    }

    /// The text a record carries, up to its NUL.
    pub(super) fn text(&mut self) -> Result<Vec<u8>, TransportError> {
        self.until(0, TEXT_MAX)
    }

    /// The text a record carries, when it is a tool's words: for a message.
    pub(super) fn words(&mut self) -> Result<String, TransportError> {
        Ok(String::from_utf8_lossy(&self.text()?).trim().to_string())
    }

    /// Hands the next `length` bytes to `take` as they arrive. `take` may stop the stream.
    pub(super) fn block(
        &mut self,
        mut length: u64,
        take: &mut dyn FnMut(&[u8]) -> Result<(), Halt>,
    ) -> Result<(), TransportError> {
        while length > 0 {
            if self.at == self.buffer.len() && !self.more()? {
                return Err(self.broken());
            }
            let unread = &self.buffer[self.at..];
            let now = usize::try_from(length).map_or(unread.len(), |left| left.min(unread.len()));
            take(&unread[..now]).map_err(|Halt| TransportError::Halted)?;
            self.at += now;
            length -= now as u64;
        }
        Ok(())
    }

    /**
     * Passes over whatever comes, up to and including the first of `lines` to appear, each a
     * line only this call knows: what a login shell's profile prints is not the transport's
     * to read. Returns which of them it was.
     */
    pub(super) fn skip_to(&mut self, lines: &[&str]) -> Result<usize, TransportError> {
        let longest = lines.iter().map(|line| line.len()).max().unwrap_or(0);
        loop {
            let unread = &self.buffer[self.at..];
            let first = lines
                .iter()
                .enumerate()
                .filter_map(|(which, line)| Some((find(unread, line.as_bytes())?, which)))
                .min();
            if let Some((found, which)) = first {
                self.at += found + lines[which].len();
                self.last = "a line of the call's own".to_string();
                return Ok(which);
            }
            /* The end of what was searched may be the start of one of them. */
            self.at = self.buffer.len() - unread.len().min(longest);
            if !self.more()? {
                return Err(self.broken());
            }
        }
    }

    /// Reads the answer's last line.
    pub(super) fn end(mut self) -> Result<(), TransportError> {
        self.field(END).map(drop)
    }
}

/// Where `needle` first is in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The answer has `line` where a record that starts with `letter` belongs.
pub(super) fn unexpected(letter: &str, line: &str) -> TransportError {
    TransportError::Protocol(format!("{line:?} where a {letter} record belongs"))
}

/// A launcher that ended before the answer began: what it left behind.
struct Unanswered {
    exit: Option<ExitStatus>,
    /// The end of what it wrote to stderr, and the start of what it wrote to stdout.
    stderr: Vec<u8>,
    stdout: Vec<u8>,
    written: Written,
}

impl Unanswered {
    /// Why the call failed, by the launcher's account, with how the write of the script went.
    fn failure(&self, launcher: &Launcher) -> TransportError {
        match launcher.failure(self.exit, &self.stderr, &self.stdout) {
            TransportError::Failed(said) => {
                TransportError::Failed(format!("{said}\n{}", self.written))
            }
            other => other,
        }
    }

    /// Whether the launcher ended as it does when the script reached its shell cut short,
    /// having run none of it (the module docs). `failure` is what the launcher's end means.
    fn cut_short(&self, failure: &TransportError) -> bool {
        match self.exit {
            Some(exit) if exit.success() => self.stdout.is_empty() && self.stderr.is_empty(),
            Some(_) => {
                let said = String::from_utf8_lossy(&self.stderr).to_lowercase();
                matches!(failure, TransportError::Failed(_))
                    && UNFINISHED.iter().any(|words| said.contains(words))
            }
            None => false,
        }
    }
}

/// How writing the script to the launcher's stdin went.
enum Written {
    Whole(usize),
    /// After `went` of its `of` bytes.
    Failed {
        went: usize,
        of: usize,
        error: ProcessError,
    },
    /// The writer had not finished when the launcher ended.
    Unknown,
}

impl fmt::Display for Written {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Whole(length) => write!(f, "the script was written whole ({length} bytes)"),
            Self::Failed { went, of, error } => write!(
                f,
                "the script could not be written: {error} (after {went} of {of} bytes)"
            ),
            Self::Unknown => write!(f, "the script was still being written"),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::*;

    /// A launcher that ended with exit status `code`, having written `stderr` and `stdout`.
    fn ended(code: i32, stderr: &str, stdout: &str) -> Unanswered {
        Unanswered {
            exit: Some(ExitStatus::from_raw(code << 8)),
            stderr: stderr.as_bytes().to_vec(),
            stdout: stdout.as_bytes().to_vec(),
            written: Written::Whole(10),
        }
    }

    #[test]
    fn only_the_two_ends_of_a_script_cut_short_are_run_again() {
        let wsl = Launcher::Wsl {
            distro: "Ubuntu-24.04".to_string(),
        };
        let ssh = Launcher::Ssh {
            host: "user@macbook".to_string(),
        };
        let again = |launcher: &Launcher, unanswered: Unanswered| {
            unanswered.cut_short(&unanswered.failure(launcher))
        };
        /* Nothing arrived; or the shell found the script unfinished, after a greeting. */
        assert!(again(&wsl, ended(0, "", "")));
        let dash = "sh: 12: Syntax error: end of file unexpected (expecting \"}\")\n";
        assert!(again(&wsl, ended(2, dash, "")));
        assert!(again(&ssh, ended(1, "zsh: unmatched '\n", "Welcome!\n")));
        let busybox = "sh: syntax error: unterminated quoted string\n";
        assert!(again(&ssh, ended(2, busybox, "")));
        /* Anything else, however it looks. */
        for (launcher, unanswered) in [
            (&wsl, ended(0, "", "Welcome!\n")),
            (&wsl, ended(0, "\n", "")),
            (&wsl, ended(1, "", "")),
            (
                &ssh,
                ended(127, "bash: line 1: sh: command not found\n", ""),
            ),
            (
                &ssh,
                ended(
                    255,
                    "bash: syntax error\nConnection closed by 192.0.2.7\n",
                    "",
                ),
            ),
            (&ssh, ended(255, "Host key verification failed.\n", "")),
        ] {
            let said = String::from_utf8_lossy(&unanswered.stderr).into_owned();
            assert!(!again(launcher, unanswered), "{said:?}");
        }
        let unknown = Unanswered {
            exit: None,
            ..ended(0, "", "")
        };
        assert!(!again(&wsl, unknown));
    }
}
