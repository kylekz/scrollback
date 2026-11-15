//! The transcript model across agents (PLAN §7.4, §12). Sessions of both shapes of source, the
//! file-tree agent's and those of the made-up agent that keeps a database, parse into the same
//! model, and what is derived from it (the index, filing) works on either.

mod common;

use std::collections::BTreeMap;

use pretty_assertions::assert_eq;
use sb_core::filing::{self, AttachedPath, FolderId, NormalPath};
use sb_core::transcript::{
    BlockId, Content, Output, SpanKind, ToolInput, ToolKind, ToolStatus, Transcript, index,
};
use sb_test_support::{Agent, DbAgent, FileAgent, Items};
use serde_json::json;

use common::Bench;

fn parse(agent: &impl Agent, key: &str) -> (Transcript, Items) {
    let items = Items::of(agent, key);
    (agent.provider().parse(&items).unwrap(), items)
}

#[test]
fn a_session_kept_as_files_parses_into_turns() {
    a_session_parses_into_turns::<FileAgent>();
}

#[test]
fn a_session_kept_in_a_database_parses_into_turns() {
    a_session_parses_into_turns::<DbAgent>();
}

fn a_session_parses_into_turns<A: Agent + Default>() {
    let mut agent = A::default();
    let key = agent.add_session(8);
    let (transcript, _) = parse(&agent, &key);
    let conversation = &transcript.conversation;
    assert_eq!(transcript.facts.agent, agent.provider().id());
    assert_eq!(&*conversation.item, key);

    /* Every block is shown, in the order recorded, each after the one it follows, and every
    turn starts at a prompt. */
    let shown: Vec<BlockId> = conversation
        .turns
        .iter()
        .flat_map(|turn| turn.blocks.clone())
        .collect();
    assert_eq!(
        shown,
        (0..conversation.blocks.len())
            .map(BlockId)
            .collect::<Vec<_>>()
    );
    assert!(conversation.turns.len() >= 2);
    for turn in &conversation.turns {
        assert!(matches!(
            conversation[turn.blocks[0]].content,
            Content::Prompt { .. }
        ));
    }
    for (id, block) in conversation.blocks.iter().enumerate() {
        assert_eq!(block.parent, id.checked_sub(1).map(BlockId));
    }

    /* The index names the session by its first prompt and has a turn per turn. */
    let Content::Prompt { text: first, .. } = &conversation.blocks[0].content else {
        unreachable!("the first turn starts at a prompt");
    };
    let derived = index(&transcript);
    assert!(first.starts_with(derived.title.as_deref().unwrap()));
    assert_eq!(derived.turns.len(), conversation.turns.len());
    assert!(derived.turns.is_sorted());
    assert_eq!(
        derived
            .spans
            .iter()
            .filter(|span| span.kind == SpanKind::Prompt)
            .count(),
        conversation.turns.len()
    );

    /* What the agent appends extends the transcript and leaves what was parsed as it was. */
    agent.append(&key, 4);
    let (later, _) = parse(&agent, &key);
    assert!(later.conversation.blocks.len() > conversation.blocks.len());
    assert_eq!(
        later.conversation.blocks[..conversation.blocks.len()],
        conversation.blocks[..]
    );
    assert_eq!(
        index(&later).turns[..derived.turns.len()],
        derived.turns[..]
    );
}

