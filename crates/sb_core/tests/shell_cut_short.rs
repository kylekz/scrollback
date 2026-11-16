//! A script that reaches the location's shell cut short (PLAN §6.2): the shell runs none of it
//! and ends in one of two ways, the call is run once more, and nothing reaches a sink twice.
//! Through stand-in launchers that lose or cut the first script they are given, and through
//! each shell this machine has.

#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::transport::{
    Launcher, PrefixRequest, RangeRequest, ShellTransport, Transport, TransportError,
};
use sb_test_support::shells;
use tempfile::TempDir;

use common::shell::{Collect, noise, text};

/// Every warning logged in this process.
static WARNED: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Warnings;

impl log::Log for Warnings {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            WARNED.lock().unwrap().push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

/// A launcher whose first run does `first` and every later one `later`: lines of `sh` with
/// the script on their stdin. It counts its runs in `dir`, whose path its warnings name.
fn far_side(dir: &Path, first: &str, later: &str) -> ShellTransport {
    /* Tests in this binary run at once: the logger is set by whichever is first. */
    let _ = log::set_logger(&Warnings);
    log::set_max_level(log::LevelFilter::Warn);
    let dir = text(dir);
    launched(&format!(
        "echo run >>'{dir}/runs'; if mkdir '{dir}/ran' 2>/dev/null; then {first}; else {later}; fi"
    ))
}

/**
 * [`common::shell::launched`], in the userland `SB_TEST_SHELL_VIA` names when it names one:
 * the stand-ins' `head`, `cat` and shell are then that userland's, as `shells()` are.
 */
fn launched(line: &str) -> ShellTransport {
    let via = std::env::var(sb_test_support::SHELL_VIA).unwrap_or_default();
    let mut words = via.split_whitespace().map(str::to_string).chain([
        "env".to_string(),
        "-i".to_string(),
        format!("PATH={}", std::env::var("PATH").unwrap()),
        "HOME=/nonexistent".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        line.to_string(),
    ]);
    ShellTransport::new(Launcher::Command {
        program: words.next().unwrap().into(),
        args: words.collect(),
    })
}

/// How many times the launcher in `dir` was run, and the warnings that name it.
fn runs(dir: &Path) -> (usize, Vec<String>) {
    let runs = fs::read_to_string(dir.join("runs")).unwrap_or_default();
    let warned = WARNED
        .lock()
        .unwrap()
        .iter()
        .filter(|warning| warning.contains(&text(dir)))
        .cloned()
        .collect();
    (runs.lines().count(), warned)
}

/// What swallows the script and ends with exit status 0, having said nothing.
const SWALLOW: &str = "cat >/dev/null";

/// A folder of three small files, and what each operation finds in it, in text that does not
/// change from one call to the next.
fn files() -> (TempDir, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let paths = (0..3)
        .map(|n| {
            let path = dir.path().join(format!("file {n}"));
            fs::write(&path, noise(100 + n)).unwrap();
            text(&path)
        })
        .collect();
    (dir, paths)
}

type Operation = fn(&ShellTransport, &Path, &[String]) -> Result<String, TransportError>;

/// Every operation of the transport, each with what it answered.
const OPERATIONS: [(&str, Operation); 7] = [
    ("env", |transport, _, _| Ok(transport.env(&[])?.home)),
    ("probe", |transport, _, _| {
        Ok(format!("{:?}", transport.probe()?.missing))
    }),
    ("stat", |transport, _, paths| {
        Ok(format!("{:?}", transport.stat(&[paths[0].as_str()])?))
    }),
    ("list", |transport, dir, _| {
        let mut files = transport.list(&text(dir))?.files;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(format!("{files:?}"))
    }),
    ("hash_prefixes", |transport, _, paths| {
        let requests: Vec<_> = paths
            .iter()
            .map(|path| PrefixRequest {
                path: path.clone(),
                length: 50,
            })
            .collect();
        Ok(format!("{:?}", transport.hash_prefixes(&requests)?))
    }),
    ("read_ranges", |transport, _, paths| {
        /* One range by itself, then two whole files that are read as a group. */
        let requests = [
            RangeRequest {
                path: paths[0].clone(),
                offset: 10,
                length: 20,
            },
            RangeRequest {
                path: paths[1].clone(),
                offset: 0,
                length: 101,
            },
            RangeRequest {
                path: paths[2].clone(),
                offset: 0,
                length: 102,
            },
        ];
        let mut sink = Collect::default();
        transport.read_ranges(&requests, &mut sink)?;
        Ok(format!("{sink:?}"))
    }),
    ("read_small", |transport, _, paths| {
        Ok(format!("{:?}", transport.read_small(&paths[0])?))
    }),
];

#[test]
fn every_operation_whose_script_was_lost_is_run_again_and_answers_once() {
    let (files, paths) = files();
    for (name, operation) in OPERATIONS {
        let expected = operation(&launched("exec sh -s"), files.path(), &paths).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let transport = far_side(dir.path(), SWALLOW, "exec sh -s");
        /* For `read_ranges`, the sink's record of each range: had anything arrived twice, it
        would not be the one an undisturbed call leaves. */
        assert_eq!(
            operation(&transport, files.path(), &paths).unwrap(),
            expected,
            "{name}"
        );
        let (runs, warned) = runs(dir.path());
        assert_eq!((runs, warned.len()), (2, 1), "{name}: {warned:?}");
        assert!(warned[0].contains("exit status: 0"), "{}", warned[0]);
        assert!(
            warned[0].contains("the script was written whole"),
            "{}",
            warned[0]
        );
        /* Its words, not the script. */
        assert!(warned[0].contains(SWALLOW) && !warned[0].contains("flavour_of_stat"));
    }
}

#[test]
fn a_script_cut_inside_a_function_is_run_again_under_each_shell() {
    let (_files, paths) = files();
    for shell in shells() {
        let expected = format!("{:?}", through(&shell).stat(&[paths[0].as_str()]).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let transport = far_side(
            dir.path(),
            &format!("head -c 3000 | {shell} -s"),
            &format!("exec {shell} -s"),
        );
        let stat = transport.stat(&[paths[0].as_str()]).unwrap();
        assert_eq!(format!("{stat:?}"), expected, "{shell}");
        let (runs, warned) = runs(dir.path());
        assert_eq!((runs, warned.len()), (2, 1), "{shell}: {warned:?}");
    }
}

/// A transport over `shell`, with nothing in the way.
fn through(shell: &str) -> ShellTransport {
    launched(&format!("exec {shell} -s"))
}

#[test]
fn a_script_cut_short_twice_fails_the_call_once_with_both_attempts() {
    for cut in [SWALLOW, "head -c 3000 | sh -s"] {
        let dir = tempfile::tempdir().unwrap();
        let transport = far_side(dir.path(), cut, cut);
        let said = match transport.env(&[]) {
            Err(TransportError::Failed(said)) => said,
            other => panic!("{cut}: {other:?}"),
        };
        /* Each attempt as the launcher ended it, and how far the script was written. */
        let attempts: Vec<&str> = said.split("\nit was run twice; the first time: ").collect();
        assert_eq!(attempts.len(), 2, "{said}");
        for attempt in attempts {
            assert!(
                attempt.lines().last().unwrap().starts_with("the script "),
                "{said}"
            );
            if cut == SWALLOW {
                assert!(attempt.starts_with("it ended before it answered"), "{said}");
            }
        }
        let (runs, warned) = runs(dir.path());
        assert_eq!((runs, warned.len()), (2, 1), "{cut}");
    }
}

#[test]
fn an_answer_that_began_and_then_stopped_is_not_run_again() {
    let dir = tempfile::tempdir().unwrap();
    /* The marker, the word that no tool is missing, and two of a range's five bytes. */
    let transport = far_side(
        dir.path(),
        "marker=$(sed -n \"s/^printf '%s\\\\\\\\n' '\\(scrollback answer [0-9a-f]*\\)'$/\\1/p\"); \
         printf '%s\\nK\\nR 5\\nab' \"$marker\"",
        "exec sh -s",
    );
    let requests = [RangeRequest {
        path: "/file".to_string(),
        offset: 0,
        length: 5,
    }];
    let mut sink = Collect::default();
    let failed = transport.read_ranges(&requests, &mut sink);
    assert!(
        matches!(&failed, Err(TransportError::Failed(said)) if said.contains("ended before")),
        "{failed:?}"
    );
    /* What arrived was the start of a range that never ended: had the call been run again,
    it would arrive a second time. */
    assert_eq!(sink.ends.len(), 0);
    assert_eq!(runs(dir.path()), (1, Vec::new()));
}

#[test]
fn a_script_the_launcher_would_not_take_is_said_to_be_unwritten() {
    /* More than a pipe holds, to a launcher that closes its stdin and fails. */
    let paths: Vec<String> = (0..1000)
        .map(|n| format!("/nowhere/{}{n}", "p".repeat(240)))
        .collect();
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    let dir = tempfile::tempdir().unwrap();
    /* On this machine whatever the userland: a container's client would take the script. */
    let closed = common::shell::launched(&format!(
        "echo run >>'{}/runs'; exec 0<&-; echo 'no such distro' >&2; exit 3",
        text(dir.path())
    ));
    let said = match closed.stat(&paths) {
        Err(TransportError::Failed(said)) => said,
        other => panic!("{other:?}"),
    };
    let said: Vec<&str> = said.lines().collect();
    assert_eq!(said[0], "no such distro");
    assert!(
        said[1].starts_with("the script could not be written: ") && said[1].contains(" bytes)"),
        "{said:?}"
    );
    assert_eq!(runs(dir.path()), (1, Vec::new()));
}

#[test]
fn a_cancel_during_the_first_attempt_is_not_followed_by_another() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancelToken::new();
    let transport =
        far_side(dir.path(), "cat >/dev/null; sleep 2", "exec sh -s").cancelled_by(&cancel);
    let started = Instant::now();
    let cancelled = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            cancel.cancel();
        });
        transport.env(&[])
    });
    assert!(
        matches!(cancelled, Err(TransportError::Cancelled)),
        "{cancelled:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(runs(dir.path()), (1, Vec::new()));
}

