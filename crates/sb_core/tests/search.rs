//! Search (PLAN §10, DESIGN §4.9, §12): what the index step keeps of each session's text, what
//! a query finds in it, with which filters, and where a match opens the session. The cases
//! run against both agents, the file-tree one and the made-up one that keeps a database; a
//! subagent's text, which only the file-tree agent writes, has a case of its own.

mod common;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::catalogue::{CatalogueError, Shelf};
use sb_core::search::{self, Filters, Limit, MatchKind, Names, Query, Results};
use sb_core::transcript::{At, Content};
use sb_test_support::{Agent, FileAgent, Items, ProjectsTree};
use serde_json::json;

use common::{Bench, for_both_agents};

for_both_agents!(
    a_sessions_text_is_found_where_it_is_and_tool_output_is_not,
    a_short_term_is_read_for_and_a_long_one_looked_up,
    what_is_typed_is_text_whatever_it_holds,
    the_filters_narrow_what_is_found,
    the_index_emptied_and_built_again_finds_the_same,
    a_session_in_trash_is_found_only_there_and_one_deleted_for_good_nowhere,
    the_first_matches_of_a_session_are_given_and_the_rest_counted,
);

/// A bench with two sessions, each an exchange the tests look for.
fn bench<A: Agent + Default>() -> (Bench<A>, [String; 2]) {
    let mut agent = A::default();
    let first = agent.add_session(2);
    agent.exchange(
        &first,
        "Where is parse_args defined?",
        "rg -n parse_args src/main.rs",
        "src/main.rs:12: fn parse_args_from_output",
        "It is in the main module.",
    );
    let second = agent.add_session(2);
    agent.exchange(
        &second,
        "Rename the 解析器 widget, Café edition",
        "cargo test -p widget",
        "ok",
        "Done: the widget is renamed.",
    );
    let bench = Bench::new(agent);
    bench.sync();
    (bench, [first, second])
}

/// What `input`, typed into the palette, finds.
fn find<A: Agent>(bench: &Bench<A>, input: &str) -> Results {
    try_find(bench, input, Limit::default()).unwrap()
}

fn try_find<A: Agent>(
    bench: &Bench<A>,
    input: &str,
    limit: Limit,
) -> Result<Results, CatalogueError> {
    let catalogue = bench.archive().catalogue();
    let (query, unresolved) = search::query(input, &names(bench));
    assert_eq!(unresolved, [], "{input}");
    catalogue.search(&query, limit, &CancelToken::new())
}

fn names<A: Agent>(bench: &Bench<A>) -> Names<'static> {
    let catalogue = bench.archive().catalogue();
    Names {
        folders: catalogue.folders().unwrap().leak(),
        locations: catalogue.locations().unwrap().leak(),
        agents: vec![bench.agent.provider().id()].leak(),
    }
}

/// The transcripts of the sessions found, the best first.
fn found<A: Agent>(bench: &Bench<A>, results: &Results) -> Vec<String> {
    results
        .sessions
        .iter()
        .map(|found| transcript_of(bench, &found.session.id.key))
        .collect()
}

/// The transcripts of the sessions `input` finds, the best first.
fn hits<A: Agent>(bench: &Bench<A>, input: &str) -> Vec<String> {
    found(bench, &find(bench, input))
}

/// The key of the transcript of the session `session`.
fn transcript_of<A: Agent>(bench: &Bench<A>, session: &str) -> String {
    bench
        .items()
        .into_iter()
        .find(|item| item.session.as_deref() == Some(session) && item.kind.is_transcript())
        .unwrap()
        .key
}

