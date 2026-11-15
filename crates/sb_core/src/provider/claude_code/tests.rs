//! Golden tests of the parser (PLAN §12): generated transcripts in, the model out, compared as
//! an outline with everything a `Stored` points at loaded into it.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use pretty_assertions::assert_eq;
use sb_test_support::{ProjectsTree, Session, image_block};
use serde_json::json;

use super::{Agent, classify, tools};
use crate::provider::{ItemKind, ParseError, Provider, SessionItem, SessionItems};
use crate::transcript::{
    BlockId, Content, Conversation, Encoding, Facts, Output, Span, SpanKind, Stored, Todo,
    TodoState, Tokens, ToolCall, ToolInput, ToolKind, Transcript, Unrecognised, index,
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
                let key = file.strip_prefix(&root).unwrap().to_str().unwrap();
                let class = classify(key);
                (class.session.as_deref() == Some(&session.id)).then(|| SessionItem {
                    key: key.to_string(),
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

fn session(tree: &mut ProjectsTree) -> Session {
    tree.session(PROJECT, 0)
}

fn parse(tree: &ProjectsTree, session: &Session) -> (Transcript, Files) {
    let files = Files::of(tree, session);
    (Agent.parse(&files).unwrap(), files)
}

/// The conversation as text: its turns, then the blocks that aren't shown, then what has no
/// place in it.
fn outline(conversation: &Conversation, files: &Files) -> String {
    let mut out = String::new();
    write_conversation(conversation, files, "", &mut out);
    out
}

fn write_conversation(conversation: &Conversation, files: &Files, pad: &str, out: &mut String) {
    let mut shown = HashSet::new();
    for (number, turn) in conversation.turns.iter().enumerate() {
        writeln!(out, "{pad}turn {}", number + 1).unwrap();
        for id in &turn.blocks {
            shown.insert(*id);
            write_block(conversation, *id, files, pad, out);
        }
    }
    let hidden: Vec<BlockId> = (0..conversation.blocks.len())
        .map(BlockId)
        .filter(|id| !shown.contains(id))
        .collect();
    if !hidden.is_empty() {
        writeln!(out, "{pad}not shown").unwrap();
        for id in hidden {
            write_block(conversation, id, files, pad, out);
        }
    }
    for unplaced in &conversation.unplaced {
        writeln!(out, "{pad}unplaced: {}", unrecognised(unplaced)).unwrap();
    }
    if conversation.cut_short {
        writeln!(out, "{pad}cut short").unwrap();
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
    files: &Files,
    pad: &str,
    out: &mut String,
) {
    let block = &conversation[id];
    let text = |stored: &Stored| stored.load_text(files).unwrap();
    let line = match &block.content {
        Content::Prompt { text, images, from } => {
            let mut line = format!("prompt: {text:?}");
            if let Some(from) = from {
                write!(line, " from {from}").unwrap();
            }
            for image in images {
                let bytes = image.data.load(files).unwrap();
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
                ToolInput::Other => format!("other: {raw}"),
            };
            let mut line = format!("{}: {input}", call.name);
            match &call.result {
                None => write!(line, "\n{pad}    no result").unwrap(),
                Some(result) => {
                    write!(line, "\n{pad}    {:?}", result.status).unwrap();
                    for part in &result.output {
                        match part {
                            Output::Text(stored) => {
                                write!(line, " {:?}", text(stored)).unwrap();
                                if stored.item != conversation.item {
                                    let file = stored.item.rsplit('/').next().unwrap();
                                    write!(line, " (in {file})").unwrap();
                                }
                            }
                            Output::Image(image) => {
                                let bytes = image.data.load(files).unwrap();
                                write!(line, " {} of {} bytes", image.media_type, bytes.len())
                                    .unwrap();
                            }
                        }
                    }
                }
            }
            writeln!(out, "{pad}  {line}").unwrap();
            for subagent in &call.subagents {
                writeln!(
                    out,
                    "{pad}    subagent {:?} ({:?})",
                    subagent.kind, subagent.description
                )
                .unwrap();
                write_conversation(&subagent.conversation, files, &format!("{pad}      "), out);
            }
            return;
        }
    };
    writeln!(out, "{pad}  {line}").unwrap();
}

/// The call in the block `id`.
fn call_in(conversation: &Conversation, id: usize) -> &ToolCall {
    match &conversation.blocks[id].content {
        Content::ToolCall(call) => call,
        other => panic!("block {id} is {other:?}"),
    }
}

#[test]
fn a_session_with_a_call_of_each_kind_of_tool() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("Why does the build fail?");
    t.thinking("Look at the log first.", 1500);
    t.reply("I'll build it.");
    let call = t.call(
        "Bash",
        json!({ "command": "cargo build", "description": "Build the app" }),
    );
    t.failure(&call, "Exit code 101\nerror: expected `;`");
    let call = t.call(
        "Read",
        json!({ "file_path": "/home/user/code/app/src/main.rs", "offset": 10, "limit": 20 }),
    );
    t.result(&call, json!("10\tfn main() {"));
    let call = t.call(
        "Edit",
        json!({
            "file_path": "/home/user/code/app/src/main.rs",
            "old_string": "let a = \"1\"",
            "new_string": "let a = \"1\";",
            "replace_all": true,
        }),
    );
    t.result(&call, json!("The file was updated."));
    let call = t.call(
        "Write",
        json!({ "file_path": "/home/user/code/app/notes.md", "content": "# Notes\n" }),
    );
    t.result(&call, json!("The file was created."));
    let call = t.call("Grep", json!({ "pattern": "fn main", "path": "src" }));
    t.result(&call, json!("src/main.rs:10:fn main() {"));
    let call = t.call("Glob", json!({ "pattern": "**/*.rs" }));
    t.result(&call, json!("src/main.rs"));
    let call = t.call(
        "TodoWrite",
        json!({ "todos": [
            { "content": "Find the error", "status": "completed", "activeForm": "Finding it" },
            { "content": "Fix it", "status": "in_progress", "activeForm": "Fixing it" },
            { "content": "Run the tests", "status": "pending", "activeForm": "Running them" },
        ] }),
    );
    t.result(&call, json!("The list was updated."));
    let call = t.call("WebSearch", json!({ "query": "rust expected semicolon" }));
    t.result(&call, json!("Two results."));
    let call = t.call(
        "WebFetch",
        json!({ "url": "https://example.com/errors", "prompt": "What does it say?" }),
    );
    t.result(&call, json!("It says hello."));
    let call = t.call("mcp__tracker__search_issues", json!({ "query": "build" }));
    t.result(&call, json!([{ "type": "text", "text": "No issues." }]));
    let call = t.call("Teleport", json!({ "to": "the moon" }));
    t.result(&call, json!("Done."));
    t.call("Bash", json!({ "command": "cargo test" }));

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(&transcript.conversation, &files),
        r##"turn 1
  prompt: "Why does the build fail?"
  thinking for Some(1.5s): "Look at the log first."
  reply by model-1: "I'll build it."
  Bash: shell "cargo build" (Some("Build the app"))
    Failed "Exit code 101\nerror: expected `;`"
  Read: read /home/user/code/app/src/main.rs from line Some(10) for Some(20)
    Ok "10\tfn main() {"
  Edit: edit /home/user/code/app/src/main.rs: "let a = \"1\"" to "let a = \"1\";", all: true
    Ok "The file was updated."
  Write: write /home/user/code/app/notes.md: "# Notes\n"
    Ok "The file was created."
  Grep: content search "fn main" in Some("src")
    Ok "src/main.rs:10:fn main() {"
  Glob: file search "**/*.rs" in None
    Ok "src/main.rs"
  TodoWrite: todos: Done "Find the error", InProgress "Fix it", Open "Run the tests"
    Ok "The list was updated."
  WebSearch: web search "rust expected semicolon"
    Ok "Two results."
  WebFetch: web fetch https://example.com/errors (Some("What does it say?"))
    Ok "It says hello."
  mcp__tracker__search_issues: mcp tracker search_issues: {"query":"build"}
    Ok "No issues."
  Teleport: other: {"to":"the moon"}
    Ok "Done."
  Bash: shell "cargo test" (None)
    no result
"##
    );
    assert_eq!(
        index(&transcript)
            .calls
            .into_iter()
            .collect::<Vec<(ToolKind, u32)>>(),
        [
            (ToolKind::Shell, 2),
            (ToolKind::Read, 1),
            (ToolKind::Edit, 1),
            (ToolKind::Write, 1),
            (ToolKind::ContentSearch, 1),
            (ToolKind::FileSearch, 1),
            (ToolKind::Todos, 1),
            (ToolKind::WebSearch, 1),
            (ToolKind::WebFetch, 1),
            (ToolKind::Mcp, 1),
            (ToolKind::Other, 1),
        ]
    );
    /* A call took from when it was recorded to when its result was. */
    let (asked, answered) = (
        transcript.conversation.blocks[3].at,
        call_in(&transcript.conversation, 3)
            .result
            .as_ref()
            .unwrap()
            .at,
    );
    assert_eq!(answered.unwrap() - asked.unwrap(), 1000);
}

#[test]
fn tool_names_map_to_the_common_kinds() {
    for (name, kind) in [
        ("Bash", ToolKind::Shell),
        ("Read", ToolKind::Read),
        ("Edit", ToolKind::Edit),
        ("Write", ToolKind::Write),
        ("Grep", ToolKind::ContentSearch),
        ("Glob", ToolKind::FileSearch),
        ("TodoWrite", ToolKind::Todos),
        ("Agent", ToolKind::Subagent),
        ("Task", ToolKind::Subagent),
        ("WebSearch", ToolKind::WebSearch),
        ("WebFetch", ToolKind::WebFetch),
        ("mcp__tracker__search_issues", ToolKind::Mcp),
        ("Teleport", ToolKind::Other),
        ("bash", ToolKind::Other),
    ] {
        assert_eq!(Agent.tool_kind(name), kind, "{name}");
        assert_eq!(tools::kind(name), kind, "{name}");
    }
}

#[test]
fn records_and_tools_it_does_not_know_survive() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("Go on.");
    /* A record type from a later release: one in the conversation, one beside it. */
    t.record("hologram", json!({ "payload": { "frames": 3 } }));
    t.bookkeeping("heartbeat", json!({ "beat": 7 }));
    /* Known bookkeeping. */
    t.bookkeeping("file-history-snapshot", json!({ "snapshot": {} }));
    t.bookkeeping("api-request", json!({ "params": {} }));
    /* A content block of a type it doesn't know, in a reply and in a prompt. */
    t.assistant(
        90,
        "model-1",
        json!([{ "type": "hunch", "confidence": 0.5 }]),
        json!({}),
    );
    t.user(json!([{ "type": "gesture", "name": "shrug" }]), json!({}));
    /* A tool it doesn't know, and a known one whose input isn't the known shape. */
    let teleport = t.call("Teleport", json!({ "to": "the moon" }));
    t.result(&teleport, json!([{ "type": "postcard" }]));
    let bash = t.call("Bash", json!({ "script": ["ls"] }));
    t.result(&bash, json!("a.txt"));
    /* A result for a call that isn't in the transcript. */
    t.user(
        json!([{ "type": "tool_result", "tool_use_id": "toolu_gone", "content": "?" }]),
        json!({}),
    );
    /* Known types that aren't as they are read, and a line that is no record. */
    t.record("assistant", json!({ "message": "none" }));
    t.raw(r#"{"type":"user","uuid":17,"message":{"role":"user","content":"hm"}}"#);
    t.raw("not json at all");
    t.raw(r#"["a","list"]"#);
    t.reply("Still here.");

    let (transcript, files) = parse(&tree, &session);
    let conversation = &transcript.conversation;
    assert_eq!(
        outline(conversation, &files),
        r#"turn 1
  prompt: "Go on."
  unknown "hologram"
  unknown "hunch"
  unknown "gesture"
  Teleport: other: {"to":"the moon"}
    Ok "{\"type\":\"postcard\"}"
  Bash: other: {"script":["ls"]}
    Ok "a.txt"
  "tool_result" with an unreadable tool_use_id
  "assistant" with an unreadable message
  reply by model-1: "Still here."
unplaced: unknown "heartbeat"
unplaced: "user" with an unreadable record
unplaced: "" with an unreadable not a record
unplaced: "" with an unreadable not a record
"#
    );
    /* Each is kept as it was recorded. */
    let Content::Unrecognised(hologram) = &conversation.blocks[1].content else {
        panic!("{:?}", conversation.blocks[1]);
    };
    let raw: serde_json::Value =
        serde_json::from_slice(&hologram.raw.load(&files).unwrap()).unwrap();
    assert_eq!(raw["payload"], json!({ "frames": 3 }));
    let Content::Unrecognised(hunch) = &conversation.blocks[2].content else {
        panic!("{:?}", conversation.blocks[2]);
    };
    let raw: serde_json::Value = serde_json::from_slice(&hunch.raw.load(&files).unwrap()).unwrap();
    assert_eq!(raw, json!({ "type": "hunch", "confidence": 0.5 }));
    assert_eq!(
        conversation.unplaced[2].raw.load_text(&files).unwrap(),
        "not json at all"
    );
    /* The conversation goes on through what it doesn't know. */
    assert_eq!(conversation.blocks[1].parent, Some(BlockId(0)));
    assert_eq!(conversation.blocks[2].parent, Some(BlockId(1)));
}

#[test]
fn a_rewound_conversation_shows_the_branch_that_ends_latest() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("Name the crate.");
    let first = t.reply("Call it app.");
    t.prompt("Now add a test.");
    let call = t.call("Bash", json!({ "command": "cargo test" }));
    t.result(&call, json!("ok"));
    t.reply("Added.");
    /* The user goes back to the first reply and asks something else. */
    t.follow(&first);
    t.prompt("Now add a benchmark.");
    t.reply("Benchmarked.");

    let (transcript, files) = parse(&tree, &session);
    let conversation = &transcript.conversation;
    assert_eq!(
        outline(conversation, &files),
        r#"turn 1
  prompt: "Name the crate."
  reply by model-1: "Call it app."
turn 2
  prompt: "Now add a benchmark."
  reply by model-1: "Benchmarked."
not shown
  prompt: "Now add a test."
  Bash: shell "cargo test" (None)
    Ok "ok"
  reply by model-1: "Added."
"#
    );
    /* Both branches follow the first reply. */
    let parents: Vec<Option<usize>> = conversation
        .blocks
        .iter()
        .map(|block| block.parent.map(|parent| parent.0))
        .collect();
    assert_eq!(
        parents,
        [None, Some(0), Some(1), Some(2), Some(3), Some(1), Some(5)]
    );
    assert_eq!(index(&transcript).calls.len(), 0);
}

#[test]
fn results_of_calls_made_at_once_are_on_the_shown_branch() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("Check both files.");
    /* One reply, a record per call; each result follows its own call. */
    let calls = t.calls(
        100,
        &[
            ("Read", json!({ "file_path": "/home/user/code/app/a.rs" })),
            ("Read", json!({ "file_path": "/home/user/code/app/b.rs" })),
            ("Read", json!({ "file_path": "/home/user/code/app/c.rs" })),
        ],
    );
    for (call, output) in calls.iter().zip(["a", "b", "c"]) {
        t.follow(&call.uuid);
        t.result(call, json!(output));
    }
    t.reply("All three are fine.");

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(&transcript.conversation, &files),
        r#"turn 1
  prompt: "Check both files."
  Read: read /home/user/code/app/a.rs from line None for None
    Ok "a"
  Read: read /home/user/code/app/b.rs from line None for None
    Ok "b"
  Read: read /home/user/code/app/c.rs from line None for None
    Ok "c"
  reply by model-1: "All three are fine."
"#
    );
    /* The reply's three records share its usage, which counts once; the answer's counts too. */
    assert_eq!(
        transcript.conversation.tokens,
        Tokens {
            input: 200,
            output: 40,
            cache_read: 2000,
            cache_write: 20,
        }
    );
}

