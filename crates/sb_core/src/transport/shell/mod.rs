//! [`ShellTransport`]: a WSL distro or a host over SSH, read by running POSIX `sh` scripts
//! there with the tools every such system has (PLAN D6, §6.2). Nothing is installed at the
//! location and nothing is written there.
//!
//! **One call, one process.** Each operation starts the [`Launcher`]'s `sh -s` and writes it
//! a script: `shell.sh`, which defines a function per operation and documents the answer's
//! records, then one line per request that calls one. A pass over a location where nothing
//! changed is four processes (the environment, whether the agent is there, its settings, the
//! listing); each batch it fetches adds one for the stream and one for the proofs.
//!
//! **Requests are shell words.** A path is written between single quotes, where a POSIX shell
//! reads every character as itself, a quote in it as `'\''`. So a name with spaces, newlines,
//! quotes or a leading dash is one argument, whatever shell runs the script, and nothing but
//! the launcher's fixed arguments ever crosses a command line.
//!
//! **The answer is read without trusting names or file contents** ([`reply`]): names end at a
//! NUL, a file's bytes come in a block as long as was asked for, and the tools' own output is
//! taken apart here, not in the shell.
//!
//! **A file that shrinks or goes while it is read** fills its block with zeros after a mark,
//! so the stream stays in step and the range is reported short ([`Block`]). What a range held
//! is never taken on trust anyway: the engine has the source hash it afterwards (PLAN §4.2).
//!
//! **Modification times** are in nanoseconds where the location's `stat` gives them (GNU
//! coreutils, BSD, macOS) and whole seconds otherwise (busybox). Whole seconds are safe: a
//! file's stamp is trusted only once it is two seconds old ([`crate::provider::FileTree`]).
//! The listing's clock is the location's own, to the nanosecond where its `date` can tell,
//! and otherwise in whole seconds, rounded down, which only makes a file look younger.
//!
//! **The environment** is the one a command run by `wsl.exe --exec` or `ssh host command`
//! gets: no login script is read, so a variable the owner sets in a shell profile is not seen.

mod launch;
mod reply;

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use toolbox_process::CancelToken;

use super::{
    Env, FileEntry, FileStat, Halt, Hashed, Listing, NOT_A_FILE, NOT_UNICODE, PrefixRequest,
    RangeFailure, RangeRequest, RangeSink, SYMBOLIC_LINK, Transport, TransportError, Unlisted,
};
use crate::hash::Hash;
use reply::{END, Reply, unexpected};

pub use launch::Launcher;

/// What runs at the location: a function per operation.
const FUNCTIONS: &str = include_str!("shell.sh");

/// How many names a listing asks about one by one in a single call, when `stat` answered for
/// fewer files than `find` named.
const ASK_AGAIN: usize = 256;

/// What a tool says of a path it may not look at. To the shell's own tests such a path is not
/// there, so where this was said, it is not taken for missing. (The tools run in the C locale.)
const DENIED: &str = "Permission denied";

/// What a file is, in the bits of its mode that say so.
const KIND: u32 = 0o170_000;
const REGULAR: u32 = 0o100_000;
const FOLDER: u32 = 0o040_000;
const LINK: u32 = 0o120_000;

/// What a connection test found at a location whose shell answered (DESIGN §4.12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// The user's home directory there; empty when the shell could not tell.
    pub home: String,
    /// The location's clock, in nanoseconds since the Unix epoch.
    pub clock_ns: i64,
    /// The tools the transport needs that the location lacks. With any, it can't be synced.
    pub missing: Vec<String>,
    /// Whether files' modification times come in whole seconds only.
    pub whole_seconds: bool,
    /// How long the test took, from starting the launcher to the end of the answer.
    pub took: Duration,
}

/// A location behind a [`Launcher`] (the module docs).
#[derive(Clone, Debug)]
pub struct ShellTransport {
    launcher: Launcher,
    cancel: CancelToken,
}

