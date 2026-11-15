//! Subagents, what a session says of itself, the index, and sessions of which something is
//! missing or can't be read.

use std::collections::BTreeMap;
use std::fs;

use pretty_assertions::assert_eq;
use sb_test_support::ProjectsTree;
use serde_json::json;

use super::super::Agent;
use super::{BROKEN, Broken, Files, PROJECT, outline, own, parse, session};
use crate::provider::Provider as _;
use crate::transcript::{
    At, BlockId, Conversation, Ending, Facts, Span, SpanKind, Subagent, Tokens, ToolKind, Totals,
    index,
};

/// The conversation of a subagent that was read.
fn read(subagent: &Subagent) -> &Conversation {
    subagent.conversation().unwrap()
}

#[test]
fn subagents_are_under_the_calls_that_started_them() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    session.transcript.prompt("Start.");
    session.transcript.reply("Started.");
    session.transcript.prompt("Survey the crates.");

    /* Named only by its metadata, with a subagent of its own that its call's result names. */
    let agent = json!({ "subagent_type": "explorer", "description": "List crates", "prompt": "List them." });
    let outer = session.transcript.call("Agent", agent);
    let mut sub = session.subagent_of(&outer, "explorer", "List crates");
    sub.user(json!("List them."), json!({}));
    let inner = sub.call(
        "Agent",
        json!({ "description": "Count lines", "prompt": "Count them." }),
    );
    let mut nested = session.subagent_without_meta();
    nested.user(json!("Count them."), json!({}));
    nested.reply_by("model-2", "Ninety lines.");
    sub.result_with(
        &inner,
        json!("Ninety lines."),
        false,
        json!({ "agentId": nested.agent }),
    );
    sub.reply("Two crates, ninety lines.");
    session
        .transcript
        .result(&outer, json!([{ "type": "text", "text": "Two crates." }]));

    /* A workflow's subagents: the call's result names the run they are filed under. They are
    in the order they started, not that of their files' names. */
    let call = session
        .transcript
        .call("Workflow", json!({ "scriptPath": "review.js" }));
    for (task, minute) in [("Review the parser.", 30), ("Review the tests.", 20)] {
        let mut sub = session.workflow_subagent("run-1", "reviewer", task);
        let at = |second| json!({ "timestamp": format!("2026-01-01T00:{minute}:0{second}.000Z") });
        sub.user(json!(task), at(0));
        sub.assistant(
            1,
            "model-1",
            json!([{ "type": "text", "text": "Looks right." }]),
            at(1),
        );
    }
    session.transcript.result_with(
        &call,
        json!("Reviewed."),
        false,
        json!({ "runId": "run-1" }),
    );

    /* Started by nothing the session knows of, with a subagent of its own. */
    let mut stray = session.workflow_subagent("run-9", "reviewer", "Review the docs.");
    stray.user(json!("Review the docs."), json!({}));
    let call = stray.call(
        "Task",
        json!({ "description": "Spell-check", "prompt": "Check." }),
    );
    let mut checker = session.subagent_of(&call, "speller", "Spell-check");
    checker.user(json!("Check."), json!({}));
    checker.reply("No mistakes.");
    stray.result(&call, json!("No mistakes."));
    session.transcript.reply("Surveyed.");

    /* Parsing reads the session's own conversation, and names the subagents its calls started. */
    let files = Files::of(&tree, &session);
    let mut transcript = Agent.parse(&files);
    assert_eq!(
        outline(own(&transcript), &files),
        r#"turn 1
  prompt: "Start."
  reply by model-1: "Started."
turn 2
  prompt: "Survey the crates."
  Agent: subagent Some("explorer") (Some("List crates")): "List them."
    Ok "Two crates."
    subagent not read: agent-a0000000000000001.jsonl
  Workflow: other (Other): {"scriptPath":"review.js"}
    Ok "Reviewed."
    subagent not read: agent-a0000000000000003.jsonl
    subagent not read: agent-a0000000000000004.jsonl
  reply by model-1: "Surveyed."
"#
    );
    let unread: Vec<&str> = transcript
        .unattached
        .iter()
        .map(|subagent| super::file_name(subagent.item()))
        .collect();
    assert_eq!(
        unread,
        [
            "agent-a0000000000000002.jsonl",
            "agent-a0000000000000006.jsonl",
            "agent-a0000000000000005.jsonl"
        ]
    );

    /* A subagent is read on its own when its call is opened: its conversation, with the
    subagents its own calls started named. */
    let explorer = Agent.subagent(&files, transcript.unattached[0].item());
    assert_eq!(
        outline(read(&explorer), &files),
        "turn 1\n  prompt: \"Count them.\"\n  reply by model-2: \"Ninety lines.\"\n"
    );

    /* The index reads them all. */
    transcript.read_subagents(&Agent, &files);
    assert_eq!(
        outline(own(&transcript), &files),
        r#"turn 1
  prompt: "Start."
  reply by model-1: "Started."
turn 2
  prompt: "Survey the crates."
  Agent: subagent Some("explorer") (Some("List crates")): "List them."
    Ok "Two crates."
    subagent Some("explorer") (Some("List crates"))
      turn 1
        prompt: "List them."
        Agent: subagent None (Some("Count lines")): "Count them."
          Ok "Ninety lines."
          subagent None (None)
            turn 1
              prompt: "Count them."
              reply by model-2: "Ninety lines."
        reply by model-1: "Two crates, ninety lines."
  Workflow: other (Other): {"scriptPath":"review.js"}
    Ok "Reviewed."
    subagent Some("reviewer") (Some("Review the tests."))
      turn 1
        prompt: "Review the tests."
        reply by model-1: "Looks right."
    subagent Some("reviewer") (Some("Review the parser."))
      turn 1
        prompt: "Review the parser."
        reply by model-1: "Looks right."
  reply by model-1: "Surveyed."
"#
    );
    assert_eq!(transcript.unattached.len(), 1);
    assert_eq!(
        outline(read(&transcript.unattached[0]), &files),
        r#"turn 1
  prompt: "Review the docs."
  Task: subagent None (Some("Spell-check")): "Check."
    Ok "No mistakes."
    subagent Some("speller") (Some("Spell-check"))
      turn 1
        prompt: "Check."
        reply by model-1: "No mistakes."
"#
    );

    /* A subagent's text is searched where a match opens the session: at the call in the
    session's own conversation that it ran under, or in the subagent nothing started. */
    let index = index(&transcript);
    let at = |text: &str| {
        index
            .spans
            .iter()
            .find(|span| span.text == text)
            .map(|span| span.at)
    };
    assert_eq!(
        at("Ninety lines."),
        Some(At::Block {
            turn: 1,
            block: BlockId(3),
        })
    );
    assert_eq!(
        at("Survey the crates."),
        Some(At::Block {
            turn: 1,
            block: BlockId(2),
        })
    );
    assert_eq!(at("No mistakes."), Some(At::Unattached(0)));

    /* The session's own calls and tokens, and those of everything that ran for it. */
    let reply = Tokens {
        input: 100,
        output: 20,
        cache_read: 1000,
        cache_write: 10,
    };
    let times = |count: u64| Tokens {
        input: reply.input * count,
        output: reply.output * count,
        cache_read: reply.cache_read * count,
        cache_write: reply.cache_write * count,
    };
    assert_eq!(
        index.own,
        Totals {
            tokens: times(4),
            calls: BTreeMap::from([(ToolKind::Subagent, 1), (ToolKind::Other, 1)]),
        }
    );
    assert_eq!(
        index.all,
        Totals {
            tokens: times(11),
            calls: BTreeMap::from([(ToolKind::Subagent, 3), (ToolKind::Other, 1)]),
        }
    );
    assert_eq!(index.models, ["model-1"]);
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
    let call = t.call("Bash", json!({ "command": "ls" }));
    t.result(&call, json!("a.txt"));

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
    let conversation = own(&transcript);
    assert_eq!(conversation.models, ["model-2", "model-1"]);
    /* Records are a second apart, from the first of 2026. The last is the call's result. */
    let start = 1_767_225_600_000;
    assert_eq!(conversation.span(), Some((start + 2000, start + 9000)));
    assert_eq!(
        (conversation[BlockId(0)].at, conversation[BlockId(1)].at),
        (Some(start + 2000), Some(start + 3000))
    );

    let index = index(&transcript);
    assert_eq!(index.title.as_deref(), Some("Second title"));
    assert_eq!(index.first_prompt.as_deref(), Some("Rename it."));
    assert_eq!(
        (index.started, index.ended),
        (Some(start + 2000), Some(start + 9000))
    );
    assert_eq!(index.own.tokens.output, 80);
    assert!(!index.transcript_missing);
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
    t.system("informational", "A note nobody searches.");
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
    let at = |turn, block| At::Block {
        turn,
        block: BlockId(block),
    };
    let span = |at, kind, text: &str| Span {
        at,
        kind,
        text: text.to_string(),
    };
    assert_eq!(
        index(&transcript).spans,
        [
            span(at(0, 0), SpanKind::Prompt, "Find the parser."),
            span(at(0, 2), SpanKind::ToolCall, "fn parse"),
            span(at(0, 2), SpanKind::ToolCall, "crates"),
            span(
                at(0, 3),
                SpanKind::ToolCall,
                "/home/user/code/app/crates/core/src/parse.rs"
            ),
            span(at(0, 4), SpanKind::Reply, "It is in the core crate."),
            span(at(1, 6), SpanKind::Prompt, "Run it."),
            span(at(1, 7), SpanKind::ToolCall, "cargo run"),
            span(at(1, 8), SpanKind::ToolCall, "https://example.com/doc"),
        ]
    );
}