#[test]
fn a_compaction_is_a_boundary_with_its_summary_and_the_history_stays() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("Start.");
    t.reply("Started.");
    t.compact(
        "The user asked to start, and it was started.",
        171_000,
        24_000,
    );
    t.reply("Going on from the summary.");
    t.prompt("Finish.");
    t.reply("Finished.");

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(&transcript.conversation, &files),
        r#"turn 1
  prompt: "Start."
  reply by model-1: "Started."
  compaction from Some(171000) to Some(24000) tokens: Some("The user asked to start, and it was started.")
  reply by model-1: "Going on from the summary."
turn 2
  prompt: "Finish."
  reply by model-1: "Finished."
"#
    );
}

#[test]
fn subagents_are_under_the_calls_that_started_them() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    session.transcript.prompt("Survey the crates.");

    /* Named by its metadata, with a subagent of its own. */
    let agent = json!({ "subagent_type": "explorer", "description": "List crates", "prompt": "List them." });
    let outer = session.transcript.call("Agent", agent);
    let mut sub = session.subagent_of(&outer, "explorer", "List crates");
    sub.user(json!("List them."), json!({}));
    let inner = sub.call(
        "Agent",
        json!({ "description": "Count lines", "prompt": "Count them." }),
    );
    let mut nested = session.subagent_of(&inner, "counter", "Count lines");
    nested.user(json!("Count them."), json!({}));
    nested.reply_by("model-2", "Ninety lines.");
    sub.result(&inner, json!("Ninety lines."));
    sub.reply("Two crates, ninety lines.");
    session
        .transcript
        .result(&outer, json!([{ "type": "text", "text": "Two crates." }]));

    /* No metadata: the call's result names the subagent. */
    let call = session.transcript.call(
        "Task",
        json!({ "description": "Check the licence", "prompt": "Check it." }),
    );
    let mut sub = session.subagent_without_meta();
    sub.user(json!("Check it."), json!({}));
    sub.reply("MIT.");
    let named = json!({ "agentId": sub.agent, "status": "completed" });
    session
        .transcript
        .result_with(&call, json!("MIT."), false, named);

    /* A workflow's subagents: the call's result names the run they are filed under. */
    let call = session
        .transcript
        .call("Workflow", json!({ "scriptPath": "review.js" }));
    for task in ["Review the parser.", "Review the tests."] {
        let mut sub = session.workflow_subagent("run-1", "reviewer", task);
        sub.user(json!(task), json!({}));
        sub.reply("Looks right.");
    }
    session.transcript.result_with(
        &call,
        json!("Reviewed."),
        false,
        json!({ "runId": "run-1" }),
    );

    /* Started by nothing the session knows of. */
    let mut sub = session.workflow_subagent("run-9", "reviewer", "Review the docs.");
    sub.user(json!("Review the docs."), json!({}));
    session.transcript.reply("Surveyed.");

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(&transcript.conversation, &files),
        r#"turn 1
  prompt: "Survey the crates."
  Agent: subagent Some("explorer") (Some("List crates")): "List them."
    Ok "Two crates."
    subagent Some("explorer") (Some("List crates"))
      turn 1
        prompt: "List them."
        Agent: subagent None (Some("Count lines")): "Count them."
          Ok "Ninety lines."
          subagent Some("counter") (Some("Count lines"))
            turn 1
              prompt: "Count them."
              reply by model-2: "Ninety lines."
        reply by model-1: "Two crates, ninety lines."
  Task: subagent None (Some("Check the licence")): "Check it."
    Ok "MIT."
    subagent None (None)
      turn 1
        prompt: "Check it."
        reply by model-1: "MIT."
  Workflow: other: {"scriptPath":"review.js"}
    Ok "Reviewed."
    subagent Some("reviewer") (Some("Review the parser."))
      turn 1
        prompt: "Review the parser."
        reply by model-1: "Looks right."
    subagent Some("reviewer") (Some("Review the tests."))
      turn 1
        prompt: "Review the tests."
        reply by model-1: "Looks right."
  reply by model-1: "Surveyed."
