//! Search (PLAN §10, DESIGN §4.9, §12): what the index step keeps of each session's text, what
//! a query finds in it, with which filters, and where a match opens the session. The cases
//! run against both agents, the file-tree one and the made-up one that keeps a database; a
//! subagent's text, which only the file-tree agent writes, has a case of its own. Each ends by
//! verifying the archive, which checks the search index against the text it indexes.

mod common;

use std::fs;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::catalogue::Shelf;
use sb_core::search::{
    self, Calendar, Filters, Key, Limit, MatchKind, Names, Query, Results, SearchError, Searched,
};
use sb_core::transcript::{At, Content};
use sb_test_support::{Agent, FileAgent, Items, ProjectsTree, read_catalogue};
use serde_json::json;

use common::{Bench, for_both_agents};

for_both_agents!(
    a_sessions_text_is_found_where_it_is_and_tool_output_is_not,
    a_session_holds_every_term_somewhere_and_spans_with_all_show_first,
    a_short_term_is_read_for_or_narrows_or_waits_for_more,
    what_is_typed_is_text_whatever_it_holds,
    the_filters_narrow_what_is_found,
    the_index_emptied_and_built_again_finds_the_same,
    the_search_text_is_made_again_when_it_is_missing_or_unreadable,
    a_search_file_of_an_older_layout_is_made_again_by_the_start_index,
    a_session_in_trash_is_found_only_there_and_one_deleted_for_good_nowhere,
    the_first_matches_of_a_session_are_given_and_the_rest_counted,
    a_term_in_any_script_is_found_as_the_index_folds_it,
    a_long_term_is_marked_whole,
    no_text_is_written_for_a_session_deleted_while_it_is_parsed,
    a_search_index_that_cannot_be_opened_stops_nothing_but_search,
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
        "Rename the 解析器 widget, Café edition, for the Юг office: Tiếng Việt, ΟΔΟΣ, ᏣᎳᎩ",
        "cargo test -p widget",
        "ok",
        "Done: the widget is renamed, abcab.",
    );
    let bench = Bench::new(agent);
    bench.sync();
    (bench, [first, second])
}

/// What `input`, typed into the palette, finds.
fn find<A: Agent>(bench: &Bench<A>, input: &str) -> Results {
    match try_find(bench, input, Limit::default()).unwrap() {
        Searched::Results(results) => results,
        Searched::KeepTyping => panic!("{input:?} waits for more"),
    }
}

fn try_find<A: Agent>(
    bench: &Bench<A>,
    input: &str,
    limit: Limit,
) -> Result<Searched, SearchError> {
    let (query, unresolved) = search::query(input, &names(bench));
    assert_eq!(unresolved, [], "{input}");
    bench.archive().search(&query, limit, &CancelToken::new())
}

