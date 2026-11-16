//! The shell transport's launchers and far sides that misbehave (PLAN §12): tools that are
//! missing, a profile that prints or hangs, a launcher that fails, is slow, has a lot to say,
//! or is cut off, and an answer that is not one.

#![cfg(unix)]

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::transport::{
    Launcher, PrefixRequest, RangeRequest, ShellTransport, Transport, TransportError,
};
use sb_test_support::{loopback, shells};
use tempfile::TempDir;

use common::shell::{
    Collect, Ends, launched, noise, stand_in, text, through, tool, tools, with_tools,
};

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
        ["stat", "tail"]
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
        assert!(probe.clock_ahead_ns.abs() < 60_000_000_000, "{shell}");
        assert!(probe.took > Duration::ZERO && probe.took < Duration::from_secs(60));
    }
}

#[test]
fn how_far_a_clock_is_ahead_does_not_count_the_rest_of_the_test() {
    /* The same clock, read through a location whose login shell takes its time. */
    let home = home_with_profiles("sleep 3");
    let slow = ShellTransport::new(loopback("sh", home.path(), &[pair("SHELL", "sh")]));
    let probe = slow.probe().unwrap();
    assert!(probe.took >= Duration::from_secs(3), "{:?}", probe.took);
    /* A clock in whole seconds reads up to one behind. */
    assert!(
        probe.clock_ahead_ns.abs() < 2_000_000_000,
        "{}",
        probe.clock_ahead_ns
    );

    /* A location whose clock is an hour ahead: its date knows no nanoseconds, and adds an
    hour to the seconds. */
    let tools = tools(&[]);
    stand_in(
        tools.path(),
        "date",
        "printf '%s\\n' \"$(($(\"$real\" +%s) + 3600))\"",
    );
    let ahead = with_tools(tools.path()).probe().unwrap().clock_ahead_ns;
    assert!((ahead - 3_600_000_000_000).abs() < 5_000_000_000, "{ahead}");
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
    /* Its verdict first, and last how far the script was written to it: whole into the pipe,
    or not at all when it ended first. */
    let said = failed(launched("echo 'no such distro' >&2; exit 3"));
    let said: Vec<&str> = said.lines().collect();
    assert_eq!(said[0], "no such distro");
    assert!(
        said[1].starts_with("the script was written whole (")
            || said[1].starts_with("the script could not be written: "),
        "{said:?}"
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
            Err(TransportError::Failed(said)) => {
                assert_eq!(said.lines().next(), Some("Connection lost"), "{cut}");
            }
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

/// A home folder whose profiles, whichever of them a login shell reads, greet, print what
/// could pass for a record, and set two variables.
fn home_with_profiles(last_line: &str) -> TempDir {
    let home = tempfile::tempdir().unwrap();
    for profile in [".profile", ".bash_profile", ".zprofile"] {
        fs::write(
            home.path().join(profile),
            format!(
                "echo 'Welcome back!'\nprintf 'V SB_FORGED\\nforged\\0'\n\
                 export SB_FROM_PROFILE=\"set in a profile, it's \\\"here\\\"\"\n\
                 export SB_BOTH=profile\n{last_line}\n"
            ),
        )
        .unwrap();
    }
    home
}

/// The variables a provider might ask about, as `transport` finds them.
fn variables(transport: &ShellTransport) -> Vec<(String, String)> {
    let names = [
        "SB_FROM_PROFILE",
        "SB_BOTH",
        "SB_BARE",
        "SB_FORGED",
        "SB_UNSET",
    ];
    let mut vars: Vec<_> = transport.env(&names).unwrap().vars.into_iter().collect();
    vars.sort();
    vars
}

fn pair(name: &str, value: &str) -> (String, String) {
    (name.to_string(), value.to_string())
}

#[test]
fn what_a_login_shells_profile_sets_is_seen_and_what_it_prints_is_not() {
    let home = home_with_profiles("");
    for shell in shells() {
        /* The shell as the user's login shell, too: each reads a profile of its own. */
        let login = text(&tool(&shell).unwrap_or_else(|| PathBuf::from(&shell)));
        let vars = [
            pair("SHELL", &login),
            pair("SB_BOTH", "command"),
            pair("SB_BARE", "bare"),
        ];
        let transport = ShellTransport::new(loopback(&shell, home.path(), &vars));
        assert_eq!(
            variables(&transport),
            [
                pair("SB_BARE", "bare"),
                pair("SB_BOTH", "profile"),
                pair("SB_FROM_PROFILE", "set in a profile, it's \"here\""),
            ],
            "{shell}"
        );
        let probe = transport.probe().unwrap();
        assert_eq!(probe.login_shell, Some(login), "{shell}");
        assert!(probe.shell.ends_with("sh"), "{shell}: {}", probe.shell);
    }
}

#[test]
fn a_login_shell_that_does_not_answer_leaves_the_bare_environment() {
    let bare = [pair("SB_BARE", "bare"), pair("SB_BOTH", "command")];
    /* A profile that fails, and a login shell that is not there: neither would answer
    another time. */
    for (last_line, login) in [("exit 3", "sh"), ("", "/nonexistent/shell")] {
        let home = home_with_profiles(last_line);
        let vars = [
            pair("SHELL", login),
            pair("SB_BOTH", "command"),
            pair("SB_BARE", "bare"),
        ];
        let transport = ShellTransport::new(loopback("sh", home.path(), &vars));
        let started = Instant::now();
        assert_eq!(variables(&transport), bare, "{last_line} {login}");
        assert_eq!(transport.probe().unwrap().login_shell, None, "{last_line}");
        assert!(started.elapsed() < Duration::from_secs(30), "{last_line}");
    }
}

/// The lines of `file`, in a home folder whose profile adds one each time it is read.
fn times_read(file: &std::path::Path) -> usize {
    fs::read_to_string(file).unwrap_or_default().lines().count()
}

#[test]
fn the_login_shell_is_asked_once_in_the_life_of_a_transport() {
    for (last_line, both) in [("", "profile"), ("exit 3", "command")] {
        let home = home_with_profiles(&format!("echo read >>\"$HOME/read\"\n{last_line}"));
        let vars = [pair("SHELL", "sh"), pair("SB_BOTH", "command")];
        let transport = ShellTransport::new(loopback("sh", home.path(), &vars));
        let read = home.path().join("read");
        for _ in 0..3 {
            assert_eq!(
                transport.env(&["SB_BOTH"]).unwrap().vars["SB_BOTH"],
                both,
                "{last_line}"
            );
        }
        /* A copy of it knows what it knows. */
        let copy = transport.clone();
        assert_eq!(copy.env(&["SB_BOTH", "7"]).unwrap().vars.len(), 1);
        assert_eq!(copy.env(&[]).unwrap().home, text(home.path()));
        assert_eq!(times_read(&read), 1, "{last_line}");
        /* A name it has not asked about is asked of a login shell that answers, and not of
        one that had no answer. */
        let more = transport.env(&["SB_FROM_PROFILE", "SB_BOTH"]).unwrap().vars;
        let (times, known) = if last_line.is_empty() { (2, 2) } else { (1, 1) };
        assert_eq!((times_read(&read), more.len()), (times, known), "{more:?}");
        /* Another transport starts over. */
        let other = ShellTransport::new(loopback("sh", home.path(), &vars));
        assert_eq!(other.env(&["SB_BOTH"]).unwrap().vars["SB_BOTH"], both);
        assert_eq!(times_read(&read), times + 1, "{last_line}");
    }
}

#[test]
fn a_login_shell_that_hangs_fails_the_call_and_is_killed_with_what_it_started() {
    for shell in shells() {
        /* A profile that leaves something waiting, and waits for it. */
        let home = home_with_profiles("sleep 61 &\necho $! >\"$HOME/waiting\"\nwait");
        let vars = [pair("SHELL", "sh"), pair("SB_BOTH", "command")];
        let transport = ShellTransport::new(loopback(&shell, home.path(), &vars));
        let started = Instant::now();
        /* Not the bare environment, which may name another place than the profile would. */
        match transport.env(&["SB_BOTH"]) {
            Err(TransportError::Failed(said)) => assert!(said.contains("login shell"), "{said}"),
            other => panic!("{shell}: {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(30), "{shell}");
        /* Through a container, what runs there can't be seen from here. */
        if std::env::var_os(sb_test_support::SHELL_VIA).is_none() {
            let waiting = fs::read_to_string(home.path().join("waiting")).unwrap();
            let waiting = waiting.trim();
            /* By the shell's own kill: not every system has the program. A process that
            has ended and that nothing collects, as in a container without an init, still
            takes a signal: Linux says in /proc that it is one. */
            let alive = || {
                let ended = fs::read_to_string(format!("/proc/{waiting}/stat"))
                    .is_ok_and(|stat| stat.rsplit(')').next().unwrap().starts_with(" Z"));
                !ended
                    && std::process::Command::new("sh")
                        .args(["-c", "kill -0 \"$1\"", "sh", waiting])
                        .stderr(std::process::Stdio::null())
                        .status()
                        .unwrap()
                        .success()
            };
            /* Killed, and a moment later no longer a process at all. */
            let gone = (0..40).any(|_| {
                std::thread::sleep(Duration::from_millis(50));
                !alive()
            });
            assert!(gone, "{shell}: {waiting} is still running");
        }
        /* It is asked again by the next call, and what does not need it works. */
        if shell == "sh" {
            assert!(matches!(transport.probe(), Err(TransportError::Failed(_))));
            assert_eq!(transport.env(&[]).unwrap().home, text(home.path()));
        }
    }
}

/// Requests enough to make a script larger than a pipe holds.
fn a_large_script() -> Vec<RangeRequest> {
    (0..256)
        .map(|n| RangeRequest {
            path: format!("/nowhere/{}{n}", "p".repeat(240)),
            offset: 0,
            length: 1,
        })
        .collect()
}

#[test]
fn a_cancel_is_seen_while_the_launcher_has_not_read_the_script() {
    /* A launcher that takes its time to read, as ssh does while it connects, and one that
    never reads. */
    for line in ["sleep 5; exec sh -s", "sleep 60"] {
        for requests in [&a_large_script()[..1], &a_large_script()] {
            let cancel = CancelToken::new();
            let transport = launched(line).cancelled_by(&cancel);
            let started = Instant::now();
            let cancelled = std::thread::scope(|scope| {
                scope.spawn(|| {
                    std::thread::sleep(Duration::from_millis(300));
                    cancel.cancel();
                });
                transport.read_ranges(requests, &mut Collect::default())
            });
            assert!(
                matches!(cancelled, Err(TransportError::Cancelled)),
                "{line}: {cancelled:?}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "{line}, {} requests: {:?}",
                requests.len(),
                started.elapsed()
            );
        }
    }
}

#[test]
fn a_call_that_is_cancelled_already_starts_nothing() {
    /* A launcher that can't be started says so, to a call that tries. */
    let nowhere = || {
        ShellTransport::new(Launcher::Command {
            program: "/nonexistent/launcher".into(),
            args: Vec::new(),
        })
    };
    assert!(matches!(
        nowhere().env(&[]),
        Err(TransportError::Launch { .. })
    ));
    let cancel = CancelToken::new();
    cancel.cancel();
    let cancelled = nowhere().cancelled_by(&cancel);
    assert!(matches!(cancelled.env(&[]), Err(TransportError::Cancelled)));
    assert!(matches!(cancelled.probe(), Err(TransportError::Cancelled)));
    assert!(matches!(
        cancelled.stat(&["/"]),
        Err(TransportError::Cancelled)
    ));
}

#[test]
fn a_launcher_with_a_lot_to_say_first_holds_nothing_up() {
    /* As ssh at a debug log level: more than a pipe holds, on stderr, before its shell
    reads a script that is itself more than a pipe holds. */
    let chatty = launched(
        "i=0; while [ $i -lt 20000 ]; do echo \"debug1: a line of ssh -v output $i\" >&2; \
         i=$((i+1)); done; exec sh -s",
    );
    let mut ends = Ends::default();
    chatty.read_ranges(&a_large_script(), &mut ends).unwrap();
    assert_eq!(ends.0, vec!["it is no longer there"; 256]);
}

#[test]
fn a_greeting_of_any_length_is_passed_over_and_what_is_no_answer_is_said_to_be_none() {
    let greeting = |bytes: usize| {
        launched(&format!(
            "head -c {bytes} /dev/zero | tr '\\0' n; echo; exec sh -s"
        ))
    };
    assert!(greeting(200_000).env(&[]).is_ok());
    match greeting(3_000_000).env(&[]) {
        Err(TransportError::Protocol(said)) => {
            assert!(said.contains("without beginning its answer"), "{said}");
        }
        other => panic!("{other:?}"),
    }
}

/// A transport whose far side answers `records` to whatever it is asked, after the line its
/// answer must begin with and the word that no tool is missing.
fn answering(records: &str) -> ShellTransport {
    launched(&format!(
        "marker=$(sed -n \"s/^printf '%s\\\\\\\\n' '\\(scrollback answer [0-9a-f]*\\)'$/\\1/p\"); \
         printf '%s\\nK\\n' \"$marker\"; printf '{records}'"
    ))
}

#[test]
fn a_group_that_is_not_the_one_asked_for_is_not_taken() {
    let requests = ["/a", "/b"].map(|path| RangeRequest {
        path: path.to_string(),
        offset: 0,
        length: 2,
    });
    let read = |records: &str| {
        let mut sink = Collect::default();
        let read = answering(records).read_ranges(&requests, &mut sink);
        (read, sink)
    };
    /* stat's line for a file of so many bytes. */
    let stat = |size: u64| format!("81a4 {size} 1700000000\\n");
    let group =
        |total: u64, size: u64| format!("G 2 {total}\\n{0}{0}abcdx{0}{0}.\\nZ\\n", stat(size));
    /* As it should be. */
    let (taken, sink) = read(&group(4, 2));
    taken.unwrap();
    assert_eq!(sink.bytes, [b"ab", b"cd"]);
    assert_eq!(sink.ends, [Ok(()), Ok(())]);
    /* More bytes than were asked for and files may have grown by, and a group of another
    number of files: neither is an answer to what was asked. */
    for records in ["G 2 99999999\\n", "G 3 4\\n"] {
        match read(records) {
            (Err(TransportError::Protocol(said)), _) => assert!(said.contains("G "), "{said}"),
            (other, _) => panic!("{records}: {other:?}"),
        }
    }
    /* Sizes that are not what the block holds: nothing of it is passed on, and the files
    are asked for again by themselves, which this far side has no answer to. */
    let (asked_again, sink) = read(&group(4, 1));
    assert!(
        matches!(&asked_again, Err(TransportError::Protocol(said)) if said.contains("R record")),
        "{asked_again:?}"
    );
    assert_eq!((sink.bytes.len(), sink.ends.len()), (0, 0));
}

#[test]
fn a_block_that_is_not_one_fails_the_call() {
    let request = |length| {
        [RangeRequest {
            path: "/file".to_string(),
            offset: 0,
            length,
        }]
    };
    let protocol = |transport: ShellTransport, length| {
        let mut sink = Collect::default();
        match transport.read_ranges(&request(length), &mut sink) {
            Err(TransportError::Protocol(said)) => said,
            other => panic!("{other:?}, {sink:?}"),
        }
    };
    /* As it should be. */
    let mut sink = Collect::default();
    answering("R 2\\nabxE\\n\\0Z\\n")
        .read_ranges(&request(2), &mut sink)
        .unwrap();
    assert_eq!(
        (sink.bytes, sink.ends),
        (vec![b"ab".to_vec()], vec![Ok(())])
    );
    /* No mark where the file's bytes end; a mark that is not the last byte that is not
    zero; a block longer than the range. */
    for (records, length) in [
        ("R 2\\nabcE\\n\\0Z\\n", 2),
        ("R 2\\n\\0\\0\\0E\\n\\0Z\\n", 2),
        ("R 3\\naxbcE\\n\\0Z\\n", 3),
        ("R 5\\nabcdexE\\n\\0Z\\n", 2),
    ] {
        let said = protocol(answering(records), length);
        assert!(said.contains("block"), "{records}: {said}");
    }
}
