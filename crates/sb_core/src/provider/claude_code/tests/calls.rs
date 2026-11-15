//! Tool calls: their kinds and typed inputs, their results and short results, images, and what
//! a `Stored` loads.

use std::time::Duration;

use pretty_assertions::assert_eq;
use sb_test_support::{ProjectsTree, image_block};
use serde_json::json;

use super::super::tools;
use super::{call_in, outline, own, parse, patch, session};
use crate::provider::SessionItems as _;
use crate::transcript::{
    BlockId, Brief, Content, Encoding, LoadError, Output, Stored, Todo, TodoState, ToolInput,
    ToolKind, index,
};

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
    let read = json!({ "type": "text", "file": {
        "filePath": "/home/user/code/app/src/main.rs", "content": "fn main() {",
        "numLines": 1, "startLine": 10, "totalLines": 40,
    } });
    t.result_with(&call, json!("10\tfn main() {"), false, read);
    let call = t.call(
        "Edit",
        json!({
            "file_path": "/home/user/code/app/src/main.rs",
            "old_string": "let a = \"1\"",
            "new_string": "let a = \"1\";",
            "replace_all": true,
        }),
    );
    let patch = json!({ "filePath": "/home/user/code/app/src/main.rs", "structuredPatch": [{
        "oldStart": 9, "oldLines": 3, "newStart": 9, "newLines": 4,
        "lines": [" fn main() {", "-    let a = \"1\"", "+    let a = \"1\";", "+    run(a);", " }"],
    }] });
    t.result_with(&call, json!("The file was updated."), false, patch);
    let call = t.call(
        "Write",
        json!({ "file_path": "/home/user/code/app/notes.md", "content": "# Notes\n" }),
    );
    let written = json!({
        "type": "create", "filePath": "/home/user/code/app/notes.md", "content": "# Notes\n",
        "structuredPatch": [], "originalFile": null,
    });
    t.result_with(&call, json!("The file was created."), false, written);
    let call = t.call("Grep", json!({ "pattern": "fn main", "path": "src" }));
    let found = json!({ "mode": "content", "numFiles": 1, "numMatches": 2 });
    t.result_with(&call, json!("src/main.rs:10:fn main() {"), false, found);
    let call = t.call("Glob", json!({ "pattern": "**/*.rs" }));
    let found = json!({ "filenames": ["src/main.rs"], "numFiles": 1, "durationMs": 3 });
    t.result_with(&call, json!("src/main.rs"), false, found);
    let call = t.call(
        "TodoWrite",
        json!({ "todos": [
            { "content": "Find the error", "status": "completed", "activeForm": "Finding it" },
            { "content": "Fix it", "status": "in_progress", "activeForm": "Fixing it" },
            { "content": "Run the tests", "status": "pending", "activeForm": "Running them" },
            { "content": "Later", "status": "someday" },
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
    let conversation = own(&transcript);
    assert_eq!(
        outline(conversation, &files),
        r##"turn 1
  prompt: "Why does the build fail?"
  thinking for Some(1.5s): "Look at the log first."
  reply by model-1: "I'll build it."
  Bash: shell "cargo build" (Some("Build the app"))
    Failed Exit(101) "Exit code 101\nerror: expected `;`"
  Read: read /home/user/code/app/src/main.rs from line Some(10) for Some(20)
    Ok Lines(1) "10\tfn main() {"
  Edit: edit /home/user/code/app/src/main.rs: "let a = \"1\"" to "let a = \"1\";", all: true
    Ok Changed { added: 2, removed: 1 } "The file was updated."
  Write: write /home/user/code/app/notes.md: "# Notes\n"
    Ok Written { new: true, lines: 1 } "The file was created."
  Grep: content search "fn main" in Some("src")
    Ok Matches(2) "src/main.rs:10:fn main() {"
  Glob: file search "**/*.rs" in None
    Ok Matches(1) "src/main.rs"
  TodoWrite: todos: Done "Find the error", InProgress "Fix it", Open "Run the tests", Open "Later"
    Ok "The list was updated."
  WebSearch: web search "rust expected semicolon"
    Ok "Two results."
  WebFetch: web fetch https://example.com/errors (Some("What does it say?"))
    Ok "It says hello."
  mcp__tracker__search_issues: mcp tracker search_issues: {"query":"build"}
    Ok "No issues."
  Teleport: other (Other): {"to":"the moon"}
    Ok "Done."
  Bash: shell "cargo test" (None)
    no result
"##
    );
    assert_eq!(
        index(&transcript).own.calls.into_iter().collect::<Vec<_>>(),
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
        conversation[BlockId(3)].at,
        call_in(conversation, 3).result.as_ref().unwrap().at,
    );
    assert_eq!(answered.unwrap() - asked.unwrap(), 1000);
    assert_eq!(
        call_in(conversation, 9).input,
        ToolInput::Todos(vec![
            Todo {
                text: "Find the error".to_string(),
                state: TodoState::Done,
            },
            Todo {
                text: "Fix it".to_string(),
                state: TodoState::InProgress,
            },
            Todo {
                text: "Run the tests".to_string(),
                state: TodoState::Open,
            },
            Todo {
                text: "Later".to_string(),
                state: TodoState::Open,
            },
        ])
    );
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
        assert_eq!(tools::kind(name), kind, "{name}");
    }
}

#[test]
fn a_call_is_of_its_tools_kind_whatever_its_input() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    let call = t.call("Bash", json!({ "cmd": "ls" }));
    t.result(&call, json!("a.txt"));
    let call = t.call("Read", json!({ "file_path": "a.txt", "offset": "ten" }));
    t.result(&call, json!("a"));
    t.call("mcp__odd", json!({}));

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(own(&transcript), &files),
        r#"turn 1
  Bash: other (Shell): {"cmd":"ls"}
    Ok "a.txt"
  Read: other (Read): {"file_path":"a.txt","offset":"ten"}
    Ok "a"
  mcp__odd: other (Mcp): {}
    no result
"#
    );
    assert_eq!(
        index(&transcript).own.calls.into_iter().collect::<Vec<_>>(),
        [
            (ToolKind::Shell, 1),
            (ToolKind::Read, 1),
            (ToolKind::Mcp, 1)
        ]
    );
}