fn names<A: Agent>(bench: &Bench<A>) -> Names<'static> {
    let catalogue = bench.archive().catalogue();
    Names {
        folders: catalogue.folders().unwrap().leak(),
        locations: catalogue.locations().unwrap().leak(),
        agents: vec![bench.agent.provider().id()].leak(),
        calendar: Calendar::Local,
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

/// The kind of each match shown of the one session `input` finds.
fn kinds<A: Agent>(bench: &Bench<A>, input: &str) -> Vec<MatchKind> {
    let results = find(bench, input);
    assert_eq!(results.sessions.len(), 1, "{input}");
    results.sessions[0]
        .shown
        .iter()
        .map(|found| found.kind)
        .collect()
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
        assert!(found.subagent.is_none() && !found.cut);
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
    assert_eq!(hits(&bench, "renamed"), [second.as_str()]);
    /* Without regard to accents either, both ways. */
    assert_eq!(hits(&bench, "cafe"), [second.as_str()]);
    assert_eq!(hits(&bench, "CAFÉ"), [second.as_str()]);
    /* A path, whole or in part. */
    assert_eq!(hits(&bench, "src/main.rs"), [first.as_str()]);
    assert_eq!(hits(&bench, "main.rs"), [first.as_str()]);
    /* What the tool said is not searched (PLAN D10). */
    assert_eq!(find(&bench, "parse_args_from_output").total_matches, 0);
    /* Text in any script. */
    assert_eq!(hits(&bench, "解析器"), [second.as_str()]);
    /* Both sessions have "the" somewhere: the one that did something last comes first. */
    let both = find(&bench, "the");
    let mut sessions = found(&bench, &both);
    sessions.sort();
    assert_eq!(sessions, [first.clone(), second.clone()]);
    assert!(
        both.sessions[0].session.ended >= both.sessions[1].session.ended,
        "{both:?}"
    );
    bench.assert_verifies();
}

fn a_session_holds_every_term_somewhere_and_spans_with_all_show_first<A: Agent + Default>() {
    let (bench, [first, second]) = bench::<A>();
    /* "defined" is in the prompt, "module" in the reply: the session holds both. */
    assert_eq!(hits(&bench, "defined module"), [first.as_str()]);
    assert_eq!(
        kinds(&bench, "defined module"),
        [MatchKind::Prompt, MatchKind::Reply]
    );
    /* The command holds both terms, so it comes first; then each that holds one. */
    assert_eq!(
        kinds(&bench, "main parse_args"),
        [MatchKind::Command, MatchKind::Prompt, MatchKind::Reply]
    );
    /* Every term, not any: nothing holds both of these. */
    assert_eq!(find(&bench, "defined renamed").total_sessions, 0);
    assert_eq!(hits(&bench, "widget renamed"), [second.as_str()]);
    bench.assert_verifies();
}

fn a_short_term_is_read_for_or_narrows_or_waits_for_more<A: Agent + Default>() {
    let (bench, [first, second]) = bench::<A>();
    /* Two characters are too few for the index to hold: the text is read for them, folded
    as the index folds it. */
    let short = find(&bench, "解析");
    assert_eq!(found(&bench, &short), [second.as_str()]);
    let snippet = &short.sessions[0].shown[0].snippet;
    assert_eq!(&snippet.text[snippet.hits[0].clone()], "解析");
    assert_eq!(hits(&bench, "ЮГ"), [second.as_str()]);
    /* A letter or two of ASCII alone would match nearly everything. */
    for input in ["ok", "-p", "_", "%", "a b", "é"] {
        assert_eq!(
            try_find(&bench, input, Limit::default()).unwrap(),
            Searched::KeepTyping,
            "{input}"
        );
    }
    /* A short term with a long one narrows what the long one finds, and is marked too. */
    assert_eq!(hits(&bench, "is main"), [first.as_str()]);
    assert_eq!(hits(&bench, "-p widget"), [second.as_str()]);
    assert_eq!(find(&bench, "zz widget").total_matches, 0);
    let narrowed = find(&bench, "-p widget");
    let command = narrowed.sessions[0]
        .shown
        .iter()
        .find(|found| found.kind == MatchKind::Command)
        .unwrap();
    let marked: Vec<&str> = command
        .snippet
        .hits
        .iter()
        .map(|hit| &command.snippet.text[hit.clone()])
        .collect();
    assert_eq!(marked, ["-p", "widget"]);
    bench.assert_verifies();
}

fn what_is_typed_is_text_whatever_it_holds<A: Agent + Default>() {
    let (bench, [first, _]) = bench::<A>();
    let long = "parse_args ".repeat(10_000);
    let repeated = "parse ".repeat(500);
    let many: String = (0..2000).map(|at| format!("t{at} ")).collect();
    let short: String = (0..2000)
        .map(|at| format!("解{} ", char::from_u32(0x4e00 + at).unwrap()))
        .collect();
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
        "par\u{0}se_args",
        "\u{1}parse\u{2}",
        long.as_str(),
        repeated.as_str(),
        many.as_str(),
        short.as_str(),
    ] {
        try_find(&bench, input, Limit::default()).unwrap_or_else(|error| {
            panic!("{input:?}: {error}");
        });
    }
    /* A NUL is no part of a term. */
    assert_eq!(hits(&bench, "par\u{0}se_args"), [first.as_str()]);
    assert_eq!(find(&bench, "\u{0}"), Results::default());
    /* A quoted phrase is one term, spaces and all; an unclosed quote runs to the end. */
    assert_eq!(hits(&bench, "\"where is parse\""), [first.as_str()]);
    assert_eq!(hits(&bench, "\"is parse_args"), [first.as_str()]);
    assert_eq!(find(&bench, "\"parse_args where\"").total_matches, 0);
    /* No terms, no results. */
    assert_eq!(find(&bench, ""), Results::default());
    assert_eq!(find(&bench, "   "), Results::default());
    /* A cancelled search says so, whether it looks terms up or reads the text. */
    let cancel = CancelToken::new();
    cancel.cancel();
    for terms in [["parse_args"], ["解析"]] {
        let query = Query {
            terms: terms.map(str::to_string).to_vec(),
            filters: Filters::default(),
        };
        assert!(matches!(
            bench.archive().search(&query, Limit::default(), &cancel),
            Err(SearchError::Cancelled)
        ));
    }
    bench.assert_verifies();
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
        let Searched::Results(results) = bench
            .archive()
            .search(&query, Limit::default(), &CancelToken::new())
            .unwrap()
        else {
            panic!("the waits for more");
        };
        let mut found = found(&bench, &results);
        found.sort();
        found
    };
    let mut all = vec![first.clone(), second.clone()];
    all.sort();
    assert_eq!(both(Filters::default()), all);

    /* Pages of the library, and folders: a folder named as a page is reached by folder:. */
    let folder = catalogue.create_folder("Widgets", None).unwrap();
    catalogue.file_by_hand(&ids[1], folder).unwrap();
    let trash = catalogue.create_folder("Trash", None).unwrap();
    catalogue.file_by_hand(&ids[0], trash).unwrap();
    catalogue.set_starred(&ids[0], true).unwrap();
    catalogue.trash(&ids[1]).unwrap();
    for (input, expected) in [
        ("the in:widgets", vec![]),
        ("the folder:WIDGETS", vec![]),
        ("the in:starred", vec![first.clone()]),
        ("the in:unsorted", vec![]),
        ("the in:all", vec![first.clone()]),
        ("the in:trash", vec![second.clone()]),
        ("the folder:trash", vec![first.clone()]),
        ("the from:fixture", vec![first.clone()]),
        (
            &format!("the agent:{}", bench.agent.provider().id()),
            vec![first.clone()],
        ),
    ] {
        let mut found = found(&bench, &find(&bench, input));
        found.sort();
        assert_eq!(found, expected, "{input}");
    }
    catalogue.restore(&ids[1]).unwrap();
    assert_eq!(hits(&bench, "the folder:\"widgets\""), [second.as_str()]);
    /* A token that names nothing is left out, and said to be; one with no value yet is no
    filter and no text. */
    let (query, unresolved) =
        search::query("the from:elsewhere agent:other after:soon", &names(&bench));
    assert_eq!(query.filters, Filters::default());
    assert_eq!(unresolved.len(), 3);
    let parsed = search::parse("the in: model:");
    assert_eq!(
        (parsed.terms, parsed.tokens, parsed.incomplete),
        (vec!["the".to_string()], vec![], vec![Key::In, Key::Model])
    );

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
    bench.assert_verifies();
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
    let inputs = [
        "parse_args",
        "the",
        "解析",
        "main.rs",
        "cafe",
        "defined module",
    ];
    let before: Vec<Results> = inputs.iter().map(|input| find(&bench, input)).collect();
    assert!(before.iter().all(|results| results.total_matches > 0));

    /* Indexing what has not changed changes nothing. */
    let again = bench.index();
    assert_eq!((again.sessions, again.searchable), (0, 0));
    let found: Vec<Results> = inputs.iter().map(|input| find(&bench, input)).collect();
    assert_eq!(found, before);

    /* Emptied, the index finds nothing; built again, what it found. */
    bench.archive().catalogue().drop_index().unwrap();
    for input in inputs {
        assert_eq!(find(&bench, input), Results::default(), "{input}");
    }
    /* The rows are made again as they were, so the text made of them before still is. */
    let rebuilt = bench.index();
    assert_eq!((rebuilt.sessions, rebuilt.searchable), (2, 0));
    assert_eq!(bench.archive().unsearchable().unwrap(), 0);
    let found: Vec<Results> = inputs.iter().map(|input| find(&bench, input)).collect();
    assert_eq!(found, before);
    bench.assert_verifies();
}

