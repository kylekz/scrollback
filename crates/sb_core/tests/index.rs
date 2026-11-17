//! Indexing (PLAN §5 step 5, §7.5, §12): what a pass indexes, what a start does, and what
//! becomes of an index that stops or can't parse. The cases that apply to both shapes of
//! source run against both, the file-tree agent and the made-up agent that keeps a database;
//! the rest use what only one of the fixtures can write.

mod common;

use std::fs;
use std::thread;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::catalogue::{Filed, Session, Usage};
use sb_core::filing::NormalPath;
use sb_core::provider::{ItemKind, Provider, SessionItems as _};
use sb_core::sync::{IndexReport, Progress, SyncError, index, index_session};
use sb_core::transcript::{self, Ending, INDEX_VERSION, SessionIndex, Totals};
use sb_test_support::{
    Agent, DbAgent, DbProvider, FileAgent, Items, ProjectsTree, edit_catalogue, read_catalogue,
};
use serde_json::json;

use common::{Bench, Meddle, for_both_agents};

for_both_agents!(
    a_pass_indexes_the_sessions_it_archived,
    a_session_that_grew_is_indexed_again_and_one_that_did_not_is_not,
    a_new_version_of_the_parser_or_of_the_index_is_for_a_start_to_take_to_everything,
    a_start_says_what_it_reads_again_and_what_it_makes_searchable,
    the_index_emptied_and_built_again_is_as_it_was_with_what_the_owner_did,
    an_index_that_stops_is_taken_up_from_the_catalogue,
    a_session_its_parser_stops_on_keeps_what_was_known_of_it,
    a_segment_that_is_not_as_recorded_ends_the_conversation_unreadable,
    a_pass_that_fails_after_it_stored_indexes_what_it_stored,
    a_sync_is_on_record_before_it_indexes,
    two_indexes_at_once_leave_the_index_as_one_does,
    a_classifier_that_changes_reaches_what_is_gone_from_its_source,
    a_pass_before_the_index_of_a_start_moves_what_the_owner_decided,
    what_the_owner_decided_goes_with_a_session_that_is_split_merged_or_gone,
);

/// A bench whose agent has three sessions of different sizes.
fn bench<A: Agent + Default>() -> (Bench<A>, Vec<String>) {
    let mut agent = A::default();
    let keys = vec![
        agent.add_session(8),
        agent.add_session(3),
        agent.add_session(40),
    ];
    (Bench::new(agent), keys)
}

/// What the index derives from the session whose transcript is `key`, read from the source
/// as it is now.
fn derived(agent: &impl Agent, key: &str) -> SessionIndex {
    let items = Items::of(agent, key);
    let provider = agent.provider();
    let mut transcript = provider.parse(&items);
    transcript.read_subagents(provider.as_ref(), &items);
    transcript::index(&transcript)
}

fn usage(totals: &Totals) -> Usage {
    Usage {
        calls: totals.calls.values().sum(),
        tokens: totals.tokens,
    }
}

/// Asserts the catalogue holds of `session` what `index` derived.
fn assert_indexed_as(session: &Session, index: &SessionIndex) {
    assert_eq!(
        (
            &session.title,
            &session.first_prompt,
            &session.project_path,
            &session.branch,
            &session.entrypoint,
            session.started,
            session.ended,
            &session.models,
            session.own,
            session.all,
            session.unread,
        ),
        (
            &index.title,
            &index.first_prompt,
            &index.project_path,
            &index.branch,
            &index.entrypoint,
            index.started,
            index.ended,
            &index.models,
            usage(&index.own),
            usage(&index.all),
            index.unread,
        )
    );
    assert_eq!(session.failure, None);
}

/// An index as a start runs it, stopped once `sessions` sessions are indexed.
fn index_until<A: Agent>(
    bench: &Bench<A>,
    provider: Box<dyn Provider + '_>,
    sessions: usize,
) -> Result<IndexReport, SyncError> {
    let cancel = CancelToken::new();
    index(
        bench.archive(),
        None,
        &[provider],
        &cancel,
        &mut |progress| {
            if matches!(progress, Progress::Indexed { sessions: done, .. } if *done == sessions) {
                cancel.cancel();
            }
        },
    )
}

fn a_pass_indexes_the_sessions_it_archived<A: Agent + Default>() {
    let (bench, keys) = bench::<A>();
    let report = bench.sync();
    assert_eq!((report.index.sessions, report.index.failed.len()), (3, 0));
    assert_eq!(bench.sessions().len(), 3);
    for key in &keys {
        let session = bench.session(key);
        assert_indexed_as(&session, &derived(&bench.agent, key));
        assert!(session.title.is_some(), "{key}");
        assert_eq!(session.id, bench.session_id(key));
        assert_eq!(session.main_item, Some(bench.item(key).id));
        assert_eq!(
            (session.reading_version, session.newest_version),
            (Some(1), Some(1))
        );
        assert_eq!(session.ending, Some(Ending::Whole));
        assert_eq!(
            (session.parser_version, session.index_version),
            (bench.agent.provider().parser_version(), INDEX_VERSION)
        );
        assert_eq!(
            (session.filed, session.starred, session.held_back),
            (Filed::Unsorted, false, false)
        );
    }

    /* Nothing changed: the next pass indexes nothing, and nor does a start. */
    assert_eq!(bench.sync().index, IndexReport::default());
    assert_eq!(bench.index(), IndexReport::default());
}