#[test]
fn a_result_has_a_short_form_where_the_record_gives_one() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    let mut briefs = Vec::new();
    let mut expect = |brief: Option<Brief>| briefs.push(brief);

    /* A command's exit status is only in the text of one that failed and says so. */
    let call = t.call("Bash", json!({ "command": "true" }));
    t.result_with(
        &call,
        json!("ok"),
        false,
        json!({ "stdout": "ok", "interrupted": false }),
    );
    expect(None);
    let call = t.call("Bash", json!({ "command": "false" }));
    t.result_with(
        &call,
        json!([{ "type": "text", "text": "Exit code 2\nno" }]),
        true,
        json!("Error"),
    );
    expect(Some(Brief::Exit(2)));
    let call = t.call("Bash", json!({ "command": "sleep 900" }));
    t.failure(&call, "Command timed out after 2m");
    expect(None);
    let call = t.call("Bash", json!({ "command": "exit" }));
    t.failure(&call, "Exit code unknown");
    expect(None);

    /* A file read as a picture has no lines, and a failed edit no patch. */
    let call = t.call(
        "Read",
        json!({ "file_path": "/home/user/code/app/shot.png" }),
    );
    let picture = json!({ "type": "image", "file": { "base64": "AAAA", "type": "image/png" } });
    t.result_with(
        &call,
        json!([image_block("image/png", b"png")]),
        false,
        picture,
    );
    expect(None);
    let call = t.call(
        "Edit",
        json!({ "file_path": "a.rs", "old_string": "a", "new_string": "b" }),
    );
    t.result_with(
        &call,
        json!("String to replace not found"),
        true,
        json!("Error: not found"),
    );
    expect(None);

    /* A file written over an older one is not new. */
    let call = t.call(
        "Write",
        json!({ "file_path": "a.md", "content": "one\ntwo\nthree" }),
    );
    let written = json!({
        "type": "update", "filePath": "a.md", "content": "one\ntwo\nthree", "originalFile": "one\n",
        "structuredPatch": [{ "oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 3,
                              "lines": [" one", "+two", "+three"] }],
    });
    t.result_with(&call, json!("The file was updated."), false, written);
    expect(Some(Brief::Written {
        new: false,
        lines: 3,
    }));

    /* A subagent's run is summed up once it is over; one sent off to run on is only named. */
    let input = json!({ "description": "Count lines", "prompt": "Count them." });
    let call = t.call("Agent", input.clone());
    let ran = json!({
        "agentId": "a1", "status": "completed", "resolvedModel": "model-2",
        "totalToolUseCount": 9, "totalDurationMs": 41_000, "totalTokens": 5000,
    });
    t.result_with(&call, json!("Ninety."), false, ran);
    expect(Some(Brief::Ran {
        model: Some("model-2".to_string()),
        calls: Some(9),
        duration: Some(Duration::from_secs(41)),
    }));
    let call = t.call("Agent", input.clone());
    let launched = json!({ "agentId": "a2", "status": "async_launched", "isAsync": true,
                           "resolvedModel": "model-2" });
    t.result_with(&call, json!("Launched."), false, launched);
    expect(Some(Brief::Ran {
        model: Some("model-2".to_string()),
        calls: None,
        duration: None,
    }));
    let call = t.call("Agent", input);
    t.result(&call, json!("Done."));
    expect(None);

    /* A tool with no short form has none, whatever its record holds. */
    let call = t.call("WebSearch", json!({ "query": "lines" }));
    t.result_with(
        &call,
        json!("Results."),
        false,
        json!({ "numFiles": 3, "results": [] }),
    );
    expect(None);

    let (transcript, _) = parse(&tree, &session);
    let found: Vec<Option<Brief>> = own(&transcript)
        .calls()
        .map(|(_, call)| call.result.as_ref().unwrap().brief.clone())
        .collect();
    assert_eq!(found, briefs);
}