fn a_sessions_text_is_found_where_it_is_and_tool_output_is_not<A: Agent + Default>() {
    let (bench, [first, second]) = bench::<A>();
    let results = find(&bench, "parse_args");
    assert_eq!(found(&bench, &results), [first.as_str()]);
    assert_eq!((results.total_sessions, results.total_matches), (1, 2));
    let shown = &results.sessions[0].shown;
    assert_eq!(
        shown.iter().map(|found| found.kind).collect::<Vec<_>>(),
        [MatchKind::Prompt, MatchKind::Command]
    );
    /* Each match is marked in its snippet, and opens the session at the block it is in. */
    let items = Items::of(&bench.agent, &first);
    let transcript = bench.agent.provider().parse(&items);
    let conversation = transcript.conversation.unwrap();
    for found in shown {
        assert!(!found.subagent);
        assert_eq!(found.snippet.hits.len(), 1, "{found:?}");
        let hit = found.snippet.hits[0].clone();
        assert_eq!(&found.snippet.text[hit], "parse_args");
        let Some(At::Block { turn, block }) = found.at else {
            panic!("{found:?} opens at no block");
        };
        assert!(conversation.turns()[turn].blocks.contains(&block));
        match (&conversation[block].content, found.kind) {
            (Content::Prompt { text, .. }, MatchKind::Prompt) => {
                assert_eq!(text, "Where is parse_args defined?");
            }
            (Content::ToolCall(_), MatchKind::Command) => {}
            (content, kind) => panic!("a {kind:?} match opens at {content:?}"),
        }
    }

    /* A reply, without regard to case, and inside a word. */
    assert_eq!(hits(&bench, "MAIN MODULE"), [first.as_str()]);
    assert_eq!(hits(&bench, "MaIn MoD"), [first.as_str()]);
    assert_eq!(hits(&bench, "odul"), [first.as_str()]);
    assert_eq!(hits(&bench, "renamed")[..], [second.as_str()]);
    /* Without regard to accents either. */
    assert_eq!(hits(&bench, "cafe"), [second.as_str()]);
    /* A path, whole or in part. */
    assert_eq!(hits(&bench, "src/main.rs"), [first.as_str()]);
    assert_eq!(hits(&bench, "main.rs"), [first.as_str()]);
    /* Every term in one span: the two are in different ones. */
    assert_eq!(find(&bench, "defined module").total_matches, 0);
    /* What the tool said is not searched (PLAN D10). */
    assert_eq!(find(&bench, "parse_args_from_output").total_matches, 0);
    /* Text in any script. */
    assert_eq!(hits(&bench, "解析器"), [second.as_str()]);
    /* Both sessions have "the" in some span: the one that did something last comes first. */
    let both = find(&bench, "the");
    let mut sessions = found(&bench, &both);
    sessions.sort();
    assert_eq!(sessions, [first.clone(), second.clone()]);
    assert!(
        both.sessions[0].session.ended >= both.sessions[1].session.ended,
        "{both:?}"
    );
}

fn a_short_term_is_read_for_and_a_long_one_looked_up<A: Agent + Default>() {
    let (bench, [first, second]) = bench::<A>();
    /* Two characters are too few for the index to hold: the text is read for them. */
    let short = find(&bench, "解析");
    assert_eq!(found(&bench, &short), [second.as_str()]);
    let hit = short.sessions[0].shown[0].snippet.hits[0].clone();
    assert_eq!(&short.sessions[0].shown[0].snippet.text[hit], "解析");
    assert_eq!(hits(&bench, "-p"), [second.as_str()]);
    /* A short term with a long one narrows what the long one finds. */
    assert_eq!(hits(&bench, "is main"), [first.as_str()]);
    assert_eq!(hits(&bench, "-p widget"), [second.as_str()]);
    assert_eq!(find(&bench, "zz widget").total_matches, 0);
    /* `%` and `_` are what they are, not patterns. */
    assert_eq!(hits(&bench, "_"), [first.as_str()]);
    assert_eq!(find(&bench, "%").total_matches, 0);
}

fn what_is_typed_is_text_whatever_it_holds<A: Agent + Default>() {
    let (bench, [first, _]) = bench::<A>();
    let long = "parse_args ".repeat(10_000);
    for input in [
        "\"",
        "\"\"",
        "'",
        "\\",
        "*",
        "^parse",
        "parse*",
        "NEAR(parse args)",
        "parse OR args",
        "parse AND NOT args",
        "-parse",
        "text:parse",
        "{text}: parse",
        "parse\"_args",
        "(",
        ")",
        "\u{0}",
        "\u{1}parse\u{2}",
        long.as_str(),
    ] {
        try_find(&bench, input, Limit::default()).unwrap_or_else(|error| {
            panic!("{input:?}: {error}");
        });
    }
    /* A quoted phrase is one term, spaces and all; an unclosed quote runs to the end. */
    assert_eq!(hits(&bench, "\"where is parse\""), [first.as_str()]);
    assert_eq!(hits(&bench, "\"is parse_args"), [first.as_str()]);
    assert_eq!(find(&bench, "\"parse_args where\"").total_matches, 0);
    /* No terms, no results. */
    assert_eq!(find(&bench, ""), Results::default());
    assert_eq!(find(&bench, "   "), Results::default());
    /* A cancelled search says so. */
    let catalogue = bench.archive().catalogue();
    let cancel = CancelToken::new();
    cancel.cancel();
    for terms in [["parse_args"], ["is"]] {
        let query = Query {
            terms: terms.map(str::to_string).to_vec(),
            filters: Filters::default(),
        };
        assert!(matches!(
            catalogue.search(&query, Limit::default(), &cancel),
            Err(CatalogueError::Cancelled)
        ));
    }
}