fn a_session_that_grew_is_indexed_again_and_one_that_did_not_is_not<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    let before = bench.sessions();
    bench.agent.append(&keys[0], 6);

    /* Each index of a session is one `Indexed`: only the session that grew was parsed. */
    let mut indexed = Vec::new();
    let passes = bench
        .try_sync(&CancelToken::new(), &mut |progress| {
            if let Progress::Indexing { sessions, .. } | Progress::Indexed { sessions, .. } =
                progress
            {
                indexed.push(*sessions);
            }
        })
        .unwrap();
    assert_eq!(indexed, [1, 1]);
    assert_eq!(passes[0].index.as_ref().unwrap().sessions, 1);

    assert_indexed_as(&bench.session(&keys[0]), &derived(&bench.agent, &keys[0]));
    for key in &keys[1..] {
        assert!(before.contains(&bench.session(key)), "{key}");
    }

    /* One session indexed on its own, whatever its row says. */
    assert_eq!(bench.index_session(&keys[1]).sessions, 1);
    assert_eq!(bench.sessions().len(), 3);
    assert!(before.contains(&bench.session(&keys[1])));
}

fn a_new_version_of_the_parser_or_of_the_index_is_for_a_start_to_take_to_everything<
    A: Agent + Default,
>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    let before = bench.sessions();
    let parser = bench.agent.provider().parser_version();
    let newer = |bench: &Bench<A>| bench.meddling(Meddle::ParserVersion(parser + 1));

    /* A pass indexes what it changed, and leaves what an older parser made to a start. */
    bench.agent.append(&keys[0], 2);
    assert_eq!(bench.synced_with(newer(&bench)).index.sessions, 1);
    let by_newer = |bench: &Bench<A>| -> Vec<_> {
        keys.iter()
            .map(|key| bench.session(key).parser_version == parser + 1)
            .collect()
    };
    assert_eq!(by_newer(&bench), [true, false, false]);

    /* A start finds every row made by the older parser. */
    assert_eq!(bench.index_with(newer(&bench)).sessions, 2);
    assert_eq!(by_newer(&bench), [true, true, true]);
    assert_eq!(bench.index_with(newer(&bench)).sessions, 0);
    assert_eq!(bench.index().sessions, 3);
    for key in &keys[1..] {
        assert!(before.contains(&bench.session(key)), "{key}");
    }

    /* Rows made by an older index: the same. */
    let before = bench.sessions();
    edit_catalogue(
        bench.dir(),
        "UPDATE session SET index_version = index_version - 1",
    );
    assert_ne!(bench.sessions(), before);
    assert_eq!(bench.sync().index.sessions, 0);
    assert_eq!(bench.index().sessions, 3);
    assert_eq!(bench.sessions(), before);
}

/// Every progress event of an index of a start with `provider`.
fn index_steps<A: Agent>(bench: &Bench<A>, provider: Box<dyn Provider + '_>) -> Vec<Progress> {
    let mut steps = Vec::new();
    index(
        bench.archive(),
        None,
        &[provider],
        &CancelToken::new(),
        &mut |progress| steps.push(*progress),
    )
    .unwrap();
    steps
}

fn a_start_says_what_it_reads_again_and_what_it_makes_searchable<A: Agent + Default>() {
    let (bench, _) = bench::<A>();
    bench.sync();

    /* Rows an older parser made: each is read again, then made searchable again, one at a
    time, the text written with the last. */
    let newer = |bench: &Bench<A>| {
        bench.meddling(Meddle::ParserVersion(
            bench.agent.provider().parser_version() + 1,
        ))
    };
    let steps = index_steps(&bench, newer(&bench));
    assert!(matches!(
        steps[0],
        Progress::Indexing {
            sessions: 3,
            outdated: 3,
            ..
        }
    ));
    assert!(matches!(steps[3], Progress::Indexed { sessions: 3, .. }));
    let searched = |sessions, searchable| Progress::Searched {
        sessions,
        searchable,
    };
    assert_eq!(
        steps[4..],
        [
            Progress::Searching { sessions: 3 },
            searched(1, 0),
            searched(2, 0),
            searched(3, 3),
        ]
    );

    /* Text that is already of its row, though its flag said otherwise, is done at once. */
    edit_catalogue(bench.dir(), "UPDATE session SET searchable = 0");
    let nothing = Progress::Indexing {
        sessions: 0,
        bytes: 0,
        outdated: 0,
    };
    assert_eq!(
        index_steps(&bench, newer(&bench)),
        [nothing, Progress::Searching { sessions: 3 }, searched(3, 0)]
    );

    /* With nothing to do, the second step says so. */
    assert_eq!(
        index_steps(&bench, newer(&bench)),
        [nothing, Progress::Searching { sessions: 0 }]
    );
}