#[test]
fn a_result_written_before_its_call_is_still_its_result() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("List the files.");
    t.user(
        json!([{ "type": "tool_result", "tool_use_id": "toolu_late", "content": "a.txt" }]),
        json!({}),
    );
    let call = json!({ "type": "tool_use", "id": "toolu_late", "name": "Bash",
                       "input": { "command": "ls" } });
    t.assistant(50, "model-1", json!([call]), json!({}));

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(own(&transcript), &files),
        r#"turn 1
  prompt: "List the files."
  Bash: shell "ls" (None)
    Ok "a.txt"
"#
    );
}

#[test]
fn a_reply_of_only_a_call_has_no_empty_text_before_it() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.assistant(
        50,
        "model-1",
        json!([
            { "type": "text", "text": "" },
            { "type": "text", "text": "\n\n" },
            { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": { "command": "ls" } },
            { "type": "text", "text": "Listing." },
        ]),
        json!({}),
    );

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(own(&transcript), &files),
        "turn 1\n  Bash: shell \"ls\" (None)\n    no result\n  reply by model-1: \"Listing.\"\n"
    );
}

#[test]
fn output_saved_outside_the_transcript_is_read_from_its_file() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let output = "line of output\n".repeat(400);
    let (key, note) = session.saved_output("b1c2d3e4f.txt", output.as_bytes());
    let (_, other) = session.saved_output("mcp-tracker-list-1.txt", b"a list");
    let t = &mut session.transcript;
    t.prompt("Show the log.");
    let call = t.call("Bash", json!({ "command": "cat build.log" }));
    t.result(&call, json!(note));
    /* The same note for a file that was never archived stays the result. */
    let call = t.call("Bash", json!({ "command": "cat other.log" }));
    let gone = note.replace("b1c2d3e4f", "gone");
    t.result(&call, json!(gone));
    /* The path is the location's own, whatever its separators, and only its file's name
    is looked for. */
    let call = t.call("mcp__tracker__list", json!({}));
    let windows = other.replace(
        &tree.path("").display().to_string(),
        r"C:\Users\user\.claude\",
    );
    let windows = windows.replace('/', r"\");
    assert!(windows.contains(r"\tool-results\mcp-tracker-list-1.txt"));
    t.result(&call, json!([{ "type": "text", "text": windows }]));

    let (transcript, files) = parse(&tree, &session);
    let conversation = own(&transcript);
    let output_of = |block| {
        let [Output::Text(output)] =
            &call_in(conversation, block).result.as_ref().unwrap().output[..]
        else {
            panic!("{:?}", call_in(conversation, block).result);
        };
        output
    };
    assert_eq!(
        output_of(1),
        &Stored {
            item: key.as_str().into(),
            offset: 0,
            length: output.len() as u64,
            encoding: Encoding::Plain,
        }
    );
    assert_eq!(output_of(1).load_text(&files).unwrap(), output);
    assert_eq!(output_of(2).item, conversation.item);
    assert_eq!(output_of(2).load_text(&files).unwrap(), gone);
    assert_eq!(output_of(3).load_text(&files).unwrap(), "a list");
}

