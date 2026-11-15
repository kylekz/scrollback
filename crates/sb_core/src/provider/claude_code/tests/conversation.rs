//! The conversation's tree and its branches, what the parser doesn't know, and what in the
//! user's place is a prompt and what a note.

use std::fs::{self, File};
use std::io::Write as _;

use pretty_assertions::assert_eq;
use sb_test_support::ProjectsTree;
use serde_json::json;

use super::{PROJECT, outline, own, parse, session};
use crate::transcript::{BlockId, Content, Conversation, Tokens, index};

/// Each block's parent.
fn parents(conversation: &Conversation) -> Vec<Option<usize>> {
    conversation
        .blocks()
        .iter()
        .map(|block| block.parent.map(|parent| parent.0))
        .collect()
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
    /* Known bookkeeping, and the attachments and notices known to hold no conversation. */
    t.bookkeeping("file-history-snapshot", json!({ "snapshot": {} }));
    t.bookkeeping("api-request", json!({ "params": {} }));
    t.record(
        "attachment",
        json!({ "attachment": { "type": "deferred_tools_delta", "addedNames": ["Teleport"] } }),
    );
    t.record(
        "system",
        json!({ "subtype": "stop_hook_summary", "hookCount": 1 }),
    );
    /* An attachment and a notice of kinds from a later release. */
    t.record(
        "attachment",
        json!({ "attachment": { "type": "mood_ring", "mood": "calm" } }),
    );
    t.record("system", json!({ "subtype": "weather_report" }));
    t.record("attachment", json!({ "attachment": "none" }));
    /* A content block of a type it doesn't know, in a reply and in a prompt. */
    t.assistant(
        90,
        "model-1",
        json!([{ "type": "hunch", "confidence": 0.5 }]),
        json!({}),
    );
    t.user(json!([{ "type": "gesture" }]), json!({}));
    /* A tool it doesn't know, whose result has a part it doesn't know. */
    let teleport = t.call("Teleport", json!({ "to": "the moon" }));
    t.result(&teleport, json!([{ "type": "postcard" }]));
    /* A result for a call that isn't in the transcript. */
    t.user(
        json!([{ "type": "tool_result", "tool_use_id": "toolu_gone", "content": "?" }]),
        json!({}),
    );
    /* Known types that aren't as they are read, and lines that are no record. */
    t.record("assistant", json!({ "message": "none" }));
    t.raw(r#"{"type":"user","uuid":17,"message":{"role":"user","content":"hm"}}"#);
    t.raw("not json at all");
    t.raw(r#"["a","list"]"#);
    t.reply("Still here.");

    let (transcript, files) = parse(&tree, &session);
    let conversation = own(&transcript);
    assert_eq!(
        outline(conversation, &files),
        r#"turn 1
  prompt: "Go on."
  unknown "hologram"
  unknown "attachment/mood_ring"
  unknown "system/weather_report"
  "attachment" with an unreadable attachment
  unknown "hunch"
  unknown "gesture"
  Teleport: other (Other): {"to":"the moon"}
    Ok "{\"type\":\"postcard\"}"
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
    let recorded = |block: usize| {
        let Content::Unrecognised(unknown) = &conversation[BlockId(block)].content else {
            panic!("{:?}", conversation[BlockId(block)]);
        };
        serde_json::from_slice::<serde_json::Value>(&unknown.raw.load(&files).unwrap()).unwrap()
    };
    assert_eq!(recorded(1)["payload"], json!({ "frames": 3 }));
    assert_eq!(recorded(2)["attachment"]["mood"], "calm");
    assert_eq!(recorded(5), json!({ "type": "hunch", "confidence": 0.5 }));
    assert_eq!(
        conversation.unplaced[2].raw.load_text(&files).unwrap(),
        "not json at all"
    );
    /* The conversation goes on through what it doesn't know. */
    assert_eq!(
        parents(conversation)[..4],
        [None, Some(0), Some(1), Some(2)]
    );
}

#[test]
fn a_rewound_conversation_shows_the_branch_that_ends_latest() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("Name the crate.");
    let first = t.reply("Call it app.");
    /* Context before the prompt belongs to the branch the prompt starts. */
    t.user(
        json!("<system-reminder>The date changed.</system-reminder>"),
        json!({}),
    );
    t.prompt("Now add a test.");
    let call = t.call("Bash", json!({ "command": "cargo test" }));
    t.result(&call, json!("ok"));
    t.reply("Added.");
    /* The user goes back to the first reply and asks something else. */
    t.follow(&first);
    t.prompt("Now add a benchmark.");
    t.reply("Benchmarked.");

    let (transcript, files) = parse(&tree, &session);
    let conversation = own(&transcript);
    assert_eq!(
        outline(conversation, &files),
        r#"turn 1
  prompt: "Name the crate."
  reply by model-1: "Call it app."
turn 2
  prompt: "Now add a benchmark."
  reply by model-1: "Benchmarked."
not shown
  note (system-reminder): "The date changed."
  prompt: "Now add a test."
  Bash: shell "cargo test" (None)
    Ok "ok"
  reply by model-1: "Added."
"#
    );
    /* Both branches follow the first reply. */
    assert_eq!(
        parents(conversation),
        [
            None,
            Some(0),
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            Some(1),
            Some(6)
        ]
    );
    assert_eq!(index(&transcript).own.calls.len(), 0);
}

#[test]
fn results_of_calls_made_at_once_are_on_the_shown_branch() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("Check the files.");
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
        outline(own(&transcript), &files),
        r#"turn 1
  prompt: "Check the files."
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
        own(&transcript).tokens,
        Tokens {
            input: 200,
            output: 40,
            cache_read: 2000,
            cache_write: 20,
        }
    );
}