impl ShellTransport {
    pub fn new(launcher: Launcher) -> Self {
        Self {
            launcher,
            cancel: CancelToken::new(),
        }
    }

    /// Makes cancelling `cancel` kill whatever the transport is running, with everything it
    /// started; the call it was in fails with [`TransportError::Cancelled`].
    pub fn cancelled_by(mut self, cancel: &CancelToken) -> Self {
        self.cancel = cancel.clone();
        self
    }

    /**
     * The connection test: whether the launcher reaches a shell, and what the transport would
     * find there. A location that can't be reached, or whose launcher fails, is an error; one
     * that lacks tools answers, and names them in [`Probe::missing`].
     */
    pub fn probe(&self) -> Result<Probe, TransportError> {
        let started = Instant::now();
        let (mut reply, missing) = self.start(&Script::new().line("probe"))?;
        reply.field("H")?;
        let home = reply.words()?;
        let clock_ns = clock(&reply.field("C")?)?;
        let whole_seconds = reply.field("N")? == "0";
        reply.end()?;
        Ok(Probe {
            home,
            clock_ns,
            missing,
            whole_seconds,
            took: started.elapsed(),
        })
    }

    /// Runs `script` at the location and reads its answer up to the first record.
    fn start(&self, script: &Script) -> Result<(Reply<'_>, Vec<String>), TransportError> {
        Reply::start(
            &self.launcher,
            script.text().into_bytes(),
            &script.marker,
            &self.cancel,
        )
    }

    /// Runs `script`, which needs `tools`, and reads its answer up to the first record.
    fn ask(&self, tools: &str, script: Script) -> Result<Reply<'_>, TransportError> {
        let (reply, missing) = self.start(&Script {
            requests: format!("need {tools}\n{}", script.requests),
            ..script
        })?;
        if missing.is_empty() {
            Ok(reply)
        } else {
            Err(TransportError::MissingTools(missing))
        }
    }

    /// Asks about each of `names` by itself, without following a symbolic link: for names a
    /// listing could not match to what `stat` said of them.
    fn list_again(
        &self,
        names: &[String],
        root: &str,
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
                match code.as_str() {
                    "0" => entry(listing, root, name.clone().into_bytes(), &said)?,
                    /* Gone since find saw it; or in a folder that may not be searched, which
                    the listing has reported. */
                    "1" => {}
                    _ => listing.unlisted.push(Unlisted {
                        path: name.clone(),
                        reason: said,
                    }),
                }
            }
            reply.end()?;
        }
        Ok(())
    }
}

impl Transport for ShellTransport {
    fn env(&self, names: &[&str]) -> Result<Env, TransportError> {
        let mut script = Script::new().line("home");
        /* Any other name is not a variable's, and is not set. */
        for name in names.iter().filter(|name| is_identifier(name)) {
            script = script.line(&format!("var {name}"));
        }
        let mut reply = self.ask("", script)?;
        reply.field("H")?;
        let home = String::from_utf8(reply.text()?)
            .ok()
            .filter(|home| home.starts_with('/'))
            .ok_or(TransportError::NoHome)?;
        let mut vars = HashMap::new();
        loop {
            let line = reply.line()?;
            let Some(name) = line.strip_prefix("V ") else {
                return if line == END {
                    Ok(Env { home, vars })
                } else {
                    Err(unexpected("V", &line))
                };
            };
            /* As on this machine, a value that is not Unicode counts as not set. */
            if let Ok(value) = String::from_utf8(reply.text()?) {
                vars.insert(name.to_string(), value);
            }
        }
    }

    fn stat(&self, paths: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        let mut script = Script::new();
        for path in paths {
            script = script.call("file L", &[], path)?;
        }
        let mut reply = self.ask("stat", script)?;
        let mut stats = Vec::new();
        for path in paths {
            let code = reply.field("S")?;
            let said = reply.words()?;
            stats.push(match code.as_str() {
                "0" => Some(parse_stat(&said)?.1),
                "1" if !said.contains(DENIED) => None,
                _ => return Err(remote("stat", path, said)),
            });
        }
        reply.end()?;
        Ok(stats)
    }

