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
//! Where copies ran (PLAN §7.7): the records the agent's app keeps of its sessions are read
//! where the provider finds them for the first home (under `%APPDATA%`, which `--appdata`
//! names for a home given, else `<home>/AppData/Roaming` when there is one; or under
//! `<home>/Library/Application Support`), and it counts the copies with a record, with a host,
//! with a host this machine's `ssh -G` resolves (it opens no SSH connection, but runs the
//! owner's `Match exec` commands and may look names up in DNS with `CanonicalizeHostname`
//! set), with one that is the same machine as a host given with `--location` (any number of
//! them, as the SSH locations would be added), and with one that doesn't resolve; how the
//! recorded transcript paths are written, and how many name their original's file; which of
//! a record's fields a copy folder's id is in (a field's name printed only as far as it is an
//! identifier); the records too large to be read as one, and the records'
//! `subagentsTruncatedFor` beside the copies' loose
//! subagents left unattached. Of each copy with an original, it reads the subagents of both
//! and counts those attached in both, in the original only (by how the original's were found:
//! in a workflow's folder, with a `.meta.json`, or by their call's id alone), in the copy only,
//! and in neither.
//!
//! ```text
//! cargo run --release -p sb_core --example check                    this machine's user
//! cargo run --release -p sb_core --example check -- <home folder>   a home folder elsewhere
//! cargo run --release -p sb_core --example check -- [<home folder>] --with <home folder>
//!     [--appdata <folder>] [--location <host>]…
//! ```

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::io::{self, Cursor, Read};
use std::path::Path;

