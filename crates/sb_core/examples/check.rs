//! A check of the parsers against real sessions, for development: it parses every session the
//! agents found at a location hold, and counts what the parsers did not know and where a parse
//! differs from the session's records read another way.
//!
//! It reads the location and writes nothing. What it prints is safe to pass on: counts, and
//! the agents' own names for kinds of record and for tools. Nothing a session says is printed,
//! nor any path, file name, session id or project, and tools of an MCP server (whose names
//! hold the server's) are only counted.
//!
//! ```text
//! cargo run --release -p sb_core --example check                    this machine's user
//! cargo run --release -p sb_core --example check -- <home folder>   a home folder elsewhere
//! ```

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::io::{self, Cursor, Read};

use sb_core::provider::{self, ItemKind, ItemRange, SessionItem, SessionItems, Source};
use sb_core::transcript::{
    BlockId, Content, Conversation, Ending, Subagent, ToolInput, ToolKind, Transcript,
};
use sb_core::transport::{Halt, LocalTransport, RangeFailure, RangeSink};

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

fn main() -> Result<(), Box<dyn Error>> {
    let transport = match std::env::args().nth(1) {
        Some(home) => LocalTransport::with_env(home, []),
        None => LocalTransport::new(),
    };
    let mut totals = Totals::new();
    for provider in provider::builtin() {
        let Some(source) = provider.detect(&transport)? else {
            continue;
        };
        let source = RefCell::new(source);
        let mut sessions: BTreeMap<String, Vec<SessionItem>> = BTreeMap::new();
        for item in source.borrow_mut().scan()? {
            let Some(session) = item.session else {
                continue;
            };
            sessions.entry(session).or_default().push(SessionItem {
                key: item.key,
                kind: item.kind,
                length: item.length,
            });
        }
        for mut list in sessions.into_values() {
            /* A session's sidecars alone are no session. */
            if !list
                .iter()
                .any(|item| matches!(item.kind, ItemKind::Transcript | ItemKind::Subagent))
            {
                continue;
            }
            list.sort_by(|a, b| a.key.cmp(&b.key));
            let items = Session {
                source: &source,
                list,
            };
            let mut transcript = provider.parse(&items);
            transcript.read_subagents(provider.as_ref(), &items);
            totals.session(&transcript);
            let Some(audit) = provider.audit(&items, &transcript) else {
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
    totals.print();
    Ok(())
}