"#
    );
    assert_eq!(transcript.unattached.len(), 1);
    assert_eq!(
        outline(&transcript.unattached[0].conversation, &files),
        "turn 1\n  prompt: \"Review the docs.\"\n"
    );

    /* A subagent's text is searched under the turn of the call that started it; its calls
    and tokens are its own. */
    let index = index(&transcript);
    assert!(index.spans.contains(&Span {
        turn: 0,
        kind: SpanKind::Reply,
        text: "Ninety lines.".to_string(),
    }));
    assert_eq!(index.calls.get(&ToolKind::Subagent), Some(&2));
    assert_eq!(index.models, ["model-1"]);
    let explorer = &call_in(&transcript.conversation, 1).subagents[0].conversation;
    assert_eq!(
        call_in(explorer, 1).subagents[0].conversation.models,
        ["model-2"]
    );
}

#[test]
fn output_saved_outside_the_transcript_is_read_from_its_file() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let output = "line of output\n".repeat(400);
    let (key, note) = session.saved_output("b1c2d3e4f.txt", output.as_bytes());
    let t = &mut session.transcript;
    t.prompt("Show the log.");
    let call = t.call("Bash", json!({ "command": "cat build.log" }));
    t.result(&call, json!(note));
    /* The same note for a file that was never archived stays the result. */
    let call = t.call("Bash", json!({ "command": "cat other.log" }));
    let gone = note.replace("b1c2d3e4f", "gone");
    t.result(&call, json!(gone));

    let (transcript, files) = parse(&tree, &session);
    let conversation = &transcript.conversation;
    let [Output::Text(saved)] = &call_in(conversation, 1).result.as_ref().unwrap().output[..]
    else {
        panic!("{:?}", call_in(conversation, 1).result);
    };
    assert_eq!(
        saved,
        &Stored {
            item: key.as_str().into(),
            offset: 0,
            length: output.len() as u64,
            encoding: Encoding::Plain,
        }
    );
    assert_eq!(saved.load_text(&files).unwrap(), output);
    let [Output::Text(inline)] = &call_in(conversation, 2).result.as_ref().unwrap().output[..]
    else {
        panic!("{:?}", call_in(conversation, 2).result);
    };
    assert_eq!(inline.item, conversation.item);
    assert_eq!(inline.load_text(&files).unwrap(), gone);
}