/**
 * The experiment the retry rests on, over the real script: cut short anywhere, it is either
 * answered whole the first time or ends in a way that has the call run again, and is then
 * answered whole. It is never taken for an answer, and never fails the call.
 */
#[test]
fn a_script_cut_anywhere_runs_nothing_and_the_call_is_answered_whole() {
    let (_files, paths) = files();
    let file = paths[0].as_str();
    std::thread::scope(|scope| {
        for shell in shells() {
            scope.spawn(move || {
                /* The script of the call, as the shell gets it. */
                let saved = tempfile::tempdir().unwrap();
                let script = saved.path().join("script");
                let expected = launched(&format!("tee '{}' | {shell} -s", text(&script)))
                    .stat(&[file])
                    .unwrap();
                let script = fs::read(&script).unwrap();
                let length = script.len();
                /* Evenly through it; where each line starts, inside a function's name or
                between two of them; and each of its last bytes. */
                let mut cuts: Vec<usize> = (0..length).step_by(89).collect();
                for (at, pair) in script.windows(2).enumerate() {
                    if pair[0] == b'\n' && pair[1].is_ascii_alphabetic() {
                        cuts.extend(at + 1..at + 4);
                    }
                }
                cuts.extend(length - 16..=length);
                cuts.sort_unstable();
                cuts.dedup();
                for cut in cuts {
                    let dir = tempfile::tempdir().unwrap();
                    let first = match cut {
                        0 => format!("cat >/dev/null; : | {shell} -s"),
                        _ => format!("head -c {cut} | {shell} -s"),
                    };
                    let answered =
                        far_side(dir.path(), &first, &format!("exec {shell} -s")).stat(&[file]);
                    match answered {
                        Ok(stat) => assert_eq!(stat, expected, "{shell}, cut at {cut}"),
                        Err(error) => panic!("{shell}, cut at {cut} of {length}: {error}"),
                    }
                    /* Short of its last newline, the script was not the whole call. */
                    let (runs, _) = runs(dir.path());
                    assert_eq!(runs, if cut + 1 < length { 2 } else { 1 }, "{shell} {cut}");
                }
            });
        }
    });
}
