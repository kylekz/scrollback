//! The shell transport, hermetically (PLAN §12): the scripts a WSL distro or an SSH host is
//! read with, run by each shell this machine has against trees in a temp dir, and held to
//! what `LocalTransport` says of the same trees. On Windows these run inside WSL.
//!
//! `SB_TEST_SHELL` and `SB_TEST_SHELL_VIA` (`sb_test_support`) run them in one shell, or in
//! another userland's.

#![cfg(unix)]

mod common;

use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::hash::Hash;
use sb_core::transport::{
    FileEntry, FileStat, Halt, Hashed, Listing, LocalTransport, PrefixRequest, RangeFailure,
    RangeRequest, RangeSink, ShellTransport, Transport, TransportError, Unlisted,
};
use sb_test_support::{loopback, shells};
use tempfile::TempDir;

use common::shell::{
    Collect, Ends, megabytes, noise, set_mode, stand_in, text, through, tool, tools, with_tools,
};

/// A listing in an order that does not depend on how the folder was walked.
fn sorted(mut listing: Listing) -> (Vec<FileEntry>, Vec<Unlisted>, Vec<Unlisted>) {
    listing.files.sort_by(|a, b| a.path.cmp(&b.path));
    listing.skipped.sort_by(|a, b| a.path.cmp(&b.path));
    listing.unlisted.sort_by(|a, b| a.path.cmp(&b.path));
    (listing.files, listing.skipped, listing.unlisted)
}

/// What this machine lists under `root`, as `transport` is to list it: with the files' times
/// to the nanosecond, or to the second where the shell's `stat` tells no more (busybox).
fn listed_here(
    transport: &ShellTransport,
    root: &str,
) -> (Vec<FileEntry>, Vec<Unlisted>, Vec<Unlisted>) {
    let mut listing = LocalTransport.list(root).unwrap();
    let whole_seconds = transport.probe().unwrap().whole_seconds;
    for file in &mut listing.files {
        file.stat = as_told(whole_seconds, file.stat);
    }
    sorted(listing)
}

/// `stat` with its time as a `stat` that tells nanoseconds, or only `whole_seconds`, has it.
fn as_told(whole_seconds: bool, stat: FileStat) -> FileStat {
    if whole_seconds {
        FileStat {
            mtime_ns: stat.mtime_ns.div_euclid(1_000_000_000) * 1_000_000_000,
            ..stat
        }
    } else {
        stat
    }
}

/// Names a shell script must not trip over.
const HOSTILE: [&str; 20] = [
    "plain.txt",
    "with space.txt",
    " leading and trailing ",
    "new\nline\n",
    "tab\there",
    "it's",
    "\"double\"",
    "-rf",
    "--",
    "-",
    "$HOME",
    "`id`",
    "$(id)",
    "back\\slash\\",
    "*?[glob]{a,b}",
    "semi;colon&and|pipe<in>out",
    "#hash!bang~tilde",
    ".hidden",
    "ünïcödé-日本語-🙂",
    "'",
];

/// A tree of hostile names: each as a file, and as a folder holding a file of each name.
fn hostile_tree() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (n, folder) in HOSTILE.iter().enumerate() {
        fs::write(dir.path().join(format!("file {folder}")), noise(n * 7)).unwrap();
        let folder = dir.path().join(folder);
        fs::create_dir(&folder).unwrap();
        for (m, file) in HOSTILE.iter().enumerate() {
            fs::write(folder.join(file), noise(n + m)).unwrap();
        }
    }
    dir
}

#[test]
fn a_listing_is_the_one_this_machine_makes_whatever_the_names() {
    let dir = hostile_tree();
    let root = dir.path().join("-rf");
    fs::create_dir_all(root.join("empty folder/deeper/-and deeper")).unwrap();
    fs::write(root.join("empty file"), "").unwrap();
    /* Links are not followed and not read, whatever they lead to; nor is a pipe. */
    symlink(root.join("plain.txt"), root.join("link to a file")).unwrap();
    symlink(dir.path(), root.join("link to a folder")).unwrap();
    symlink("nowhere", root.join("link to nothing")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(root.join("a pipe"))
            .status()
            .unwrap()
            .success()
    );

    for shell in shells() {
        let transport = through(&shell, dir.path());
        let here = listed_here(&transport, &text(dir.path()));
        assert_eq!(here.0.len(), HOSTILE.len() * (HOSTILE.len() + 1) + 1);
        assert_eq!(here.1.len(), 4);
        let listing = transport.list(&text(dir.path())).unwrap();
        let clock = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        assert!(
            (clock.as_nanos() as i64 - listing.clock_ns).abs() < 60_000_000_000,
            "{shell}: the clock read {}",
            listing.clock_ns
        );
        assert_eq!(sorted(listing), here, "{shell}");
        /* A folder named with a slash at its end, and one that starts with a dash. */
        let inner = sorted(transport.list(&format!("{}/", text(&root))).unwrap());
        assert_eq!(inner, listed_here(&transport, &text(&root)), "{shell}");
    }
}