fn the_index_emptied_and_built_again_is_as_it_was_with_what_the_owner_did<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    bench.agent.rewrite(&keys[2]);
    bench.sync();
    let catalogue = bench.archive().catalogue();
    let work = catalogue.create_folder("work", None).unwrap();
    let inner = catalogue.create_folder("inner", Some(work)).unwrap();
    catalogue
        .attach_path(work, "/home/user/code", None)
        .unwrap();
    catalogue
        .file_by_hand(&bench.session_id(&keys[1]), inner)
        .unwrap();
    catalogue
        .set_starred(&bench.session_id(&keys[0]), true)
        .unwrap();
    let older = bench.versions(&keys[2]).remove(0);
    catalogue
        .set_reading_version(
            &bench.session_id(&keys[2]),
            bench.item(&keys[2]).id,
            Some(older.id),
        )
        .unwrap();
    assert_eq!(bench.index().sessions, 1);
    let before = (
        bench.sessions(),
        catalogue.library_counts().unwrap(),
        catalogue.folders().unwrap(),
        catalogue.folder_paths(work).unwrap(),
    );
    assert_eq!(bench.session(&keys[1]).filed, Filed::ByHand(inner));
    assert!(bench.session(&keys[0]).starred);
    assert_eq!(bench.session(&keys[2]).reading_version, Some(1));

    /* Emptied, the library lists nothing, and the owner's tables are as they were. */
    catalogue.drop_index().unwrap();
    assert!(bench.sessions().is_empty());
    assert_eq!(catalogue.library_counts().unwrap().all, 0);
    assert_eq!(catalogue.folders().unwrap(), before.2);
    assert_eq!(catalogue.folder_paths(work).unwrap(), before.3);

    /* A start builds it again from the archive alone: the agent's data is not there. */
    for key in &keys {
        bench.agent.delete(key);
    }
    let catalogue = bench.archive().catalogue();
    assert_eq!(bench.index().sessions, 3);
    assert_eq!(
        (
            bench.sessions(),
            catalogue.library_counts().unwrap(),
            catalogue.folders().unwrap(),
            catalogue.folder_paths(work).unwrap(),
        ),
        before
    );
}

fn an_index_that_stops_is_taken_up_from_the_catalogue<A: Agent + Default>() {
    /* A pass that stops when it has stored, before it indexes anything. */
    let (mut bench, keys) = bench::<A>();
    let cancel = CancelToken::new();
    let stopped = bench.try_sync(&cancel, &mut |progress| {
        if matches!(progress, Progress::Stored { .. }) {
            cancel.cancel();
        }
    });
    assert!(matches!(stopped, Err(SyncError::Cancelled)), "{stopped:?}");
    assert_eq!(bench.items().len(), 3);
    assert!(bench.sessions().is_empty());

    /* A start that stops after one session, and the app closed. */
    let stopped = index_until(&bench, bench.agent.provider(), 1);
    assert!(matches!(stopped, Err(SyncError::Cancelled)), "{stopped:?}");
    assert_eq!(bench.sessions().len(), 1);
    bench.reopen();

    /* The next pass indexes the two that are left, and a start after it none. */
    assert_eq!(bench.sync().index.sessions, 2);
    assert_eq!(bench.index().sessions, 0);
    for key in &keys {
        assert_indexed_as(&bench.session(key), &derived(&bench.agent, key));
    }
}

fn a_session_its_parser_stops_on_keeps_what_was_known_of_it<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    let faulty = |bench: &Bench<A>| bench.meddling(Meddle::FaultyParser(keys[1].clone()));
    let report = bench.synced_with(faulty(&bench));
    assert_eq!(report.index.sessions, 3);
    let [(id, reason)] = &report.index.failed[..] else {
        panic!("{:?}", report.index.failed);
    };
    assert_eq!(*id, bench.session_id(&keys[1]));
    assert!(reason.contains(&keys[1]), "{reason}");

    /* The session is listed, with why there is nothing else to say of it. */
    let failed = bench.session(&keys[1]);
    assert_eq!(failed.failure.as_ref(), Some(reason));
    assert_eq!((&failed.title, failed.main_item), (&None, None));
    for key in [&keys[0], &keys[2]] {
        assert_indexed_as(&bench.session(key), &derived(&bench.agent, key));
    }
    /* Its bytes are archived all the same: the sync is a success. */
    assert_eq!(bench.location().last_error, None);
    bench.assert_mirrors(&keys);

    /* It is not parsed again as it is, and is once it changes. */
    assert_eq!(bench.synced_with(faulty(&bench)).index.sessions, 0);
    let key = keys[1].clone();
    bench.agent.append(&key, 2);
    assert_eq!(bench.sync().index.sessions, 1);
    let parsed = bench.session(&key);
    assert_indexed_as(&parsed, &derived(&bench.agent, &key));

    /* A session that was indexed and is filed, and that the parser then stops on, keeps
    what its row said and where it is: only why is new. */
    let catalogue = bench.archive().catalogue();
    let folder = catalogue.create_folder("kept", None).unwrap();
    catalogue.file_by_hand(&parsed.id, folder).unwrap();
    bench.agent.append(&key, 2);
    let report = bench.synced_with(faulty(&bench));
    assert_eq!((report.index.sessions, report.index.failed.len()), (1, 1));
    let failed = bench.session(&key);
    assert!(failed.failure.is_some());
    assert_eq!(
        Session {
            failure: None,
            ..failed
        },
        Session {
            filed: Filed::ByHand(folder),
            ..parsed
        }
    );
}

