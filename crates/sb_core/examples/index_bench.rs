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
//! and made searchable again, and times it; with `--outdated`, that percent of them alone, the
//! rest as they were. With `--changed` every span of their text is first made to differ from
//! what the parser makes of it, as when a new parser or index says each session's text
//! otherwise. With `--passes`, passes of the location run one after another beside the index,
//! as the app's do beside a start's, and it counts the sessions parsed more than once; the
//! folder is to be made with it, which keeps the tree for the passes to read. It prints the
//! archive's shape and each run's times and counts: aggregates only.
//!
//! ```text
//! cargo run --release -p sb_core --example index_bench -- target/bench/index
//! cargo run --release -p sb_core --example index_bench -- target/bench/index --sessions 3000 --runs 3
//! cargo run --release -p sb_core --example index_bench -- target/bench/index --outdated 40 --changed
//! cargo run --release -p sb_core --example index_bench -- target/bench/passes --sessions 600 --passes
//! ```

use std::collections::HashMap;
use std::error::Error;
use std::ops::Range;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::location::{LocationId, LocationKind};
use sb_core::provider::{
    CopyRecord, ItemKind, Provider, ProviderError, ProviderId, SessionItems, Source,
};
use sb_core::sync::{Progress, SyncError, index, sync_location};
use sb_core::transcript::{CopyWords, Subagent, Transcript};
use sb_core::transport::Transport;
use sb_test_support::{Agent as _, FileAgent, ProjectsTree, Records, Session, read_catalogue};
use serde_json::json;
use toolbox_store::rusqlite::Connection;