#[test]
fn a_tree_too_large_for_one_command_line_is_listed_whole() {
    let dir = tempfile::tempdir().unwrap();
    for folder in 0..12 {
        let folder = dir.path().join(format!("folder {folder}"));
        fs::create_dir(&folder).unwrap();
        for file in 0..600 {
            fs::write(
                folder.join(format!("a file with a longer name {file}")),
                "x",
            )
            .unwrap();
        }
    }
    for shell in shells() {
        let transport = through(&shell, dir.path());
        let here = listed_here(&transport, &text(dir.path()));
        assert_eq!(here.0.len(), 7200);
        let listing = transport.list(&text(dir.path())).unwrap();
        assert_eq!(sorted(listing), here, "{shell}");
    }
}

#[test]
fn a_name_that_is_not_unicode_is_reported_and_the_rest_is_listed() {
    let dir = tempfile::tempdir().unwrap();
    let name = |bytes: &'static [u8]| dir.path().join(std::ffi::OsStr::from_bytes(bytes));
    /* Some file systems take only Unicode names; there is nothing to list wrongly on those. */
    if fs::write(name(b"bad-\xff.txt"), "1").is_err() {
        return;
    }
    fs::create_dir(name(b"bad-\xfe folder")).unwrap();
    fs::write(name(b"bad-\xfe folder").join("inside.txt"), "1").unwrap();
    fs::write(dir.path().join("good.txt"), "1").unwrap();

    for shell in shells() {
        let listing = through(&shell, dir.path()).list(&text(dir.path())).unwrap();
        let files: Vec<_> = listing.files.iter().map(|file| &file.path).collect();
        assert_eq!(files, ["good.txt"], "{shell}");
        /* The file, and the folder once: not again for what it holds. */
        let reasons: Vec<_> = listing.unlisted.iter().map(|entry| &entry.reason).collect();
        assert_eq!(reasons, ["its name is not Unicode"; 2], "{shell}");
    }
}

#[test]
fn what_may_not_be_read_is_reported_and_never_taken_for_empty() {
    let dir = tempfile::tempdir().unwrap();
    /* A folder that can't be listed, and one that can be listed and not looked into: a find
    that says nothing of the second must not make its files look gone. */
    let (locked, opaque) = (dir.path().join("locked"), dir.path().join("opaque"));
    for folder in [&locked, &opaque] {
        fs::create_dir_all(folder.join("deeper")).unwrap();
        fs::write(folder.join("inside.txt"), "1").unwrap();
    }
    fs::write(dir.path().join("open.txt"), "1").unwrap();
    set_mode(&locked, 0o000);
    set_mode(&opaque, 0o444);
    /* Whoever may read everything has nothing to report. */
    if fs::read_dir(&locked).is_ok() {
        return;
    }

    let listings: Vec<_> = shells()
        .into_iter()
        .map(|shell| {
            let transport = through(&shell, dir.path());
            (transport.list(&text(dir.path())).unwrap(), shell)
        })
        .collect();
    set_mode(&locked, 0o755);
    set_mode(&opaque, 0o755);
    for (listing, shell) in listings {
        let files: Vec<_> = listing.files.iter().map(|file| &file.path).collect();
        assert_eq!(files, ["open.txt"], "{shell}");
        let mut unlisted: Vec<_> = listing.unlisted.iter().map(|entry| &entry.path).collect();
        unlisted.sort();
        assert_eq!(unlisted, [&text(&locked), &text(&opaque)], "{shell}");
    }
}

#[test]
fn a_folder_that_is_not_one_is_reported_and_a_link_to_one_is_followed() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("real")).unwrap();
    fs::write(dir.path().join("real/file.txt"), "12345").unwrap();
    symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
    symlink(dir.path().join("gone"), dir.path().join("dangling")).unwrap();
    for shell in shells() {
        let transport = through(&shell, dir.path());
        for missing in ["missing", "real/file.txt", "dangling"] {
            let root = text(&dir.path().join(missing));
            let listing = transport.list(&root).unwrap();
            assert!(listing.files.is_empty() && listing.skipped.is_empty());
            let unlisted: Vec<_> = listing.unlisted.iter().map(|entry| &entry.path).collect();
            assert_eq!(unlisted, [&root], "{shell}: {missing}");
        }
        /* As on this machine, the folder asked for may itself be a link. */
        let link = text(&dir.path().join("link"));
        assert_eq!(
            sorted(transport.list(&link).unwrap()),
            listed_here(&transport, &link),
            "{shell}"
        );
    }
}