use sb_core::provider::{
    self, CopyRecord, ItemKind, ItemRange, Provider, ProviderId, RegisterAudit, SessionItem,
    SessionItems, Source,
};
use sb_core::transcript::{
    BlockId, Content, Conversation, Ending, Subagent, ToolInput, ToolKind, Transcript,
};
use sb_core::transport::{
    Halt, LocalTransport, RangeFailure, RangeSink, ResolveHost, Resolved, SshConfig, Transport,
};
use sb_test_support::HomeTransport;
use serde_json::Value;

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
                Content::ContextFile { differs, .. } => self.notes.add(
                    if *differs {
                        "files loaded into the context, differing on disk"
                    } else {
                        "files loaded into the context"
                    },
                    1,
                ),
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
    /// The provider's id for the session.
    key: String,
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
    /// How the subagents of copies with an original attach beside the original's.
    subagents: Counts,
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
        let mut subagents = Counts::default();
        for what in [
            "subagents attached in both",
            "subagents attached only in the original",
            "subagents attached only in the copy",
            "subagents attached in neither",
        ] {
            subagents.add(what, 0);
        }
        Self {
            counts,
            fields: Counts::default(),
            subagents,
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
        self.subagents
            .print("Subagents of copies beside their original's, by file name");
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

/// What the command line asks for.
#[derive(Default)]
struct Args {
    first: Option<String>,
    second: Option<String>,
    appdata: Option<String>,
    locations: Vec<String>,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        const USAGE: &str = "usage: check [<home folder>] [--with <home folder>] \
                             [--appdata <folder>] [--location <host>]...";
        let mut args = Self::default();
        let mut words = std::env::args().skip(1);
        while let Some(word) = words.next() {
            let mut value = || words.next().ok_or(USAGE);
            match word.as_str() {
                "--with" => args.second = Some(value()?),
                "--appdata" => args.appdata = Some(value()?),
                "--location" => args.locations.push(value()?),
                _ if word.starts_with("--") || args.first.is_some() => return Err(USAGE.into()),
                _ => args.first = Some(word),
            }
        }
        Ok(args)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse()?;
    let home = |home: &str, appdata: Option<String>| -> Box<dyn Transport> {
        let vars = appdata.map(|appdata| ("APPDATA".to_string(), appdata));
        Box::new(HomeTransport::new(
            Path::new(home),
            vars.into_iter().collect(),
        ))
    };
    let first: Box<dyn Transport> = match (&args.first, &args.appdata) {
        (None, None) => Box::new(LocalTransport),
        (Some(first), appdata) => {
            let roaming = Path::new(first).join("AppData").join("Roaming");
            let appdata = (appdata.clone())
                .or_else(|| roaming.is_dir().then(|| roaming.display().to_string()));
            home(first, appdata)
        }
        (None, Some(appdata)) => {
            let here = std::env::home_dir().ok_or("this user has no home folder")?;
            home(&here.display().to_string(), Some(appdata.clone()))
        }
    };
    let transports: Vec<Box<dyn Transport>> = std::iter::once(first)
        .chain(args.second.as_deref().map(|second| home(second, None)))
        .collect();
    let providers = provider::builtin();
    /* No provider that ships keeps anything for a pass, but the seam asks for a place. */
    let scratch = tempfile::tempdir()?;
    let mut totals = Totals::new();
    let mut copies = Copies::new();
    let mut sources: Vec<RefCell<Box<dyn Source + '_>>> = Vec::new();
    let mut parsed: Vec<Parsed> = Vec::new();
    /* The first home's register items, each with its source. */
    let mut register: Vec<(usize, SessionItem)> = Vec::new();
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
                if home == 0 && item.kind == ItemKind::Register {
                    register.push((
                        at,
                        SessionItem {
                            key: item.key,
                            kind: item.kind,
                            length: item.length,
                        },
                    ));
                    continue;
                }
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
                    key: key.clone(),
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
        let provider = (providers.iter())
            .find(|provider| provider.id() == copy.provider)
            .ok_or("a session of no provider")?;
        let attached = |session: &Parsed| {
            subagents(
                provider.as_ref(),
                &Session {
                    source: &sources[session.source],
                    list: session.list.clone(),
                },
            )
        };
        compare_subagents(
            &mut copies.subagents,
            &attached(copy),
            &attached(found[0]),
            &found[0].list,
            &copy.list,
        );
    }
    let mut hosts = Hosts::new(&args.locations);
    for provider in &providers {
        /* What the provider makes of the register as a whole, and of each file of it. */
        let mut records = Vec::new();
        let mut raw = Vec::new();
        let mut by_source: BTreeMap<usize, Vec<SessionItem>> = BTreeMap::new();
        for (source, item) in &register {
            by_source.entry(*source).or_default().push(item.clone());
        }
        for (source, list) in by_source {
            let items = Session {
                source: &sources[source],
                list,
            };
            records.extend(provider.copy_records(&items));
            for item in &items.list {
                let mut bytes = Vec::new();
                if items
                    .open(&item.key, 0)
                    .and_then(|mut file| file.read_to_end(&mut bytes))
                    .is_err()
                {
                    hosts.counts.add("register files that could not be read", 1);
                    continue;
                }
                let Some(audit) = provider::register_audit(provider.id(), &bytes) else {
                    continue;
                };
                hosts.counts.add("register files", 1);
                if audit.too_large {
                    hosts
                        .counts
                        .add("register files too large to be read as records", 1);
                }
                if !audit.json {
                    hosts.counts.add("register files that are not JSON", 1);
                }
                raw.push((audit, serde_json::from_slice::<Value>(&bytes).ok()));
            }
        }
        for copy in
            (parsed.iter()).filter(|it| it.copy && it.home == 0 && it.provider == provider.id())
        {
            let original = (copy.conversation())
                .and_then(|it| originals.get(&it))
                .map(|found| found[0]);
            hosts.copy(copy, &records, &raw, original);
        }
    }
    totals.print();
    copies.print();
    hosts.print();
    Ok(())
}

/// The keys of the subagent transcripts of the session whose items are `items`, each with
/// whether a call was found to have started it, every subagent read.
fn subagents(provider: &dyn Provider, items: &Session<'_, '_>) -> BTreeMap<String, bool> {
    let mut transcript = provider.parse(items);
    transcript.read_subagents(provider, items);
    let unattached: HashSet<&str> = (transcript.unattached.iter())
        .map(|subagent| &**subagent.item())
        .collect();
    (items.list.iter())
        .filter(|item| item.kind == ItemKind::Subagent)
        .map(|item| (item.key.clone(), !unattached.contains(item.key.as_str())))
        .collect()
}

