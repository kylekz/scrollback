//! The shell transport, hermetically (PLAN §12): the scripts a WSL distro or an SSH host is
//! read with, run by each shell this machine has against trees in a temp dir, and held to
//! what `LocalTransport` says of the same trees. On Windows these run inside WSL.
//!
//! `SB_TEST_SHELL` and `SB_TEST_SHELL_VIA` (`sb_test_support`) run them in one shell, or in
//! another userland's.

#![cfg(unix)]

use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::hash::Hash;
use sb_core::transport::{
    FileEntry, FileStat, Halt, Hashed, Launcher, Listing, LocalTransport, PrefixRequest,
    RangeFailure, RangeRequest, RangeSink, ShellTransport, Transport, TransportError, Unlisted,
};
use sb_test_support::{loopback, shells};
use tempfile::TempDir;

/// What a sink was given: each range's bytes, and how each ended.
#[derive(Default, Debug, PartialEq)]
struct Collect {
    bytes: Vec<Vec<u8>>,
    ends: Vec<Result<(), String>>,
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

fn text(path: &Path) -> String {
    path.to_str().unwrap().to_string()
}

/// A transport over `shell` whose home folder is `home`.
fn through(shell: &str, home: &Path) -> ShellTransport {
    ShellTransport::new(loopback(shell, home, &[]))
}

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

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Bytes that are not text: every value, zeros in runs, and no pattern a shell would keep.
fn noise(length: usize) -> Vec<u8> {
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
fn megabytes(count: usize) -> Vec<u8> {
    noise(1024 * 1024).repeat(count)
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
fn a_file_removed_while_it_is_read_is_missing_and_the_stream_stays_in_step() {
    for shell in shells() {
        let (read, _) = read_while(&shell, |large| fs::remove_file(large).unwrap());
        assert_eq!(
            read.ends,
            [Err("it is no longer there".to_string()), Ok(())],
            "{shell}"
        );
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

/// A folder to be a `PATH`: links to the tools the transport needs, but `without`.
fn tools(without: &[&str]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = std::env::var_os("PATH").unwrap();
    for tool in [
        "sh",
        "find",
        "stat",
        "head",
        "tail",
        "date",
        "id",
        "sha256sum",
        "shasum",
    ] {
        if without.contains(&tool) {
            continue;
        }
        if let Some(found) = std::env::split_paths(&path)
            .map(|dir| dir.join(tool))
            .find(|found| found.is_file())
        {
            symlink(found, dir.path().join(tool)).unwrap();
        }
    }
    dir
}

/// A transport over `sh` that finds only the tools in `path`.
fn with_tools(path: &Path) -> ShellTransport {
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

#[test]
fn a_missing_tool_is_named_and_what_does_not_need_it_still_works() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "0123456789").unwrap();
    let file = text(&dir.path().join("file"));
    let prefix = [PrefixRequest {
        path: file.clone(),
        length: 4,
    }];
    let missing = |result: Result<(), TransportError>| match result {
        Err(TransportError::MissingTools(tools)) => tools,
        other => panic!("{other:?}"),
    };

    /* Everything is there. */
    let all = tools(&[]);
    let whole = with_tools(all.path());
    assert_eq!(whole.probe().unwrap().missing, Vec::<String>::new());
    assert_eq!(
        whole.hash_prefixes(&prefix).unwrap()[0]
            .as_ref()
            .unwrap()
            .size,
        10
    );

    /* Neither tool that hashes: said by the connection test, and by a call that needs one. */
    let no_hash = tools(&["sha256sum", "shasum"]);
    let unhashed = with_tools(no_hash.path());
    assert_eq!(unhashed.probe().unwrap().missing, ["sha256sum or shasum"]);
    assert_eq!(
        missing(unhashed.hash_prefixes(&prefix).map(drop)),
        ["sha256sum or shasum"]
    );
    assert_eq!(unhashed.list(&text(dir.path())).unwrap().files.len(), 1);
    assert_eq!(unhashed.read_small(&file).unwrap().unwrap(), b"0123456789");

    /* Several at once. */
    let little = tools(&["find", "tail", "stat"]);
    let lacking = with_tools(little.path());
    assert_eq!(lacking.probe().unwrap().missing, ["find", "stat", "tail"]);
    assert_eq!(
        missing(lacking.list(&text(dir.path())).map(drop)),
        ["find", "stat"]
    );
    assert_eq!(
        missing(lacking.read_ranges(&[], &mut Collect::default())),
        ["tail"]
    );
}

#[test]
fn the_connection_test_says_what_is_there_and_how_long_it_took() {
    let home = tempfile::tempdir().unwrap();
    for shell in shells() {
        let probe = through(&shell, home.path()).probe().unwrap();
        assert_eq!(probe.home, text(home.path()), "{shell}");
        assert_eq!(probe.missing, Vec::<String>::new(), "{shell}");
        let clock = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        assert!(
            (clock.as_nanos() as i64 - probe.clock_ns).abs() < 60_000_000_000,
            "{shell}"
        );
        assert!(probe.took > Duration::ZERO && probe.took < Duration::from_secs(60));
    }
}

/// A transport whose launcher is `sh -c <line>`.
fn launched(line: &str) -> ShellTransport {
    ShellTransport::new(Launcher::Command {
        program: "sh".into(),
        args: vec!["-c".to_string(), line.to_string()],
    })
}

#[test]
fn what_is_printed_before_the_answer_is_passed_over() {
    /* As a login script's greeting is, on a host that runs one for every command. */
    let noisy = launched("echo 'Welcome!'; printf 'K\\nZ\\n\\0\\377'; echo; exec sh -s");
    assert!(noisy.env(&[]).unwrap().home.starts_with('/'));
    assert!(noisy.probe().unwrap().missing.is_empty());
}

#[test]
fn a_launcher_that_fails_says_why_in_its_own_words() {
    let failed = |transport: ShellTransport| match transport.env(&[]) {
        Err(TransportError::Failed(said)) => said,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        failed(launched("echo 'no such distro' >&2; exit 3")),
        "no such distro"
    );
    assert!(failed(launched("exit 3")).contains("ended before it answered"));
    assert!(failed(launched("echo 'not a shell at all'")).contains("ended before it answered"));

    let nowhere = ShellTransport::new(Launcher::Command {
        program: "/nonexistent/launcher".into(),
        args: Vec::new(),
    });
    match nowhere.env(&[]) {
        Err(TransportError::Launch { program, source }) => {
            assert_eq!(program, "/nonexistent/launcher");
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_answer_cut_off_fails_the_call_and_never_passes_for_a_whole_one() {
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<_> = (0..40)
        .map(|n| {
            let path = dir.path().join(format!("file {n}"));
            fs::write(&path, noise(5000)).unwrap();
            text(&path)
        })
        .collect();
    let requests: Vec<_> = files
        .iter()
        .map(|path| RangeRequest {
            path: path.clone(),
            offset: 0,
            length: 5000,
        })
        .collect();
    /* The connection goes after so many bytes of the answer, wherever in it that is. */
    for cut in [1, 20, 47, 48, 60, 5000, 5060, 12_345, 150_000] {
        let dropped = launched(&format!(
            "sh -s | head -c {cut}; echo 'Connection lost' >&2; exit 255"
        ));
        let mut sink = Collect::default();
        match dropped.read_ranges(&requests, &mut sink) {
            Err(TransportError::Failed(said)) => assert_eq!(said, "Connection lost", "{cut}"),
            other => panic!("cut at {cut}: {other:?}"),
        }
        /* No range ended well that had not arrived whole. */
        for (range, end) in sink.ends.iter().enumerate() {
            assert!(end.is_ok() && sink.bytes[range].len() == 5000, "{cut}");
        }
        assert!(sink.ends.len() < requests.len(), "{cut}");
        /* The other answers are shorter: only an early cut is inside them. */
        if cut < 2000 {
            let prefixes: Vec<_> = files
                .iter()
                .map(|path| PrefixRequest {
                    path: path.clone(),
                    length: 5000,
                })
                .collect();
            let failed = |result| matches!(result, Err(TransportError::Failed(_)));
            assert!(failed(dropped.list(&text(dir.path())).map(drop)), "{cut}");
            assert!(failed(dropped.hash_prefixes(&prefixes).map(drop)), "{cut}");
            assert!(failed(dropped.stat(&[&files[0]]).map(drop)), "{cut}");
            assert!(failed(dropped.read_small(&files[0]).map(drop)), "{cut}");
            assert!(cut > 48 || failed(dropped.env(&[]).map(drop)), "{cut}");
        }
    }
}
