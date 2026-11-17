//! A check of the parsers against real sessions, for development: it parses every session the
//! agents found at a location hold, and counts what the parsers did not know and where a parse
//! differs from the session's records read another way.
//!
//! It reads the location and writes nothing. What it prints is safe to pass on: counts, and
//! the agents' own names for kinds of record and for tools. Nothing a session says is printed,
//! nor any path, file name, session id or project, and tools of an MCP server (whose names
//! hold the server's) are only counted.
//!
//! It also counts the copies an agent's app keeps of sessions that ran elsewhere (PLAN §7.7)
//! and how each compares with its original, looked for under the home folder itself and under
//! a second one given with `--with` (a WSL distro's home, through `\\wsl.localhost\…`): the
//! same, behind it, ahead of it, or different; the copies with no original, and those the
//! matching may have missed (an original under the same id started elsewhere, or no project
//! path); the loose subagent transcripts of copies, how many were attached under a call, and of
//! those left unattached, how their copies compare; and the names of the record fields seen in
//! copies and in none of their originals, which is where the name of the machine a copy ran on
//! would be. The second home's sessions are only parsed, to be matched,
//! and not counted with the rest.
//!
//! ```text
//! cargo run --release -p sb_core --example check                    this machine's user
//! cargo run --release -p sb_core --example check -- <home folder>   a home folder elsewhere
//! cargo run --release -p sb_core --example check -- [<home folder>] --with <home folder>
//! ```

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::io::{self, Cursor, Read};
use std::path::Path;

use sb_core::provider::{self, ItemKind, ItemRange, ProviderId, SessionItem, SessionItems, Source};
use sb_core::transcript::{
    BlockId, Content, Conversation, Ending, Subagent, ToolInput, ToolKind, Transcript,
};
use sb_core::transport::{Halt, LocalTransport, RangeFailure, RangeSink, Transport};
use sb_test_support::HomeTransport;

/// One session's items, fetched from the location as they are asked for.
struct Session<'a, 'b> {
    source: &'a RefCell<Box<dyn Source + 'b>>,
    list: Vec<SessionItem>,
}

impl SessionItems for Session<'_, '_> {
    fn list(&self) -> &[SessionItem] {
        &self.list
    }

    fn open(&self, key: &str, offset: u64) -> io::Result<Box<dyn Read + '_>> {
        let item = self
            .list
            .iter()
            .find(|item| item.key == key)
            .ok_or(io::ErrorKind::NotFound)?;
        let range = ItemRange {
            key: key.to_string(),
            offset,
            length: item.length.saturating_sub(offset),
        };
        let mut fetched = Fetched::default();
        self.source
            .borrow_mut()
            .fetch(&[range], &mut fetched)
            .map_err(|error| io::Error::other(error.to_string()))?;
        match fetched.failure {
            Some(failure) => Err(io::Error::other(failure)),
            None => Ok(Box::new(Cursor::new(fetched.bytes))),
        }
    }
}

/// The bytes of one fetched range, or why it couldn't be fetched.
#[derive(Default)]
struct Fetched {
    bytes: Vec<u8>,
    failure: Option<String>,
}

impl RangeSink for Fetched {
    fn data(&mut self, _request: usize, bytes: &[u8]) -> Result<(), Halt> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn end(&mut self, _request: usize, result: Result<(), RangeFailure>) -> Result<(), Halt> {
        self.failure = result.err().map(|failure| failure.to_string());
        Ok(())
    }
}

/// What is counted, by what it is.
#[derive(Default)]
struct Counts(BTreeMap<String, u64>);

impl Counts {
    fn add(&mut self, what: impl Into<String>, count: u64) {
        *self.0.entry(what.into()).or_default() += count;
    }

    fn print(&self, title: &str) {
        println!("\n{title}");
        if self.0.is_empty() {
            println!("  none");
        }
        for (what, count) in &self.0 {
            println!("  {what}: {count}");
        }
    }
}

#[derive(Default)]
struct Totals {
    sessions: Counts,
    conversations: Counts,
    records: BTreeMap<String, (u64, bool)>,
    unknown: Counts,
    written: Counts,
    unrecognised: Counts,
    notes: Counts,
    kinds: Counts,
    unmapped: Counts,
    untyped: Counts,
    mismatches: Counts,
}

impl Totals {
    /// Nothing counted yet, with what should always be said counted as none.
    fn new() -> Self {
        let mut totals = Self::default();
        for what in [
            "sessions",
            "sessions with no transcript of their own",
            "sessions in which nothing was asked",
            "sessions that open with something other than a prompt",
            "subagents no call is known to have started",
            "files that could not be listed",
        ] {
            totals.sessions.add(what, 0);
        }
        for what in [
            "conversations that end inside a record",
            "conversations that could not be read through",
            "conversations with a branch not shown, the session's own",
            "conversations with a branch not shown, a subagent's",
            "subagents left unread",
            "calls with no result",
        ] {
            totals.conversations.add(what, 0);
        }
        totals
    }

