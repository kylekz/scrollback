//! Golden tests of the parser (PLAN §12): generated transcripts in, the model out, compared as
//! an outline with everything a `Stored` points at loaded into it.
//!
//! - [`conversation`]: the tree, its branches, and what is a prompt and what a note.
//! - [`calls`]: tool calls, their results, images and stored content.
//! - [`session`]: subagents, a session's facts, the index, and what can't be read.
//! - [`shapes`]: what older releases wrote and what only some sessions have.

mod calls;
mod conversation;
mod display;
mod session;
mod shapes;

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use sb_test_support::{ProjectsTree, Session};

use super::{Agent, classify};
use crate::provider::{Provider, SessionItem, SessionItems};
use crate::transcript::{
    BlockId, Content, Conversation, Ending, Output, Stored, Subagent, Todo, ToolCall, ToolInput,
    Transcript, Unrecognised,
};

const PROJECT: &str = "/home/user/code/app";

/// The files of one generated session, as its archived items.
struct Files {
    root: PathBuf,
    list: Vec<SessionItem>,
}

impl Files {
    fn of(tree: &ProjectsTree, session: &Session) -> Self {
        fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, files);
                } else {
                    files.push(path);
                }
            }
        }
        let root = tree.path("");
        let mut files = Vec::new();
        walk(&tree.path("projects"), &mut files);
        let mut list: Vec<SessionItem> = files
            .iter()
            .filter_map(|file| {
                /* A key is written with `/`, whatever the system's paths are written with. */
                let key = file
                    .strip_prefix(&root)
                    .unwrap()
                    .iter()
                    .map(|part| part.to_str().unwrap())
                    .collect::<Vec<_>>()
                    .join("/");
                let class = classify(&key);
                (class.session.as_deref() == Some(session.session_key())).then(|| SessionItem {
                    key,
                    kind: class.kind,
                    length: file.metadata().unwrap().len(),
                })
            })
            .collect();
        list.sort_by(|a, b| a.key.cmp(&b.key));
        Self { root, list }
    }
}

impl SessionItems for Files {
    fn list(&self) -> &[SessionItem] {
        &self.list
    }

    fn open(&self, key: &str, offset: u64) -> io::Result<Box<dyn Read + '_>> {
        let mut file = File::open(self.root.join(key))?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(Box::new(file))
    }
}

/// A session's files, of which those whose key ends with `ending` can't be read: not at all,
/// or not past their first `after` bytes.
struct Broken<'a> {
    files: &'a Files,
    ending: &'static str,
    after: Option<u64>,
}

/// What reading a broken file fails with.
const BROKEN: &str = "the disk is gone";

struct Fails;

impl Read for Fails {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other(BROKEN))
    }
}

impl SessionItems for Broken<'_> {
    fn list(&self) -> &[SessionItem] {
        self.files.list()
    }

    fn open(&self, key: &str, offset: u64) -> io::Result<Box<dyn Read + '_>> {
        match self.after {
            _ if !key.ends_with(self.ending) => self.files.open(key, offset),
            Some(after) => Ok(Box::new(
                self.files
                    .open(key, offset)?
                    .take(after.saturating_sub(offset))
                    .chain(Fails),
            )),
            None => Err(io::Error::other(BROKEN)),
        }
    }
}

fn session(tree: &mut ProjectsTree) -> Session {
    tree.session(PROJECT, 0)
}

/// The session's transcript with every subagent read, as the index reads it.
fn parse(tree: &ProjectsTree, session: &Session) -> (Transcript, Files) {
    let files = Files::of(tree, session);
    let mut transcript = Agent.parse(&files);
    transcript.read_subagents(&Agent, &files);
    (transcript, files)
}

/// The session's own conversation.
fn own(transcript: &Transcript) -> &Conversation {
    transcript.conversation.as_ref().unwrap()
}

/// Replaces `from` with `to` in the session's main transcript, for what JSON can hold and the
/// fixture's writer can't write.
fn patch(tree: &ProjectsTree, session: &Session, from: &str, to: &str) {
    let path = tree.path(&session.key);
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(from));
    fs::write(path, text.replace(from, to)).unwrap();
}

/// The conversation as text: its turns, then the blocks that aren't shown, then what has no
/// place in it and what couldn't be read.
fn outline(conversation: &Conversation, items: &dyn SessionItems) -> String {
    let mut out = String::new();
    write_conversation(conversation, items, "", &mut out);
    out
}

fn file_name(key: &str) -> &str {
    key.rsplit('/').next().unwrap()
}

fn write_conversation(
    conversation: &Conversation,
    items: &dyn SessionItems,
    pad: &str,
    out: &mut String,
) {
    let mut shown = HashSet::new();
    for (number, turn) in conversation.turns().iter().enumerate() {
        writeln!(out, "{pad}turn {}", number + 1).unwrap();
        for id in &turn.blocks {
            shown.insert(*id);
            write_block(conversation, *id, items, pad, out);
        }
    }
    let hidden: Vec<BlockId> = (0..conversation.blocks().len())
        .map(BlockId)
        .filter(|id| !shown.contains(id))
        .collect();
    if !hidden.is_empty() {
        writeln!(out, "{pad}not shown").unwrap();
        for id in hidden {
            write_block(conversation, id, items, pad, out);
        }
    }
    for unplaced in &conversation.unplaced {
        writeln!(out, "{pad}unplaced: {}", unrecognised(unplaced)).unwrap();
    }
    match &conversation.ending {
        Ending::Whole => {}
        Ending::MidRecord => writeln!(out, "{pad}ends inside a record").unwrap(),
        Ending::Unreadable(reason) => writeln!(out, "{pad}unreadable from here: {reason}").unwrap(),
    }
    for unreadable in &conversation.unreadable {
        writeln!(
            out,
            "{pad}could not read {}: {}",
            file_name(&unreadable.item),
            unreadable.reason
        )
        .unwrap();
    }
}