#[test]
fn stat_follows_links_and_tells_a_missing_file_from_one_that_is_there() {
    let dir = hostile_tree();
    symlink(dir.path().join("file it's"), dir.path().join("link")).unwrap();
    symlink("nowhere", dir.path().join("dangling")).unwrap();
    let paths: Vec<String> = [
        "file it's",
        "file new\nline\n",
        "-rf",
        "link",
        "dangling",
        "no",
    ]
    .iter()
    .map(|name| text(&dir.path().join(name)))
    .collect();
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    let here = LocalTransport.stat(&paths).unwrap();
    assert_eq!(
        here.iter().map(Option::is_some).collect::<Vec<_>>(),
        [true, true, true, true, false, false]
    );
    for shell in shells() {
        let transport = through(&shell, dir.path());
        let whole_seconds = transport.probe().unwrap().whole_seconds;
        let here: Vec<_> = here
            .iter()
            .map(|stat| stat.map(|stat| as_told(whole_seconds, stat)))
            .collect();
        assert_eq!(transport.stat(&paths).unwrap(), here, "{shell}");
    }
}

#[test]
fn the_environment_is_the_shells_own_home_and_variables() {
    let home = tempfile::tempdir().unwrap();
    let vars = [
        ("SB_PLAIN", "/somewhere/else"),
        ("SB_HOSTILE", " it's \"here\"\n$HOME `id` \\ \n\n"),
        ("SB_EMPTY", ""),
    ]
    .map(|(name, value)| (name.to_string(), value.to_string()));
    for shell in shells() {
        let transport = ShellTransport::new(loopback(&shell, home.path(), &vars));
        let env = transport
            .env(&[
                "SB_PLAIN",
                "SB_HOSTILE",
                "SB_EMPTY",
                "SB_UNSET",
                "not a name",
                "1",
            ])
            .unwrap();
        assert_eq!(env.home, text(home.path()), "{shell}");
        let mut set: Vec<_> = env.vars.into_iter().collect();
        set.sort();
        let mut expected = vars.to_vec();
        expected.sort();
        assert_eq!(set, expected, "{shell}");
    }
}

#[test]
fn a_small_file_is_read_whole_whatever_it_holds() {
    let dir = tempfile::tempdir().unwrap();
    let contents: [(&str, Vec<u8>); 6] = [
        ("empty", Vec::new()),
        ("text", b"{\"days\": 30}\n\n\n".to_vec()),
        ("zeros", vec![0; 1000]),
        ("ends in zeros", b"x\0x\0\0\0".to_vec()),
        ("-noise", noise(300_000)),
        ("new\nline", b"x".to_vec()),
    ];
    for (name, content) in &contents {
        fs::write(dir.path().join(name), content).unwrap();
    }
    fs::write(dir.path().join("locked"), "secret").unwrap();
    set_mode(&dir.path().join("locked"), 0o000);
    let may_read_anything = fs::read(dir.path().join("locked")).is_ok();
    for shell in shells() {
        let transport = through(&shell, dir.path());
        for (name, content) in &contents {
            let read = transport.read_small(&text(&dir.path().join(name))).unwrap();
            assert!(read.as_ref() == Some(content), "{shell}: {name}");
        }
        let read = |name| transport.read_small(&text(&dir.path().join(name)));
        assert_eq!(read("missing").unwrap(), None, "{shell}");
        if !may_read_anything {
            let refused = read("locked").unwrap_err();
            assert!(
                matches!(refused, TransportError::Io { .. }),
                "{shell}: {refused}"
            );
        }
    }
}

/// A folder of files to read ranges and prefixes of, and requests that cover every way one
/// can end: with the offsets and lengths as `(file, offset, length)`.
fn readable() -> (TempDir, Vec<(PathBuf, u64, u64)>) {
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str, content: Vec<u8>| {
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        path
    };
    let empty = file("empty", Vec::new());
    let small = file("-it's small\n", b"0123456789".to_vec());
    let zeros = file("zeros", vec![0; 5000]);
    let marked = file("ends like a block", b"data x\0\0\0x\0\0\0\0".to_vec());
    let large = file("large", noise(3_000_000));
    let locked = file("locked", b"secret".to_vec());
    set_mode(&locked, 0o000);
    let missing = dir.path().join("missing");
    let requests = vec![
        (small.clone(), 0, 10),
        (small.clone(), 2, 5),
        (small.clone(), 9, 1),
        /* Shorter than asked: from inside it, from its end, and from past its end. */
        (small.clone(), 8, 5),
        (small.clone(), 10, 3),
        (small.clone(), 50, 3),
        (small.clone(), 0, 0),
        (empty.clone(), 0, 0),
        (empty, 0, 1),
        (zeros.clone(), 0, 5000),
        (zeros.clone(), 4000, 1000),
        (zeros, 4000, 2000),
        (marked.clone(), 0, 15),
        (marked.clone(), 5, 10),
        (marked, 5, 100),
        (large.clone(), 0, 3_000_000),
        (large.clone(), 1_234_567, 1_000_001),
        (large, 2_999_999, 2),
        (missing.clone(), 0, 4),
        (missing, 0, 0),
        (locked.clone(), 0, 6),
        (locked, 0, 0),
        (small, 0, 10),
    ];
    (dir, requests)
}

