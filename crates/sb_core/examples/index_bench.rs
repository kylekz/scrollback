//! A benchmark of the index of a start (PLAN §7.5) on a generated archive: every row out of
//! date, as a new version of the parser leaves them, and indexed again from the archive alone.
//!
//! The first run in a folder writes a tree of the file-tree agent's sessions under it
//! (`home/`), `--sessions` of them, syncs it into an archive beside it (`archive/`) and removes
//! the tree; later runs in the folder reuse the archive. The sessions have a long tail of
//! sizes: most are a few turns, some a few dozen, one in ten a few hundred with subagents, and
//! one in two hundred run to several megabytes with many subagents. Tool output has a long tail
//! of its own. The longer a session, the more syncs it spans while it is written, each adding
//! a segment to its items, as an interval sync of a running session does. It is the same tree
//! for the same `--sessions` and `--seed`.
//!
//! Each of `--runs` runs marks every row and every session's search text as made by an older
//! parser, so the items are said again what they are and every session is parsed, indexed
//! and made searchable again, and times it. With `--changed` every span of the text is first
//! made to differ from what the parser makes of it, as when a new parser or index says each
//! session's text otherwise. It prints the archive's shape and each run's times: aggregates
//! only.
//!
//! ```text
//! cargo run --release -p sb_core --example index_bench -- target/bench/index
//! cargo run --release -p sb_core --example index_bench -- target/bench/index --sessions 3000 --runs 3
//! ```

use std::error::Error;
use std::ops::Range;
use std::path::Path;
use std::time::Instant;

use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::location::{LocationId, LocationKind};
use sb_core::provider::Provider;
use sb_core::sync::{Progress, SyncError, index, sync_location};
use sb_test_support::{
    Agent as _, FileAgent, ProjectsTree, Records, Session, edit_catalogue, read_catalogue,
};
use serde_json::json;
use toolbox_store::rusqlite::Connection;

const USAGE: &str = "usage: index_bench <folder under target/> [--sessions <n>] [--seed <n>] \
                     [--runs <n>] [--changed]";

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let folder = args.next().ok_or(USAGE)?;
    let (mut sessions, mut seed, mut runs, mut changed) = (3000, 1, 1, false);
    while let Some(flag) = args.next() {
        if flag == "--changed" {
            changed = true;
            continue;
        }
        let value: u64 = args.next().ok_or(USAGE)?.parse()?;
        match flag.as_str() {
            "--sessions" => sessions = usize::try_from(value)?,
            "--seed" => seed = value,
            "--runs" => runs = value,
            _ => return Err(USAGE.into()),
        }
    }
    let folder = Path::new(&folder);
    let dir = folder.join("archive");
    if !dir.exists() {
        let started = Instant::now();
        let home = folder.join("home");
        generate(&home, &dir, sessions, seed)?;
        std::fs::remove_dir_all(home)?;
        println!("generated and archived in {:.1?}", started.elapsed());
    }
    let count = |sql: &str| read_catalogue(&dir, sql).remove(0);
    println!(
        "{} sessions, {} items, {} segments; {} MB of transcripts, {} MB on disk",
        count("SELECT count(*) FROM session"),
        count("SELECT count(*) FROM item"),
        count("SELECT count(*) FROM segment"),
        count(
            "SELECT sum(v.length) >> 20 FROM item i JOIN version v ON v.item_id = i.id
             WHERE i.kind IN ('transcript', 'subagent')"
        ),
        stored(&dir.join("files")) >> 20,
    );
    for run in 1..=runs {
        /* What a new parser finds at a start: rows and text it did not make, and items it has
        not classified. */
        edit_catalogue(
            &dir,
            "UPDATE session SET parser_version = parser_version - 1;
             UPDATE location_provider SET classified_by = NULL;",
        );
        let search = Connection::open(dir.join("search.db"))?;
        search.execute("UPDATE doc SET made = 'older'", [])?;
        if changed {
            /* Text the new parser says otherwise, every span of it: the most an index has to
            take out of the search index and write again. */
            search.execute_batch(
                "INSERT INTO span_text (span_text) VALUES ('delete-all');
                 UPDATE span SET text = text || ' (older)';
                 INSERT INTO span_text (span_text) VALUES ('rebuild');",
            )?;
        }
        drop(search);
        let archive = Archive::open(&dir)?;
        let started = Instant::now();
        let mut first = None;
        let report = index(
            &archive,
            None,
            &[provider()],
            &CancelToken::new(),
            &mut |progress| {
                if let Progress::Searching { .. } = progress {
                    first.get_or_insert(started.elapsed());
                }
            },
        )?;
        let total = started.elapsed();
        let first = first.unwrap_or(total);
        println!(
            "run {run}: {} sessions in {:.2?} (step 1 {:.2?}, step 2 {:.2?}); {} failed, {} \
             searchable",
            report.sessions,
            total,
            first,
            total.saturating_sub(first),
            report.failed.len(),
            report.searchable,
        );
    }
    Ok(())
}

fn provider() -> Box<dyn Provider> {
    FileAgent::new().provider()
}

/// How many bytes the files under `dir` hold.
fn stored(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => stored(&entry.path()),
            _ => entry.metadata().map_or(0, |metadata| metadata.len()),
        })
        .sum()
}

/// A session being written: how many turns it has left, over how many more syncs.
struct Writing {
    session: Session,
    turns: usize,
    syncs: usize,
    subagents: usize,
}