/// The file name the key `key` ends in.
fn file_name(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/**
 * Counts how the subagents of a copy, `copy` (each by key, with whether it attached), attach
 * beside its original's, `original`, matched by their file names: in both, in one, in neither.
 * Of those attached in the original only, how the original's were found, from its items and
 * the copy's: `original_items`, `copy_items`.
 */
fn compare_subagents(
    counts: &mut Counts,
    copy: &BTreeMap<String, bool>,
    original: &BTreeMap<String, bool>,
    original_items: &[SessionItem],
    copy_items: &[SessionItem],
) {
    let by_name = |of: &BTreeMap<String, bool>| -> HashMap<String, (String, bool)> {
        (of.iter())
            .map(|(key, attached)| (file_name(key).to_string(), (key.clone(), *attached)))
            .collect()
    };
    let (copy, original) = (by_name(copy), by_name(original));
    let has = |items: &[SessionItem], key: &str| items.iter().any(|item| item.key == key);
    let meta = |key: &str| format!("{}.meta.json", key.strip_suffix(".jsonl").unwrap_or(key));
    for (name, (key, in_copy)) in &copy {
        let Some((original_key, in_original)) = original.get(name) else {
            counts.add("subagents of copies that their original doesn't have", 1);
            continue;
        };
        match (in_copy, in_original) {
            (true, true) => counts.add("subagents attached in both", 1),
            (false, false) => counts.add("subagents attached in neither", 1),
            (true, false) => counts.add("subagents attached only in the copy", 1),
            (false, true) => {
                counts.add("subagents attached only in the original", 1);
                let how = if original_key.contains("/workflows/") {
                    "in a workflow's folder"
                } else if has(original_items, &meta(original_key)) {
                    "with a .meta.json beside it"
                } else {
                    "by its call's id alone"
                };
                counts.add(
                    format!("subagents attached only in the original, found there {how}"),
                    1,
                );
                if has(copy_items, &meta(key)) {
                    counts.add("subagents attached only in the original, the copy keeping a .meta.json beside it", 1);
                }
            }
        }
    }
    counts.add(
        "subagents of originals that their copy doesn't have",
        original
            .keys()
            .filter(|name| !copy.contains_key(*name))
            .count() as u64,
    );
}

/// Where the copies ran, as the app's records say and this machine's `ssh -G` makes of them.
struct Hosts {
    counts: Counts,
    /// What each host given with `--location` resolves to, of those that do.
    locations: Vec<Resolved>,
    resolved: HashMap<String, Option<Resolved>>,
    ssh: SshConfig,
}

impl Hosts {
    fn new(locations: &[String]) -> Self {
        let mut counts = Counts::default();
        for what in [
            "copies",
            "copies with a record",
            "copies with a host",
            "copies whose host ssh -G resolves",
            "copies whose host is a --location's machine",
            "copies whose host doesn't resolve",
            "copies whose record has subagentsTruncatedFor",
            "copies with loose subagents left unattached",
        ] {
            counts.add(what, 0);
        }
        counts.add("hosts given with --location", locations.len() as u64);
        let ssh = SshConfig::default();
        let resolved: Vec<Resolved> = locations
            .iter()
            .filter_map(|host| ssh.resolve(host).ok().flatten())
            .collect();
        counts.add(
            "hosts given with --location that resolve",
            resolved.len() as u64,
        );
        Self {
            counts,
            locations: resolved,
            resolved: HashMap::new(),
            ssh,
        }
    }

    /**
     * Counts the copy `copy` against the provider's records `records` and the register files
     * as the provider audits them, each with its JSON, `raw`, with its original, if one was
     * found.
     */
    fn copy(
        &mut self,
        copy: &Parsed,
        records: &[CopyRecord],
        raw: &[(RegisterAudit, Option<Value>)],
        original: Option<&Parsed>,
    ) {
        self.counts.add("copies", 1);
        let Some(conversation) = copy.conversation.as_deref() else {
            self.counts.add("copies with no conversation id", 1);
            return;
        };
        let files: Vec<&(RegisterAudit, Option<Value>)> = (raw.iter())
            .filter(|(audit, _)| audit.ids.iter().any(|id| id == conversation))
            .collect();
        if !files.is_empty() {
            self.counts.add("copies with a record", 1);
        }
        if files.len() > 1 {
            self.counts.add("copies with more than one record", 1);
        }
        let truncated = files.iter().any(|(audit, _)| audit.truncated);
        let unattached = copy.unattached > 0;
        if truncated {
            self.counts
                .add("copies whose record has subagentsTruncatedFor", 1);
        }
        if unattached {
            self.counts
                .add("copies with loose subagents left unattached", 1);
        }
        match (truncated, unattached) {
            (true, true) => self.counts.add(
                "copies with subagentsTruncatedFor and loose subagents left unattached",
                1,
            ),
            (false, true) => self.counts.add(
                "copies with loose subagents left unattached and no subagentsTruncatedFor",
                1,
            ),
            (true, false) => self.counts.add(
                "copies with subagentsTruncatedFor and every loose subagent attached",
                1,
            ),
            (false, false) => {}
        }
        /* Which of the record's fields the copy's folder's id (after its first `-`) is in. */
        if let Some((_, id)) = copy.key.split_once('-') {
            for (_, file) in &files {
                for field in file.iter().flat_map(|file| fields_holding(file, id)) {
                    self.counts.add(
                        format!("copies whose folder's id is in the record's {field}"),
                        1,
                    );
                }
            }
        }
        let Some(record) = records.iter().find(|it| it.conversation == conversation) else {
            return;
        };
        if let Some(path) = &record.path {
            self.counts.add("copies with a recorded transcript path", 1);
            self.counts.add(
                match path.chars().next() {
                    Some('/') => "copies with a recorded transcript path written from /",
                    Some('~') => "copies with a recorded transcript path written from ~",
                    _ => "copies with a recorded transcript path written otherwise",
                },
                1,
            );
            if record.key.is_some() {
                self.counts.add(
                    "copies with a recorded transcript path the provider makes a key of",
                    1,
                );
            }
            if let (Some(key), Some(original)) = (&record.key, original)
                && original.list.iter().any(|item| item.key == *key)
            {
                self.counts.add(
                    "copies with a recorded transcript path that is their original's key",
                    1,
                );
            }
        }
        let Some(host) = &record.host else {
            return;
        };
        self.counts.add("copies with a host", 1);
        let resolved = self
            .resolved
            .entry(host.clone())
            .or_insert_with(|| self.ssh.resolve(host).ok().flatten())
            .clone();
        match resolved {
            Some(resolved) => {
                self.counts.add("copies whose host ssh -G resolves", 1);
                if self.locations.contains(&resolved) {
                    self.counts
                        .add("copies whose host is a --location's machine", 1);
                }
            }
            None => self.counts.add("copies whose host doesn't resolve", 1),
        }
    }

    fn print(&self) {
        self.counts
            .print("Where copies ran, as the app's records say");
        println!("  distinct hosts recorded: {}", self.resolved.len());
    }
}

/**
 * The names (as JSON pointers) of the text fields of `file` that hold `id`. A name is printed
 * only as far as it is an identifier: a part of it with anything but ASCII letters, digits and
 * `_` is printed as `*`, so no value a record holds can be printed as a field's name.
 */
fn fields_holding(file: &Value, id: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![(String::new(), file)];
    while let Some((at, value)) = pending.pop() {
        match value {
            Value::String(text) if text.contains(id) => found.push(at),
            Value::Object(fields) => {
                pending.extend(fields.iter().map(|(name, value)| {
                    let identifier = !name.is_empty()
                        && name
                            .bytes()
                            .all(|it| it.is_ascii_alphanumeric() || it == b'_');
                    let name = if identifier { name.as_str() } else { "*" };
                    (format!("{at}/{name}"), value)
                }));
            }
            Value::Array(values) => {
                pending.extend(values.iter().map(|value| (format!("{at}/[]"), value)));
            }
            _ => {}
        }
    }
    found.sort();
    found.dedup();
    found
}