fn the_filters_narrow_what_is_found<A: Agent + Default>() {
    let (bench, [first, second]) = bench::<A>();
    let catalogue = bench.archive().catalogue();
    let ids = [bench.session_id(&first), bench.session_id(&second)];
    let both = |filters: Filters| {
        let query = Query {
            terms: vec!["the".to_string()],
            filters,
        };
        let results = catalogue
            .search(&query, Limit::default(), &CancelToken::new())
            .unwrap();
        let mut found = found(&bench, &results);
        found.sort();
        found
    };
    let mut all = vec![first.clone(), second.clone()];
    all.sort();
    assert_eq!(both(Filters::default()), all);

    /* Pages of the library. */
    let folder = catalogue.create_folder("Widgets", None).unwrap();
    catalogue.file_by_hand(&ids[1], folder).unwrap();
    catalogue.set_starred(&ids[0], true).unwrap();
    for (input, expected) in [
        ("the in:widgets", vec![second.clone()]),
        ("the IN:\"Widgets\"", vec![second.clone()]),
        ("the in:starred", vec![first.clone()]),
        ("the in:unsorted", vec![first.clone()]),
        ("the in:all", all.clone()),
        ("the in:trash", vec![]),
        ("the from:fixture", all.clone()),
        (
            &format!("the agent:{}", bench.agent.provider().id()),
            all.clone(),
        ),
    ] {
        let mut found = found(&bench, &find(&bench, input));
        found.sort();
        assert_eq!(found, expected, "{input}");
    }
    /* A token that names nothing is left out, and said to be. */
    let (query, unresolved) =
        search::query("the from:elsewhere agent:other after:soon", &names(&bench));
    assert_eq!(query.filters, Filters::default());
    assert_eq!(unresolved.len(), 3);

    /* What a session's row says: its model, its branch, its times. */
    let session = catalogue.session(&ids[0]).unwrap().unwrap();
    for model in &session.models {
        let part = model[1..].to_uppercase();
        let found = both(Filters {
            model: Some(part),
            ..Filters::default()
        });
        assert!(found.contains(&first), "{model}");
    }
    assert_eq!(
        both(Filters {
            model: Some("no such model".to_string()),
            ..Filters::default()
        }),
        Vec::<String>::new()
    );
    if let Some(branch) = &session.branch {
        assert!(
            both(Filters {
                branch: Some(branch.to_uppercase()),
                ..Filters::default()
            })
            .contains(&first)
        );
    }
    assert_eq!(
        both(Filters {
            branch: Some("no-such-branch".to_string()),
            ..Filters::default()
        }),
        Vec::<String>::new()
    );
    let within = |after: Option<i64>, before: Option<i64>| {
        both(Filters {
            after,
            before,
            ..Filters::default()
        })
        .contains(&first)
    };
    /* A session with no time is in no span of time. */
    let (Some(started), Some(ended)) = (session.started, session.ended) else {
        assert!(!within(Some(0), Some(i64::MAX)));
        return;
    };
    assert!(within(Some(ended), None));
    assert!(!within(Some(ended + 1), None));
    assert!(within(None, Some(started + 1)));
    assert!(!within(None, Some(started)));
    assert!(within(Some(started), Some(ended + 1)));
    assert!(within(Some(0), Some(i64::MAX)));
}