    fn list(&self, root: &str) -> Result<Listing, TransportError> {
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
        /* Whether a folder was reported as one this user may not read. */
        let mut barred = false;
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
                    let folder = reply.text()?;
                    barred = true;
                    listing.unlisted.push(Unlisted {
                        path: String::from_utf8_lossy(&folder).into_owned(),
                        reason: "it may not be read".to_string(),
                    });
                }
                ["F", status] => {
                    let said = reply.words()?;
                    /* With folders that may not be read named already, find's complaint is
                    of them. */
                    if status != "0" && !barred {
                        listing.unlisted.push(Unlisted {
                            path: root.to_string(),
                            reason: if said.is_empty() {
                                format!("find failed ({status})")
                            } else {
                                said
                            },
                        });
                    }
                    break;
                }
                _ => return Err(unexpected("B, U or F", &line)),
            }
        }
        reply.end()?;
        self.list_again(&again, root, &mut listing)?;
        Ok(listing)
    }

    fn hash_prefixes(
        &self,
        requests: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError> {
        let mut script = Script::new();
        for request in requests {
            script = script.call("prefix", &[request.length], &request.path)?;
        }
        let mut reply = self.ask("head stat sha256", script)?;
        let mut hashes = Vec::new();
        for request in requests {
            let found = reply.line()?;
            let said = reply.words()?;
            let ["P", there, size] = found.split(' ').collect::<Vec<_>>()[..] else {
                return Err(unexpected("P", &found));
            };
            /* The hashing tool prints the hash, a space, and a name for its input. Anything
            else, or more, is a tool's complaint. Nothing is hashed for a prefix of no bytes. */
            let hash = match said.split_once(' ') {
                Some((hash, name)) if !name.contains('\n') => Hash::from_hex(hash),
                None if said.is_empty() && request.length == 0 => Some(Hash::of(b"")),
                _ => None,
            };
            hashes.push(match (hash, size.parse::<u64>()) {
                (Some(_), Ok(size)) if size < request.length => Err(RangeFailure::Short {
                    expected: request.length,
                    got: size,
                }),
                (Some(hash), Ok(size)) => Ok(Hashed { hash, size }),
                (Some(_), Err(_)) => Err(unread(there == "1", String::new())),
                (None, _) => Err(unread(
                    there == "1",
                    said.lines()
                        .filter(|line| Hash::from_hex(first(line)).is_none())
                        .collect::<Vec<_>>()
                        .join("; "),
                )),
            });
        }
        reply.end()?;
        Ok(hashes)
    }

    fn read_ranges(
        &self,
        requests: &[RangeRequest],
        sink: &mut dyn RangeSink,
    ) -> Result<(), TransportError> {
        let mut script = Script::new();
        for request in requests {
            script = script.call("range", &[request.offset, request.length], &request.path)?;
        }
        let mut reply = self.ask("head tail", script)?;
        for (index, request) in requests.iter().enumerate() {
            let result = range(&mut reply, request.length, &mut |bytes| {
                sink.data(index, bytes)
            })?;
            sink.end(index, result)
                .map_err(|Halt| TransportError::Halted)?;
        }
        reply.end()
    }

    fn read_small(&self, path: &str) -> Result<Option<Vec<u8>>, TransportError> {
        let mut reply = self.ask("stat head", Script::new().call("small", &[], path)?)?;
        let found = reply.line()?;
        let content = match found.split(' ').collect::<Vec<_>>()[..] {
            ["M", "0", size] => {
                let mut content = Vec::new();
                let read = range(&mut reply, number(size)?, &mut |bytes| {
                    content.extend_from_slice(bytes);
                    Ok(())
                })?;
                match read {
                    /* Shorter than it was a moment ago: this is the whole of it now. */
                    Ok(()) | Err(RangeFailure::Short { .. }) => Some(content),
                    Err(RangeFailure::Missing) => None,
                    Err(RangeFailure::Unreadable(why)) => return Err(remote("read", path, why)),
                }
            }
            ["M", "1"] => None,
            ["M", "2"] => return Err(remote("read", path, reply.words()?)),
            _ => return Err(unexpected("M", &found)),
        };
        reply.end()?;
        Ok(content)
    }
}