    fn session(&mut self, transcript: &Transcript) {
        self.sessions.add("sessions", 1);
        match &transcript.conversation {
            Some(conversation) => {
                self.conversation(conversation, "the session's own");
                let prompts = conversation
                    .turns()
                    .iter()
                    .filter(|turn| {
                        matches!(conversation[turn.blocks[0]].content, Content::Prompt { .. })
                    })
                    .count();
                if prompts == 0 {
                    self.sessions.add("sessions in which nothing was asked", 1);
                } else if prompts < conversation.turns().len() {
                    self.sessions
                        .add("sessions that open with something other than a prompt", 1);
                }
            }
            None => self
                .sessions
                .add("sessions with no transcript of their own", 1),
        }
        for subagent in &transcript.unattached {
            self.sessions
                .add("subagents no call is known to have started", 1);
            self.subagent(subagent);
        }
    }

    fn subagent(&mut self, subagent: &Subagent) {
        match subagent {
            Subagent::Unread { .. } => self.conversations.add("subagents left unread", 1),
            Subagent::Read { conversation, .. } => self.conversation(conversation, "a subagent's"),
        }
    }

    fn conversation(&mut self, conversation: &Conversation, whose: &str) {
        self.conversations.add(format!("conversations, {whose}"), 1);
        match conversation.ending {
            Ending::Whole => {}
            Ending::MidRecord => self
                .conversations
                .add("conversations that end inside a record", 1),
            Ending::Unreadable(_) => self
                .conversations
                .add("conversations that could not be read through", 1),
        }
        self.conversations.add(
            "items beside a conversation that could not be read",
            conversation.unreadable.len() as u64,
        );
        let shown: HashSet<BlockId> = conversation
            .turns()
            .iter()
            .flat_map(|turn| turn.blocks.iter().copied())
            .collect();
        if shown.len() < conversation.blocks().len() {
            self.conversations
                .add(format!("conversations with a branch not shown, {whose}"), 1);
        }
        for unplaced in &conversation.unplaced {
            self.unrecognised.add(
                format!(
                    "beside the conversation: {:?} ({})",
                    unplaced.kind,
                    unplaced.misfit.unwrap_or("unknown kind")
                ),
                1,
            );
        }
        for block in conversation.blocks() {
            match &block.content {
                Content::Prompt { from: None, .. } => self.notes.add("prompts", 1),
                Content::Prompt { from: Some(_), .. } => {
                    self.notes.add("prompts from another sender", 1);
                }
                Content::Note { kind, .. } => self.notes.add(format!("notes: {kind}"), 1),
                Content::Unrecognised(unknown) => self.unrecognised.add(
                    format!(
                        "among the blocks: {:?} ({})",
                        unknown.kind,
                        unknown.misfit.unwrap_or("unknown kind")
                    ),
                    1,
                ),
                Content::ToolCall(call) => {
                    self.kinds.add(format!("{:?}", call.kind), 1);
                    match call.kind {
                        ToolKind::Other => self.unmapped.add(call.name.as_str(), 1),
                        ToolKind::Mcp => self.unmapped.add("(tools of MCP servers)", 1),
                        _ if call.input == ToolInput::Other => {
                            self.untyped.add(call.name.as_str(), 1);
                        }
                        _ => {}
                    }
                    if call.result.is_none() {
                        self.conversations.add("calls with no result", 1);
                    }
                    for subagent in &call.subagents {
                        self.subagent(subagent);
                    }
                }
                _ => {}
            }
        }
    }

    fn print(&self) {
        self.sessions.print("Sessions");
        self.conversations.print("Conversations");
        println!("\nRecord types (count, and whether the parser knows the type)");
        for (kind, (count, known)) in &self.records {
            let known = if *known { "known" } else { "NOT KNOWN" };
            println!("  {kind}: {count} ({known})");
        }
        self.unknown
            .print("Kinds the parser does not know, inside record types it knows");
        self.written
            .print("How sessions were written, where the parser has to allow for it");
        self.unrecognised
            .print("Kept as unrecognised (kind, and what did not fit)");
        self.notes.print("Prompts and notes");
        self.kinds.print("Tool calls by common kind");
        self.unmapped.print("Tools with no common kind");
        self.untyped
            .print("Calls of a mapped tool whose input was not in its kind's shape");
        self.mismatches
            .print("Sessions whose parse differs from their records, by what differs");
    }
}