fn the_search_text_is_made_again_when_it_is_missing_or_unreadable<A: Agent + Default>() {
    let (mut bench, _) = bench::<A>();
    let before = find(&bench, "parse_args");
    for spoil in [
        |path: &std::path::Path| fs::remove_file(path).unwrap(),
        |path: &std::path::Path| fs::write(path, b"not a database at all").unwrap(),
    ] {
        bench.close();
        let search = bench.dir().join("search.db");
        for suffix in ["-wal", "-shm"] {
            let _ = fs::remove_file(bench.dir().join(format!("search.db{suffix}")));
        }
        spoil(&search);
        bench.reopen();
        /* The library is all there; its text is to be made again, once the search index is
        found missing or unreadable, and then is. */
        assert_eq!(bench.sessions().len(), 2);
        assert_eq!(find(&bench, "parse_args").total_sessions, 0);
        assert_eq!(bench.archive().unsearchable().unwrap(), 2);
        assert_eq!(bench.index().searchable, 2);
        assert_eq!(find(&bench, "parse_args"), before);
    }
    /* The catalogue's backups hold no search text: it is never backed up. */
    let backups = common::files_under(&bench.dir().join("backups"));
    assert!(!backups.is_empty());
    assert!(backups.iter().all(|backup| {
        !fs::read(backup)
            .unwrap()
            .windows(10)
            .any(|run| run == b"parse_args")
    }));
    bench.assert_verifies();
}