#[test]
fn images_are_loaded_from_where_they_are_recorded() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    let pasted: Vec<u8> = (0..=255).collect();
    t.user(
        json!([
            { "type": "text", "text": "What is in this picture?" },
            image_block("image/png", &pasted),
        ]),
        json!({ "origin": { "kind": "human" } }),
    );
    let call = t.call(
        "Read",
        json!({ "file_path": "/home/user/code/app/shot.jpg" }),
    );
    let shot = b"not really a jpeg";
    t.result(
        &call,
        json!([
            image_block("image/jpeg", shot),
            { "type": "text", "text": "[Image: source: /home/user/code/app/shot.jpg]" },
        ]),
    );

    let (transcript, files) = parse(&tree, &session);
    let conversation = &transcript.conversation;
    assert_eq!(
        outline(conversation, &files),
        r#"turn 1
  prompt: "What is in this picture?" + image/png of 256 bytes
  Read: read /home/user/code/app/shot.jpg from line None for None
    Ok image/jpeg of 17 bytes "[Image: source: /home/user/code/app/shot.jpg]"
"#
    );
    let Content::Prompt { images, .. } = &conversation.blocks[0].content else {
        panic!("{:?}", conversation.blocks[0]);
    };
    assert_eq!(images[0].data.load(&files).unwrap(), pasted);
    let Output::Image(image) = &call_in(conversation, 1).result.as_ref().unwrap().output[0] else {
        panic!("{:?}", call_in(conversation, 1).result);
    };
    assert_eq!(image.data.load(&files).unwrap(), shot);
}