#[test]
fn ranges_arrive_as_this_machine_reads_them_and_each_ends_on_its_own() {
    let (dir, requests) = readable();
    let requests: Vec<_> = requests
        .into_iter()
        .map(|(path, offset, length)| RangeRequest {
            path: text(&path),
            offset,
            length,
        })
        .collect();
    let mut here = Collect::default();
    LocalTransport.read_ranges(&requests, &mut here).unwrap();
    assert_eq!(
        here.ends[3],
        Err("the source ended after 2 of 5 bytes".into())
    );
    for shell in shells() {
        let mut there = Collect::default();
        through(&shell, dir.path())
            .read_ranges(&requests, &mut there)
            .unwrap();
        assert_eq!(there.ends, here.ends, "{shell}");
        for (request, (there, here)) in requests.iter().zip(there.bytes.iter().zip(&here.bytes)) {
            assert!(there == here, "{shell}: {request:?}");
        }
    }
}

#[test]
fn prefixes_are_hashed_as_this_machine_hashes_them_in_the_order_asked() {
    let (dir, requests) = readable();
    let requests: Vec<_> = requests
        .into_iter()
        .filter(|(_, offset, _)| *offset == 0)
        .map(|(path, _, length)| PrefixRequest {
            path: text(&path),
            length,
        })
        .chain([PrefixRequest {
            path: text(&dir.path().join("large")),
            length: 3_000_001,
        }])
        .collect();
    let plain = |hashes: Vec<Result<Hashed, RangeFailure>>| -> Vec<Result<Hashed, String>> {
        hashes
            .into_iter()
            .map(|hashed| {
                hashed.map_err(|failure| match failure {
                    RangeFailure::Unreadable(_) => "unreadable".to_string(),
                    failure => failure.to_string(),
                })
            })
            .collect()
    };
    let here = plain(LocalTransport.hash_prefixes(&requests).unwrap());
    assert_eq!(
        here[0],
        Ok(Hashed {
            hash: Hash::of(b"0123456789"),
            size: 10
        })
    );
    for shell in shells() {
        let there = through(&shell, dir.path())
            .hash_prefixes(&requests)
            .unwrap();
        assert_eq!(plain(there), here, "{shell}");
    }
}

/// A sink that does something to the first file once its first bytes have arrived.
struct Meddling<F: FnMut()> {
    collect: Collect,
    meddle: Option<F>,
}

impl<F: FnMut()> RangeSink for Meddling<F> {
    fn data(&mut self, request: usize, bytes: &[u8]) -> Result<(), Halt> {
        if let Some(mut meddle) = self.meddle.take() {
            meddle();
        }
        self.collect.data(request, bytes)
    }

    fn end(&mut self, request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        self.collect.end(request, result)
    }
}

/// Reads a large file and then a small one, with `meddle` run on the large one as soon as
/// its first bytes are here, while most of it is still to be read: more than the pipes
/// between the shell and here can hold.
fn read_while(shell: &str, meddle: impl Fn(&Path)) -> (Collect, Vec<u8>) {
    const LARGE: u64 = 48 * 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let (large, after) = (dir.path().join("large"), dir.path().join("after"));
    let content = megabytes(48);
    fs::write(&large, &content).unwrap();
    fs::write(&after, "the next range is where it belongs").unwrap();
    let mut sink = Meddling {
        collect: Collect::default(),
        meddle: Some(|| meddle(&large)),
    };
    through(shell, dir.path())
        .read_ranges(
            &[
                RangeRequest {
                    path: text(&large),
                    offset: 0,
                    length: LARGE,
                },
                RangeRequest {
                    path: text(&after),
                    offset: 4,
                    length: 10,
                },
            ],
            &mut sink,
        )
        .unwrap();
    (sink.collect, content)
}

#[test]
fn a_file_that_shrinks_while_it_is_read_ends_short_and_the_stream_stays_in_step() {
    for shell in shells() {
        let (read, content) = read_while(&shell, |large| {
            File::options()
                .write(true)
                .open(large)
                .unwrap()
                .set_len(100)
                .unwrap();
        });
        /* What arrived of it is what it held, and the range says how much that was. */
        let got = read.bytes[0].len();
        assert!(
            got < content.len() && read.bytes[0] == content[..got],
            "{shell}"
        );
        assert_eq!(
            read.ends,
            [
                Err(format!(
                    "the source ended after {got} of {} bytes",
                    content.len()
                )),
                Ok(())
            ],
            "{shell}"
        );
        assert_eq!(read.bytes[1], b"next range", "{shell}");
    }
}