/// A session parsed under one of the homes, for copies to be matched to their originals by.
struct Parsed {
    /// Which home and which of the sources it was found in.
    home: usize,
    source: usize,
    provider: ProviderId,
    list: Vec<SessionItem>,
    conversation: Option<String>,
    project_path: Option<String>,
    copy: bool,
    /// Of a copy, its loose subagent transcripts that no call was found to have started.
    unattached: u64,
}

/// What a copy and its original share (PLAN §7.7).
type Shared<'a> = (ProviderId, &'a str, Option<&'a str>);

impl Parsed {
    fn conversation(&self) -> Option<Shared<'_>> {
        Some((
            self.provider,
            self.conversation.as_deref()?,
            self.project_path.as_deref(),
        ))
    }
}

/// What the copies come to.
struct Copies {
    counts: Counts,
    /// Each record field's name, with how many copies have it and their originals don't.
    fields: Counts,
}

impl Copies {
    /// Nothing counted yet, with what should always be said counted as none.
    fn new() -> Self {
        let mut counts = Counts::default();
        for what in [
            "copies",
            "copies with more than one transcript of their own",
            "copies with no original",
            "copies with their original under the first home",
            "copies with their original under the second home",
            "copies with more than one original",
            "copies the same as their original",
            "copies behind their original",
            "copies ahead of their original",
            "copies that differ from their original",
            "copies with no project path",
            "copies with no original of their path, but one of their id with another path",
            "loose subagent transcripts of copies",
            "loose subagent transcripts attached under a call",
        ] {
            counts.add(what, 0);
        }
        Self {
            counts,
            fields: Counts::default(),
        }
    }

    /**
     * Counts the copy whose items are `items` and whose transcript, its subagents read, is
     * `transcript`. Returns how many of its loose subagent transcripts were left unattached.
     */
    fn copy(&mut self, items: &[SessionItem], transcript: &Transcript) -> u64 {
        self.counts.add("copies", 1);
        if transcript.facts.project_path.is_none() {
            self.counts.add("copies with no project path", 1);
        }
        if (items.iter())
            .filter(|item| item.kind == ItemKind::Transcript)
            .count()
            > 1
        {
            self.counts
                .add("copies with more than one transcript of their own", 1);
        }
        let loose: HashSet<&str> = (items.iter())
            .filter(|item| item.kind == ItemKind::Subagent && !item.key.contains("/subagents/"))
            .map(|item| item.key.as_str())
            .collect();
        let unattached: HashSet<&str> = (transcript.unattached.iter())
            .map(|subagent| &**subagent.item())
            .collect();
        self.counts
            .add("loose subagent transcripts of copies", loose.len() as u64);
        let attached = loose.difference(&unattached).count() as u64;
        self.counts
            .add("loose subagent transcripts attached under a call", attached);
        loose.len() as u64 - attached
    }

    /**
     * Counts how the copy's main transcript `copy` compares with its original's, `original`.
     * Returns how it compares, as the copies are counted.
     */
    fn compare(&mut self, copy: &[u8], original: &[u8]) -> &'static str {
        let what = if copy == original {
            "the same as their original"
        } else if original.starts_with(copy) {
            "behind their original"
        } else if copy.starts_with(original) {
            "ahead of their original"
        } else {
            "that differ from their original"
        };
        self.counts.add(format!("copies {what}"), 1);
        for field in fields(copy).difference(&fields(original)) {
            self.fields.add(field.as_str(), 1);
        }
        what
    }

    /// Counts `unattached` loose subagent transcripts left unattached of a copy `what` says.
    fn unattached(&mut self, unattached: u64, what: &str) {
        if unattached > 0 {
            self.counts.add(
                format!("loose subagent transcripts left unattached, of copies {what}"),
                unattached,
            );
        }
    }

    fn print(&self) {
        self.counts
            .print("Copies kept of sessions that ran elsewhere");
        self.fields
            .print("Record fields in copies and not in their originals (copies with each)");
    }
}