/**
 * Writes `sessions` sessions over a few dozen projects into a tree in `home`, syncing it into
 * a new archive in `dir` after each round of writing, and indexes it once all is written.
 */
fn generate(home: &Path, dir: &Path, sessions: usize, seed: u64) -> Result<(), Box<dyn Error>> {
    let mut draw = Draw(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut tree = ProjectsTree::in_home(home);
    let mut writing: Vec<Writing> = (0..sessions)
        .map(|_| {
            let project = format!("/home/user/code/project-{}", draw.within(0, 40));
            let (turns, syncs, subagents) = match draw.within(0, 200) {
                0 => (
                    draw.within(400, 1500),
                    draw.within(20, 40),
                    draw.within(4, 12),
                ),
                1..=19 => (draw.within(80, 400), draw.within(4, 20), draw.within(1, 4)),
                20..=69 => (draw.within(8, 80), draw.within(1, 6), 0),
                _ => (draw.within(1, 8), 1, 0),
            };
            Writing {
                session: tree.session(&project, 0),
                turns,
                syncs,
                subagents,
            }
        })
        .collect();
    let archive = Archive::create(dir)?;
    let location = archive
        .catalogue()
        .add_location(LocationKind::Local, "generated")?
        .id;
    while writing.iter().any(|writing| writing.syncs > 0) {
        for writing in writing.iter_mut().filter(|writing| writing.syncs > 0) {
            let turns = writing.turns / writing.syncs;
            for _ in 0..turns {
                let subagent = draw.within(0, writing.turns) < writing.subagents;
                turn(&mut writing.session, &mut draw, subagent);
                writing.subagents -= usize::from(subagent);
                writing.turns -= 1;
            }
            writing.syncs -= 1;
        }
        sync(&archive, &location, &tree)?;
    }
    index(
        &archive,
        None,
        &[provider()],
        &CancelToken::new(),
        &mut |_| {},
    )?;
    Ok(())
}

/// Stores what changed in the tree since the last sync, and stops before the pass indexes:
/// the archive is indexed once, when it is all written.
fn sync(
    archive: &Archive,
    location: &LocationId,
    tree: &ProjectsTree,
) -> Result<(), Box<dyn Error>> {
    let cancel = CancelToken::new();
    let synced = sync_location(
        archive,
        location,
        &tree.transport(),
        &[provider()],
        &cancel,
        &mut |progress| {
            if let Progress::Indexing { .. } = progress {
                cancel.cancel();
            }
        },
    );
    match synced {
        Ok(_) | Err(SyncError::Cancelled) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// One turn: a prompt, a few calls with output of every length, maybe a subagent, a reply.
fn turn(session: &mut Session, draw: &mut Draw, subagent: bool) {
    let prompt = words(draw, 5..80);
    session.transcript.prompt(&prompt);
    calls(&mut session.transcript, draw, 0..6);
    if subagent {
        let task = words(draw, 10..60);
        let call = session.transcript.call(
            "Agent",
            json!({ "subagent_type": "general-purpose", "description": "Look into it",
                    "prompt": task }),
        );
        let mut records = session.subagent_of(&call, "general-purpose", "Look into it");
        records.prompt(&task);
        calls(&mut records, draw, 10..60);
        records.reply(&words(draw, 20..200));
        let agent = records.agent.clone();
        session.transcript.result_with(
            &call,
            json!("Done."),
            false,
            json!({ "agentId": agent, "status": "completed" }),
        );
    }
    let reply = words(draw, 20..200);
    session.transcript.reply(&reply);
}

/// Some of `count` shell calls, each with its output: mostly short, some long, a few very long.
fn calls(records: &mut Records, draw: &mut Draw, count: Range<usize>) {
    for _ in 0..draw.within(count.start, count.end) {
        let command = words(draw, 2..12);
        let call = records.call("Bash", json!({ "command": command }));
        let output = match draw.within(0, 20) {
            0 => 1000..6000,
            1..=5 => 100..1000,
            _ => 10..100,
        };
        records.result(&call, json!(words(draw, output)));
    }
}

/// Some of `count` words drawn from a vocabulary of a thousand or so: text that compresses about as
/// well as what an agent writes.
fn words(draw: &mut Draw, count: Range<usize>) -> String {
    let count = draw.within(count.start, count.end);
    const PARTS: [&str; 32] = [
        "ka", "re", "mi", "to", "su", "na", "lo", "pe", "vi", "da", "go", "ze", "ru", "fa", "ti",
        "bo", "ne", "sa", "ko", "li", "mu", "he", "ja", "po", "we", "xi", "yo", "cu", "de", "fi",
        "gu", "ha",
    ];
    let mut text = String::with_capacity(count * 7);
    for _ in 0..count {
        let word = draw.next();
        text.push_str(PARTS[word as usize % 32]);
        text.push_str(PARTS[(word >> 5) as usize % 32]);
        if word >> 63 == 1 {
            text.push_str(PARTS[(word >> 10) as usize % 4]);
        }
        text.push(' ');
    }
    text
}

/// A small generator of the same numbers for the same seed: xorshift64*.
struct Draw(u64);

impl Draw {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A number in `low..high`.
    fn within(&mut self, low: usize, high: usize) -> usize {
        low + (self.next() % (high - low) as u64) as usize
    }
}