#[test]
fn a_file_removed_while_it_is_read_is_read_to_its_end_as_on_this_machine() {
    for shell in shells() {
        /* The tool that reads it has it open, as `std::fs` would. */
        let (read, content) = read_while(&shell, |large| fs::remove_file(large).unwrap());
        assert_eq!(read.ends, [Ok(()), Ok(())], "{shell}");
        assert!(read.bytes[0] == content, "{shell}");
        assert_eq!(read.bytes[1], b"next range", "{shell}");
    }
}

/// A sink that stops the stream at its first bytes, or cancels a token there and goes on.
struct Stopping<'a>(Option<&'a CancelToken>);

impl RangeSink for Stopping<'_> {
    fn data(&mut self, _request: usize, _bytes: &[u8]) -> Result<(), Halt> {
        match self.0 {
            Some(cancel) => {
                cancel.cancel();
                Ok(())
            }
            None => Err(Halt),
        }
    }

    fn end(&mut self, _request: usize, _result: Result<(), RangeFailure>) -> Result<(), Halt> {
        Ok(())
    }
}

#[test]
fn a_halt_or_a_cancel_stops_the_shell_at_once() {
    let dir = tempfile::tempdir().unwrap();
    /* More than the pipes between the shell and here can hold: the tools are still at it. */
    let large = dir.path().join("large");
    fs::write(&large, megabytes(64)).unwrap();
    let requests = [RangeRequest {
        path: text(&large),
        offset: 0,
        length: 64 * 1024 * 1024,
    }];
    for shell in shells() {
        let started = Instant::now();
        let halted = through(&shell, dir.path()).read_ranges(&requests, &mut Stopping(None));
        assert!(matches!(halted, Err(TransportError::Halted)), "{shell}");

        let cancel = CancelToken::new();
        let transport = through(&shell, dir.path()).cancelled_by(&cancel);
        let cancelled = transport.read_ranges(&requests, &mut Stopping(Some(&cancel)));
        assert!(
            matches!(cancelled, Err(TransportError::Cancelled)),
            "{shell}"
        );
        /* And nothing more is started. */
        let listed = transport.list(&text(dir.path()));
        assert!(matches!(listed, Err(TransportError::Cancelled)), "{shell}");
        assert!(started.elapsed() < Duration::from_secs(20), "{shell}");
    }
}

/// How each way of asking about the file at `path` ends when the file can't be had: `stat`,
/// a range from its start, one from further in, one of no bytes, a prefix, a prefix of no
/// bytes, and the small file whole.
fn each_way(transport: &ShellTransport, path: &str) -> Vec<String> {
    let range = |offset, length| RangeRequest {
        path: path.to_string(),
        offset,
        length,
    };
    let prefix = |length| PrefixRequest {
        path: path.to_string(),
        length,
    };
    let source = |error: TransportError| match error {
        TransportError::Io { source, .. } => source.to_string(),
        other => panic!("{other}"),
    };
    let mut ends = Ends::default();
    transport
        .read_ranges(&[range(0, 4), range(2, 3), range(0, 0)], &mut ends)
        .unwrap();
    let hashed = transport.hash_prefixes(&[prefix(4), prefix(0)]).unwrap();
    std::iter::once(source(transport.stat(&[path]).unwrap_err()))
        .chain(ends.0)
        .chain(
            hashed
                .into_iter()
                .map(|hashed| hashed.unwrap_err().to_string()),
        )
        .chain([source(transport.read_small(path).unwrap_err())])
        .collect()
}

#[test]
fn a_path_in_a_folder_that_may_not_be_searched_is_an_error_and_not_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    let inside = locked.join("inside.txt");
    fs::write(&inside, "0123456789").unwrap();
    set_mode(&locked, 0o000);
    /* Whoever may read everything has no such folder. */
    if fs::metadata(&inside).is_ok() {
        return;
    }
    assert!(LocalTransport.stat(&[&text(&inside)]).is_err());
    let ends: Vec<_> = shells()
        .into_iter()
        .map(|shell| {
            (
                each_way(&through(&shell, dir.path()), &text(&inside)),
                shell,
            )
        })
        .collect();
    set_mode(&locked, 0o755);
    for (ends, shell) in ends {
        /* The same plain reason, whichever tool it was that could not look. */
        assert_eq!(ends, ["it may not be read"; 7], "{shell}");
    }
}