/**
 * A search file an older release left, of another layout, is made again by the first index
 * after a start, before anything searches: the archive is searchable once the start index ends.
 */
fn a_search_file_of_an_older_layout_is_made_again_by_the_start_index<A: Agent + Default>() {
    let (mut bench, _) = bench::<A>();
    let before = find(&bench, "parse_args");
    bench.close();
    let search = bench.dir().join("search.db");
    toolbox_store::rusqlite::Connection::open(&search)
        .unwrap()
        .execute(
            "UPDATE migrations SET domain = 'scrollback-search' WHERE domain = 'scrollback-search-2'",
            [],
        )
        .unwrap();
    bench.reopen();
    assert_eq!(
        bench.index().searchable,
        2,
        "the start index makes it all again"
    );
    assert_eq!(bench.archive().unsearchable().unwrap(), 0);
    assert_eq!(find(&bench, "parse_args"), before);
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
    bench.delete_for_good(&id).unwrap();
    assert_eq!(find(&bench, "parse_args in:trash").total_matches, 0);
    assert_eq!(find(&bench, "parse_args in:all").total_matches, 0);
    assert_eq!(hits(&bench, "the"), [second.as_str()]);
    bench.sync();
    assert_eq!(find(&bench, "parse_args in:trash").total_matches, 0);
    bench.assert_verifies();
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
    let Searched::Results(results) = try_find(&bench, "parse_args", one).unwrap() else {
        panic!("parse_args waits for more");
    };
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
    let Searched::Results(none) = try_find(
        &bench,
        "the",
        Limit {
            sessions: 1,
            per_session: 0,
        },
    )
    .unwrap() else {
        panic!("the waits for more");
    };
    assert_eq!((none.sessions.len(), none.total_sessions), (1, 2));
    assert!(none.sessions[0].shown.is_empty());
    bench.assert_verifies();
}

/**
 * A subagent's text is searched under its parent session, and a match in it opens the parent
 * at the call that started the subagent (PLAN §10). A session's title is searched too, and
 * a session whose title holds the words comes before one that only says them; one with no
 * title of its own is not taken for one whose title matches.
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
        (title.kind, title.at, &title.subagent),
        (MatchKind::Title, None, &None)
    );
    let inside = &results.sessions[1].shown[0];
    assert_eq!(inside.kind, MatchKind::Reply);
    let items = Items::of(&bench.agent, &said.key);
    let provider = bench.agent.provider();
    let conversation = provider.parse(&items).conversation.unwrap();
    let Some(At::Block { block, .. }) = inside.at else {
        panic!("{inside:?}");
    };
    assert!(matches!(conversation[block].content, Content::ToolCall(_)));
    /* And where in the subagent's own conversation the text is: its reply. */
    let place = inside
        .subagent
        .as_ref()
        .expect("the text is the subagent's");
    let Some(own) = provider
        .subagent(&items, &place.item)
        .conversation()
        .cloned()
    else {
        panic!("{place:?} is no subagent's conversation");
    };
    assert_eq!(place.turn, 0);
    assert!(
        matches!(&own[place.block].content, Content::Reply { text } if text.contains("frobnicator"))
    );
    bench.assert_verifies();
}

/**
 * A session's text made again replaces only the spans that changed (PLAN §10): what it said
 * before and still says is found, what it no longer says is not, and the index holds nothing
 * else, as FTS5's own check says.
 */
