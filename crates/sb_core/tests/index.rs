//! The index step of a pass, and indexing at a start (PLAN §5 step 5, §7.5, §12): the cases
//! that apply to both shapes of source run against both, the file-tree agent and the made-up
//! agent that keeps a database; the rest use what only one of the fixtures can write.

mod common;

use std::fs;
use std::io::Read as _;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::catalogue::{CatalogueError, Filed, Session, Usage};
use sb_core::filing::NormalPath;
use sb_core::provider::{ItemKind, Provider, SessionItems};
use sb_core::sync::{IndexReport, Progress, SyncError, index};
use sb_core::transcript::{self, Ending, INDEX_VERSION, SessionIndex, Totals};
use sb_test_support::{Agent, DbAgent, FileAgent, Items, ProjectsTree, edit_catalogue};
use serde_json::json;

use common::{Bench, Meddle, for_both_agents};

for_both_agents!(
    a_pass_indexes_the_sessions_it_archived,
    a_session_that_grew_is_indexed_again_and_one_that_did_not_is_not,
    a_new_version_of_the_parser_or_of_the_index_indexes_everything_again,
    the_index_emptied_and_built_again_is_as_it_was_with_what_the_owner_did,
    an_index_that_stops_is_taken_up_from_the_catalogue,
    a_session_its_parser_stops_on_is_recorded_and_the_others_are_indexed,
    the_owner_chooses_the_version_that_is_read,
    a_segment_that_is_not_as_recorded_ends_the_conversation_unreadable,
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
    assert_eq!(passes[0].outcome.as_ref().unwrap().index.sessions, 1);

    assert_indexed_as(&bench.session(&keys[0]), &derived(&bench.agent, &keys[0]));
    for key in &keys[1..] {
        assert!(before.contains(&bench.session(key)), "{key}");
    }
}

fn a_new_version_of_the_parser_or_of_the_index_indexes_everything_again<A: Agent + Default>() {
    let (bench, keys) = bench::<A>();
    bench.sync();
    let before = bench.sessions();
    let parser = bench.agent.provider().parser_version();

    /* A newer parser: a start finds every row made by the older one. */
    let newer = || bench.meddling(Meddle::ParserVersion(parser + 1));
    assert_eq!(bench.index_with(newer()).sessions, 3);
    for key in &keys {
        assert_eq!(bench.session(key).parser_version, parser + 1);
    }
    assert_eq!(bench.index_with(newer()).sessions, 0);
    assert_eq!(bench.index().sessions, 3);
    assert_eq!(bench.sessions(), before);

    /* Rows made by an older index: the same. */
    edit_catalogue(
        bench.dir(),
        "UPDATE session SET index_version = index_version - 1",
    );
    assert_ne!(bench.sessions(), before);
    assert_eq!(bench.index().sessions, 3);
    assert_eq!(bench.sessions(), before);
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
        .set_reading_version(&bench.session_id(&keys[2]), Some(older.id))
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

    /* The next start indexes the two that are left, and the pass after it none. */
    assert_eq!(bench.index().sessions, 2);
    assert_eq!(bench.sync().index.sessions, 0);
    for key in &keys {
        assert_indexed_as(&bench.session(key), &derived(&bench.agent, key));
    }
}

fn a_session_its_parser_stops_on_is_recorded_and_the_others_are_indexed<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    let faulty = || bench.meddling(Meddle::FaultyParser(keys[1].clone()));
    let report = bench.pass_with(faulty()).unwrap();
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
    assert_eq!(bench.pass_with(faulty()).unwrap().index.sessions, 0);
    let key = keys[1].clone();
    bench.agent.append(&key, 2);
    assert_eq!(bench.sync().index.sessions, 1);
    assert_indexed_as(&bench.session(&key), &derived(&bench.agent, &key));
}