#[test]
fn a_file_stat_cannot_look_at_is_unlisted_and_an_error_and_never_missing() {
    /* A file whose path is longer than the system takes, in a folder whose path is not: it
    is there for `find`, and `stat` fails on it with something other than "no such file". */
    let limit = if cfg!(target_os = "macos") {
        1024
    } else {
        4096
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut folder = root.clone();
    while folder.to_str().unwrap().len() + 101 < limit - 24 {
        folder.push("e".repeat(100));
    }
    fs::create_dir_all(&folder).unwrap();
    let long = format!("{}.txt", "f".repeat(120));
    assert!(
        Command::new("sh")
            .current_dir(&folder)
            .args(["-c", &format!("echo hello > {long}; echo ok > short.txt")])
            .status()
            .unwrap()
            .success()
    );
    let path = format!("{}/{long}", text(&folder));
    assert!(LocalTransport.stat(&[&path]).is_err());

    for shell in shells() {
        let transport = through(&shell, dir.path());
        /* Its batch is asked about again, name by name: the rest of it is listed, and the
        file is reported, with what stat said of it. */
        let listing = transport.list(&text(&root)).unwrap();
        assert_eq!(listing.files.len(), 1, "{shell}");
        assert!(listing.files[0].path.ends_with("/short.txt"), "{shell}");
        let unlisted: Vec<_> = listing.unlisted.iter().map(|entry| &entry.path).collect();
        assert_eq!(unlisted, [&path], "{shell}");
        let said = "File name too long";
        assert!(listing.unlisted[0].reason.contains(said), "{shell}");
        for end in each_way(&transport, &path) {
            assert!(end.contains(said), "{shell}: {end}");
        }
    }
}

#[test]
fn a_file_that_goes_between_find_and_stat_is_left_out_and_the_rest_is_listed() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", "victim.txt"] {
        fs::write(dir.path().join(name), name).unwrap();
    }
    /* A stat that removes the file as it is asked about the folder's whole batch. */
    let tools = tools(&[]);
    stand_in(
        tools.path(),
        "stat",
        &format!(
            "[ \"$#\" -lt 6 ] || /bin/rm -f '{}'\nexec \"$real\" \"$@\"",
            text(&dir.path().join("victim.txt"))
        ),
    );
    let listing = with_tools(tools.path()).list(&text(dir.path())).unwrap();
    let mut files: Vec<_> = listing
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    files.sort_unstable();
    assert_eq!(files, ["a.txt", "b.txt"]);
    assert_eq!(listing.unlisted, []);
    assert!(!dir.path().join("victim.txt").exists());
}

#[test]
fn a_small_file_that_shrinks_as_it_is_read_is_what_is_left_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let (victim, flag) = (dir.path().join("victim"), dir.path().join("flag"));
    fs::write(&victim, "0123456789").unwrap();
    /* A stat that, the first time it has told the file's size, has the file rewritten. */
    let tools = tools(&[]);
    stand_in(
        tools.path(),
        "stat",
        &format!(
            "\"$real\" \"$@\"\nstatus=$?\ncase \"$*\" in *victim) [ -e '{flag}' ] || \
             {{ : > '{flag}'; printf short > '{victim}'; }} ;; esac\nexit \"$status\"",
            flag = text(&flag),
            victim = text(&victim),
        ),
    );
    let read = with_tools(tools.path()).read_small(&text(&victim)).unwrap();
    assert_eq!(read.as_deref(), Some(&b"short"[..]));
}

#[test]
fn either_tool_that_hashes_gives_the_same_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<_> = (0..12)
        .map(|n| {
            let path = dir.path().join(format!("file {n}"));
            fs::write(&path, noise(1000 + n)).unwrap();
            text(&path)
        })
        .collect();
    /* A start of a file, and whole files, which are hashed as a group. */
    let requests: Vec<_> = std::iter::once((files[0].clone(), 400))
        .chain(
            files
                .iter()
                .enumerate()
                .map(|(n, file)| (file.clone(), 1000 + n as u64)),
        )
        .map(|(path, length)| PrefixRequest { path, length })
        .collect();
    let here = LocalTransport.hash_prefixes(&requests).unwrap();
    for without in [&["shasum"][..], &["sha256sum"]] {
        let tools = tools(without);
        let there = with_tools(tools.path()).hash_prefixes(&requests);
        /* Not every system has both. */
        if matches!(there, Err(TransportError::MissingTools(_))) {
            continue;
        }
        let there = there.unwrap();
        for (there, here) in there.iter().zip(&here) {
            assert_eq!(
                there.as_ref().ok(),
                here.as_ref().ok(),
                "without {without:?}"
            );
        }
    }
}