#[test]
fn what_cannot_be_read_is_said_and_the_rest_is_kept() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    session.transcript.prompt("Count the lines.");
    let call = session.transcript.call(
        "Agent",
        json!({ "description": "Count lines", "prompt": "Count them." }),
    );
    let mut sub = session.subagent_of(&call, "counter", "Count lines");
    sub.user(json!("Count them."), json!({}));
    sub.reply("Ninety lines.");
    let named = json!({ "agentId": sub.agent });
    session
        .transcript
        .result_with(&call, json!("Ninety lines."), false, named);
    session.transcript.reply("Ninety.");
    session.title("Counting");
    let files = Files::of(&tree, &session);
    let broken = |ending, after| Broken {
        files: &files,
        ending,
        after,
    };
    let whole = "turn 1\n  prompt: \"Count the lines.\"\n  \
                 Agent: subagent None (Some(\"Count lines\")): \"Count them.\"\n    \
                 Ok \"Ninety lines.\"\n";
    let parse = |items: &Broken| {
        let mut transcript = Agent.parse(items);
        transcript.read_subagents(&Agent, items);
        transcript
    };

    /* The session's own transcript: what was read before the failure is the conversation. */
    let main = fs::read(tree.path(&session.key)).unwrap();
    let first_line = main.iter().position(|byte| *byte == b'\n').unwrap() as u64;
    let items = broken(".jsonl", Some(first_line + 5));
    let transcript = Agent.parse(&items);
    assert_eq!(
        outline(own(&transcript), &files),
        format!("turn 1\n  prompt: \"Count the lines.\"\nunreadable from here: {BROKEN}\n")
    );
    let items = broken(".jsonl", None);
    let transcript = Agent.parse(&items);
    assert_eq!(
        outline(own(&transcript), &files),
        format!("unreadable from here: {BROKEN}\n")
    );
    assert_eq!(index(&transcript).title.as_deref(), Some("Counting"));

    /* A subagent's: the session is whole, and the subagent says how far it was read. */
    let subagent = "subagent Some(\"counter\") (Some(\"Count lines\"))";
    let transcript = parse(&broken("agent-a0000000000000001.jsonl", None));
    assert_eq!(
        outline(own(&transcript), &files),
        format!(
            "{whole}    {subagent}\n      unreadable from here: {BROKEN}\n  \
             reply by model-1: \"Ninety.\"\n"
        )
    );

    /* A subagent's description of itself: it is read without one. */
    let transcript = parse(&broken(".meta.json", None));
    assert_eq!(
        outline(own(&transcript), &files),
        format!(
            "{whole}    subagent None (None)\n      turn 1\n        prompt: \"Count them.\"\n        \
             reply by model-1: \"Ninety lines.\"\n      \
             could not read agent-a0000000000000001.meta.json: {BROKEN}\n  \
             reply by model-1: \"Ninety.\"\n"
        )
    );

    /* The title file: the session has no title but its first prompt. */
    let transcript = parse(&broken("custom-title.json", None));
    assert_eq!(transcript.facts.title, None);
    assert_eq!(
        own(&transcript).unreadable[0].item,
        format!(
            "{}/custom-title.json",
            session.key.strip_suffix(".jsonl").unwrap()
        )
    );
    assert_eq!(own(&transcript).ending, Ending::Whole);
    assert_eq!(
        index(&transcript).title.as_deref(),
        Some("Count the lines.")
    );
}