#[test]
fn text_made_again_is_the_text_as_it_is_now_and_nothing_else() {
    let mut tree = ProjectsTree::new();
    let mut session = tree.session("/home/user/code/app", 0);
    session.transcript.prompt("Look for the marmalade.");
    session.transcript.reply("It is in the pantry.");
    let key = session.key.clone();
    let bench = Bench::new(FileAgent::on(tree));
    bench.sync();
    let sessions = |bench: &Bench<FileAgent>, term: &str| find(bench, term).total_sessions;

    /* It grows: what it said is found as before, and what it says now too. */
    session.transcript.prompt("What about the kettle?");
    bench.sync();
    assert_eq!(
        ["marmalade", "pantry", "kettle"].map(|term| sessions(&bench, term)),
        [1; 3]
    );

    /* Its first prompt, which is its title too, is rewritten in place. */
    let content = String::from_utf8(bench.agent.content(&key)).unwrap();
    bench
        .agent
        .overwrite(&key, content.replace("marmalade", "quincejam").as_bytes());
    assert_eq!(bench.sync().new_versions, 1);
    assert_eq!(
        ["marmalade", "quincejam", "pantry", "kettle"].map(|term| sessions(&bench, term)),
        [0, 1, 1, 1]
    );
    bench.assert_search_index_intact();
    /* The two spans that said it otherwise, its title and its first prompt, are marked
    deleted in the index, and counted till it is compacted. */
    let marked: i64 = toolbox_store::rusqlite::Connection::open(bench.dir().join("search.db"))
        .unwrap()
        .query_row("SELECT spans FROM marked", [], |row| row.get(0))
        .unwrap();
    assert_eq!(marked, 2);

    /* Its last prompt is no longer one: the version is held back, and once the owner reads it
    all the same, the prompt's text is gone from search. */
    let content = String::from_utf8(bench.agent.content(&key)).unwrap();
    let last = content.lines().last().unwrap();
    let blank = json!({ "type": "mode", "pad": "" }).to_string();
    let blank = json!({ "type": "mode", "pad": "x".repeat(last.len() - blank.len()) });
    bench
        .agent
        .overwrite(&key, content.replace(last, &blank.to_string()).as_bytes());
    assert_eq!(bench.sync().index.held_back, 1);
    let id = bench.session_id(&key);
    let newest = bench.versions(&key).pop().unwrap();
    bench
        .archive()
        .catalogue()
        .set_reading_version(&id, bench.item(&key).id, Some(newest.id))
        .unwrap();
    bench.index_session(&key);
    assert_eq!(
        ["quincejam", "pantry", "kettle"].map(|term| sessions(&bench, term)),
        [1, 1, 0]
    );
    bench.assert_verifies();
}

/**
 * A session deleted for good leaves none of its text anywhere in the archive's databases or
 * their logs (PLAN §4.5), read byte by byte: not in the search index's text, its entries,
 * those of its text replaced before among them, nor the free space of a page, of the search
 * index or of the catalogue. `TERM` is a prompt of its own of three of a character nothing
 * else holds: the index's one term that starts with it, held whole wherever it is.
 */