#[test]
fn text_in_the_users_place_that_nobody_asked_is_a_note() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.user(
        json!("Read the project's instructions."),
        json!({ "isMeta": true }),
    );
    t.user(
        json!([
            { "type": "text", "text": "<system-reminder>\nToday is a Monday.\n</system-reminder>" },
            { "type": "text", "text": "Run the linter." },
            { "type": "text", "text": "Then the tests." },
        ]),
        json!({ "origin": { "kind": "human" } }),
    );
    t.reply("Running.");
    /* Typed while the agent was working. */
    t.record(
        "attachment",
        json!({ "attachment": {
            "type": "queued_command", "prompt": "Skip the slow ones.", "commandMode": "prompt",
            "origin": { "kind": "human" },
        } }),
    );
    t.record(
        "attachment",
        json!({ "attachment": { "type": "deferred_tools_delta", "addedNames": ["Teleport"] } }),
    );
    t.user(
        json!("<task-notification>\n<status>completed</status>\n</task-notification>"),
        json!({ "origin": { "kind": "task-notification" } }),
    );
    t.user(
        json!("<local-command-stdout>done</local-command-stdout>"),
        json!({}),
    );
    t.system("informational", "The context is nearly full.");
    t.record(
        "system",
        json!({ "subtype": "api_error", "error": { "formatted": "529 overloaded", "message": "x" } }),
    );
    t.record(
        "system",
        json!({ "subtype": "stop_hook_summary", "hookCount": 1 }),
    );
    /* Another session's message asks for something, as a prompt does. */
    t.user(
        json!("Report how far you are."),
        json!({ "isMeta": true, "origin": { "kind": "peer", "name": "main" } }),
    );
    t.assistant(
        91,
        "<synthetic>",
        json!([{ "type": "text", "text": "No response requested." }]),
        json!({}),
    );

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(&transcript.conversation, &files),
        r#"turn 1
  note (meta): "Read the project's instructions."