fn unrecognised(record: &Unrecognised) -> String {
    match record.misfit {
        Some(part) => format!("{:?} with an unreadable {part}", record.kind),
        None => format!("unknown {:?}", record.kind),
    }
}

fn write_block(
    conversation: &Conversation,
    id: BlockId,
    items: &dyn SessionItems,
    pad: &str,
    out: &mut String,
) {
    let block = &conversation[id];
    let text = |stored: &Stored| stored.load_text(items).unwrap();
    let line = match &block.content {
        Content::Prompt { text, images, from } => {
            let mut line = format!("prompt: {text:?}");
            if let Some(from) = from {
                write!(line, " from {from}").unwrap();
            }
            for image in images {
                let bytes = image.data.load(items).unwrap();
                write!(line, " + {} of {} bytes", image.media_type, bytes.len()).unwrap();
            }
            line
        }
        Content::Reply { text } => format!(
            "reply by {}: {text:?}",
            block.model.as_deref().unwrap_or("nobody")
        ),
        Content::Thinking { text, duration } => format!("thinking for {duration:?}: {text:?}"),
        Content::Compaction {
            tokens_before,
            tokens_after,
            summary,
        } => format!(
            "compaction from {tokens_before:?} to {tokens_after:?} tokens: {:?}",
            summary.as_ref().map(text)
        ),
        Content::Note { kind, text } => format!("note ({kind}): {text:?}"),
        Content::Unrecognised(record) => unrecognised(record),
        Content::ToolCall(call) => {
            let raw = text(&call.raw_input);
            let input = match &call.input {
                ToolInput::Shell {
                    command,
                    description,
                } => format!("shell {command:?} ({description:?})"),
                ToolInput::Read {
                    path,
                    first_line,
                    lines,
                } => format!("read {path} from line {first_line:?} for {lines:?}"),
                ToolInput::Edit {
                    path,
                    old,
                    new,
                    all,
                } => format!(
                    "edit {path}: {:?} to {:?}, all: {all}",
                    text(old),
                    text(new)
                ),
                ToolInput::Write { path, content } => {
                    format!("write {path}: {:?}", text(content))
                }
                ToolInput::ContentSearch { pattern, path } => {
                    format!("content search {pattern:?} in {path:?}")
                }
                ToolInput::FileSearch { pattern, path } => {
                    format!("file search {pattern:?} in {path:?}")
                }
                ToolInput::Todos(todos) => format!(
                    "todos: {}",
                    todos
                        .iter()
                        .map(|Todo { text, state }| format!("{state:?} {text:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                ToolInput::Subagent {
                    kind,
                    description,
                    prompt,
                } => format!("subagent {kind:?} ({description:?}): {:?}", text(prompt)),
                ToolInput::WebSearch { query } => format!("web search {query:?}"),
                ToolInput::WebFetch { url, prompt } => format!("web fetch {url} ({prompt:?})"),
                ToolInput::Mcp { server, tool } => format!("mcp {server} {tool}: {raw}"),
                ToolInput::Other => format!("other ({:?}): {raw}", call.kind),
            };
            let mut line = format!("{}: {input}", call.name);
            match &call.result {
                None => write!(line, "\n{pad}    no result").unwrap(),
                Some(result) => {
                    write!(line, "\n{pad}    {:?}", result.status).unwrap();
                    if let Some(brief) = &result.brief {
                        write!(line, " {brief:?}").unwrap();
                    }
                    for part in &result.output {
                        match part {
                            Output::Text(stored) => {
                                write!(line, " {:?}", text(stored)).unwrap();
                                if stored.item != conversation.item {
                                    write!(line, " (in {})", file_name(&stored.item)).unwrap();
                                }
                            }
                            Output::Image(image) => {
                                let bytes = image.data.load(items).unwrap();
                                write!(line, " {} of {} bytes", image.media_type, bytes.len())
                                    .unwrap();
                            }
                        }
                    }
                }
            }
            writeln!(out, "{pad}  {line}").unwrap();
            for subagent in &call.subagents {
                write_subagent(subagent, items, &format!("{pad}    "), out);
            }
            return;
        }
    };
    writeln!(out, "{pad}  {line}").unwrap();
}

fn write_subagent(subagent: &Subagent, items: &dyn SessionItems, pad: &str, out: &mut String) {
    match subagent {
        Subagent::Unread { item } => {
            writeln!(out, "{pad}subagent not read: {}", file_name(item)).unwrap();
        }
        Subagent::Read {
            kind,
            description,
            conversation,
        } => {
            writeln!(out, "{pad}subagent {kind:?} ({description:?})").unwrap();
            write_conversation(conversation, items, &format!("{pad}  "), out);
        }
    }
}

/// The call in the block `id`.
fn call_in(conversation: &Conversation, id: usize) -> &ToolCall {
    match &conversation.blocks()[id].content {
        Content::ToolCall(call) => call,
        other => panic!("block {id} is {other:?}"),
    }
}