fn a_segment_that_is_not_as_recorded_ends_the_conversation_unreadable<A: Agent + Default>() {
    let mut agent = A::default();
    let key = agent.add_session(8);
    let bench = Bench::new(agent);
    bench.sync();
    let [segment] = &bench.segment_files()[..] else {
        panic!("{:?}", bench.segment_files());
    };
    fs::write(segment, "not what was stored").unwrap();
    bench.archive().catalogue().drop_index().unwrap();

    /* What is stored is damaged: that is the session's state, and is indexed as it is. */
    let report = bench.index();
    assert_eq!((report.sessions, report.failed.len()), (1, 0));
    let session = bench.session(&key);
    assert!(
        matches!(session.ending, Some(Ending::Unreadable(_))),
        "{:?}",
        session.ending
    );
    assert_eq!((&session.title, &session.failure), (&None, &None));
    assert!(session.unread);
    assert_eq!(session.main_item, Some(bench.item(&key).id));
    assert_eq!(bench.index().sessions, 0);
}

fn a_pass_that_fails_after_it_stored_indexes_what_it_stored<A: Agent + Default>() {
    let mut agent = A::default();
    for _ in 0..600 {
        agent.add_session(1);
    }
    let bench = Bench::new(agent);
    /* The connection drops once the first batch is stored. */
    let pass = bench.pass(bench.meddling(Meddle::SecondProofsFail(Default::default())));
    assert!(pass.outcome.is_err(), "{:?}", pass.outcome);
    let stored = bench.items().len();
    assert!((1..600).contains(&stored));
    assert!(bench.location().last_error.is_some());

    /* What the pass stored is archived, and so is indexed. */
    assert_eq!(pass.index.unwrap().sessions, stored);
    assert_eq!(bench.sessions().len(), stored);
}

fn a_sync_is_on_record_before_it_indexes<A: Agent + Default>() {
    let (bench, _) = bench::<A>();
    let cancel = CancelToken::new();
    let stopped = bench.try_sync(&cancel, &mut |progress| {
        if matches!(progress, Progress::Indexing { .. }) {
            cancel.cancel();
        }
    });
    assert!(matches!(stopped, Err(SyncError::Cancelled)), "{stopped:?}");
    /* The backup was whole: the location says so, though nothing is indexed yet. */
    let location = bench.location();
    assert!(location.last_success.is_some());
    assert_eq!(location.last_error, None);
    assert!(bench.sessions().is_empty());
    assert_eq!(bench.index().sessions, 3);
}

fn two_indexes_at_once_leave_the_index_as_one_does<A: Agent + Default>() {
    let (bench, _) = bench::<A>();
    bench.sync();
    let before = bench.sessions();
    bench.archive().catalogue().drop_index().unwrap();

    let archive = bench.archive();
    let providers = [bench.agent.provider(), bench.agent.provider()];
    let indexed: Vec<usize> = thread::scope(|scope| {
        let running: Vec<_> = providers
            .into_iter()
            .map(|provider| {
                scope.spawn(move || {
                    index(archive, None, &[provider], &CancelToken::new(), &mut |_| {})
                        .unwrap()
                        .sessions
                })
            })
            .collect();
        running
            .into_iter()
            .map(|index| index.join().unwrap())
            .collect()
    });
    assert!(indexed.iter().sum::<usize>() >= 3, "{indexed:?}");
    assert_eq!(bench.sessions(), before);
    assert_eq!(bench.index().sessions, 0);
}

fn a_classifier_that_changes_reaches_what_is_gone_from_its_source<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    let before = bench.sessions();
    let starred = bench.session_id(&keys[0]);
    bench
        .archive()
        .catalogue()
        .set_starred(&starred, true)
        .unwrap();
    /* One session is gone from its source: no scan will ever classify its items again. */
    bench.agent.delete(&keys[1]);
    assert_eq!(bench.sync().gone, 1);

    /* A newer parser gives every session another id. A start says so of everything
    archived: each session is listed once, under the new id, and what the owner decided
    about it went with it. */
    let moved = |bench: &Bench<A>| {
        bench.meddling(Meddle::Classifier(|_, (kind, session)| {
            (kind, session.map(|session| format!("moved/{session}")))
        }))
    };
    assert_eq!(bench.index_with(moved(&bench)).sessions, 3);
    let after = bench.sessions();
    assert_eq!(after.len(), 3);
    for (after, before) in after.iter().zip(&before) {
        assert_eq!(after.id.key, format!("moved/{}", before.id.key));
        assert_eq!(after.title, before.title);
        assert_eq!(after.starred, before.id == starred);
    }
    assert_eq!(bench.index_with(moved(&bench)).sessions, 0);

    /* And back, by the parser it was. */
    assert_eq!(bench.index().sessions, 3);
    let back: Vec<_> = bench
        .sessions()
        .into_iter()
        .map(|session| session.id)
        .collect();
    assert_eq!(
        back,
        before
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>()
    );
    assert!(bench.session(&keys[0]).starred);
}