turn 2
  prompt: "Run the linter.\n\nThen the tests."
  note (system-reminder): "<system-reminder>\nToday is a Monday.\n</system-reminder>"
  reply by model-1: "Running."
turn 3
  prompt: "Skip the slow ones."
  note (task-notification): "<task-notification>\n<status>completed</status>\n</task-notification>"
  note (local-command-stdout): "<local-command-stdout>done</local-command-stdout>"
  note (informational): "The context is nearly full."
  note (api_error): "529 overloaded"
turn 4
  prompt: "Report how far you are." from peer
  reply by nobody: "No response requested."
"#
    );
    assert_eq!(transcript.conversation.models, ["model-1"]);
    assert_eq!(
        index(&transcript).first_prompt.as_deref(),
        Some("Run the linter. Then the tests.")
    );
}

#[test]
fn a_transcript_cut_inside_its_last_record_parses_up_to_there() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    session.transcript.prompt("One.");
    session.transcript.reply("Two.");
    let whole = fs::read(tree.path(&session.key)).unwrap();
    File::options()
        .append(true)
        .open(tree.path(&session.key))
        .unwrap()
        .write_all(br#"{"type":"user","uuid":"cut","message":{"role":"user","content":"Thr"#)
        .unwrap();

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(&transcript.conversation, &files),
        "turn 1\n  prompt: \"One.\"\n  reply by model-1: \"Two.\"\ncut short\n"
    );

    /* Cut anywhere, it parses, and never into more than the whole. */
    let blocks = transcript.conversation.blocks.len();
    for length in 0..whole.len() {
        fs::write(tree.path(&session.key), &whole[..length]).unwrap();
        let (cut, _) = parse(&tree, &session);
        assert!(cut.conversation.blocks.len() <= blocks, "{length}");
        assert!(cut.conversation.unplaced.is_empty(), "{length}");
    }
}