/// The script of one call: the functions, then the requests between braces, so that the
/// shell has read all of it before it runs any of it.
struct Script {
    /// The line the answer starts at: one that nothing else at the location can have printed.
    marker: String,
    requests: String,
}

impl Script {
    fn new() -> Self {
        Self {
            marker: format!("scrollback {}", uuid::Uuid::new_v4().simple()),
            requests: String::new(),
        }
    }

    /// Adds a request that names no path.
    fn line(mut self, request: &str) -> Self {
        self.requests.push_str(request);
        self.requests.push('\n');
        self
    }

    /// Adds a call of `function` with `numbers` and then `path`, quoted so that any shell
    /// reads it as one word. A path that is not absolute is made to start with `./`: no
    /// tool then takes one that begins with a dash for an option.
    fn call(self, function: &str, numbers: &[u64], path: &str) -> Result<Self, TransportError> {
        if path.contains('\0') {
            return Err(remote("name", path, "a path can't hold a NUL".to_string()));
        }
        let numbers: String = numbers.iter().map(|number| format!(" {number}")).collect();
        let dot = if path.starts_with('/') { "" } else { "./" };
        Ok(self.line(&format!(
            "{function}{numbers} '{dot}{}'",
            path.replace('\'', "'\\''")
        )))
    }

    /// What goes to the shell's stdin.
    fn text(&self) -> String {
        format!(
            "{FUNCTIONS}{{\nprintf '%s\\n' '{}'\n{}printf '{END}\\n'\n}} </dev/null\n",
            self.marker, self.requests
        )
    }
}

/**
 * A range's block as `shell.sh` sends it: the range's bytes as far as the file had them, a
 * mark, and zeros up to one byte more than the range's length. The last byte in the block
 * that is not zero is the mark, so everything before it is the file's. Bytes are passed on as
 * soon as a later one that is not zero shows they are not the mark or the padding.
 */
struct Block {
    /// The last byte so far that is not zero, and how many zeros have followed it.
    last: Option<u8>,
    zeros: u64,
    /// How many bytes were passed on.
    passed: u64,
}

impl Block {
    const MARK: u8 = b'x';

    fn take(
        &mut self,
        bytes: &[u8],
        pass: &mut dyn FnMut(&[u8]) -> Result<(), Halt>,
    ) -> Result<(), Halt> {
        const ZEROS: [u8; 4096] = [0; 4096];
        let Some(last) = bytes.iter().rposition(|byte| *byte != 0) else {
            self.zeros += bytes.len() as u64;
            return Ok(());
        };
        if let Some(byte) = self.last {
            pass(&[byte])?;
            self.passed += 1;
        }
        self.passed += self.zeros + last as u64;
        while self.zeros > 0 {
            let now = usize::try_from(self.zeros).map_or(ZEROS.len(), |left| left.min(ZEROS.len()));
            pass(&ZEROS[..now])?;
            self.zeros -= now as u64;
        }
        pass(&bytes[..last])?;
        self.last = Some(bytes[last]);
        self.zeros = (bytes.len() - last - 1) as u64;
        Ok(())
    }
}