const USAGE: &str = "usage: index_bench <folder under target/> [--sessions <n>] [--seed <n>] \
                     [--runs <n>] [--outdated <percent>] [--changed] [--passes]";

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let folder = args.next().ok_or(USAGE)?;
    let (mut sessions, mut seed, mut runs, mut changed) = (3000, 1, 1, false);
    let (mut outdated, mut passes) = (100, false);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--changed" => changed = true,
            "--passes" => passes = true,
            _ => {}
        }
        if matches!(flag.as_str(), "--changed" | "--passes") {
            continue;
        }
        let value: u64 = args.next().ok_or(USAGE)?.parse()?;
        match flag.as_str() {
            "--sessions" => sessions = usize::try_from(value)?,
            "--seed" => seed = value,
            "--runs" => runs = value,
            "--outdated" if value <= 100 => outdated = value,
            _ => return Err(USAGE.into()),
        }
    }
    let folder = Path::new(&folder);
    let dir = folder.join("archive");
    if !dir.exists() {
        let started = Instant::now();
        let home = folder.join("home");
        generate(&home, &dir, sessions, seed)?;
        /* Passes read the tree; without them it is let go of. */
        if !passes {
            std::fs::remove_dir_all(home)?;
        }
        println!("generated and archived in {:.1?}", started.elapsed());
    }
    let home = folder.join("home");
    if passes && !home.exists() {
        return Err("--passes needs a folder generated with --passes".into());
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
        let catalogue = Connection::open(dir.join("scrollback.db"))?;
        let search = Connection::open(dir.join("search.db"))?;
        catalogue.execute("UPDATE location_provider SET classified_by = NULL", [])?;
        let keys: Vec<(String, String, String)> = catalogue
            .prepare("SELECT location_id, provider, session FROM session ORDER BY session")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?;
        /* Text the new parser says otherwise is all of it, the most an index has to take
        out of the search index and write again: each span is replaced as an index replaces
        it, so the search index is as a real one would be after it. */
        search.execute_batch(
            "BEGIN;
             INSERT INTO span_text (span_text, rank) VALUES ('secure-delete', 0);",
        )?;
        for (n, key) in (0u64..).zip(&keys) {
            if n * 37 % 100 >= outdated {
                continue;
            }
            let (location, provider, session) = key;
            catalogue.execute(
                "UPDATE session SET parser_version = parser_version - 1
                 WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                [location, provider, session],
            )?;
            search.execute(
                "UPDATE doc SET made = 'older'
                 WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                [location, provider, session],
            )?;
            if changed {
                let doc: i64 = search.query_row(
                    "SELECT id FROM doc WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                    [location, provider, session],
                    |row| row.get(0),
                )?;
                search.execute_batch(&format!(
                    "INSERT INTO span_text (span_text, rowid, text)
                         SELECT 'delete', id, text FROM span WHERE id BETWEEN {first} AND {last};
                     UPDATE span SET text = text || ' (older)'
                         WHERE id BETWEEN {first} AND {last};
                     INSERT INTO span_text (rowid, text)
                         SELECT id, text FROM span WHERE id BETWEEN {first} AND {last};",
                    first = doc << 24,
                    last = (doc << 24) + (1 << 24) - 1,
                ))?;
            }
        }
        search.execute_batch("COMMIT")?;
        drop((catalogue, search));
        let archive = Archive::open(&dir)?;
        let started = Instant::now();
        let mut first = None;
        let parses = Mutex::default();
        let (indexing, location) = (AtomicBool::new(true), archive.catalogue().locations()?);
        let report = thread::scope(|scope| {
            /* With `--passes`, passes of the location one after another while the index of a
            start runs, as the app's do beside it, each with its index of what it changed. */
            let beside = passes.then(|| {
                scope.spawn(|| {
                    let (mut ran, tree) = (0, ProjectsTree::in_home(&home));
                    while indexing.load(Ordering::Relaxed) {
                        let counted = Box::new(Counting(provider(), &parses));
                        let cancel = CancelToken::new();
                        sync_location(
                            &archive,
                            &location[0].id,
                            &tree.transport(),
                            &[counted],
                            &cancel,
                            &mut |_| {},
                        )
                        .ok();
                        ran += 1;
                    }
                    ran
                })
            });
            let report = index(
                &archive,
                None,
                &[Box::new(Counting(provider(), &parses))],
                &CancelToken::new(),
                &mut |progress| {
                    if let Progress::Searching { .. } = progress {
                        first.get_or_insert(started.elapsed());
                    }
                },
            );
            indexing.store(false, Ordering::Relaxed);
            if let Some(beside) = beside {
                println!("  {} passes beside it", beside.join().unwrap_or_default());
            }
            report
        })?;
        let total = started.elapsed();
        let parses = parses.into_inner().unwrap_or_default();
        let again: usize = parses.values().map(|times| times - 1).sum();
        println!(
            "  {} sessions parsed, {} of them more than once, {again} parses again",
            parses.len(),
            parses.values().filter(|times| **times > 1).count(),
        );
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

/// A provider that counts how many times it parses each session, by its first item's key.
struct Counting<'a>(Box<dyn Provider>, &'a Mutex<HashMap<String, usize>>);

impl Provider for Counting<'_> {
    fn id(&self) -> ProviderId {
        self.0.id()
    }

    fn detect<'s>(
        &'s self,
        transport: &'s dyn Transport,
        scratch: &'s Path,
    ) -> Result<Option<Box<dyn Source + 's>>, ProviderError> {
        self.0.detect(transport, scratch)
    }

    fn parser_version(&self) -> u32 {
        self.0.parser_version()
    }

    fn parse(&self, items: &dyn SessionItems) -> Transcript {
        if let Some(first) = items.list().iter().map(|item| &item.key).min() {
            let mut parses = self
                .1
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *parses.entry(first.clone()).or_default() += 1;
        }
        self.0.parse(items)
    }

    fn subagent(&self, items: &dyn SessionItems, item: &str) -> Subagent {
        self.0.subagent(items, item)
    }

    fn classify(&self, key: &str) -> (ItemKind, Option<String>) {
        self.0.classify(key)
    }

    fn copy_of(&self, session: &str) -> Option<CopyWords> {
        self.0.copy_of(session)
    }

    fn copy_records(&self, register: &dyn SessionItems) -> Vec<CopyRecord> {
        self.0.copy_records(register)
    }

    fn conversation_of(&self, transcript: &str) -> Option<String> {
        self.0.conversation_of(transcript)
    }

    fn home_of(&self, root: &str) -> Option<String> {
        self.0.home_of(root)
    }
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