#[test]
fn records_written_again_are_read_once() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    session.transcript.prompt("One.");
    session.transcript.reply("Two.");
    /* A resumed session writes its history again, some fields changed. */
    let again = fs::read_to_string(tree.path(&session.key))
        .unwrap()
        .replace("/home/user/code/app", "/home/user/code/elsewhere");
    for line in again.lines() {
        session.transcript.raw(line);
    }
    session.transcript.prompt("Three.");

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(&transcript.conversation, &files),
        "turn 1\n  prompt: \"One.\"\n  reply by model-1: \"Two.\"\nturn 2\n  prompt: \"Three.\"\n"
    );
    assert_eq!(transcript.facts.project_path.as_deref(), Some(PROJECT));
}

#[test]
fn a_session_says_where_it_started_and_what_it_is_called() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.bookkeeping("mode", json!({ "mode": "normal" }));
    t.title("First title");
    t.prompt("Rename it.");
    t.reply_by("model-2", "Renamed.");
    /* The session moves on to another directory and branch; where it started stands. */
    t.user(
        json!("And the other one."),
        json!({ "cwd": "/home/user/code/other", "gitBranch": "feature", "entrypoint": "desktop" }),
    );
    t.reply_by("model-1", "Done.");
    t.reply_by("model-2", "Really done.");
    t.title("Second title");

    let (transcript, _) = parse(&tree, &session);
    assert_eq!(
        transcript.facts,
        Facts {
            agent: "claude-code",
            title: Some("Second title".to_string()),
            project_path: Some(PROJECT.to_string()),
            branch: Some("main".to_string()),
            entrypoint: Some("cli".to_string()),
        }
    );
    let conversation = &transcript.conversation;
    assert_eq!(conversation.models, ["model-2", "model-1"]);
    /* Records are a second apart, from the first of 2026. */
    let start = 1_767_225_600_000;
    assert_eq!(conversation.span(), Some((start + 2000, start + 6000)));
    assert_eq!(
        (conversation.blocks[0].at, conversation.blocks[1].at),
        (Some(start + 2000), Some(start + 3000))
    );

    let index = index(&transcript);
    assert_eq!(index.title.as_deref(), Some("Second title"));
    assert_eq!(index.first_prompt.as_deref(), Some("Rename it."));
    assert_eq!(
        (index.started, index.ended),
        (Some(start + 2000), Some(start + 6000))
    );
    assert_eq!(index.tokens.output, 60);
    /* Each turn's offset is where its prompt's record starts. */
    let bytes = fs::read(tree.path(&session.key)).unwrap();
    let prompts: Vec<&str> = index
        .turns
        .iter()
        .map(|offset| {
            let line = bytes[*offset as usize..]
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap();
            let record: serde_json::Value = serde_json::from_slice(line).unwrap();
            if record["message"]["content"] == "Rename it." {
                "first"
            } else {
                "second"
            }
        })
        .collect();
    assert_eq!(prompts, ["first", "second"]);
}

