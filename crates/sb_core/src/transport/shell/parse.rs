//! What the tools at a location printed, taken apart: `stat`'s lines, a hashing tool's, a
//! range's block, and what a tool said when it could not do what it was asked.

use super::reply::Reply;
use super::script::anchor;
use crate::hash::Hash;
use crate::transport::{
    FileEntry, FileStat, Halt, Hashed, Listing, MAY_NOT_BE_READ, NOT_A_FILE, NOT_UNICODE,
    PrefixRequest, RangeFailure, SYMBOLIC_LINK, TransportError, Unlisted,
};

/// What a file is, in the bits of its mode that say so.
const KIND: u32 = 0o170_000;
const REGULAR: u32 = 0o100_000;
const FOLDER: u32 = 0o040_000;
const LINK: u32 = 0o120_000;

/// The byte that ends the file's bytes in a block.
pub(super) const MARK: u8 = b'x';

/**
 * What went wrong with a file, read in what a tool said of it. The tools run in the C locale
 * and end a complaint with the system's words for the error, whichever tool it is; the words
 * before those differ from tool to tool and are kept only where the error is not one of the
 * two that have a plain reason.
 */
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Trouble {
    /// The tool said nothing.
    Nothing,
    /// "No such file or directory": it is not there. Nothing else is taken for that.
    Gone,
    /// "Permission denied": this user may not read it, or may not look in its folder.
    Denied,
    /// Anything else, in the tool's words.
    Other(String),
}

impl Trouble {
    pub(super) fn of(said: &str) -> Self {
        let ends = |error: &str| said.lines().any(|line| line.trim_end().ends_with(error));
        if said.trim().is_empty() {
            Self::Nothing
        } else if ends("No such file or directory") {
            Self::Gone
        } else if ends("Permission denied") {
            Self::Denied
        } else {
            Self::Other(said.trim().to_string())
        }
    }

    /// Why the file is not among what was listed or read, for the owner.
    pub(super) fn reason(self) -> String {
        match self {
            Self::Nothing => "it could not be read".to_string(),
            Self::Gone => RangeFailure::Missing.to_string(),
            Self::Denied => MAY_NOT_BE_READ.to_string(),
            Self::Other(said) => said,
        }
    }
}

/// Why a file gave no bytes, or not all, when a tool `said` this of it.
pub(super) fn unread(said: &str) -> RangeFailure {
    match Trouble::of(said) {
        Trouble::Gone => RangeFailure::Missing,
        trouble => RangeFailure::Unreadable(trouble.reason()),
    }
}

/**
 * A block as `shell.sh` sends one: a file's bytes as far as it had them, a mark, and zeros
 * up to the block's length. The last byte in the block that is not zero is the mark, so
 * everything before it is the file's. Bytes are passed on as soon as a later one that is not
 * zero shows they are not the mark or the padding.
 */
struct Block {
    /// The last byte so far that is not zero, and how many zeros have followed it.
    last: Option<u8>,
    zeros: u64,
    /// How many bytes were passed on.
    passed: u64,
}