fn the_owner_chooses_the_version_that_is_read<A: Agent + Default>() {
    let (mut bench, keys) = bench::<A>();
    bench.sync();
    let key = keys[0].clone();
    let first = (derived(&bench.agent, &key), bench.agent.content(&key));
    bench.agent.rewrite(&key);
    assert_eq!(bench.sync().index.sessions, 1);
    let second = (derived(&bench.agent, &key), bench.agent.content(&key));
    let versions = bench.versions(&key);
    let id = bench.session_id(&key);
    let catalogue = bench.archive().catalogue();
    /* What a parser is given of the session's transcript. */
    let read = || {
        let items = bench.archive().session(&id).unwrap();
        let mut content = Vec::new();
        items
            .open(&key, 0)
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        let listed = items.list().iter().find(|item| item.key == key).unwrap();
        assert_eq!(
            (listed.kind, listed.length),
            (ItemKind::Transcript, content.len() as u64)
        );
        content
    };

    /* The newest version is read, since it is not held back. */
    assert_eq!((versions.len(), versions[1].held_back), (2, false));
    assert!(read() == second.1);
    assert_indexed_as(&bench.session(&key), &second.0);

    /* The owner chooses the older one: it is read at once, and indexed by the next index. */
    catalogue
        .set_reading_version(&id, Some(versions[0].id))
        .unwrap();
    assert!(read() == first.1);
    assert_indexed_as(&bench.session(&key), &second.0);
    assert_eq!(bench.index().sessions, 1);
    assert_indexed_as(&bench.session(&key), &first.0);
    assert_eq!(bench.index().sessions, 0);

    /* A version of another session's item is not this session's to read. */
    let other = bench.versions(&keys[1]).remove(0).id;
    assert!(matches!(
        catalogue.set_reading_version(&id, Some(other)),
        Err(CatalogueError::NotOfSession(version)) if version == other
    ));
    assert!(read() == first.1);

    /* Back to the rule. */
    catalogue.set_reading_version(&id, None).unwrap();
    assert!(read() == second.1);
    assert_eq!(bench.index().sessions, 1);
    assert_indexed_as(&bench.session(&key), &second.0);
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

    let report = bench.index();
    assert_eq!((report.sessions, report.failed.len()), (1, 0));
    let session = bench.session(&key);
    assert!(
        matches!(session.ending, Some(Ending::Unreadable(_))),
        "{:?}",
        session.ending
    );
    assert_eq!((&session.title, &session.failure), (&None, &None));
    assert_eq!(session.main_item, Some(bench.item(&key).id));
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
    assert_eq!((report.index.sessions, report.index.filed), (2, 1));
    let session = bench.session(&titled.key);
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
fn a_session_without_its_transcript_is_indexed_from_its_subagents() {
    let mut bench = files();
    let mut session = bench.agent.tree_mut().session("/home/user/code/app", 4);
    session.subagent(8);
    fs::remove_file(bench.agent.tree().path(&session.key)).unwrap();

    assert_eq!(bench.sync().index.sessions, 1);
    let [indexed] = &bench.sessions()[..] else {
        panic!("{:?}", bench.sessions());
    };
    assert_eq!(indexed.id.key, session.id);
    assert_eq!(
        (indexed.main_item, &indexed.ending, &indexed.failure),
        (None, &None, &None)
    );
    assert_eq!((indexed.own.calls, indexed.all.calls), (0, 2));
    assert!(indexed.started.is_some());
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
    let cancel = CancelToken::new();
    let added = bench.agent.add_session(2);
    let stopped = bench.try_sync_with(newer(&bench), &cancel, &mut |progress| {
        if matches!(progress, Progress::Indexed { .. }) {
            cancel.cancel();
        }
    });
    assert!(matches!(stopped, Err(SyncError::Cancelled)), "{stopped:?}");
    assert_eq!(bench.session(&added).parser_version, parser + 1);
    assert_eq!(by_newer(&bench), [false, false, true]);
    assert_eq!(bench.index_with(newer(&bench)).sessions, 2);
}

#[test]
fn a_version_that_no_longer_parses_is_held_back_however_long_it_grows() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(12);
    let mut bench = Bench::new(agent);
    bench.sync();
    let before = bench.session(&key);
    let whole = bench.agent.content(&key);

    /* The file is replaced by something longer that holds no conversation: no loss by its
    length, and one by what it is. */
    let garbage = "this is not a transcript\n".repeat(whole.len() / 20);
    bench.agent.overwrite(&key, garbage.as_bytes());
    let report = bench.sync();
    assert_eq!(
        (
            report.new_versions,
            report.held_back,
            report.index.held_back
        ),
        (1, 0, 1)
    );
    let held = |bench: &Bench<FileAgent>| {
        let newest = bench.versions(&key).remove(1);
        (newest.held_back, newest.held_by_parse)
    };
    assert_eq!(held(&bench), (true, true));
    /* The copy being read is still the one from before, and the session says so. */
    assert_eq!(
        bench.session(&key),
        Session {
            held_back: true,
            ..before.clone()
        }
    );
    let read = |bench: &Bench<FileAgent>| {
        let mut content = Vec::new();
        bench
            .archive()
            .session(&bench.session_id(&key))
            .unwrap()
            .open(&key, 0)
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        content
    };
    assert!(read(&bench) == whole);

    /* It grows, which would end a hold for its length; this one stays. */
    bench.agent.append_raw(&key, &garbage);
    let report = bench.sync();
    assert_eq!((report.appended, report.index.held_back), (1, 0));
    assert_eq!(held(&bench), (true, true));
    assert!(read(&bench) == whole);
    assert_eq!(bench.sync().index.sessions, 0);

    /* The owner can read it all the same. */
    let id = bench.session_id(&key);
    let newest = bench.versions(&key).remove(1);
    let catalogue = bench.archive().catalogue();
    catalogue.set_reading_version(&id, Some(newest.id)).unwrap();
    assert_eq!(bench.index().sessions, 1);
    let chosen = bench.session(&key);
    assert_eq!((&chosen.title, chosen.held_back), (&None, false));
    assert_eq!(held(&bench), (true, true));
    catalogue.set_reading_version(&id, None).unwrap();
    assert_eq!(bench.index().sessions, 1);
    assert_eq!(bench.session(&key).title, before.title);

    /* The agent goes on in the file, until it shows as much as the copy being read: the
    version is no loss any more, and is the one read. */
    bench.agent.session(&key).append(40);
    let report = bench.sync();
    assert_eq!((report.appended, report.index.sessions), (1, 1));
    assert_eq!(held(&bench), (false, false));
    assert!(read(&bench) == bench.agent.content(&key));
    let after = bench.session(&key);
    assert!(!after.held_back);
    assert_indexed_as(&after, &derived(&bench.agent, &key));
}

#[test]
fn a_database_session_that_lost_records_is_held_back_though_it_is_longer() {
    let mut bench = Bench::new(DbAgent::new());
    let key = bench.agent.add_session(10);
    bench.sync();
    let before = bench.session(&key);

    /* Half its records are gone and one long one is new: longer, and less of a session. */
    bench.agent.truncate(&key, 5);
    bench.agent.say(&key, "user", &"and then ".repeat(200));
    let report = bench.sync();
    assert_eq!(
        (
            report.new_versions,
            report.held_back,
            report.index.held_back
        ),
        (1, 0, 1)
    );
    let versions = bench.versions(&key);
    assert!(versions[1].length > versions[0].length);
    assert!(versions[1].held_back && versions[1].held_by_parse);
    assert_eq!(
        bench.session(&key),
        Session {
            held_back: true,
            ..before
        }
    );

    /* Records are added until it holds as many as before: it is the copy that is read. */
    bench.agent.append(&key, 4);
    assert_eq!(bench.sync().index.sessions, 1);
    let newest = bench.versions(&key).remove(1);
    assert!(!newest.held_back && !newest.held_by_parse);
    let after = bench.session(&key);
    assert!(!after.held_back);
    assert_indexed_as(&after, &derived(&bench.agent, &key));
}