fn a_pass_before_the_index_of_a_start_moves_what_the_owner_decided<A: Agent + Default>() {
    let (bench, keys) = bench::<A>();
    bench.sync();
    let catalogue = bench.archive().catalogue();
    let folder = catalogue.create_folder("kept", None).unwrap();
    let old = bench.session_id(&keys[0]);
    catalogue.set_starred(&old, true).unwrap();
    catalogue.file_by_hand(&old, folder).unwrap();

    /* The app is upgraded to a parser that gives every session another id, and a sync of
    the location runs before the index of the start does: its scan says of each item what
    the classifier would. */
    let newer = |bench: &Bench<A>| {
        bench.meddling(Meddle::Classifier(|_, (kind, session)| {
            (kind, session.map(|session| format!("moved/{session}")))
        }))
    };
    assert_eq!(bench.synced_with(newer(&bench)).index.sessions, 3);
    assert_eq!(bench.index_with(newer(&bench)).sessions, 0);
    let sessions = bench.sessions();
    assert_eq!(sessions.len(), 3);
    for session in &sessions {
        let its = session.id.key == format!("moved/{}", old.key);
        assert_eq!(
            (session.starred, session.filed),
            (
                its,
                if its {
                    Filed::ByHand(folder)
                } else {
                    Filed::Unsorted
                }
            ),
            "{}",
            session.id.key
        );
    }
    /* Nothing is left under the id no session has any more. */
    assert_eq!(
        read_catalogue(bench.dir(), "SELECT session FROM session_state"),
        [format!("moved/{}", old.key)]
    );
}

fn what_the_owner_decided_goes_with_a_session_that_is_split_merged_or_gone<A: Agent + Default>() {
    let (bench, mut keys) = bench::<A>();
    keys.sort();
    /* An earlier parser took every transcript for one session's. */
    let one = |bench: &Bench<A>| {
        bench.meddling(Meddle::Classifier(|_, (kind, _)| {
            (kind, Some("one".to_string()))
        }))
    };
    bench.synced_with(one(&bench));
    let [merged] = &bench.sessions()[..] else {
        panic!("{:?}", bench.sessions());
    };
    let catalogue = bench.archive().catalogue();
    let folder = catalogue.create_folder("kept", None).unwrap();
    catalogue.set_starred(&merged.id, true).unwrap();
    catalogue.file_by_hand(&merged.id, folder).unwrap();

    /* The parser that tells them apart: one session becomes three, each with a transcript
    of its own. What was decided goes with the first of them, and nothing is left behind. */
    assert_eq!(bench.index().sessions, 3);
    let starred = |bench: &Bench<A>| -> Vec<_> {
        keys.iter()
            .map(|key| {
                let session = bench.session(key);
                (session.starred, session.filed)
            })
            .collect()
    };
    assert_eq!(
        starred(&bench),
        [
            (true, Filed::ByHand(folder)),
            (false, Filed::Unsorted),
            (false, Filed::Unsorted)
        ]
    );
    let decided =
        |bench: &Bench<A>| read_catalogue(bench.dir(), "SELECT session FROM session_state");
    assert_eq!(decided(&bench), [bench.session_id(&keys[0]).key]);

    /* Three become one again: the one keeps what was decided about the first to arrive,
    and what was decided about the others is dropped with them. */
    catalogue
        .set_starred(&bench.session_id(&keys[2]), true)
        .unwrap();
    assert_eq!(bench.index_with(one(&bench)).sessions, 1);
    let [merged] = &bench.sessions()[..] else {
        panic!("{:?}", bench.sessions());
    };
    assert_eq!(
        (merged.starred, merged.filed),
        (true, Filed::ByHand(folder))
    );
    assert_eq!(decided(&bench), ["one"]);

    /* Its transcripts are no session's at all: nothing is decided about nothing. */
    let none = bench.meddling(Meddle::SameParser(|_, (kind, _)| (kind, None)));
    bench.synced_with(none);
    assert!(bench.sessions().is_empty());
    assert_eq!(decided(&bench), Vec::<String>::new());
}

/// A bench on a file tree with nothing in it yet.
fn files() -> Bench<FileAgent> {
    Bench::new(FileAgent::on(ProjectsTree::new()))
}