impl Block {
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

/// Reads one `range` record for a range of `length` bytes, passing the file's bytes to
/// `pass`, and says how the range ended.
pub(super) fn range(
    reply: &mut Reply<'_>,
    length: u64,
    pass: &mut dyn FnMut(&[u8]) -> Result<(), Halt>,
) -> Result<Result<(), RangeFailure>, TransportError> {
    /* As much of the range as the file was long enough for when the shell looked. */
    let count = number::<u64>(&reply.field("R")?)?;
    if count > length {
        return Err(TransportError::Protocol(format!(
            "a block of {count} bytes for a range of {length}"
        )));
    }
    let mut block = Block {
        last: None,
        zeros: 0,
        passed: 0,
    };
    reply.block(count + 1, &mut |bytes| block.take(bytes, pass))?;
    if block.last != Some(MARK) {
        return Err(TransportError::Protocol(
            "a range's block does not end as one does".to_string(),
        ));
    }
    reply.field("E")?;
    let said = reply.words()?;
    /* A range of no bytes runs no tool: all that is said of it is whether it can be read. */
    Ok(
        if block.passed == length && (length > 0 || said.is_empty()) {
            Ok(())
        } else if said.is_empty() {
            Err(RangeFailure::Short {
                expected: length,
                got: block.passed,
            })
        } else {
            Err(unread(&said))
        },
    )
}

/// The hash a tool printed for its input, if `line` is such a line: the hash, a space, and
/// the rest, which names the input.
fn hash_line(line: &str) -> Option<(Hash, &str)> {
    let (hash, named) = line.split_once(' ')?;
    Some((Hash::from_hex(hash)?, named))
}

/// What a `prefix` record says of `request`: the file's `size` once it was hashed, or `-`,
/// and what the hashing `said`.
pub(super) fn prefix(
    request: &PrefixRequest,
    size: &str,
    said: &str,
) -> Result<Hashed, RangeFailure> {
    /* The hash's line and nothing more; anything else, or more, is a tool's complaint.
    Nothing is hashed for a prefix of no bytes. */
    let hash = match hash_line(said) {
        Some((hash, named)) if !named.contains('\n') => Some(hash),
        None if said.is_empty() && request.length == 0 => Some(Hash::of(b"")),
        _ => None,
    };
    let complaint = || {
        said.lines()
            .filter(|line| hash_line(line).is_none())
            .collect::<Vec<_>>()
            .join("\n")
    };
    match (hash, size.parse::<u64>()) {
        (Some(_), Ok(size)) if size < request.length => Err(RangeFailure::Short {
            expected: request.length,
            got: size,
        }),
        (Some(hash), Ok(size)) => Ok(Hashed { hash, size }),
        _ => Err(unread(&complaint())),
    }
}

/**
 * What a `hashed` record says of `requests`, a run of files hashed whole by one tool: its
 * lines `sums`, and each file's size afterwards in `sizes`. `None` unless the run holds
 * together: a line for each file, in order, that names it as it was asked for, and each file
 * as long afterwards as the prefix asked of it, which the whole file's hash is then the
 * hash of.
 */
pub(super) fn wholes(requests: &[PrefixRequest], sums: &str, sizes: &str) -> Option<Vec<Hashed>> {
    let (sums, sizes): (Vec<&str>, Vec<&str>) = (sums.lines().collect(), sizes.lines().collect());
    if sums.len() != requests.len() || sizes.len() != requests.len() {
        return None;
    }
    requests
        .iter()
        .zip(sums.iter().zip(sizes))
        .map(|(request, (sum, size))| {
            let (hash, named) = hash_line(sum)?;
            /* A space, or a star where the tool says it read the file as it is. */
            let name = named.strip_prefix([' ', '*'])?;
            (name == anchor(&request.path) && size.parse() == Ok(request.length)).then_some(
                Hashed {
                    hash,
                    size: request.length,
                },
            )
        })
        .collect()
}

/// Files one entry of a listing of `root`: `name` as find gave it, and what stat said of it.
pub(super) fn entry(
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
pub(super) fn not_unicode(listing: &mut Listing, name: &[u8]) {
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
pub(super) fn parse_stat(line: &str) -> Result<(u32, FileStat), TransportError> {
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

/**
 * Where a WSL distro mounts the Windows drives, from its `/etc/wsl.conf`: the `root` of its
 * `[automount]` section, with a slash at each end, as the default `/mnt/` has. `None` when
 * the file sets none, or sets the default.
 */
pub(super) fn automount_root(conf: &str) -> Option<String> {
    let mut section = "";
    let root = conf.lines().map(str::trim).find_map(|line| {
        if let Some(name) = line.strip_prefix('[') {
            section = name.trim_end_matches(']').trim();
            return None;
        }
        let (key, value) = line.split_once('=')?;
        (section.eq_ignore_ascii_case("automount") && key.trim().eq_ignore_ascii_case("root"))
            .then(|| value.trim().trim_matches(['"', '\'']))
    })?;
    let root = format!("/{}/", root.trim_matches('/'));
    (root.len() > 2 && root != crate::filing::WSL_DRIVE_MOUNT).then_some(root)
}

/// What a clock record read.
pub(super) fn clock(time: &str) -> Result<i64, TransportError> {
    nanoseconds(time).ok_or_else(|| TransportError::Protocol(format!("the clock read {time:?}")))
}

pub(super) fn number<N: std::str::FromStr>(field: &str) -> Result<N, TransportError> {
    field
        .parse()
        .map_err(|_| TransportError::Protocol(format!("{field:?} where a number belongs")))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

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
            stat("41ED 96 1700000000").1.mtime_ns,
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
                block.push(MARK);
                block.resize(length + 1, 0);
                for piece in [1, 2, 3, 7, 4096, block.len()] {
                    assert_eq!(
                        unblocked(&block, piece),
                        (content.to_vec(), Some(MARK)),
                        "{content:?} in a block of {length} + 1, {piece} at a time"
                    );
                }
            }
        }
        /* A block without its mark is not one. */
        assert_eq!(unblocked(b"\0\0\0", 2), (Vec::new(), None));
    }

    /// What each userland's tools say of a file, as they say it in the C locale.
    #[test]
    fn a_file_is_missing_only_when_a_tool_says_so_in_so_many_words() {
        for gone in [
            "stat: cannot statx '/home/user/x': No such file or directory",
            "stat: /Users/user/x: stat: No such file or directory",
            "stat: can't stat '/home/user/x': No such file or directory",
            "head: cannot open '/home/user/x' for reading: No such file or directory\n",
            "head: /Users/user/x: No such file or directory",
        ] {
            assert_eq!(Trouble::of(gone), Trouble::Gone, "{gone}");
            assert_eq!(unread(gone).to_string(), "it is no longer there");
        }
        for denied in [
            "stat: cannot statx '/home/user/x/y': Permission denied",
            "head: /Users/user/x: Permission denied",
            "Permission denied",
        ] {
            assert_eq!(Trouble::of(denied), Trouble::Denied, "{denied}");
            assert_eq!(unread(denied).to_string(), "it may not be read");
        }
        /* Not a file that is gone, whatever its name says: these are errors. */
        for other in [
            "stat: cannot statx '/home/user/xxx': File name too long",
            "stat: /Users/user/No such file or directory: stat: Input/output error",
            "head: error reading '/home/user/x': Is a directory",
            "stat: cannot statx '/mnt/share/x': Stale file handle",
        ] {
            assert_eq!(Trouble::of(other), Trouble::Other(other.to_string()));
            assert_eq!(unread(other).to_string(), other);
        }
        assert_eq!(Trouble::of(" \n"), Trouble::Nothing);
    }

    #[test]
    fn a_distros_drive_mount_is_read_from_its_wsl_conf() {
        let conf = "# where the drives go\n[boot]\nsystemd=true\nroot = /not-this\n\n\
                    [automount]\nenabled = true\nroot = /windows\noptions = \"metadata\"\n\
                    [network]\nroot=/nor-this/\n";
        assert_eq!(automount_root(conf), Some("/windows/".to_string()));
        assert_eq!(
            automount_root("[Automount]\r\nRoot=\"/c drives/\"\r\n"),
            Some("/c drives/".to_string())
        );
        /* None set, the usual one set, the root of everything, and a file of something
        else. */
        for conf in [
            "",
            "[automount]\nenabled = true\n",
            "[automount]\nroot = /mnt/\n",
            "[automount]\nroot = /mnt\n",
            "[automount]\nroot = /\n",
            "root = /windows/\n",
            "\0\u{1}binary",
        ] {
            assert_eq!(automount_root(conf), None, "{conf:?}");
        }
    }

    fn request(path: &str, length: u64) -> PrefixRequest {
        PrefixRequest {
            path: path.to_string(),
            length,
        }
    }

    #[test]
    fn a_prefixes_hash_is_the_tools_one_line_and_nothing_else() {
        let hash = Hash::of(b"0123");
        let line = format!("{}  -", hash.to_hex());
        let of = |length, size: &str, said: &str| {
            prefix(&request("/f", length), size, said).map_err(|failure| failure.to_string())
        };
        assert_eq!(of(4, "10", &line), Ok(Hashed { hash, size: 10 }));
        assert_eq!(
            of(4, "3", &line),
            Err("the source ended after 3 of 4 bytes".to_string())
        );
        /* More than the hash's line is a complaint, whatever the hash. */
        assert_eq!(
            of(4, "10", &format!("sha256sum: warning\n{line}")),
            Err("sha256sum: warning".to_string())
        );
        assert_eq!(
            of(4, "10", &format!("{line}\n{line}")),
            Err("it could not be read".to_string())
        );
        assert_eq!(
            of(
                4,
                "-",
                &format!("{line}\nstat: /f: stat: No such file or directory")
            ),
            Err("it is no longer there".to_string())
        );
        /* Nothing is hashed of no bytes. */
        assert_eq!(
            of(0, "10", ""),
            Ok(Hashed {
                hash: Hash::of(b""),
                size: 10
            })
        );
        assert_eq!(of(4, "10", ""), Err("it could not be read".to_string()));
        assert_eq!(
            of(0, "10", "Permission denied"),
            Err("it may not be read".to_string())
        );
    }

    #[test]
    fn a_run_hashed_whole_holds_only_with_every_line_its_files_own() {
        let requests = [request("/a b", 3), request("relative", 0), request("/c", 5)];
        let hashes = [Hash::of(b"abc"), Hash::of(b""), Hash::of(b"01234")];
        let sums = |names: [&str; 3]| -> String {
            hashes
                .iter()
                .zip(names)
                .map(|(hash, name)| format!("{} {name}\n", hash.to_hex()))
                .collect()
        };
        let whole = [" /a b", "*./relative", " /c"];
        let held = wholes(&requests, &sums(whole), "3\n0\n5").unwrap();
        assert_eq!(
            held.iter().map(|hashed| hashed.hash).collect::<Vec<_>>(),
            hashes
        );
        assert_eq!(
            held.iter().map(|hashed| hashed.size).collect::<Vec<_>>(),
            [3, 0, 5]
        );
        /* A file that is not as long as its prefix any more; a line for another file, or
        out of order; a line too few; a line that is not a hash's. */
        assert_eq!(wholes(&requests, &sums(whole), "3\n0\n6"), None);
        assert_eq!(
            wholes(&requests, &sums([" /a b", " /c", " /c"]), "3\n0\n5"),
            None
        );
        assert_eq!(
            wholes(&requests, &sums([" /a", "*./relative", " /c"]), "3\n0\n5"),
            None
        );
        assert_eq!(wholes(&requests[..2], &sums(whole), "3\n0"), None);
        assert_eq!(wholes(&requests, &sums(whole), "3\n0"), None);
        let escaped = sums(whole).replacen(&hashes[0].to_hex(), "\\", 1);
        assert_eq!(wholes(&requests, &escaped, "3\n0\n5"), None);
    }
}