/// Reads one `range` record of `length` bytes, passing the file's bytes to `pass`, and says
/// how the range ended.
fn range(
    reply: &mut Reply<'_>,
    length: u64,
    pass: &mut dyn FnMut(&[u8]) -> Result<(), Halt>,
) -> Result<Result<(), RangeFailure>, TransportError> {
    reply.field("R")?;
    let mut block = Block {
        last: None,
        zeros: 0,
        passed: 0,
    };
    reply.block(length + 1, &mut |bytes| block.take(bytes, pass))?;
    if block.last != Some(Block::MARK) {
        return Err(TransportError::Protocol(
            "a range's block does not end as one does".to_string(),
        ));
    }
    let there = reply.field("E")? == "1";
    let said = reply.words()?;
    /* A range of no bytes runs no tool: all that is said of it is whether it can be read. */
    Ok(
        if there && block.passed == length && (length > 0 || said.is_empty()) {
            Ok(())
        } else if there && said.is_empty() {
            Err(RangeFailure::Short {
                expected: length,
                got: block.passed,
            })
        } else {
            Err(unread(there, said))
        },
    )
}

/// Why a file gave no bytes, or not all: it is `there` or not, and the reading tool `said`
/// this of it.
fn unread(there: bool, said: String) -> RangeFailure {
    if there || said.contains(DENIED) {
        RangeFailure::Unreadable(if said.is_empty() {
            "it could not be read".to_string()
        } else {
            said
        })
    } else {
        RangeFailure::Missing
    }
}

/// Files one entry of a listing of `root`: `name` as find gave it, and what stat said of it.
fn entry(
    listing: &mut Listing,
    root: &str,
    name: Vec<u8>,
    stat: &str,
) -> Result<(), TransportError> {
    let (mode, stat) = parse_stat(stat)?;
    let name = match String::from_utf8(name) {
        Ok(name) => name,
        Err(error) => {
            not_unicode(listing, error.as_bytes());
            return Ok(());
        }
    };
    if name == root {
        return Ok(());
    }
    let path = name
        .strip_prefix(root.trim_end_matches('/'))
        .and_then(|path| path.strip_prefix('/'))
        .ok_or_else(|| TransportError::Protocol(format!("{name:?} is not under {root:?}")))?;
    let skipped = |reason: &str| Unlisted {
        path: name.clone(),
        reason: reason.to_string(),
    };
    match mode & KIND {
        REGULAR => listing.files.push(FileEntry {
            path: path.to_string(),
            stat,
        }),
        FOLDER => {}
        LINK => listing.skipped.push(skipped(SYMBOLIC_LINK)),
        _ => listing.skipped.push(skipped(NOT_A_FILE)),
    }
    Ok(())
}

/// Reports a name that is not Unicode, once: not again for what is inside a folder so named.
fn not_unicode(listing: &mut Listing, name: &[u8]) {
    let folder = name.iter().rposition(|byte| *byte == b'/').unwrap_or(0);
    if std::str::from_utf8(&name[..folder]).is_ok() {
        listing.unlisted.push(Unlisted {
            path: String::from_utf8_lossy(name).into_owned(),
            reason: NOT_UNICODE.to_string(),
        });
    }
}

/// A file's mode, size and modification time from stat's line for it: the mode in hex, the
/// size, and the time as seconds, with or without a fraction of up to nine digits.
fn parse_stat(line: &str) -> Result<(u32, FileStat), TransportError> {
    let parsed = || {
        let mut fields = line.split(' ');
        let mode = u32::from_str_radix(fields.next()?, 16).ok()?;
        let size = fields.next()?.parse().ok()?;
        let mtime_ns = nanoseconds(fields.next().filter(|_| fields.next().is_none())?)?;
        Some((mode, FileStat { size, mtime_ns }))
    };
    parsed().ok_or_else(|| TransportError::Protocol(format!("stat printed {line:?}")))
}

/// A time since the Unix epoch in nanoseconds, from seconds with or without a fraction of up
/// to nine digits.
fn nanoseconds(time: &str) -> Option<i64> {
    let (seconds, fraction) = time.split_once('.').unwrap_or((time, ""));
    let fraction: i64 = format!("{fraction:0<9}")
        .get(..9)
        .filter(|digits| digits.bytes().all(|digit| digit.is_ascii_digit()))?
        .parse()
        .ok()?;
    seconds
        .parse::<i64>()
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(fraction)
}