#[test]
fn calls_beside_the_chain_are_shown_when_it_goes_on_from_an_earlier_result() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("Check both files.");
    let calls = t.calls(
        100,
        &[
            ("Read", json!({ "file_path": "/home/user/code/app/a.rs" })),
            ("Read", json!({ "file_path": "/home/user/code/app/b.rs" })),
        ],
    );
    /* The second call's result comes first, with the notice that the user stopped it. The
    first call's comes after, and the conversation goes on from that one: the second call and
    all under it are beside the chain. */
    t.follow(&calls[1].uuid);
    t.user(
        json!([
            { "type": "tool_result", "tool_use_id": calls[1].id, "content": "b", "is_error": true },
            { "type": "text", "text": "[Request interrupted by user for tool use]" },
        ]),
        json!({}),
    );
    t.record(
        "attachment",
        json!({ "attachment": { "type": "queued_command", "prompt": "A reminder.", "isMeta": true } }),
    );
    t.follow(&calls[0].uuid);
    t.result(&calls[0], json!("a"));
    t.prompt("Only the first, then.");
    t.reply("The first is fine.");

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(own(&transcript), &files),
        r#"turn 1
  prompt: "Check both files."
  Read: read /home/user/code/app/a.rs from line None for None
    Ok "a"
  Read: read /home/user/code/app/b.rs from line None for None
    Failed "b"
  note (interrupted): "[Request interrupted by user for tool use]"
  note (meta): "A reminder."
turn 2
  prompt: "Only the first, then."
  reply by model-1: "The first is fine."
"#
    );
    assert_eq!(index(&transcript).own.calls.values().sum::<u32>(), 2);
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
    /* A summary whose boundary was never written still says the agent compacted. */
    t.user(
        json!([{ "type": "text", "text": "It was finished." }]),
        json!({ "isCompactSummary": true }),
    );

    let (transcript, files) = parse(&tree, &session);
    let conversation = own(&transcript);
    assert_eq!(
        outline(conversation, &files),
        r#"turn 1
  prompt: "Start."
  reply by model-1: "Started."
  compaction from Some(171000) to Some(24000) tokens: Some("The user asked to start, and it was started.")
  reply by model-1: "Going on from the summary."
turn 2
  prompt: "Finish."
  reply by model-1: "Finished."
  compaction from None to None tokens: Some("It was finished.")
"#
    );
    /* The boundary starts no tree of its own: it follows the record it names. */
    assert_eq!(
        parents(conversation),
        [None, Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)]
    );
}

#[test]
fn every_tree_in_a_transcript_is_shown() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("One.");
    let first = t.reply("First.");
    t.prompt("Rewound.");
    t.follow(&first);
    t.prompt("Two.");
    /* A record that follows one that isn't in the transcript starts another tree. */
    t.follow("00000000-0000-4000-8000-00000000gone");
    t.prompt("Three.");
    t.reply("Third.");

    let (transcript, files) = parse(&tree, &session);
    let conversation = own(&transcript);
    assert_eq!(
        outline(conversation, &files),
        r#"turn 1
  prompt: "One."
  reply by model-1: "First."
turn 2
  prompt: "Two."
turn 3
  prompt: "Three."
  reply by model-1: "Third."
not shown
  prompt: "Rewound."
"#
    );
    assert_eq!(
        parents(conversation),
        [None, Some(0), Some(1), Some(1), None, Some(4)]
    );
}