#[test]
fn the_title_file_names_a_session_without_a_title_record() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    session.transcript.prompt("  Fix the\n  flaky   test in the sync engine, the one that fails every other run on the build machine, and say why it failed.");
    let (transcript, _) = parse(&tree, &session);
    assert_eq!(transcript.facts.title, None);
    /* With no title stored, the start of the first prompt stands in. */
    assert_eq!(
        index(&transcript).title.as_deref(),
        Some("Fix the flaky test in the sync engine, the one that fails every other run on the")
    );

    session.title("Flaky sync test");
    let (transcript, _) = parse(&tree, &session);
    assert_eq!(transcript.facts.title.as_deref(), Some("Flaky sync test"));
}

#[test]
fn search_text_is_prompts_replies_and_what_calls_were_aimed_at() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("Find the parser.");
    t.thinking("Where would it be?", 100);
    let call = t.call("Grep", json!({ "pattern": "fn parse", "path": "crates" }));
    t.result(&call, json!("crates/core/src/parse.rs:1:fn parse() {"));
    let call = t.call(
        "Read",
        json!({ "file_path": "/home/user/code/app/crates/core/src/parse.rs" }),
    );
    t.result(&call, json!("fn parse() {}"));
    t.reply("It is in the core crate.");
    t.prompt("Run it.");
    let call = t.call(
        "Bash",
        json!({ "command": "cargo run", "description": "Run the app" }),
    );
    t.result(&call, json!("secret output"));
    let call = t.call(
        "WebFetch",
        json!({ "url": "https://example.com/doc", "prompt": "Summarise" }),
    );
    t.result(&call, json!("A summary."));

    let (transcript, _) = parse(&tree, &session);
    let index = index(&transcript);
    let spans: Vec<(usize, SpanKind, &str)> = index
        .spans
        .iter()
        .map(|span| (span.turn, span.kind, span.text.as_str()))
        .collect();
    assert_eq!(
        spans,
        [
            (0, SpanKind::Prompt, "Find the parser."),
            (0, SpanKind::ToolCall, "fn parse"),
            (0, SpanKind::ToolCall, "crates"),
            (
                0,
                SpanKind::ToolCall,
                "/home/user/code/app/crates/core/src/parse.rs"
            ),
            (0, SpanKind::Reply, "It is in the core crate."),
            (1, SpanKind::Prompt, "Run it."),
            (1, SpanKind::ToolCall, "cargo run"),
            (1, SpanKind::ToolCall, "https://example.com/doc"),
        ]
    );
}

#[test]
fn a_session_without_a_transcript_does_not_parse() {
    let files = Files {
        root: PathBuf::new(),
        list: vec![SessionItem {
            key: "projects/-home-user-code-app/s1/custom-title.json".to_string(),
            kind: ItemKind::Sidecar,
            length: 0,
        }],
    };
    assert!(matches!(Agent.parse(&files), Err(ParseError::NoTranscript)));
}

#[test]
fn todo_states_are_read_from_their_names() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    session.transcript.call(
        "TodoWrite",
        json!({ "todos": [{ "content": "Later", "status": "someday" }] }),
    );
    let (transcript, _) = parse(&tree, &session);
    assert_eq!(
        call_in(&transcript.conversation, 0).input,
        ToolInput::Todos(vec![Todo {
            text: "Later".to_string(),
            state: TodoState::Open,
        }])
    );
}