#[test]
fn a_hash_that_comes_with_anything_else_is_not_taken() {
    if tool("sha256sum").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<_> = (0..3)
        .map(|n| {
            let path = dir.path().join(format!("file {n}"));
            fs::write(&path, "0123456789").unwrap();
            text(&path)
        })
        .collect();
    let tools = tools(&["shasum"]);
    stand_in(
        tools.path(),
        "sha256sum",
        "echo 'sha256sum: WARNING: 1 listed file could not be read'\nexec \"$real\" \"$@\"",
    );
    /* Asked for by itself, and as one of a group. */
    let requests: Vec<_> = [(0, 4), (1, 10), (2, 10)]
        .map(|(file, length)| PrefixRequest {
            path: files[file].clone(),
            length,
        })
        .to_vec();
    let hashed = with_tools(tools.path()).hash_prefixes(&requests).unwrap();
    for hashed in hashed {
        assert_eq!(
            hashed.unwrap_err().to_string(),
            "sha256sum: WARNING: 1 listed file could not be read"
        );
    }
}

/// A folder of many small files with every kind of name, and requests for them, as
/// `(file, offset, length)`, that a transport may take in groups: with, among them, files
/// that are not as long as they were asked for, one that is missing, one that may not be
/// read, one too large for a group, and one asked for from further in.
fn many_small_files() -> (TempDir, Vec<(PathBuf, u64, u64)>) {
    let dir = tempfile::tempdir().unwrap();
    let mut requests = Vec::new();
    for n in 0..320_u64 {
        let name = format!("{} {n}", HOSTILE[n as usize % HOSTILE.len()]);
        let path = dir.path().join(name);
        let size = (n * 37) % 3000;
        fs::write(&path, noise(size as usize)).unwrap();
        /* Longer than was scanned, shorter than was scanned, and empty since. */
        let length = match n % 50 {
            7 => size / 2,
            13 => size + 100,
            _ => size,
        };
        requests.push((path, 0, length.max(1)));
    }
    let large = dir.path().join("large");
    fs::write(&large, megabytes(3)).unwrap();
    requests.insert(100, (large.clone(), 0, 3 * 1024 * 1024));
    requests.insert(150, (large, 1_000_000, 5000));
    requests.insert(200, (dir.path().join("missing"), 0, 10));
    let locked = dir.path().join("locked");
    fs::write(&locked, "secret").unwrap();
    set_mode(&locked, 0o000);
    requests.insert(250, (locked, 0, 6));
    (dir, requests)
}

/// The ranges `requests` name, and the prefixes: each file's start as far as its range.
fn asked(requests: &[(PathBuf, u64, u64)]) -> (Vec<RangeRequest>, Vec<PrefixRequest>) {
    requests
        .iter()
        .map(|(path, offset, length)| {
            (
                RangeRequest {
                    path: text(path),
                    offset: *offset,
                    length: *length,
                },
                PrefixRequest {
                    path: text(path),
                    length: offset + length,
                },
            )
        })
        .unzip()
}

/// Holds what `transport` reads and hashes of `requests` to what this machine does.
fn assert_as_here(transport: &ShellTransport, requests: &[(PathBuf, u64, u64)], what: &str) {
    let (ranges, prefixes) = asked(requests);
    let (mut here, mut there) = (Collect::default(), Collect::default());
    LocalTransport.read_ranges(&ranges, &mut here).unwrap();
    transport.read_ranges(&ranges, &mut there).unwrap();
    assert_eq!(there.ends, here.ends, "{what}");
    for (range, (there, here)) in ranges.iter().zip(there.bytes.iter().zip(&here.bytes)) {
        assert!(there == here, "{what}: {range:?}");
    }
    let plain = |hashes: Vec<Result<Hashed, RangeFailure>>| -> Vec<Result<Hashed, String>> {
        hashes
            .into_iter()
            .map(|hashed| {
                hashed.map_err(|failure| match failure {
                    RangeFailure::Unreadable(_) => "unreadable".to_string(),
                    failure => failure.to_string(),
                })
            })
            .collect()
    };
    assert_eq!(
        plain(transport.hash_prefixes(&prefixes).unwrap()),
        plain(LocalTransport.hash_prefixes(&prefixes).unwrap()),
        "{what}"
    );
}

#[test]
fn many_small_files_are_read_and_hashed_as_they_would_be_one_by_one() {
    let (dir, requests) = many_small_files();
    for shell in shells() {
        assert_as_here(&through(&shell, dir.path()), &requests, &shell);
    }
}

