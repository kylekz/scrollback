//! [`Reply`]: the shell's answer to one call, read as it arrives from the launcher's process.
//!
//! The answer starts at a line only this call knows (whatever a login script printed before
//! it is passed over) and is a sequence of records: a line of ASCII fields, then, by its first
//! letter, text ended by a NUL or a block of as many bytes as the caller asked for
//! (`shell.sh`). Its last line is `Z`. An answer that ends without it is not one: the launcher
//! failed or the connection dropped, and the call fails with what was read thrown away.

use std::process::ExitStatus;
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::time::Duration;

use toolbox_process::{CancelToken, Event, Handle, ProcessError, spawn_streaming};

use super::Launcher;
use crate::transport::{Halt, TransportError};

/// How often a read that is waiting for the process looks at the cancel token.
const POLL: Duration = Duration::from_millis(50);

/// How many chunks of output wait here for the caller before the process is made to wait.
const QUEUE: usize = 8;

/// The longest line of fields a record starts with; a longer one is not a record.
const LINE_MAX: usize = 256;

/// The longest text a record carries: every complaint of a `find` over a large tree.
const TEXT_MAX: usize = 16 * 1024 * 1024;

/// How much is kept of what the launcher wrote besides the answer, for an error message.
const KEPT: usize = 16 * 1024;

/// The answer's last line.
pub(super) const END: &str = "Z";

pub(super) struct Reply<'a> {
    /// Dropped before `process`, whose own thread may be waiting to send here.
    events: Receiver<Event>,
    /// Killed, with everything it started, when the reply is dropped.
    _process: Handle,
    launcher: &'a Launcher,
    cancel: &'a CancelToken,
    /// Output not yet read, from `at` on.
    buffer: Vec<u8>,
    at: usize,
    stderr: Vec<u8>,
    /// What came before the answer began.
    before: Vec<u8>,
    /// Set once the process has ended: its exit status, if it could be waited for.
    exit: Option<Option<ExitStatus>>,
}

impl<'a> Reply<'a> {
    /**
     * Starts the launcher with `script` on its stdin and reads up to where the records begin:
     * past the line `marker`, and the script's word on the tools it needs. Returns the tools
     * that are missing with the reply.
     */
    pub(super) fn start(
        launcher: &'a Launcher,
        script: Vec<u8>,
        marker: &str,
        cancel: &'a CancelToken,
    ) -> Result<(Self, Vec<String>), TransportError> {
        let (tx, events) = sync_channel(QUEUE);
        let process = spawn_streaming(&launcher.command().input(script), move |event| {
            let _ = tx.send(event);
        })
        .map_err(|error| match error {
            ProcessError::Start { program, source } => TransportError::Launch {
                program: program.display().to_string(),
                source,
            },
            other => TransportError::Failed(other.to_string()),
        })?;
        /* The script is all the shell is to read. */
        process.close_stdin();
        let mut reply = Self {
            events,
            _process: process,
            launcher,
            cancel,
            buffer: Vec::new(),
            at: 0,
            stderr: Vec::new(),
            before: Vec::new(),
            exit: None,
        };
        let marker = format!("{marker}\n");
        loop {
            if let Some(found) = find(&reply.buffer, marker.as_bytes()) {
                reply.before.extend_from_slice(&reply.buffer[..found]);
                reply.at = found + marker.len();
                break;
            }
            if reply.buffer.len() > KEPT {
                return Err(TransportError::Protocol(format!(
                    "it began with {:?}…",
                    String::from_utf8_lossy(&reply.buffer[..64])
                )));
            }
            if !reply.more()? {
                reply.before = std::mem::take(&mut reply.buffer);
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
                    if self.stderr.len() < KEPT {
                        self.stderr.extend_from_slice(&line);
                        self.stderr.push(b'\n');
                    }
                }
                Ok(Event::Exit(exit)) => self.exit = Some(exit),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.exit = Some(None),
            }
        }
        Ok(false)
    }

    /// The process ended where there should have been more of the answer: why, by the
    /// launcher's account.
    fn broken(&self) -> TransportError {
        self.launcher.failure(
            self.exit.flatten(),
            &String::from_utf8_lossy(&self.stderr),
            &self.before,
        )
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