#[test]
fn a_record_written_before_the_one_it_follows_is_placed_after_it() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    let reply = |uuid: &str, follows: &str, text: &str| {
        json!({
            "uuid": uuid, "parentUuid": follows,
            "message": { "role": "assistant", "model": "model-1",
                         "content": [{ "type": "text", "text": text }] },
        })
    };
    t.record("assistant", reply("answer-2", "answer-1", "Second."));
    t.record("assistant", reply("answer-1", "prompt", "First."));
    t.user(
        json!("Go."),
        json!({ "uuid": "prompt", "parentUuid": null }),
    );
    /* Records that follow each other in a ring are kept, one of them starting a tree. */
    t.record("assistant", reply("ring-1", "ring-2", "Round."));
    t.record("assistant", reply("ring-2", "ring-1", "And round."));
    t.record("assistant", reply("alone", "alone", "By itself."));

    let (transcript, files) = parse(&tree, &session);
    let conversation = own(&transcript);
    assert_eq!(
        outline(conversation, &files),
        r#"turn 1
  prompt: "Go."
  reply by model-1: "First."
  reply by model-1: "Second."
  reply by model-1: "And round."
  reply by model-1: "Round."
  reply by model-1: "By itself."
"#
    );
    assert_eq!(
        parents(conversation),
        [None, Some(0), Some(1), None, Some(3), None]
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
        outline(own(&transcript), &files),
        "turn 1\n  prompt: \"One.\"\n  reply by model-1: \"Two.\"\nends inside a record\n"
    );

    /* Cut anywhere, it parses, and never into more than the whole. */
    let blocks = own(&transcript).blocks().len();
    for length in 0..whole.len() {
        fs::write(tree.path(&session.key), &whole[..length]).unwrap();
        let (cut, _) = parse(&tree, &session);
        assert!(own(&cut).blocks().len() <= blocks, "{length}");
        assert!(own(&cut).unplaced.is_empty(), "{length}");
    }
}

#[test]
fn lines_can_end_as_windows_ends_them() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    session.transcript.prompt("One.");
    session.transcript.reply("Two.");
    let path = tree.path(&session.key);
    let unix = fs::read_to_string(&path).unwrap();
    fs::write(&path, unix.replace('\n', "\r\n\r\n")).unwrap();

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(own(&transcript), &files),
        "turn 1\n  prompt: \"One.\"\n  reply by model-1: \"Two.\"\n"
    );
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
        outline(own(&transcript), &files),
        "turn 1\n  prompt: \"One.\"\n  reply by model-1: \"Two.\"\nturn 2\n  prompt: \"Three.\"\n"
    );
    assert_eq!(transcript.facts.project_path.as_deref(), Some(PROJECT));
}

#[test]
fn a_prompt_behind_what_was_put_before_it_is_still_a_prompt() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    /* Reminders and an event, with nothing asked. */
    t.user(
        json!(
            "<system-reminder>\nToday is a Monday.\n</system-reminder>\n\
               <system-reminder>It rains.</system-reminder>\n\
               <side-sessions-event>A side session ended.</side-sessions-event>"
        ),
        json!({}),
    );
    /* The same in front of what the user asked, as text and as blocks. */
    t.user(
        json!("<system-reminder>It still rains.</system-reminder>\n\nRun the linter."),
        json!({ "origin": { "kind": "human" } }),
    );
    t.reply("Running.");
    t.user(
        json!([
            { "type": "text", "text": "<system-reminder>The sun is out.</system-reminder>" },
            { "type": "text", "text": "<artifact-view-context artifact=\"a1\">Viewing a page.</artifact-view-context>\nNow the tests." },
            { "type": "text", "text": "All of them." },
        ]),
        json!({ "origin": { "kind": "human" } }),
    );
    /* A wrapper that is never closed takes the rest with it. */
    t.user(json!("<local-command-stdout>half an out"), json!({}));

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(own(&transcript), &files),
        r#"turn 1
  note (system-reminder): "Today is a Monday."
  note (system-reminder): "It rains."
  note (side-sessions-event): "A side session ended."
turn 2
  prompt: "Run the linter."
  note (system-reminder): "It still rains."
  reply by model-1: "Running."
turn 3
  prompt: "Now the tests.\n\nAll of them."
  note (system-reminder): "The sun is out."
  note (artifact-view-context): "Viewing a page."
  note (local-command-stdout): "half an out"
"#
    );
    let index = index(&transcript);
    assert_eq!(index.first_prompt.as_deref(), Some("Run the linter."));
    assert_eq!(index.title.as_deref(), Some("Run the linter."));
}

