//! Shapes that older releases wrote, or that only some sessions have: subagents kept in the
//! session's own transcript, commands run in shell mode, a call answered twice, and files
//! beside a transcript that are not what they should be.

use pretty_assertions::assert_eq;
use sb_test_support::ProjectsTree;
use serde_json::json;

use super::super::audit::audit;
use super::{outline, own, parse, session};
use crate::transcript::{Content, Conversation, Subagent, index};

/// What was asked in `conversation`, turn by turn.
fn prompts(conversation: &Conversation) -> Vec<&str> {
    conversation
        .turns()
        .iter()
        .filter_map(|turn| match &conversation[turn.blocks[0]].content {
            Content::Prompt { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_subagent_kept_in_the_sessions_own_transcript_is_not_the_sessions_conversation() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    let side = || json!({ "isSidechain": true });
    t.prompt("Survey the crates.");
    let look = t.call(
        "Task",
        json!({ "subagent_type": "explorer", "description": "List crates", "prompt": "List them." }),
    );

    /* The subagent's records, among the session's: they follow each other, from a record
    that follows nothing. */
    t.record(
        "user",
        json!({ "isSidechain": true, "parentUuid": null,
                "message": { "role": "user", "content": "List them." } }),
    );
    t.assistant(
        90,
        "model-2",
        json!([{ "type": "tool_use", "id": "toolu_side", "name": "Bash",
                 "input": { "command": "ls crates" } }]),
        side(),
    );
    t.user(
        json!([{ "type": "tool_result", "tool_use_id": "toolu_side", "content": "core ui" }]),
        side(),
    );
    t.assistant(
        91,
        "model-2",
        json!([{ "type": "text", "text": "Two crates." }]),
        side(),
    );
    /* The session goes on from the call. */
    t.follow(&look.uuid);
    t.result(&look, json!("Two crates."));
    t.reply("There are two.");
    t.prompt("And the tests?");

    /* Two calls that give the same prompt, and one subagent that opens with it: which call
    started it can't be told, so it is under neither. */
    for _ in 0..2 {
        let again = t.call(
            "Task",
            json!({ "description": "Count", "prompt": "Count the tests." }),
        );
        t.result(&again, json!("Forty."));
    }
    let after = t.reply("Forty tests.");
    t.record(
        "user",
        json!({ "isSidechain": true, "parentUuid": null,
                "message": { "role": "user", "content": "Count the tests." } }),
    );
    t.assistant(
        92,
        "model-2",
        json!([{ "type": "text", "text": "Forty." }]),
        side(),
    );
    t.follow(&after);
    t.reply("That is all.");

    let (transcript, files) = parse(&tree, &session);
    let conversation = own(&transcript);
    /* Nothing the subagents were asked or did is the session's. */
    assert_eq!(
        prompts(conversation),
        ["Survey the crates.", "And the tests?"]
    );
    let calls: Vec<_> = conversation.calls().map(|(_, call)| call).collect();
    assert_eq!(calls.len(), 3);
    let [
        Subagent::Read {
            kind,
            description,
            conversation: listed,
        },
    ] = &calls[0].subagents[..]
    else {
        panic!("{:?}", calls[0].subagents);
    };
    assert_eq!(
        (kind.as_deref(), description.as_deref()),
        (Some("explorer"), Some("List crates"))
    );
    assert_eq!(prompts(listed), ["List them."]);
    assert_eq!(listed.calls().count(), 1);
    assert_eq!(listed.models, ["model-2"]);
    assert!(calls[1].subagents.is_empty() && calls[2].subagents.is_empty());
    let [
        Subagent::Read {
            conversation: counted,
            ..
        },
    ] = &transcript.unattached[..]
    else {
        panic!("{:?}", transcript.unattached);
    };
    assert_eq!(prompts(counted), ["Count the tests."]);

    /* What the subagents used and did is counted with them, not as the session's own. */
    let derived = index(&transcript);
    assert_eq!(derived.title.as_deref(), Some("Survey the crates."));
    assert_eq!(
        (
            derived.own.calls.values().sum::<u32>(),
            derived.all.calls.values().sum::<u32>()
        ),
        (3, 4)
    );
    assert_eq!(derived.models, ["model-1"]);
    assert_eq!(
        derived.all.tokens.output,
        derived.own.tokens.output + listed.tokens.output + counted.tokens.output
    );
    assert!(listed.tokens.output > 0 && counted.tokens.output > 0);

    /* The check counts them, and finds the parse to hold what the records do. */
    let audit = audit(&files, &transcript);
    assert_eq!(audit.mismatches, Vec::<&str>::new());
    let count = |what: &str| audit.counts.get(what).copied();
    assert_eq!(
        count("subagents kept in a session's own transcript"),
        Some(2)
    );
    assert_eq!(
        count("subagents kept in a session's own transcript, under the call that started them"),
        Some(1)
    );
}

#[test]
fn a_command_run_in_shell_mode_is_a_prompt_and_what_it_printed_a_note() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("<bash-input>ls -la crates</bash-input>");
    t.prompt("<bash-stdout>core\nui</bash-stdout><bash-stderr></bash-stderr>");
    t.prompt("<mystery-box>What is this?</mystery-box>");
    t.prompt("<div>Typed by hand</div>");

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(own(&transcript), &files),
        r#"turn 1
  prompt: "!ls -la crates"
  note (bash-stdout): "core\nui"
  note (bash-stderr): ""
turn 2
  prompt: "<mystery-box>What is this?</mystery-box>"
turn 3
  prompt: "<div>Typed by hand</div>"
"#
    );
    assert_eq!(index(&transcript).title.as_deref(), Some("!ls -la crates"));

    /* The check names an element of the agent's own making that the parser doesn't know,
    and only counts one that may be the user's. */
    let unknown: Vec<_> = audit(&files, &transcript).unknown.into_iter().collect();
    assert_eq!(
        unknown,
        [
            (
                "a prompt that starts with the element (another)".to_string(),
                1
            ),
            (
                "a prompt that starts with the element mystery-box".to_string(),
                1
            ),
        ]
    );
}

#[test]
fn a_second_result_for_a_call_is_kept_as_it_was_recorded() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("List the files.");
    let call = t.call("Bash", json!({ "command": "ls" }));
    t.result(&call, json!("a.txt"));
    t.result(&call, json!("b.txt"));

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(own(&transcript), &files),
        r#"turn 1
  prompt: "List the files."
  Bash: shell "ls" (None)
    Ok "a.txt"
  "tool_result" with an unreadable a second result
"#
    );
}