#[test]
fn a_session_whose_own_transcript_is_missing_is_its_subagents() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let call = session.transcript.call(
        "Agent",
        json!({ "description": "Count lines", "prompt": "Count them." }),
    );
    let mut sub = session.subagent_of(&call, "counter", "Count lines");
    sub.user(json!("Count them."), json!({}));
    let call = sub.call("Bash", json!({ "command": "wc -l src/*.rs" }));
    sub.result(&call, json!("90"));
    sub.reply("Ninety lines.");
    session.title("Counting");
    fs::remove_file(tree.path(&session.key)).unwrap();

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(transcript.conversation, None);
    assert_eq!(transcript.facts.title.as_deref(), Some("Counting"));
    assert_eq!(transcript.unattached.len(), 1);
    assert_eq!(
        outline(read(&transcript.unattached[0]), &files),
        r#"turn 1
  prompt: "Count them."
  Bash: shell "wc -l src/*.rs" (None)
    Ok "90"
  reply by model-1: "Ninety lines."
"#
    );

    let index = index(&transcript);
    assert!(index.transcript_missing);
    assert_eq!(index.title.as_deref(), Some("Counting"));
    assert_eq!(index.first_prompt, None);
    let start = 1_767_225_600_000;
    assert_eq!(
        (index.started, index.ended),
        (Some(start), Some(start + 3000))
    );
    assert_eq!(index.own, Totals::default());
    assert_eq!(index.all.calls, BTreeMap::from([(ToolKind::Shell, 1)]));
    assert_eq!(
        index
            .spans
            .iter()
            .map(|span| (span.at, span.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (At::Unattached(0), "Count them."),
            (At::Unattached(0), "wc -l src/*.rs"),
            (At::Unattached(0), "Ninety lines."),
        ]
    );
}