#[test]
fn what_the_agent_wrote_in_the_users_place_is_a_note() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.user(
        json!("Read the project's instructions."),
        json!({ "isMeta": true }),
    );
    t.prompt("Build it.");
    t.user(
        json!("[Request interrupted by user]"),
        json!({ "origin": { "kind": "human" } }),
    );
    t.user(
        json!("[Workflow harness — computed task] The task text follows."),
        json!({}),
    );
    t.user(
        json!("<task-notification>\n<status>completed</status>\n</task-notification>"),
        json!({ "origin": { "kind": "task-notification" } }),
    );
    t.user(
        json!("The task ended."),
        json!({ "origin": { "kind": "task-notification" } }),
    );
    t.user(
        json!("<local-command-stdout>done</local-command-stdout>"),
        json!({}),
    );
    t.system("informational", "The context is nearly full.");
    t.system("informational", "");
    t.record(
        "system",
        json!({ "subtype": "api_error", "error": { "formatted": "529 overloaded", "message": "x" } }),
    );
    t.record(
        "system",
        json!({ "subtype": "api_error", "error": { "message": "reset" } }),
    );
    t.record("system", json!({ "subtype": "api_error", "error": {} }));
    t.assistant(
        91,
        "<synthetic>",
        json!([{ "type": "text", "text": "No response requested." }]),
        json!({}),
    );

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(own(&transcript), &files),
        r#"turn 1
  note (meta): "Read the project's instructions."
turn 2
  prompt: "Build it."
  note (interrupted): "[Request interrupted by user]"
  note (workflow-harness): "[Workflow harness — computed task] The task text follows."
  note (task-notification): "<status>completed</status>"
  note (task-notification): "The task ended."
  note (local-command-stdout): "done"
  note (informational): "The context is nearly full."
  note (api_error): "529 overloaded"
  note (api_error): "reset"
  reply by nobody: "No response requested."
"#
    );
    assert_eq!(own(&transcript).models, Vec::<String>::new());
}

#[test]
fn what_others_asked_is_a_prompt_from_them_without_its_markup() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    /* A slash command, in both orders its elements have been written in. */
    t.user(
        json!("<command-name>/review</command-name>\n<command-message>review</command-message>\n<command-args>the parser</command-args>"),
        json!({}),
    );
    t.user(
        json!("<command-message>clear</command-message>\n<command-name>/clear</command-name>\n<command-args></command-args>"),
        json!({ "origin": { "kind": "human" } }),
    );
    /* Another session's message: its record says who sent it and what it said. */
    t.user(
        json!("<cross-session-message from=\"s-1\" from-mode=\"auto\" from-name=\"Main session\">\nReport how far you are.\n</cross-session-message>"),
        json!({ "isMeta": true, "origin": {
            "kind": "peer", "from": "s-1", "name": "Main session", "body": "Report how far you are.",
        } }),
    );
    t.user(
        json!("A message from the reviewer: Looks right."),
        json!({ "isMeta": true, "origin": { "kind": "peer", "from": "a-7", "body": "Looks right." } }),
    );
    /* Without that, the markup around it does. */
    t.user(
        json!("<agent-message from=\"a-7\">\nOne more thing.\n</agent-message>"),
        json!({ "isMeta": true, "origin": { "kind": "peer" } }),
    );
    t.user(
        json!("<cross-session-message from=\"s-1\" from-name=\"Main session\">Stop there.</cross-session-message>"),
        json!({}),
    );
    t.user(
        json!("Carry on."),
        json!({ "isMeta": true, "origin": { "kind": "coordinator" } }),
    );
    /* Typed while the agent was working, and delivered the same way. */
    t.record(
        "attachment",
        json!({ "attachment": {
            "type": "queued_command", "prompt": "Skip the slow ones.", "commandMode": "prompt",
            "origin": { "kind": "human" },
        } }),
    );
    t.record(
        "attachment",
        json!({ "attachment": {
            "type": "queued_command", "isMeta": true,
            "prompt": "<agent-message from=\"a-7\">Done.</agent-message>",
            "origin": { "kind": "peer", "from": "a-7", "name": "reviewer", "body": "Done." },
        } }),
    );
    t.record(
        "attachment",
        json!({ "attachment": { "type": "queued_command", "prompt": 7 } }),
    );

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(own(&transcript), &files),
        r#"turn 1
  prompt: "/review the parser"
turn 2
  prompt: "/clear"
turn 3
  prompt: "Report how far you are." from Main session
turn 4
  prompt: "Looks right." from a-7
turn 5
  prompt: "One more thing." from peer
turn 6
  prompt: "Stop there." from Main session
turn 7
  prompt: "Carry on." from coordinator
turn 8
  prompt: "Skip the slow ones."
turn 9
  prompt: "Done." from reviewer
  "attachment/queued_command" with an unreadable prompt
"#
    );
}