#[test]
fn a_file_beside_a_transcript_that_is_not_what_it_should_be_is_said() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    session.transcript.prompt("Start.");
    let call = session
        .transcript
        .call("Agent", json!({ "description": "Look", "prompt": "Look." }));
    let mut sub = session.subagent_without_meta();
    sub.user(json!("Look."), json!({}));
    session.transcript.result_with(
        &call,
        json!("Looked."),
        false,
        json!({ "agentId": sub.agent }),
    );
    /* A title that is no text, and a description of the subagent that is no JSON. */
    session.file("custom-title.json", br#"{"customTitle": 7}"#);
    let meta = format!("subagents/agent-{}.meta.json", sub.agent.as_ref().unwrap());
    session.file(&meta, b"{ not json");

    let (transcript, _) = parse(&tree, &session);
    assert_eq!(transcript.facts.title, None);
    let conversation = own(&transcript);
    let said: Vec<_> = conversation
        .unreadable
        .iter()
        .map(|unreadable| super::file_name(&unreadable.item))
        .collect();
    assert_eq!(said, ["custom-title.json"]);
    let [subagent] = &conversation.calls().next().unwrap().1.subagents[..] else {
        panic!("the call started one subagent");
    };
    let its = subagent.conversation().unwrap();
    assert_eq!(its.unreadable.len(), 1);
    assert!(its.unreadable[0].item.ends_with(".meta.json"));
    assert!(index(&transcript).unread);

    /* Text cut inside a character is still the title, and the description. */
    session.file("custom-title.json", br#"{"customTitle": "Fix \ud83c"}"#);
    session.file(
        &meta,
        br#"{"agentType": "explorer", "description": "Look \ud83c"}"#,
    );
    let (transcript, _) = parse(&tree, &session);
    assert_eq!(transcript.facts.title.as_deref(), Some("Fix \u{fffd}"));
    let Subagent::Read { description, .. } =
        &own(&transcript).calls().next().unwrap().1.subagents[0]
    else {
        panic!("the subagent was read");
    };
    assert_eq!(description.as_deref(), Some("Look \u{fffd}"));
    assert!(!index(&transcript).unread);
}

#[test]
fn a_transcript_of_nothing_but_a_subagents_records_still_says_where_it_started() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    let side = || json!({ "isSidechain": true });
    t.record(
        "user",
        json!({ "isSidechain": true, "parentUuid": null,
                "message": { "role": "user", "content": "Warm up." } }),
    );
    t.assistant(
        90,
        "model-2",
        json!([{ "type": "text", "text": "Ready." }]),
        side(),
    );
    /* A tree of one record that holds no conversation is no subagent. */
    t.record(
        "attachment",
        json!({ "isSidechain": true, "parentUuid": null, "attachment": { "type": "date" } }),
    );

    let (transcript, _) = parse(&tree, &session);
    assert_eq!(
        (
            transcript.facts.project_path.as_deref(),
            transcript.facts.branch.as_deref(),
            transcript.facts.entrypoint.as_deref(),
        ),
        (Some(super::PROJECT), Some("main"), Some("cli"))
    );
    assert!(own(&transcript).blocks().is_empty());
    let [Subagent::Read { conversation, .. }] = &transcript.unattached[..] else {
        panic!("{:?}", transcript.unattached);
    };
    assert_eq!(prompts(conversation), ["Warm up."]);
    assert_eq!(
        index(&transcript).project_path.as_deref(),
        Some(super::PROJECT)
    );
}

#[test]
fn two_subagents_that_open_alike_are_under_no_call() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("Go.");
    let call = t.call(
        "Task",
        json!({ "description": "Count", "prompt": "Count the tests." }),
    );
    for (message, reply) in [(90, "Forty."), (91, "Forty-one.")] {
        t.record(
            "user",
            json!({ "isSidechain": true, "parentUuid": null,
                    "message": { "role": "user", "content": "Count the tests." } }),
        );
        t.assistant(
            message,
            "model-2",
            json!([{ "type": "text", "text": reply }]),
            json!({ "isSidechain": true }),
        );
    }
    t.follow(&call.uuid);
    t.result(&call, json!("Forty."));

    /* One call gives the prompt and two subagents open with it: which it started can't be
    told, so neither is put under it. */
    let (transcript, _) = parse(&tree, &session);
    let calls: Vec<_> = own(&transcript).calls().collect();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].1.subagents.is_empty());
    assert_eq!(transcript.unattached.len(), 2);
}