#[test]
fn images_are_loaded_from_where_they_are_recorded() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    let pasted: Vec<u8> = (0..=255).collect();
    /* A picture that isn't in the transcript itself is kept as it was recorded. */
    let linked =
        json!({ "type": "image", "source": { "type": "url", "url": "https://example.com/a.png" } });
    t.user(
        json!([
            { "type": "text", "text": "What is in this picture?" },
            image_block("image/png", &pasted),
            linked.clone(),
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
    let call = t.call("WebFetch", json!({ "url": "https://example.com/a.png" }));
    t.result(&call, json!([linked.clone()]));
    /* A picture in a record the user didn't write is no prompt. */
    t.user(
        json!([image_block("image/png", b"png")]),
        json!({ "isMeta": true }),
    );

    let (transcript, files) = parse(&tree, &session);
    let conversation = own(&transcript);
    let outline = outline(conversation, &files);
    let (before, after) = outline.split_once("  WebFetch").unwrap();
    assert_eq!(
        before,
        r#"turn 1
  prompt: "What is in this picture?" + image/png of 256 bytes
  "image" with an unreadable source
  Read: read /home/user/code/app/shot.jpg from line None for None
    Ok image/jpeg of 17 bytes "[Image: source: /home/user/code/app/shot.jpg]"
"#
    );
    assert_eq!(
        after.lines().last(),
        Some(r#"  "image" with an unreadable isMeta"#)
    );
    let Content::Prompt { images, .. } = &conversation[BlockId(0)].content else {
        panic!("{:?}", conversation[BlockId(0)]);
    };
    assert_eq!(images[0].data.load(&files).unwrap(), pasted);
    let Output::Image(image) = &call_in(conversation, 2).result.as_ref().unwrap().output[0] else {
        panic!("{:?}", call_in(conversation, 2).result);
    };
    assert_eq!(image.data.load(&files).unwrap(), shot);
    let [Output::Text(recorded)] = &call_in(conversation, 3).result.as_ref().unwrap().output[..]
    else {
        panic!("{:?}", call_in(conversation, 3).result);
    };
    let recorded: serde_json::Value =
        serde_json::from_slice(&recorded.load(&files).unwrap()).unwrap();
    assert_eq!(recorded, linked);
}

#[test]
fn text_cut_inside_a_character_is_still_read() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    let t = &mut session.transcript;
    t.prompt("Say it. CUT");
    t.reply("Done CUT and on.");
    let call = t.call("Bash", json!({ "command": "echo CUT" }));
    t.result(&call, json!("party CUT"));
    t.thinking("Hm CUT", 10);
    /* What a cut in the middle of an emoji leaves: half of its escaped surrogate pair. */
    patch(&tree, &session, "CUT", r"\ud83d");

    let (transcript, files) = parse(&tree, &session);
    assert_eq!(
        outline(own(&transcript), &files),
        "turn 1\n  prompt: \"Say it. \u{fffd}\"\n  reply by model-1: \"Done \u{fffd} and on.\"\n  \
         Bash: shell \"echo \u{fffd}\" (None)\n    Ok \"party \u{fffd}\"\n  \
         thinking for Some(10ms): \"Hm \u{fffd}\"\n"
    );
}

#[test]
fn content_that_cannot_be_loaded_says_why() {
    let mut tree = ProjectsTree::new();
    let mut session = session(&mut tree);
    session.transcript.prompt("One.");
    let (_, files) = parse(&tree, &session);
    let length = files.list()[0].length;
    let stored = |item: &str, offset, length, encoding| Stored {
        item: item.into(),
        offset,
        length,
        encoding,
    };
    let key = session.key.as_str();

    assert_eq!(
        stored(key, 0, 9, Encoding::Plain).load(&files).unwrap(),
        std::fs::read(tree.path(key)).unwrap()[..9]
    );
    assert!(matches!(
        stored("projects/gone.jsonl", 0, 1, Encoding::Plain).load(&files),
        Err(LoadError::Read { .. })
    ));
    /* An earlier version of the item can be shorter than the one that was parsed. */
    assert!(matches!(
        stored(key, length - 2, 5, Encoding::Plain).load(&files),
        Err(LoadError::Short { offset, .. }) if offset == length - 2
    ));
    assert!(matches!(
        stored(key, 0, 9, Encoding::JsonString).load(&files),
        Err(LoadError::Encoding {
            encoding: Encoding::JsonString,
            ..
        })
    ));
    assert!(matches!(
        stored(key, 0, 9, Encoding::Base64).load(&files),
        Err(LoadError::Encoding {
            encoding: Encoding::Base64,
            ..
        })
    ));
}