#[test]
fn a_group_that_does_not_hold_together_is_asked_for_again_file_by_file() {
    let (_dir, requests) = many_small_files();
    /* A cat that gives more than the files hold: the bytes do not add up. */
    let tools_cat = tools(&[]);
    stand_in(tools_cat.path(), "cat", "\"$real\" \"$@\"\nprintf more");
    assert_as_here(&with_tools(tools_cat.path()), &requests, "cat gives more");
    /* And one that gives less. */
    let tools_less = tools(&[]);
    stand_in(tools_less.path(), "cat", "\"$real\" \"$@\" | head -c 1000");
    assert_as_here(&with_tools(tools_less.path()), &requests, "cat gives less");
    /* A file that grows between the look at its size and the reading of it. */
    let grows = text(&requests[20].0);
    let tools_grow = tools(&[]);
    stand_in(
        tools_grow.path(),
        "cat",
        &format!(
            "[ -e \"$0.once\" ] || {{ : > \"$0.once\"; printf more >> '{grows}'; }}\n\
             exec \"$real\" \"$@\""
        ),
    );
    assert_as_here(&with_tools(tools_grow.path()), &requests, "a file grows");
    /* A hashing tool that puts its lines for many files in another order, and a file that
    grows as its group is about to be hashed. */
    for name in ["sha256sum", "shasum"] {
        if tool(name).is_none() {
            continue;
        }
        /* The second of them is used only where the first is not there. */
        let tools_sum = tools(if name == "shasum" {
            &["sha256sum"]
        } else {
            &[]
        });
        stand_in(
            tools_sum.path(),
            name,
            "if [ \"$#\" -gt 4 ]; then \"$real\" \"$@\" | sort; else exec \"$real\" \"$@\"; fi",
        );
        assert_as_here(
            &with_tools(tools_sum.path()),
            &requests,
            "lines out of order",
        );
        /* As the run it is in is hashed, and only then. */
        let grows = text(&requests[if name == "shasum" { 60 } else { 40 }].0);
        stand_in(
            tools_sum.path(),
            name,
            &format!(
                "[ \"$#\" -le 3 ] || case \"$* \" in *'{grows} '*) printf more >> '{grows}' ;; esac\n\
                 exec \"$real\" \"$@\""
            ),
        );
        assert_as_here(&with_tools(tools_sum.path()), &requests, "a file grows");
    }
}

#[test]
fn a_range_far_into_a_large_file_is_found_without_reading_up_to_it() {
    use std::io::{Seek as _, SeekFrom, Write as _};

    /* Three gibibytes that take no room, with something written past the second. */
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sparse");
    let mut file = File::create(&path).unwrap();
    file.set_len(3 << 30).unwrap();
    let far = (1 << 31) + 12_345;
    file.seek(SeekFrom::Start(far)).unwrap();
    file.write_all(b"past two gibibytes").unwrap();
    drop(file);
    let requests: Vec<_> = [
        (far, 18),
        (far - 5, 30),
        (far + 7, 1 << 20),
        ((3 << 30) - 4, 9),
    ]
    .map(|(offset, length)| RangeRequest {
        path: text(&path),
        offset,
        length,
    })
    .to_vec();
    let mut here = Collect::default();
    LocalTransport.read_ranges(&requests, &mut here).unwrap();
    assert_eq!(here.bytes[0], b"past two gibibytes");
    for shell in shells() {
        let started = Instant::now();
        let mut there = Collect::default();
        through(&shell, dir.path())
            .read_ranges(&requests, &mut there)
            .unwrap();
        assert!(there == here, "{shell}");
        assert!(started.elapsed() < Duration::from_secs(30), "{shell}");
    }
}

#[test]
fn a_request_for_more_than_any_file_holds_ends_as_on_this_machine() {
    let dir = tempfile::tempdir().unwrap();
    let path = text(&dir.path().join("file"));
    fs::write(&path, "0123456789").unwrap();
    let ranges = [(0, u64::MAX), (3, u64::MAX - 3), (0, u64::MAX / 2)].map(|(offset, length)| {
        RangeRequest {
            path: path.clone(),
            offset,
            length,
        }
    });
    let prefixes = [u64::MAX, u64::MAX / 2, 1 << 62].map(|length| PrefixRequest {
        path: path.clone(),
        length,
    });
    let mut here = Collect::default();
    LocalTransport.read_ranges(&ranges, &mut here).unwrap();
    let hashed = |hashes: Vec<Result<Hashed, RangeFailure>>| -> Vec<String> {
        hashes
            .into_iter()
            .map(|hashed| hashed.unwrap_err().to_string())
            .collect()
    };
    let hashed_here = hashed(LocalTransport.hash_prefixes(&prefixes).unwrap());
    for shell in shells() {
        let transport = through(&shell, dir.path());
        let mut there = Collect::default();
        transport.read_ranges(&ranges, &mut there).unwrap();
        assert!(there == here, "{shell}: {:?}", there.ends);
        assert_eq!(
            hashed(transport.hash_prefixes(&prefixes).unwrap()),
            hashed_here,
            "{shell}"
        );
    }
}