fn the_index_emptied_and_built_again_finds_the_same<A: Agent + Default>() {
    let (bench, _) = bench::<A>();
    let inputs = ["parse_args", "the", "解析", "main.rs", "cafe"];
    let before: Vec<Results> = inputs.iter().map(|input| find(&bench, input)).collect();
    assert!(before.iter().all(|results| results.total_matches > 0));

    /* Indexing what has not changed changes nothing. */
    assert_eq!(bench.index().sessions, 0);
    let again: Vec<Results> = inputs.iter().map(|input| find(&bench, input)).collect();
    assert_eq!(again, before);

    /* Emptied, the index finds nothing; built again, what it found. */
    bench.archive().catalogue().drop_index().unwrap();
    for input in inputs {
        assert_eq!(find(&bench, input), Results::default(), "{input}");
    }
    assert_eq!(bench.index().sessions, 2);
    let rebuilt: Vec<Results> = inputs.iter().map(|input| find(&bench, input)).collect();
    assert_eq!(rebuilt, before);
}

fn a_session_in_trash_is_found_only_there_and_one_deleted_for_good_nowhere<A: Agent + Default>() {
    let (bench, [first, second]) = bench::<A>();
    let catalogue = bench.archive().catalogue();
    let id = bench.session_id(&first);
    catalogue.trash(&id).unwrap();
    assert_eq!(hits(&bench, "the"), [second.as_str()]);
    assert_eq!(find(&bench, "parse_args").total_matches, 0);
    assert_eq!(hits(&bench, "parse_args in:trash"), [first.as_str()]);
    assert!(
        find(&bench, "parse_args in:trash").sessions[0]
            .session
            .trashed
            .is_some()
    );

    /* Its text stays in the index while it is in Trash, and goes with it for good. */
    bench.archive().delete_for_good(&id).unwrap();
    assert_eq!(find(&bench, "parse_args in:trash").total_matches, 0);
    assert_eq!(find(&bench, "parse_args in:all").total_matches, 0);
    assert_eq!(hits(&bench, "the"), [second.as_str()]);
    bench.sync();
    assert_eq!(find(&bench, "parse_args in:trash").total_matches, 0);
}

fn the_first_matches_of_a_session_are_given_and_the_rest_counted<A: Agent + Default>() {
    let (mut bench, [first, _]) = bench::<A>();
    for round in 0..5 {
        bench.agent.exchange(
            &first,
            &format!("Once more with parse_args, round {round}."),
            "true",
            "",
            "Again.",
        );
    }
    bench.sync();
    let one = Limit {
        sessions: 1,
        per_session: 3,
    };
    let results = try_find(&bench, "parse_args", one).unwrap();
    assert_eq!((results.total_sessions, results.total_matches), (1, 7));
    let session = &results.sessions[0];
    assert_eq!(session.matches, 7);
    /* In the order of the conversation: the first exchange's prompt, then its command. */
    assert_eq!(
        session
            .shown
            .iter()
            .map(|found| found.kind)
            .collect::<Vec<_>>(),
        [MatchKind::Prompt, MatchKind::Command, MatchKind::Prompt]
    );
    let at = |found: &search::Match| match found.at {
        Some(At::Block { turn, block }) => (turn, block.0),
        other => panic!("{other:?}"),
    };
    assert!(at(&session.shown[0]) < at(&session.shown[1]));
    assert!(at(&session.shown[1]) < at(&session.shown[2]));
    /* Of the sessions, as many as asked for. */
    let none = try_find(
        &bench,
        "the",
        Limit {
            sessions: 1,
            per_session: 0,
        },
    )
    .unwrap();
    assert_eq!((none.sessions.len(), none.total_sessions), (1, 2));
    assert!(none.sessions[0].shown.is_empty());
}

/**
 * A subagent's text is searched under its parent session, and a match in it opens the parent
 * at the call that started the subagent (PLAN §10). A session's title is searched too, and
 * a session whose title matches comes before one that only says the words.
 */
