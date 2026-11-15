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
//! releases a write that was still waiting.

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
     * Returns the tools that are missing with the reply.
     */
    pub(super) fn start(
        launcher: &'a Launcher,
        script: Vec<u8>,
        marker: &str,
        cancel: &'a CancelToken,
        idle: Duration,
    ) -> Result<(Self, Vec<String>), TransportError> {
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
        };
        /* The script is all the shell is to read. A launcher that ends without reading it
        says why through its exit. */
        std::thread::Builder::new()
            .name("sb-shell-in".into())
            .spawn(move || {
                let _ = process.write(&script);
                process.close_stdin();
            })
            .map_err(|error| TransportError::Failed(error.to_string()))?;

        let marker = format!("{marker}\n");
        loop {
            if let Some(found) = find(&reply.buffer, marker.as_bytes()) {
                reply.before = reply.buffer[..found.min(KEPT)].to_vec();
                reply.at = found + marker.len();
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
                reply.before = reply.buffer[..reply.buffer.len().min(KEPT)].to_vec();
                return Err(reply.broken());
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
        Ok((reply, missing))
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
                    return Err(self.launcher.silent(self.idle));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.exit = Some(None),
            }
        }
        Ok(false)
    }

    /// The process ended where there should have been more of the answer: why, by the
    /// launcher's account.
    fn broken(&self) -> TransportError {
        self.launcher
            .failure(self.exit.flatten(), &self.stderr, &self.before)
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
        String::from_utf8(self.until(b'\n', LINE_MAX)?).map_err(|error| {
            TransportError::Protocol(format!(
                "{:?} is not a record",
                String::from_utf8_lossy(error.as_bytes())
            ))
        })
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