#[test]
fn a_session_kept_as_files_is_indexed_with_its_title_path_and_counts() {
    let mut bench = files();
    let tree = bench.agent.tree_mut();
    let mut titled = tree.session("/home/user/code/app/crates/core", 0);
    titled.transcript.prompt("Fix   the build,\nplease.");
    let call = titled
        .transcript
        .call("Bash", json!({ "command": "cargo build" }));
    titled.transcript.result(&call, json!("ok"));
    titled.transcript.reply_by("model-2", "Fixed.");
    titled.transcript.title("Build fix");
    let subagent = titled.subagent(8);
    let mut untitled = tree.session(r"D:\Projects\App", 0);
    untitled.transcript.prompt("What does this do?");
    untitled.transcript.reply("It syncs.");
    /* A title that says nothing is none, and one of any length is kept by its start. */
    let mut blank = tree.session("/mnt/d/Projects/App/docs", 0);
    blank.transcript.prompt("Write the notes.");
    blank.transcript.title("   ");
    let mut long = tree.session("/srv/long", 0);
    long.transcript.prompt("Name it.");
    long.transcript.title(&"long title ".repeat(1000));
    /* A session's sidecar alone is no session. */
    tree.project_file(
        "/home/user/code/app",
        "00000000-0000-4000-8000-00000000abcd.released.json",
        b"{}",
    );
    let catalogue = bench.archive().catalogue();
    let folder = catalogue.create_folder("app", None).unwrap();
    catalogue
        .attach_path(folder, "/home/user/code/app/", None)
        .unwrap();

    let report = bench.sync();
    assert_eq!((report.index.sessions, report.index.filed), (4, 1));
    let session = bench.session(&titled.key);
    assert_eq!(session.id.key, titled.session_key());
    assert_eq!(
        (
            session.title.as_deref(),
            session.first_prompt.as_deref(),
            session.project_path.as_deref(),
            session.project.as_ref().map(NormalPath::as_str),
            session.branch.as_deref(),
            session.entrypoint.as_deref(),
        ),
        (
            Some("Build fix"),
            Some("Fix the build, please."),
            Some("/home/user/code/app/crates/core"),
            Some("/home/user/code/app/crates/core"),
            Some("main"),
            Some("cli"),
        )
    );
    assert_eq!(session.models, ["model-1", "model-2"]);
    /* Its own calls are the shell's and the one that started the subagent; the subagent's
    are counted with them in all. */
    assert_eq!(session.own.calls, 2);
    assert_eq!(session.all.calls, 4);
    assert!(session.all.tokens.output > session.own.tokens.output);
    assert!(session.started < session.ended);
    assert_eq!(session.filed, Filed::ByRule(folder));
    assert_indexed_as(&session, &derived(&bench.agent, &titled.key));

    /* Without a title of its own, a session is named by how its first prompt starts. */
    let session = bench.session(&untitled.key);
    assert_eq!(session.title.as_deref(), Some("What does this do?"));
    assert_eq!(
        session.project.as_ref().map(NormalPath::as_str),
        Some("d:/projects/app")
    );
    assert_eq!(session.filed, Filed::Unsorted);
    let session = bench.session(&blank.key);
    assert_eq!(session.title.as_deref(), Some("Write the notes."));
    /* A path under the drive mount is the drive's. */
    assert_eq!(
        session.project.as_ref().map(NormalPath::as_str),
        Some("d:/projects/app/docs")
    );
    let title = bench.session(&long.key).title.unwrap();
    assert!(title.starts_with("long title long title"));
    assert!(title.chars().count() <= 300, "{}", title.len());

    /* A subagent's transcript that grows is its session's change. */
    bench.agent.append_raw(&subagent, "{\"type\":\"mode\"}\n");
    assert_eq!(bench.sync().index.sessions, 1);
}

#[test]
fn a_session_kept_in_a_database_is_indexed_with_its_title_path_and_counts() {
    let mut bench = Bench::new(DbAgent::new());
    let key = bench.agent.add_session(0);
    bench.agent.say(&key, "start", r"D:\Projects\App\crates");
    bench.agent.say(&key, "user", "List the files.");
    bench
        .agent
        .call(&key, "exec", json!({ "cmd": "ls" }), "a.txt", true);
    bench
        .agent
        .call(&key, "open", json!({ "file": "a.txt" }), "text", true);
    bench.agent.say(&key, "assistant", "One file.");
    let catalogue = bench.archive().catalogue();
    let folder = catalogue.create_folder("app", None).unwrap();
    catalogue
        .attach_path(folder, "d:/projects/app", None)
        .unwrap();

    let report = bench.sync();
    assert_eq!((report.index.sessions, report.index.filed), (1, 1));
    let session = bench.session(&key);
    assert_eq!(
        (
            session.title.as_deref(),
            session.project_path.as_deref(),
            session.project.as_ref().map(NormalPath::as_str),
            session.own.calls,
            session.all.calls,
        ),
        (
            Some("List the files."),
            Some(r"D:\Projects\App\crates"),
            Some("d:/projects/app/crates"),
            2,
            2,
        )
    );
    assert_eq!(session.id.provider, "db-agent");
    assert_eq!(session.filed, Filed::ByRule(folder));
    assert_indexed_as(&session, &derived(&bench.agent, &key));
}