/// The names of the fields of the records of the JSON Lines transcript `bytes`.
fn fields(bytes: &[u8]) -> BTreeSet<String> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| match serde_json::from_slice(line) {
            Ok(serde_json::Value::Object(record)) => Some(record.into_iter().map(|(name, _)| name)),
            _ => None,
        })
        .flatten()
        .collect()
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (first, second) = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => (None, None),
        ["--with", with] => (None, Some(with)),
        [home] => (Some(home), None),
        [home, "--with", with] => (Some(home), Some(with)),
        _ => return Err("usage: check [<home folder>] [--with <home folder>]".into()),
    };
    let home = |home: &str| -> Box<dyn Transport> {
        Box::new(HomeTransport::new(Path::new(home), Vec::new()))
    };
    let transports: Vec<Box<dyn Transport>> =
        std::iter::once(first.map_or_else(|| Box::new(LocalTransport) as Box<dyn Transport>, home))
            .chain(second.map(home))
            .collect();
    let providers = provider::builtin();
    /* No provider that ships keeps anything for a pass, but the seam asks for a place. */
    let scratch = tempfile::tempdir()?;
    let mut totals = Totals::new();
    let mut copies = Copies::new();
    let mut sources: Vec<RefCell<Box<dyn Source + '_>>> = Vec::new();
    let mut parsed: Vec<Parsed> = Vec::new();
    for (home, transport) in transports.iter().enumerate() {
        for provider in &providers {
            let Some(source) = provider.detect(transport.as_ref(), scratch.path())? else {
                continue;
            };
            sources.push(RefCell::new(source));
            let at = sources.len() - 1;
            let source = &sources[at];
            let mut sessions: BTreeMap<String, Vec<SessionItem>> = BTreeMap::new();
            let scanned = source.borrow_mut().scan()?;
            if home == 0 {
                totals.sessions.add(
                    "files that could not be listed",
                    scanned.unlisted.len() as u64,
                );
            }
            for item in scanned.items {
                let Some(session) = item.session else {
                    continue;
                };
                sessions.entry(session).or_default().push(SessionItem {
                    key: item.key,
                    kind: item.kind,
                    length: item.length,
                });
            }
            for (key, mut list) in sessions {
                /* A session's sidecars alone are no session. */
                if !list
                    .iter()
                    .any(|item| matches!(item.kind, ItemKind::Transcript | ItemKind::Subagent))
                {
                    continue;
                }
                list.sort_by(|a, b| a.key.cmp(&b.key));
                let items = Session { source, list };
                let mut transcript = provider.parse(&items);
                let copy = provider.copy_of(&key).is_some();
                let mut unattached = 0;
                if home == 0 {
                    transcript.read_subagents(provider.as_ref(), &items);
                    totals.session(&transcript);
                    if copy {
                        unattached = copies.copy(&items.list, &transcript);
                    }
                }
                parsed.push(Parsed {
                    home,
                    source: at,
                    provider: provider.id(),
                    list: items.list.clone(),
                    conversation: transcript.facts.conversation.clone(),
                    project_path: transcript.facts.project_path.clone(),
                    copy,
                    unattached,
                });
                if home > 0 {
                    continue;
                }
                let Some(audit) = provider::audit(provider.id(), &items, &transcript) else {
                    continue;
                };
                for (kind, (count, known)) in audit.records {
                    let seen = totals.records.entry(kind).or_insert((0, known));
                    seen.0 += count;
                }
                for (kind, count) in audit.unknown {
                    totals.unknown.add(kind, count);
                }
                for (what, count) in audit.counts {
                    totals.written.add(what, count);
                }
                for check in audit.mismatches {
                    totals.mismatches.add(check, 1);
                }
            }
        }
    }
    /* The main transcript of a session, read whole. */
    let read = |session: &Parsed| -> io::Result<Vec<u8>> {
        let items = Session {
            source: &sources[session.source],
            list: session.list.clone(),
        };
        let mut bytes = Vec::new();
        if let Some(main) = (session.list.iter()).find(|item| item.kind == ItemKind::Transcript) {
            items.open(&main.key, 0)?.read_to_end(&mut bytes)?;
        }
        Ok(bytes)
    };
    let mut originals: HashMap<Shared<'_>, Vec<&Parsed>> = HashMap::new();
    let mut ids: HashSet<(ProviderId, &str)> = HashSet::new();
    for session in parsed.iter().filter(|session| !session.copy) {
        if let Some(conversation) = session.conversation() {
            originals.entry(conversation).or_default().push(session);
            ids.insert((conversation.0, conversation.1));
        }
    }
    for copy in (parsed.iter()).filter(|session| session.copy && session.home == 0) {
        let Some(found) = (copy.conversation()).and_then(|it| originals.get(&it)) else {
            copies.counts.add("copies with no original", 1);
            copies.unattached(copy.unattached, "with no original");
            /* A likely miss of the matching: an original under the same id, started elsewhere. */
            if (copy.conversation()).is_some_and(|it| ids.contains(&(it.0, it.1))) {
                copies.counts.add(
                    "copies with no original of their path, but one of their id with another path",
                    1,
                );
            }
            continue;
        };
        if found.len() > 1 {
            copies.counts.add("copies with more than one original", 1);
        }
        copies.counts.add(
            if found[0].home == 0 {
                "copies with their original under the first home"
            } else {
                "copies with their original under the second home"
            },
            1,
        );
        let what = copies.compare(&read(copy)?, &read(found[0])?);
        copies.unattached(copy.unattached, what);
    }
    totals.print();
    copies.print();
    Ok(())
}