#[test]
fn an_agent_of_another_shape_parses_into_the_same_model() {
    let mut bench = Bench::new(DbAgent::new());
    let agent = &mut bench.agent;
    let key = agent.add_session(0);
    agent.say(&key, "start", r"D:\Projects\App\crates");
    agent.say(&key, "user", "List the files.");
    agent.call(&key, "exec", json!({ "cmd": "ls" }), "a.txt\nb.txt", true);
    agent.call(
        &key,
        "open",
        json!({ "file": "c.txt" }),
        "no such file",
        false,
    );
    agent.call(&key, "beam", json!({ "to": "orbit" }), "done", true);
    agent.say(&key, "assistant", "Two files.");
    agent.say(&key, "narrator", "Meanwhile.");
    agent.say(&key, "user", "Thanks.");
    let (transcript, items) = parse(&bench.agent, &key);
    let conversation = &transcript.conversation;

    /* Two turns: the first prompt with everything it led to, then the second. */
    let turns: Vec<Vec<usize>> = conversation
        .turns
        .iter()
        .map(|turn| turn.blocks.iter().map(|id| id.0).collect())
        .collect();
    assert_eq!(turns, [vec![0, 1, 2, 3, 4, 5], vec![6]]);

    /* The agent's tools are the common kinds, under its own names for them. */
    let provider = bench.agent.provider();
    let calls: Vec<_> = conversation.calls().collect();
    assert_eq!(
        calls
            .iter()
            .map(|call| (
                call.name.as_str(),
                &call.input,
                provider.tool_kind(&call.name)
            ))
            .collect::<Vec<_>>(),
        [
            (
                "exec",
                &ToolInput::Shell {
                    command: "ls".to_string(),
                    description: None,
                },
                ToolKind::Shell
            ),
            (
                "open",
                &ToolInput::Read {
                    path: "c.txt".to_string(),
                    first_line: None,
                    lines: None,
                },
                ToolKind::Read
            ),
            /* A tool its provider doesn't map is kept, with its input as it was recorded. */
            ("beam", &ToolInput::Other, ToolKind::Other),
        ]
    );
    assert_eq!(
        calls[2].raw_input.load_text(&items).unwrap(),
        r#"{"to":"orbit"}"#
    );
    let outputs: Vec<(ToolStatus, String)> = calls
        .iter()
        .map(|call| {
            let result = call.result.as_ref().unwrap();
            let [Output::Text(output)] = &result.output[..] else {
                panic!("{:?}", result.output);
            };
            (result.status, output.load_text(&items).unwrap())
        })
        .collect();
    assert_eq!(
        outputs,
        [
            (ToolStatus::Ok, "a.txt\nb.txt".to_string()),
            (ToolStatus::Failed, "no such file".to_string()),
            (ToolStatus::Ok, "done".to_string()),
        ]
    );

    /* A record of a kind the provider doesn't know survives where it was. */
    let Content::Unrecognised(unknown) = &conversation.blocks[5].content else {
        panic!("{:?}", conversation.blocks[5]);
    };
    assert_eq!((unknown.kind.as_str(), unknown.misfit), ("narrator", None));
    assert!(
        unknown
            .raw
            .load_text(&items)
            .unwrap()
            .contains("Meanwhile.")
    );

    /* The index: counts by kind, and the text search holds, without tool output. */
    let derived = index(&transcript);
    assert_eq!(derived.title.as_deref(), Some("List the files."));
    assert_eq!(
        derived.calls,
        BTreeMap::from([
            (ToolKind::Shell, 1),
            (ToolKind::Read, 1),
            (ToolKind::Other, 1)
        ])
    );
    assert_eq!(
        derived
            .spans
            .iter()
            .map(|span| (span.turn, span.kind, span.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (0, SpanKind::Prompt, "List the files."),
            (0, SpanKind::ToolCall, "ls"),
            (0, SpanKind::ToolCall, "c.txt"),
            (0, SpanKind::Reply, "Two files."),
            (1, SpanKind::Prompt, "Thanks."),
        ]
    );

    /* Filing: the session started under a path attached to a folder. */
    let project = NormalPath::new(derived.project_path.as_deref().unwrap(), None);
    let attached = [
        AttachedPath {
            folder: FolderId(1),
            path: NormalPath::new("/home/user/code/app", None),
            location: None,
        },
        AttachedPath {
            folder: FolderId(2),
            path: NormalPath::new("/mnt/d/Projects/app", Some(filing::WSL_DRIVE_MOUNT)),
            location: None,
        },
    ];
    assert_eq!(
        filing::file(&project, &bench.location, &attached).map(|path| path.folder),
        Some(FolderId(2))
    );
}