/// What a clock record read.
fn clock(time: &str) -> Result<i64, TransportError> {
    nanoseconds(time).ok_or_else(|| TransportError::Protocol(format!("the clock read {time:?}")))
}

fn number<N: std::str::FromStr>(field: &str) -> Result<N, TransportError> {
    field
        .parse()
        .map_err(|_| TransportError::Protocol(format!("{field:?} where a number belongs")))
}

/// The first word of `line`.
fn first(line: &str) -> &str {
    line.split(' ').next().unwrap_or(line)
}

/// Whether `name` can be an environment variable's name in a shell.
fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|first: char| first.is_ascii_digit())
        && name
            .chars()
            .all(|letter| letter.is_ascii_alphanumeric() || letter == '_')
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
    fn a_path_is_one_shell_word_whatever_is_in_it() {
        let script = Script::new()
            .call("range", &[3, 7], "/a b/it's \"here\"\n$HOME `x`")
            .unwrap()
            .call("file L", &[], "-rf")
            .unwrap();
        assert_eq!(
            script.requests,
            "range 3 7 '/a b/it'\\''s \"here\"\n$HOME `x`'\nfile L './-rf'\n"
        );
        assert!(Script::new().call("small", &[], "/a\0b").is_err());
    }

    #[test]
    fn stat_lines_of_each_flavour_are_read() {
        let stat = |line| parse_stat(line).unwrap();
        /* GNU coreutils and BSD: nanoseconds. busybox: whole seconds. */
        assert_eq!(
            stat("81a4 12 1700000000.123456789"),
            (
                0o100_644,
                FileStat {
                    size: 12,
                    mtime_ns: 1_700_000_000_123_456_789
                }
            )
        );
        assert_eq!(
            stat("41ed 96 1700000000").1.mtime_ns,
            1_700_000_000_000_000_000
        );
        assert_eq!(
            stat("a1ff 7 1700000000.5").1.mtime_ns,
            1_700_000_000_500_000_000
        );
        for bad in [
            "",
            "81a4 12",
            "81a4 12 soon",
            "81a4 12 1 2",
            "zz 1 1",
            "81a4 -1 1",
        ] {
            assert!(parse_stat(bad).is_err(), "{bad}");
        }
    }

    /// Takes `block` apart in pieces of `piece` bytes: what was passed on, and the mark.
    fn unblocked(block: &[u8], piece: usize) -> (Vec<u8>, Option<u8>) {
        let mut passed = Vec::new();
        let mut taking = Block {
            last: None,
            zeros: 0,
            passed: 0,
        };
        for bytes in block.chunks(piece) {
            taking
                .take(bytes, &mut |bytes| {
                    passed.extend_from_slice(bytes);
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(taking.passed, passed.len() as u64);
        (passed, taking.last)
    }

    #[test]
    fn a_block_gives_the_files_bytes_however_it_is_cut_up() {
        let mut long = vec![0; 10_000];
        long.extend_from_slice(b"end\0\0");
        for content in [
            &b""[..],
            b"x",
            b"\0",
            b"abc",
            b"a\0\0b\0\0\0",
            b"xx\0x",
            &long,
        ] {
            /* The file gave all of the range, or half of it. */
            for length in [content.len(), content.len() * 2 + 1] {
                let mut block = content.to_vec();
                block.push(Block::MARK);
                block.resize(length + 1, 0);
                for piece in [1, 2, 3, 7, 4096, block.len()] {
                    assert_eq!(
                        unblocked(&block, piece),
                        (content.to_vec(), Some(Block::MARK)),
                        "{content:?} in a block of {length} + 1, {piece} at a time"
                    );
                }
            }
        }
        /* A block without its mark is not one. */
        assert_eq!(unblocked(b"\0\0\0", 2), (Vec::new(), None));
    }
}