#[test]
fn text_deleted_for_good_is_in_no_file() {
    const SAID: &str = "zanzibarquokka";
    const TERM: &str = "龘龘龘";
    let mut tree = ProjectsTree::new();
    let mut doomed = tree.session("/home/user/code/app", 0);
    for turn in 0..40 {
        doomed
            .transcript
            .prompt(&format!("Turn {turn}: where is the {SAID} kept?"));
        doomed
            .transcript
            .reply(&format!("The {SAID} is in drawer {turn}."));
        doomed.transcript.prompt(TERM);
    }
    let mut others: Vec<_> = (0..30)
        .map(|n| tree.session(&format!("/home/user/code/app{n}"), 0))
        .collect();
    for (n, other) in others.iter_mut().enumerate() {
        for turn in 0..10 {
            other
                .transcript
                .prompt(&format!("Session {n}, turn {turn}: a plain question."));
            other
                .transcript
                .reply("A plain answer, long enough to fill a page or two.");
        }
    }
    let key = doomed.key.clone();
    let bench = Bench::new(FileAgent::on(tree));
    bench.sync();
    /* Its text is written again once, as an update's index does, and its old text is marked
    deleted in the search index. */
    let content = String::from_utf8(bench.agent.content(&key)).unwrap();
    bench
        .agent
        .overwrite(&key, content.replace("drawer", "cupboard").as_bytes());
    bench.sync();
    assert_eq!(find(&bench, SAID).total_sessions, 1);

    let files = || {
        ["scrollback.db", "search.db"]
            .into_iter()
            .flat_map(|name| ["", "-wal"].map(|suffix| bench.dir().join(format!("{name}{suffix}"))))
            .collect::<Vec<_>>()
    };
    let holding = |what: &str| -> Vec<String> {
        (files().into_iter())
            .filter(|file| {
                let bytes = fs::read(file).unwrap_or_default();
                bytes.windows(what.len()).any(|it| it == what.as_bytes())
            })
            .map(|file| file.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    };
    /* Before: the files hold it, the logs if not yet the databases. */
    assert!(!holding(TERM).is_empty());

    let id = bench.session_id(&key);
    bench.archive().catalogue().trash(&id).unwrap();
    bench.delete_for_good(&id).unwrap();
    assert_eq!(find(&bench, &format!("{SAID} in:all")).total_matches, 0);
    assert_eq!(holding(SAID), Vec::<String>::new());
    assert_eq!(holding(TERM), Vec::<String>::new());
    assert!(bench.archive().unscrubbed().unwrap().is_none());
    bench.assert_search_index_intact();
    bench.assert_verifies();
}

/**
 * Deleted text that could not be erased from the files yet (a read held the search index's
 * log, here) stays marked as to be, and is said to be; the next start erases it.
 */
#[test]
fn deleted_text_not_yet_erased_is_erased_at_the_next_start() {
    const TERM: &str = "龘龘龘";
    let mut tree = ProjectsTree::new();
    let mut doomed = tree.session("/home/user/code/app", 0);
    doomed.transcript.prompt("Where is it?");
    doomed.transcript.prompt(TERM);
    let mut kept = tree.session("/home/user/code/kept", 0);
    kept.transcript.prompt("And this one?");
    let key = doomed.key.clone();
    let mut bench = Bench::new(FileAgent::on(tree));
    bench.sync();
    let held = |bench: &Bench<FileAgent>| {
        let bytes = fs::read(bench.dir().join("search.db")).unwrap();
        bytes.windows(TERM.len()).any(|it| it == TERM.as_bytes())
    };

    let reader = toolbox_store::rusqlite::Connection::open(bench.dir().join("search.db")).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM doc;")
        .unwrap();
    let id = bench.session_id(&key);
    bench.archive().catalogue().trash(&id).unwrap();
    bench.delete_for_good(&id).unwrap();
    assert!(bench.archive().unscrubbed().unwrap().is_some());
    let marked = |bench: &Bench<FileAgent>| {
        read_catalogue(
            bench.dir(),
            "SELECT count(*) FROM kv WHERE key = 'deleted_to_scrub'",
        )
    };
    assert_eq!(marked(&bench), ["1"]);
    drop(reader);

    /* As a crash would leave it: the next start erases it. */
    bench.close();
    bench.reopen();
    bench.archive().wait_for_removals();
    assert_eq!(marked(&bench), ["0"]);
    assert!(bench.archive().unscrubbed().unwrap().is_none());
    assert!(!held(&bench));
    assert_eq!(find(&bench, "this one").total_sessions, 1);
}

/// What is pasted whole into a prompt is searched as far as its first tens of kilobytes, and
/// the match says so.
#[test]
fn a_long_span_is_searched_from_its_start() {
    let mut tree = ProjectsTree::new();
    let mut session = tree.session("/home/user/code/app", 0);
    let pasted = format!("startmark {} endmark", "lorem ipsum ".repeat(20_000));
    session.transcript.prompt(&pasted);
    let bench = Bench::new(FileAgent::on(tree));
    bench.sync();
    let results = find(&bench, "startmark");
    assert_eq!(results.total_sessions, 1);
    assert!(
        results.sessions[0]
            .shown
            .iter()
            .all(|found| found.cut || found.kind == MatchKind::Title)
    );
    assert_eq!(find(&bench, "endmark").total_sessions, 0);
    bench.assert_verifies();
}

#[test]
fn each_token_of_the_palette_is_parsed_in_one_place() {
    use search::{Parsed, Token};
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
             after:2026-03 before:2027 C:\\Users\\user https://example.com/a in: x:y \
             folder:Trash"
        ),
        Parsed {
            terms: terms(&[
                "parse_args",
                "two words",
                "C:\\Users\\user",
                "https://example.com/a",
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
                token(Key::Folder, "Trash"),
            ],
            incomplete: vec![Key::In],
        }
    );
    assert_eq!(
        search::parse("  \"unclosed quote  ").terms,
        terms(&["unclosed quote  "])
    );
    assert_eq!(search::parse("\"\" a").terms, terms(&["a"]));
    assert_eq!(search::parse("a\u{0}b \u{0}").terms, terms(&["ab"]));
    assert_eq!(search::parse("").terms, terms(&[]));
}