#[test]
fn a_record_the_database_agent_wrote_badly_is_exported_and_kept() {
    let mut bench = Bench::new(DbAgent::new());
    let key = bench.agent.add_session(2);
    bench.agent.call_raw(&key, "this is not JSON");
    let report = bench.sync();
    assert_eq!(
        (
            report.new_items,
            report.index.sessions,
            report.index.failed.len()
        ),
        (1, 1, 0)
    );
    bench.assert_mirrors(std::slice::from_ref(&key));
    assert_indexed_as(&bench.session(&key), &derived(&bench.agent, &key));
}

#[test]
fn a_session_without_its_transcript_is_indexed_from_its_subagents() {
    let mut bench = files();
    let mut session = bench.agent.tree_mut().session("/home/user/code/app", 4);
    session.subagent(8);
    fs::remove_file(bench.agent.tree().path(&session.key)).unwrap();

    assert_eq!(bench.sync().index.sessions, 1);
    let [indexed] = &bench.sessions()[..] else {
        panic!("{:?}", bench.sessions());
    };
    assert_eq!(indexed.id.key, session.session_key());
    assert_eq!(
        (indexed.main_item, &indexed.ending, &indexed.failure),
        (None, &None, &None)
    );
    assert_eq!(
        (indexed.reading_version, indexed.newest_version),
        (None, None)
    );
    assert_eq!((indexed.own.calls, indexed.all.calls), (0, 2));
    assert!(indexed.started.is_some());
}

#[test]
fn the_same_session_id_under_two_projects_is_two_sessions() {
    let mut bench = files();
    let mut first = bench.agent.tree_mut().session("/home/user/code/app", 0);
    first.transcript.prompt("In the first folder.");
    let copy = bench.agent.tree().project_file(
        "/home/user/code/other",
        &format!("{}.jsonl", first.id),
        format!(
            "{}\n",
            json!({ "type": "user", "uuid": "u-1", "sessionId": first.id,
                    "cwd": "/home/user/code/other",
                    "message": { "role": "user", "content": "In the second folder." } })
        )
        .as_bytes(),
    );

    assert_eq!(bench.sync().index.sessions, 2);
    let titles: Vec<_> = [&first.key, &copy]
        .map(|key| bench.session(key).title.unwrap())
        .into();
    assert_eq!(titles, ["In the first folder.", "In the second folder."]);
    assert_ne!(bench.session_id(&first.key), bench.session_id(&copy));
}

#[test]
fn a_subagent_that_cannot_be_read_shows_on_its_sessions_row() {
    let mut bench = files();
    let mut session = bench.agent.tree_mut().session("/home/user/code/app", 4);
    let subagent = session.subagent(8);
    bench.sync();
    assert!(!bench.session(&session.key).unread);
    let [segment] = &bench.segment_files_of(&subagent)[..] else {
        panic!("{:?}", bench.segment_files_of(&subagent));
    };
    fs::write(segment, "not what was stored").unwrap();

    assert_eq!(bench.index_session(&session.key).failed.len(), 0);
    let row = bench.session(&session.key);
    /* The session's own conversation is whole; a part of the session is not. */
    assert_eq!((&row.ending, row.unread), (&Some(Ending::Whole), true));
    assert_eq!(row.all.calls, row.own.calls);
}

#[test]
fn a_sidecar_that_was_rewritten_shorter_is_read_as_it_is_now() {
    let mut bench = files();
    let mut session = bench.agent.tree_mut().session("/home/user/code/app", 0);
    session.transcript.prompt("Start.");
    let title = session.title("A title that goes on for a good while");
    bench.sync();
    assert_eq!(
        bench.session(&session.key).title.as_deref(),
        Some("A title that goes on for a good while")
    );

    /* Shorter is no loss in what is not a transcript: the newest is what the agent meant. */
    session.title("Short");
    let report = bench.sync();
    assert_eq!((report.new_versions, report.held_back), (1, 0));
    assert_eq!(bench.versions(&title)[1].held_back, None);
    assert_eq!(bench.item(&title).kind, ItemKind::Sidecar);
    let row = bench.session(&session.key);
    assert_eq!(
        (row.title.as_deref(), row.held_back),
        (Some("Short"), false)
    );
}

#[test]
fn stale_sessions_are_indexed_newest_first_after_those_never_indexed() {
    let (mut bench, keys) = bench::<FileAgent>();
    bench.sync();
    /* The longest session is the one that did something last. */
    assert_eq!(bench.sessions()[0].id, bench.session_id(&keys[2]));
    let parser = bench.agent.provider().parser_version();
    let newer = |bench: &Bench<FileAgent>| bench.meddling(Meddle::ParserVersion(parser + 1));
    index_until(&bench, newer(&bench), 1).unwrap_err();
    let by_newer = |bench: &Bench<FileAgent>| -> Vec<_> {
        keys.iter()
            .map(|key| bench.session(key).parser_version == parser + 1)
            .collect()
    };
    assert_eq!(by_newer(&bench), [false, false, true]);

    /* A session with no row at all comes before every one that has an older row. */
    let added = bench.agent.add_session(2);
    bench.sync();
    edit_catalogue(
        bench.dir(),
        &format!(
            "DELETE FROM session WHERE session = '{}'",
            bench.session_id(&added).key
        ),
    );
    index_until(&bench, newer(&bench), 1).unwrap_err();
    assert_eq!(bench.session(&added).parser_version, parser + 1);
    assert_eq!(by_newer(&bench), [false, false, true]);
    assert_eq!(bench.index_with(newer(&bench)).sessions, 2);
}