#[test]
fn a_subagents_text_opens_its_session_at_the_call_and_a_title_comes_first() {
    let mut tree = ProjectsTree::new();
    let mut said = tree.session("/home/user/code/app", 0);
    said.transcript.prompt("Ask the widget expert.");
    let call = said.transcript.call(
        "Agent",
        json!({ "subagent_type": "expert", "description": "Ask", "prompt": "Explain widgets." }),
    );
    let mut subagent = said.subagent_of(&call, "expert", "Ask");
    subagent.prompt("Explain widgets.");
    subagent.reply("A widget is a thing with a frobnicator.");
    said.transcript.result_with(
        &call,
        json!("Explained."),
        false,
        json!({ "agentId": subagent.agent, "status": "completed" }),
    );
    let mut titled = tree.session("/home/user/code/app", 0);
    titled.transcript.title("All about the frobnicator");
    titled.transcript.prompt("Hello.");
    let bench = Bench::new(FileAgent::on(tree));
    bench.sync();

    let results = find(&bench, "frobnicator");
    assert_eq!(
        found(&bench, &results),
        [titled.key.clone(), said.key.clone()]
    );
    let title = &results.sessions[0].shown[0];
    assert_eq!(
        (title.kind, title.at, title.subagent),
        (MatchKind::Title, None, false)
    );
    let inside = &results.sessions[1].shown[0];
    assert_eq!((inside.kind, inside.subagent), (MatchKind::Reply, true));
    let items = Items::of(&bench.agent, &said.key);
    let conversation = bench.agent.provider().parse(&items).conversation.unwrap();
    let Some(At::Block { block, .. }) = inside.at else {
        panic!("{inside:?}");
    };
    assert!(matches!(conversation[block].content, Content::ToolCall(_)));
}

#[test]
fn each_token_of_the_palette_is_parsed_in_one_place() {
    use search::{Key, Parsed, Token};
    let token = |key, value: &str| Token {
        key,
        value: value.to_string(),
    };
    let terms = |terms: &[&str]| {
        terms
            .iter()
            .map(|term| term.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        search::parse(
            "parse_args in:fc FROM:\"my mac\" model:opus \"two words\" agent:x branch:main \
             after:2026-03 before:2027 C:\\Users\\user https://example.com/a in: x:y"
        ),
        Parsed {
            terms: terms(&[
                "parse_args",
                "two words",
                "C:\\Users\\user",
                "https://example.com/a",
                "in:",
                "x:y",
            ]),
            tokens: vec![
                token(Key::In, "fc"),
                token(Key::From, "my mac"),
                token(Key::Model, "opus"),
                token(Key::Agent, "x"),
                token(Key::Branch, "main"),
                token(Key::After, "2026-03"),
                token(Key::Before, "2027"),
            ],
        }
    );
    assert_eq!(
        search::parse("  \"unclosed quote  ").terms,
        terms(&["unclosed quote  "])
    );
    assert_eq!(search::parse("\"\" a").terms, terms(&["a"]));
    assert_eq!(search::parse("").terms, terms(&[]));
}

#[test]
fn a_date_is_the_start_of_its_day_month_or_year_in_local_time() {
    use chrono::{Local, TimeZone as _};
    let names = Names {
        folders: &[],
        locations: &[],
        agents: &[],
    };
    let at = |input: &str| search::query(input, &names).0.filters;
    let midnight = |year, month, day| {
        Local
            .with_ymd_and_hms(year, month, day, 0, 0, 0)
            .earliest()
            .unwrap()
            .timestamp_millis()
    };
    assert_eq!(at("after:2026-03-05").after, Some(midnight(2026, 3, 5)));
    assert_eq!(at("after:2026-03").after, Some(midnight(2026, 3, 1)));
    assert_eq!(at("before:2026").before, Some(midnight(2026, 1, 1)));
    for not_a_date in [
        "2026-13",
        "2026-02-30",
        "26-1",
        "soon",
        "2026-03-05-1",
        "-1",
    ] {
        let (query, unresolved) = search::query(&format!("after:{not_a_date}"), &names);
        assert_eq!(
            (query.filters.after, unresolved.len()),
            (None, 1),
            "{not_a_date}"
        );
    }
}

#[test]
fn in_names_a_folder_before_a_page() {
    use sb_core::catalogue::Folder;
    use sb_core::filing::FolderId;
    let folders = [Folder {
        id: FolderId(7),
        parent: None,
        name: "Trash".to_string(),
    }];
    let names = Names {
        folders: &folders,
        locations: &[],
        agents: &[],
    };
    let shelf = |input: &str| search::query(input, &names).0.filters.shelf;
    assert_eq!(shelf("in:trash"), Shelf::Folder(FolderId(7)));
    assert_eq!(shelf("in:Starred"), Shelf::Starred);
    assert_eq!(shelf("in:unsorted"), Shelf::Unsorted);
    assert_eq!(shelf("x"), Shelf::All);
    /* The last token of a kind is the one that counts. */
    assert_eq!(shelf("in:starred in:unsorted"), Shelf::Unsorted);
}