#[test]
fn a_date_is_the_start_of_its_day_month_or_year_in_local_time() {
    use chrono::{Local, TimeZone as _};
    let names = Names {
        folders: &[],
        locations: &[],
        agents: &[],
        calendar: Calendar::Local,
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
    /* Read in UTC, as the app shows dates under a pinned calendar. */
    let utc = Names {
        calendar: Calendar::Utc,
        ..names
    };
    assert_eq!(
        search::query("after:2026-03-05", &utc).0.filters.after,
        Some(
            chrono::Utc
                .with_ymd_and_hms(2026, 3, 5, 0, 0, 0)
                .unwrap()
                .timestamp_millis()
        )
    );
}

#[test]
fn a_quote_inside_quotes_is_written_with_a_backslash() {
    let parsed = search::parse(r#"branch:"say \"hi\"" "a \"b\" c" plain"#);
    assert_eq!(parsed.tokens[0].value, r#"say "hi""#);
    assert_eq!(parsed.terms, [r#"a "b" c"#, "plain"]);
    /* Outside quotes a backslash is text, as in a Windows path. */
    assert_eq!(search::parse(r"C:\Users\user").terms, [r"C:\Users\user"]);
    /* Inside quotes `\"` is a quote, `\\"` a backslash that closes them, so a quoted path can
    end in one; every other backslash is text, so a network path keeps its two. */
    assert_eq!(
        search::parse(r#""\\wsl.localhost\Ubuntu-24.04\home\user\my app" x"#).terms,
        [r"\\wsl.localhost\Ubuntu-24.04\home\user\my app", "x"]
    );
    assert_eq!(
        search::parse(r#""\\server\share\" x"#).terms,
        [r#"\\server\share" x"#],
        "an escaped quote, and nothing closes the quotes"
    );
    assert_eq!(
        search::parse(r#""D:\Projects\app\\" error"#).terms,
        [r"D:\Projects\app\", "error"]
    );
    assert_eq!(
        search::parse(r#""D:\Projects\app\" error"#).terms,
        [r#"D:\Projects\app" error"#],
        "the quote is escaped, and nothing closes the quotes"
    );
    /* Three in a row before a quote: one that is text, then one that closes the quotes. */
    assert_eq!(search::parse(r#""a\\\"b c"#).terms, [r"a\\b", "c"]);
    assert_eq!(
        search::parse(r#"folder:"x\\" y"#).tokens[0].value,
        r"x\",
        "a token's value reads the same"
    );
}

#[test]
fn in_names_a_page_before_a_folder_and_folder_names_a_folder() {
    use sb_core::catalogue::Folder;
    use sb_core::filing::FolderId;
    let folders = [
        Folder {
            id: FolderId(7),
            parent: None,
            name: "Trash".to_string(),
        },
        Folder {
            id: FolderId(8),
            parent: None,
            name: "Widgets".to_string(),
        },
    ];
    let names = Names {
        folders: &folders,
        locations: &[],
        agents: &[],
        calendar: Calendar::Local,
    };
    let shelf = |input: &str| search::query(input, &names).0.filters.shelf;
    assert_eq!(shelf("in:trash"), Shelf::Trash);
    assert_eq!(shelf("folder:trash"), Shelf::Folder(FolderId(7)));
    assert_eq!(shelf("in:widgets"), Shelf::Folder(FolderId(8)));
    assert_eq!(shelf("in:Starred"), Shelf::Starred);
    assert_eq!(shelf("in:unsorted"), Shelf::Unsorted);
    assert_eq!(shelf("x"), Shelf::All);
    /* The last token of a kind is the one that counts. */
    assert_eq!(shelf("in:starred in:unsorted"), Shelf::Unsorted);
}

/**
 * The text check and the index fold alike, in every script: what the index finds of a term is
 * what the text holds. A span that has every run of three characters of a term without the
 * term is no match.
 */
fn a_term_in_any_script_is_found_as_the_index_folds_it<A: Agent + Default>() {
    let (bench, [_, second]) = bench::<A>();
    for input in ["viet", "VIỆT", "οδος", "ΟΔΟΣ", "ᏣᎳᎩ"] {
        assert_eq!(hits(&bench, input), [second.as_str()], "{input}");
    }
    assert_eq!(hits(&bench, "abcab"), [second.as_str()]);
    assert_eq!(find(&bench, "abcabc").total_matches, 0);
    bench.assert_verifies();
}

/// A term longer than the part of a span a snippet shows about it is shown, and marked, whole.
fn a_long_term_is_marked_whole<A: Agent + Default>() {
    let (mut bench, [first, _]) = bench::<A>();
    let identifier = "a_very_long_identifier_name_that_runs_well_past_thirty_two_characters";
    bench.agent.exchange(
        &first,
        &format!(
            "{} {identifier} {}",
            "words before it ".repeat(10),
            "and after it ".repeat(10)
        ),
        "true",
        "",
        "Fine.",
    );
    bench.sync();
    let results = find(&bench, identifier);
    let snippet = &results.sessions[0].shown[0].snippet;
    assert_eq!(&snippet.text[snippet.hits[0].clone()], identifier);
    bench.assert_verifies();
}

/**
 * The second step of an index parses a session and then writes its text, if the catalogue
 * still has the session's row: one the owner deleted for good meanwhile gets none.
 */
fn no_text_is_written_for_a_session_deleted_while_it_is_parsed<A: Agent + Default>() {
    let (mut bench, [first, _]) = bench::<A>();
    let id = bench.session_id(&first);
    bench.archive().catalogue().trash(&id).unwrap();
    /* The search text is lost, so the next index makes it again, parsing as it goes. */
    bench.close();
    fs::remove_file(bench.dir().join("search.db")).unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(bench.dir().join(format!("search.db{suffix}")));
    }
    bench.reopen();
    let archive = bench.archive();
    let shown = archive.catalogue().deletion(&id).unwrap();
    let docs = || {
        let copy = tempfile::tempdir().unwrap();
        for suffix in ["", "-wal"] {
            let name = format!("search.db{suffix}");
            if bench.dir().join(&name).exists() {
                fs::copy(bench.dir().join(&name), copy.path().join(&name)).unwrap();
            }
        }
        toolbox_store::rusqlite::Connection::open(copy.path().join("search.db"))
            .unwrap()
            .query_row("SELECT count(*) FROM doc", [], |row| row.get::<_, i64>(0))
            .unwrap()
    };
    /* The first session is deleted as the second of the two is parsed for its text: after its
    own text is made, or after it is found to have something to make text of. */
    let made = bench.index_with(bench.meddling(common::Meddle::at_parse(2, || {
        archive.delete_for_good(&shown).unwrap();
    })));
    assert_eq!(made.searchable, 1);
    assert_eq!(docs(), 1);
    assert_eq!(find(&bench, "parse_args in:all").total_sessions, 0);
    /* Its files leave files/ in the background (PLAN §4.5); verify counts them as still to be
    removed until then, which `Bench::delete_for_good` waits out and this delete can't. */
    bench.archive().wait_for_removals();
    bench.assert_verifies();
}

/**
 * The search index is derived: one that can't be opened stops nothing but search, which says
 * so; the archive opens, syncs and verifies, and once the index can be opened it is made again.
 */
fn a_search_index_that_cannot_be_opened_stops_nothing_but_search<A: Agent + Default>() {
    let (mut bench, _) = bench::<A>();
    bench.close();
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(bench.dir().join(format!("search.db{suffix}")));
    }
    let blocked = bench.dir().join("search.db");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("in the way"), b"").unwrap();
    bench.reopen();
    assert_eq!(bench.sessions().len(), 2);
    assert!(matches!(
        try_find(&bench, "parse_args", Limit::default()),
        Err(SearchError::Unavailable(_))
    ));
    bench.sync();
    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    assert!(report.damaged.is_empty() && !report.search_rebuilt);

    fs::remove_dir_all(&blocked).unwrap();
    assert_eq!(bench.index().searchable, 2);
    assert_eq!(find(&bench, "parse_args").total_sessions, 1);
    bench.assert_verifies();
}