#[cfg(unix)]
#[test]
fn a_read_the_system_fails_is_tried_again_and_changes_nothing() {
    use std::os::unix::fs::PermissionsExt as _;

    let mut bench = files();
    let mut session = bench.agent.tree_mut().session("/home/user/code/app", 8);
    let key = session.key.clone();
    let catalogue = bench.archive().catalogue();
    let folder = catalogue.create_folder("app", None).unwrap();
    catalogue
        .attach_path(folder, "/home/user/code/app", None)
        .unwrap();
    bench.sync();
    session.append(4);
    bench.sync();
    let before = bench.session(&key);
    assert_eq!(before.filed, Filed::ByRule(folder));

    /* The newest segment can't be opened just now: another program has it, say. */
    let segments = bench.segment_files_of(&key);
    let locked = segments.last().unwrap();
    fs::set_permissions(locked, fs::Permissions::from_mode(0o000)).unwrap();
    let report = bench.index_session(&key);
    assert_eq!((report.sessions, report.failed.len()), (1, 1));
    /* Nothing the row said is lost, and the session stays filed. */
    let failed = bench.session(&key);
    assert!(failed.failure.is_some());
    assert_eq!(
        Session {
            failure: None,
            ..failed
        },
        before
    );
    /* It is tried again by the index of every start until it can be read, and not by every
    pass meanwhile. */
    assert_eq!(bench.index().failed.len(), 1);
    assert_eq!(bench.sync().index.sessions, 0);
    assert_eq!(bench.index().failed.len(), 1);
    fs::set_permissions(locked, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(bench.sync().index.sessions, 0);
    assert_eq!(bench.index().sessions, 1);
    assert_eq!(bench.session(&key), before);
    assert_eq!(bench.index().sessions, 0);
}

#[test]
fn what_an_item_is_alone_is_part_of_what_a_row_is_made_from() {
    let mut bench = files();
    let mut session = bench.agent.tree_mut().session("/home/user/code/app", 4);
    let title = session.title("A title");
    session.transcript.prompt("Go on.");
    bench.sync();
    let id = bench.session_id(&session.key);
    bench.archive().catalogue().set_starred(&id, true).unwrap();
    assert_eq!(bench.sync().index.sessions, 0);

    /* The provider says of the title file that it is something else: nothing of it or of
    any other item changed, and the session is indexed again. */
    let report = bench.synced_with(bench.meddling(Meddle::SameParser(
        |_, (kind, session)| match kind {
            ItemKind::Sidecar => (ItemKind::ToolResult, session),
            _ => (kind, session),
        },
    )));
    assert_eq!((report.unchanged, report.index.sessions), (2, 1));
    assert_eq!(bench.item(&title).kind, ItemKind::ToolResult);

    /* It says the file is another session's: that takes nothing of what the owner decided
    about this one, whose own transcript stays. */
    let report = bench.synced_with(bench.meddling(Meddle::SameParser(
        |_, (kind, session)| match kind {
            ItemKind::Sidecar => (kind, Some("another".to_string())),
            _ => (kind, session),
        },
    )));
    assert_eq!(report.index.sessions, 1);
    assert_eq!(bench.item(&title).session.as_deref(), Some("another"));
    assert!(bench.session(&session.key).starred);
    assert_eq!(bench.sessions().len(), 1);
    assert_eq!(
        read_catalogue(bench.dir(), "SELECT session FROM session_state"),
        std::slice::from_ref(&id.key)
    );

    /* A session is indexed by its own agent's provider. */
    assert!(matches!(
        index_session(bench.archive(), &DbProvider::default(), &id),
        Err(SyncError::NotItsProvider { .. })
    ));
}

#[test]
fn a_transcript_that_is_one_no_more_is_read_at_its_newest_version() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(12);
    let mut bench = Bench::new(agent);
    bench.sync();
    let id = bench.session_id(&key);
    bench.agent.truncate(&key, 6);
    assert_eq!(bench.sync().held_back, 1);
    let versions = bench.versions(&key);
    let read = |bench: &Bench<FileAgent>| {
        let items = bench.archive().session(&id).unwrap();
        items.list()[0].length
    };
    assert_eq!(read(&bench), versions[0].length);

    /* The provider no longer takes the file for a transcript. A version of anything else is
    never a loss: what is read of it is the newest, held back or not when it was one. */
    bench.synced_with(bench.meddling(Meddle::SameParser(|_, (_, session)| {
        (ItemKind::Sidecar, session)
    })));
    assert_eq!(bench.item(&key).kind, ItemKind::Sidecar);
    assert_eq!(read(&bench), versions[1].length);
    /* And a session with no transcript is none. */
    assert!(bench.sessions().is_empty());
}
