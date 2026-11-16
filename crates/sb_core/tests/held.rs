//! The reading copy (PLAN §4.2, §12): which version of an item is read, what holds a version
//! back, and what the owner's choice does.

mod common;

use std::fs;
use std::io::Read as _;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::catalogue::{CatalogueError, Held, Session};
use sb_core::provider::{ItemKind, SessionItems};
use sb_core::sync::{Progress, SyncError};
use sb_core::transcript::Ending;
use sb_test_support::{Agent, DbAgent, FileAgent, read_catalogue};
use serde_json::json;

use common::{Bench, for_both_agents};

for_both_agents!(
    the_owner_chooses_an_older_version_until_a_newer_one_arrives,
    a_history_says_what_is_read_and_any_version_reads_without_being_chosen,
);

/// What a parser is given of the item `key` of the session whose transcript is `session`.
fn read<A: Agent>(bench: &Bench<A>, session: &str, key: &str) -> Vec<u8> {
    let items = bench.archive().session(&bench.session_id(session)).unwrap();
    let mut content = Vec::new();
    items
        .open(key, 0)
        .unwrap()
        .read_to_end(&mut content)
        .unwrap();
    let listed = items.list().iter().find(|item| item.key == key).unwrap();
    assert_eq!(listed.length, content.len() as u64);
    content
}

/// The numbers of the version of the session's main transcript that is read and of its
/// newest, as its row has them.
fn versions_of(session: &Session) -> (Option<u32>, Option<u32>) {
    (session.reading_version, session.newest_version)
}

/// Lines that are no records of any transcript, longer than `than`.
fn garbage(than: &[u8]) -> String {
    "this is not a transcript\n".repeat(than.len() / 20 + 1)
}

fn the_owner_chooses_an_older_version_until_a_newer_one_arrives<A: Agent + Default>() {
    let mut agent = A::default();
    let keys = [agent.add_session(8), agent.add_session(3)];
    let mut bench = Bench::new(agent);
    bench.sync();
    let key = keys[0].clone();
    let first = (bench.session(&key), bench.agent.content(&key));
    bench.agent.rewrite(&key);
    assert_eq!(bench.sync().index.sessions, 1);
    let second = (bench.session(&key), bench.agent.content(&key));
    let versions = bench.versions(&key);
    let (id, item) = (bench.session_id(&key), bench.item(&key).id);
    let choose = |bench: &Bench<A>, version| {
        bench
            .archive()
            .catalogue()
            .set_reading_version(&id, item, version)
    };

    /* The newest version is read, since it is not held back. */
    assert_eq!((versions.len(), versions[1].held_back), (2, None));
    assert!(read(&bench, &key, &key) == second.1);
    assert_eq!(versions_of(&second.0), (Some(2), Some(2)));

    /* The owner chooses the older one: it is read at once, and its row follows the index of
    the session. */
    choose(&bench, Some(versions[0].id)).unwrap();
    assert!(read(&bench, &key, &key) == first.1);
    assert_eq!(bench.session(&key), second.0);
    assert_eq!(bench.index_session(&key).sessions, 1);
    assert_eq!(
        bench.session(&key),
        Session {
            newest_version: Some(2),
            ..first.0.clone()
        }
    );
    assert_eq!(bench.index().sessions, 0);

    /* A version of another item is not this item's, and another session's item not this
    session's. */
    let other = bench.versions(&keys[1]).remove(0).id;
    assert!(matches!(
        choose(&bench, Some(other)),
        Err(CatalogueError::NotOfItem { version, .. }) if version == other
    ));
    let elsewhere = bench.item(&keys[1]).id;
    assert!(matches!(
        bench
            .archive()
            .catalogue()
            .set_reading_version(&id, elsewhere, Some(other)),
        Err(CatalogueError::NotOfSession(not_its)) if not_its == elsewhere
    ));
    assert!(read(&bench, &key, &key) == first.1);

    /* A version arrives that was not there to choose from: the choice is over. */
    bench.agent.rewrite_same_size(&key);
    assert_eq!(bench.sync().new_versions, 1);
    let third = bench.agent.content(&key);
    assert!(read(&bench, &key, &key) == third);
    assert_eq!(versions_of(&bench.session(&key)), (Some(3), Some(3)));

    /* Chosen again, and given up for the rule. */
    choose(&bench, Some(versions[0].id)).unwrap();
    assert!(read(&bench, &key, &key) == first.1);
    choose(&bench, None).unwrap();
    assert!(read(&bench, &key, &key) == third);

    /* A chosen version that is found damaged is not read in the newest one's place. */
    choose(&bench, Some(versions[1].id)).unwrap();
    assert!(read(&bench, &key, &key) == second.1);
    let chosen = bench
        .segment_files_of(&key)
        .into_iter()
        .find(|file| file.iter().any(|part| part == "v0002"))
        .unwrap();
    fs::write(chosen, "not what was stored").unwrap();
    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    assert_eq!(report.damaged.len(), 1);
    assert!(read(&bench, &key, &key) == third);
    /* The choice is over, not waiting for the version to be mended. */
    assert!(read_catalogue(bench.dir(), "SELECT version_id FROM reading_choice").is_empty());
}

/**
 * The reader's History (DESIGN §4.3): an item's versions with the syncs that extended each,
 * which one is read and whether the owner chose it; and a version read for a look, which
 * chooses nothing.
 */
fn a_history_says_what_is_read_and_any_version_reads_without_being_chosen<A: Agent + Default>() {
    let mut agent = A::default();
    let keys = [agent.add_session(8), agent.add_session(3)];
    let mut bench = Bench::new(agent);
    bench.sync();
    let key = keys[0].clone();
    let first = bench.agent.content(&key);
    bench.agent.append(&key, 2);
    bench.sync();
    let extended = bench.agent.content(&key);
    bench.agent.rewrite(&key);
    bench.sync();
    let second = bench.agent.content(&key);
    let (id, item) = (bench.session_id(&key), bench.item(&key).id);
    let catalogue = || bench.archive().catalogue();

    let history = catalogue().history(&id, &key).unwrap().unwrap();
    let versions = bench.versions(&key);
    assert_eq!(history.item, item);
    assert_eq!(
        history
            .versions
            .iter()
            .map(|(version, extended)| (version.id, *extended))
            .collect::<Vec<_>>(),
        [(versions[0].id, 1), (versions[1].id, 0)]
    );
    assert_eq!((history.reading, history.chosen), (versions[1].id, false));
    assert_eq!(catalogue().history(&id, "not an item").unwrap(), None);

    /* The first version, read for a look: as long as it was, and nothing is chosen. */
    let looked = bench
        .archive()
        .session_at(&id, &key, versions[0].id)
        .unwrap();
    let mut content = Vec::new();
    looked
        .open(&key, 0)
        .unwrap()
        .read_to_end(&mut content)
        .unwrap();
    assert!(content == extended && content.starts_with(&first));
    assert_eq!(
        looked
            .list()
            .iter()
            .find(|it| it.key == key)
            .unwrap()
            .length,
        extended.len() as u64
    );
    assert!(read(&bench, &key, &key) == second);
    assert!(read_catalogue(bench.dir(), "SELECT version_id FROM reading_choice").is_empty());
    /* Another item's version is not this one's. */
    let other = bench.versions(&keys[1]).remove(0).id;
    assert!(bench.archive().session_at(&id, &key, other).is_err());

    /* Chosen, the history says so. */
    catalogue()
        .set_reading_version(&id, item, Some(versions[0].id))
        .unwrap();
    let history = catalogue().history(&id, &key).unwrap().unwrap();
    assert_eq!((history.reading, history.chosen), (versions[0].id, true));
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
    let garbage = garbage(&whole);
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
    let held = |bench: &Bench<FileAgent>| bench.versions(&key)[1].held_back;
    assert_eq!(held(&bench), Some(Held::ParsesWorse));
    /* The copy being read is still the one from before, and the session says so. */
    assert_eq!(
        bench.session(&key),
        Session {
            held_back: true,
            newest_version: Some(2),
            ..before.clone()
        }
    );
    assert!(read(&bench, &key, &key) == whole);

    /* It grows, which would end a hold for its length; this one stays. */
    bench.agent.append_raw(&key, &garbage);
    let report = bench.sync();
    assert_eq!((report.appended, report.index.held_back), (1, 0));
    assert_eq!(held(&bench), Some(Held::ParsesWorse));
    assert!(read(&bench, &key, &key) == whole);
    assert_eq!(bench.sync().index.sessions, 0);

    /* The agent goes on in the file, until it holds as much as the copy being read: the
    version is no loss any more, and is the one read. */
    bench.agent.session(&key).append(40);
    let report = bench.sync();
    assert_eq!((report.appended, report.index.sessions), (1, 1));
    assert_eq!(held(&bench), None);
    assert!(read(&bench, &key, &key) == bench.agent.content(&key));
    let after = bench.session(&key);
    assert!(!after.held_back);
    assert_eq!(versions_of(&after), (Some(2), Some(2)));
}

#[test]
fn a_held_back_version_the_owner_chooses_is_accepted_for_good() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(12);
    let donor = agent.add_session(80);
    let bench = Bench::new(agent);
    bench.sync();
    let whole = bench.agent.content(&key);
    let garbage = garbage(&whole);
    bench.agent.overwrite(&key, garbage.as_bytes());
    assert_eq!(bench.sync().index.held_back, 1);
    let held = bench.session(&key);
    assert!(held.held_back && held.title.is_some());
    assert_eq!(versions_of(&held), (Some(1), Some(2)));

    /* The owner reads it all the same: it is no longer held back, and is the copy that is
    read by the rule, with nothing chosen in the rule's place. */
    let (id, item) = (bench.session_id(&key), bench.item(&key).id);
    let newest = bench.versions(&key).remove(1);
    bench
        .archive()
        .catalogue()
        .set_reading_version(&id, item, Some(newest.id))
        .unwrap();
    assert_eq!(bench.versions(&key)[1].held_back, None);
    assert!(read(&bench, &key, &key) == garbage.as_bytes());
    assert_eq!(bench.index_session(&key).held_back, 0);
    let accepted = bench.session(&key);
    assert_eq!((&accepted.title, accepted.held_back), (&None, false));
    assert_eq!(versions_of(&accepted), (Some(2), Some(2)));

    /* No index holds it back again, however it parses. */
    bench.agent.append_raw(&key, &garbage);
    let report = bench.sync();
    assert_eq!((report.appended, report.index.held_back), (1, 0));
    assert_eq!(bench.versions(&key)[1].held_back, None);
    assert_eq!(bench.index().sessions, 0);

    /* A later version that is no loss is read as any newest version is. */
    let healthy = bench.agent.content(&donor);
    assert!(healthy.len() > bench.agent.content(&key).len());
    bench.agent.overwrite(&key, &healthy);
    let report = bench.sync();
    assert_eq!((report.new_versions, report.index.held_back), (1, 0));
    assert!(read(&bench, &key, &key) == healthy);
    let later = bench.session(&key);
    assert_eq!(versions_of(&later), (Some(3), Some(3)));
    assert!(later.title.is_some() && !later.held_back);

    /* One that is shorter is held back for that, and accepted the same way. */
    bench.agent.overwrite(&key, &whole);
    assert_eq!(bench.sync().held_back, 1);
    assert_eq!(bench.versions(&key)[3].held_back, Some(Held::Shorter));
    assert_eq!(versions_of(&bench.session(&key)), (Some(3), Some(4)));
    assert!(bench.session(&key).held_back);
    let shorter = bench.versions(&key).remove(3);
    bench
        .archive()
        .catalogue()
        .set_reading_version(&id, item, Some(shorter.id))
        .unwrap();
    assert!(read(&bench, &key, &key) == whole);
    bench.index_session(&key);
    assert_eq!(versions_of(&bench.session(&key)), (Some(4), Some(4)));
}

#[test]
fn a_new_version_is_compared_with_the_copy_that_is_read_not_the_version_before_it() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(12);
    let bench = Bench::new(agent);
    bench.sync();
    let whole = bench.agent.content(&key);
    let lines: Vec<&[u8]> = whole.split_inclusive(|byte| *byte == b'\n').collect();
    let garbage = garbage(&whole).repeat(3);
    bench.agent.overwrite(&key, garbage.as_bytes());
    assert_eq!(bench.sync().index.held_back, 1);

    /* The third version holds a third of the first one's conversation, and more than the
    second's, which is none: it is a loss beside the copy that is read, the first. */
    let third = [&lines[..4].concat()[..], &garbage.as_bytes()[..whole.len()]].concat();
    bench.agent.overwrite(&key, &third);
    let report = bench.sync();
    assert_eq!(
        (
            report.new_versions,
            report.held_back,
            report.index.held_back
        ),
        (1, 0, 1)
    );
    let held = |bench: &Bench<FileAgent>| -> Vec<_> {
        bench
            .versions(&key)
            .iter()
            .map(|version| version.held_back)
            .collect()
    };
    assert_eq!(
        held(&bench),
        [None, Some(Held::ParsesWorse), Some(Held::ParsesWorse)]
    );
    let row = bench.session(&key);
    assert_eq!(versions_of(&row), (Some(1), Some(3)));
    assert!(row.held_back);
    assert!(read(&bench, &key, &key) == whole);

    /* The owner accepts the second. Beside that, the third parses no worse; but it is
    shorter, and stays held back for that. */
    let (id, item) = (bench.session_id(&key), bench.item(&key).id);
    let second = bench.versions(&key).remove(1);
    bench
        .archive()
        .catalogue()
        .set_reading_version(&id, item, Some(second.id))
        .unwrap();
    assert_eq!(bench.index_session(&key).held_back, 0);
    assert_eq!(held(&bench), [None, None, Some(Held::Shorter)]);
    let row = bench.session(&key);
    assert_eq!(versions_of(&row), (Some(2), Some(3)));
    assert!(row.held_back);
}

#[test]
fn what_an_agent_appends_is_never_held_back() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(0);
    let transcript = &mut agent.session(&key).transcript;
    transcript.prompt("One.");
    let answered = transcript.reply("First.");
    transcript.prompt("Two.");
    transcript.reply("Second.");
    transcript.prompt("Three.");
    transcript.reply("Third.");
    let mut bench = Bench::new(agent);
    bench.sync();
    bench.agent.rewrite(&key);
    assert_eq!(bench.sync().new_versions, 1);
    assert_eq!(versions_of(&bench.session(&key)), (Some(2), Some(2)));

    /* The user goes back to the first answer and asks again: most of what was said is on a
    branch that is not shown any more, and all of it is still there. */
    let transcript = &mut bench.agent.session(&key).transcript;
    transcript.follow(&answered);
    transcript.prompt("Two, again.");
    transcript.reply("Second, again.");
    let report = bench.sync();
    assert_eq!((report.appended, report.index.held_back), (1, 0));
    assert_eq!(bench.versions(&key)[1].held_back, None);
    let row = bench.session(&key);
    assert_eq!(versions_of(&row), (Some(2), Some(2)));
    assert!(!row.held_back);
    assert!(read(&bench, &key, &key) == bench.agent.content(&key));
}

#[test]
fn records_the_parser_does_not_know_are_no_conversation() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(12);
    let bench = Bench::new(agent);
    bench.sync();
    let whole = bench.agent.content(&key);

    /* As many records as could be wanted, each with its place in a tree, and none of a type
    that holds conversation. */
    let unknown: String = (0..400_u32)
        .map(|n| {
            json!({ "type": "hologram", "uuid": format!("h-{n}"),
                    "parentUuid": n.checked_sub(1).map(|n| format!("h-{n}")),
                    "shape": "x".repeat(whole.len() / 100) })
            .to_string()
                + "\n"
        })
        .collect();
    assert!(unknown.len() > whole.len());
    bench.agent.overwrite(&key, unknown.as_bytes());
    let report = bench.sync();
    assert_eq!((report.new_versions, report.index.held_back), (1, 1));
    assert_eq!(bench.versions(&key)[1].held_back, Some(Held::ParsesWorse));
    assert!(read(&bench, &key, &key) == whole);
}

#[test]
fn a_subagents_transcript_is_held_back_and_chosen_on_its_own() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(8);
    let subagent = agent.session(&key).subagent(8);
    let mut bench = Bench::new(agent);
    bench.sync();
    let before = bench.session(&key);
    assert!(before.all.calls > before.own.calls);
    let whole = bench.agent.content(&subagent);

    /* The subagent's transcript is replaced by something that holds no conversation. */
    let garbage = garbage(&whole);
    bench.agent.overwrite(&subagent, garbage.as_bytes());
    let report = bench.sync();
    assert_eq!(
        (
            report.new_versions,
            report.held_back,
            report.index.held_back
        ),
        (1, 0, 1)
    );
    assert_eq!(bench.item(&subagent).kind, ItemKind::Subagent);
    assert_eq!(
        bench.versions(&subagent)[1].held_back,
        Some(Held::ParsesWorse)
    );
    assert!(read(&bench, &key, &subagent) == whole);
    /* The session reads as before, and says that something of it is held back. The
    versions it names are its own transcript's. */
    assert_eq!(
        bench.session(&key),
        Session {
            held_back: true,
            ..before.clone()
        }
    );

    /* The session's own transcript gets a second version, of which the owner chooses the
    first. That is a choice about that item: the subagent's is read as it was. */
    bench.agent.rewrite(&key);
    bench.sync();
    let rewritten = bench.session(&key);
    assert_ne!(rewritten.title, before.title);
    let (id, own, its) = (
        bench.session_id(&key),
        bench.item(&key).id,
        bench.item(&subagent).id,
    );
    let catalogue = bench.archive().catalogue();
    let first = bench.versions(&key).remove(0);
    catalogue
        .set_reading_version(&id, own, Some(first.id))
        .unwrap();
    bench.index_session(&key);
    assert_eq!(
        bench.session(&key),
        Session {
            held_back: true,
            newest_version: Some(2),
            ..before.clone()
        }
    );
    assert!(read(&bench, &key, &subagent) == whole);

    /* A version of the subagent's transcript is not one of the session's own. */
    let replaced = bench.versions(&subagent).remove(1);
    assert!(matches!(
        catalogue.set_reading_version(&id, own, Some(replaced.id)),
        Err(CatalogueError::NotOfItem { .. })
    ));

    /* The owner accepts the subagent's newest: that is read of it, and of the session's
    own transcript still the version that was chosen. */
    catalogue
        .set_reading_version(&id, its, Some(replaced.id))
        .unwrap();
    assert!(read(&bench, &key, &subagent) == garbage.as_bytes());
    assert!(read(&bench, &key, &key) == bench.read(&first));
    bench.index_session(&key);
    let row = bench.session(&key);
    assert_eq!(
        (row.title, row.held_back, row.all.calls),
        (before.title, false, before.own.calls)
    );
    assert_eq!(versions_of(&bench.session(&key)), (Some(1), Some(2)));
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
    assert_eq!(versions[1].held_back, Some(Held::ParsesWorse));
    assert_eq!(
        bench.session(&key),
        Session {
            held_back: true,
            newest_version: Some(2),
            ..before
        }
    );

    /* Records are added until it holds as many as before: it is the copy that is read. */
    bench.agent.append(&key, 4);
    assert_eq!(bench.sync().index.sessions, 1);
    assert_eq!(bench.versions(&key)[1].held_back, None);
    let after = bench.session(&key);
    assert!(!after.held_back);
    assert_eq!(versions_of(&after), (Some(2), Some(2)));
}

#[cfg(unix)]
#[test]
fn nothing_is_held_back_for_a_read_the_system_failed() {
    use std::os::unix::fs::PermissionsExt as _;

    let mut agent = FileAgent::new();
    let key = agent.add_session(12);
    let mut bench = Bench::new(agent);
    bench.sync();
    bench.agent.rewrite(&key);
    bench.sync();
    let before = bench.session(&key);
    assert_eq!(versions_of(&before), (Some(2), Some(2)));

    /* The newest version's segment can't be opened just now. It parses as nothing, which
    is no reason to hold it back: nothing was learnt of what is stored. */
    let segments = bench.segment_files_of(&key);
    let locked = segments.last().unwrap();
    fs::set_permissions(locked, fs::Permissions::from_mode(0o000)).unwrap();
    let report = bench.index_session(&key);
    assert_eq!((report.failed.len(), report.held_back), (1, 0));
    assert_eq!(bench.versions(&key)[1].held_back, None);
    fs::set_permissions(locked, fs::Permissions::from_mode(0o644)).unwrap();

    /* Nor when it is the copy it is compared with that can't be read. */
    let first = &segments[0];
    fs::set_permissions(first, fs::Permissions::from_mode(0o000)).unwrap();
    let report = bench.index_session(&key);
    assert_eq!((report.failed.len(), report.held_back), (0, 0));
    assert_eq!(bench.versions(&key)[1].held_back, None);
    fs::set_permissions(first, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(bench.session(&key), before);
}

#[test]
fn a_version_that_grows_stays_held_back_until_an_index_has_parsed_it() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(12);
    let bench = Bench::new(agent);
    bench.sync();
    let whole = bench.agent.content(&key);
    bench.agent.overwrite(&key, b"this is not a transcript\n");
    assert_eq!(bench.sync().held_back, 1);
    assert_eq!(bench.versions(&key)[1].held_back, Some(Held::Shorter));

    /* It grows past the copy that is read, with nothing that is a conversation, and the
    sync stops once that is stored, before its index. */
    bench.agent.append_raw(&key, &garbage(&whole));
    let cancel = CancelToken::new();
    let stopped = bench.try_sync(&cancel, &mut |progress| {
        if matches!(progress, Progress::Stored { .. }) {
            cancel.cancel();
        }
    });
    assert!(matches!(stopped, Err(SyncError::Cancelled)), "{stopped:?}");
    let newest = bench.versions(&key).remove(1);
    assert!(newest.length > whole.len() as u64);
    /* It is no shorter any more, and still held back: a reader is not given it. */
    assert_eq!(newest.held_back, Some(Held::Shorter));
    assert!(read(&bench, &key, &key) == whole);

    /* The index, which parses it, says what it is. */
    assert_eq!(bench.sync().index.held_back, 1);
    assert_eq!(bench.versions(&key)[1].held_back, Some(Held::ParsesWorse));
    assert!(read(&bench, &key, &key) == whole);
}

#[test]
fn a_version_is_measured_again_when_the_copy_before_it_is_another() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(12);
    let bench = Bench::new(agent);
    bench.sync();
    let whole = bench.agent.content(&key);
    let lines: Vec<&[u8]> = whole.split_inclusive(|byte| *byte == b'\n').collect();
    /* The second version is short; the third and the fourth are between it and the first. */
    for kept in [&lines[..4], &lines[1..9], &lines[2..12]] {
        bench.agent.overwrite(&key, &kept.concat());
        assert_eq!(bench.sync().held_back, 1);
    }
    let held = |bench: &Bench<FileAgent>| -> Vec<_> {
        bench
            .versions(&key)
            .iter()
            .map(|version| version.held_back)
            .collect()
    };
    let shorter = Some(Held::Shorter);
    assert_eq!(held(&bench), [None, shorter, shorter, shorter]);
    assert_eq!(versions_of(&bench.session(&key)), (Some(1), Some(4)));

    /* The owner accepts the second. The later ones were held back beside the first, and
    are no loss beside the second: the third is measured at once, the newest by the index
    of the session, which also parses it. */
    let (id, item) = (bench.session_id(&key), bench.item(&key).id);
    let second = bench.versions(&key).remove(1);
    bench
        .archive()
        .catalogue()
        .set_reading_version(&id, item, Some(second.id))
        .unwrap();
    assert_eq!(held(&bench), [None, None, None, shorter]);
    assert_eq!(bench.index_session(&key).held_back, 0);
    assert_eq!(held(&bench), [None, None, None, None]);
    /* The second is what the owner chose to read, and the rule would now read the newest:
    it is read as a choice, which a newer version or the owner ends. */
    let row = bench.session(&key);
    assert_eq!(versions_of(&row), (Some(2), Some(4)));
    assert!(!row.held_back);
    assert_eq!(bench.index().sessions, 0);
    bench
        .archive()
        .catalogue()
        .set_reading_version(&id, item, None)
        .unwrap();
    bench.index_session(&key);
    assert_eq!(versions_of(&bench.session(&key)), (Some(4), Some(4)));
}

#[test]
fn damage_is_not_a_matter_of_parsing_and_shows_on_the_row() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(12);
    let mut bench = Bench::new(agent);
    bench.sync();
    bench.agent.rewrite(&key);
    bench.sync();
    let before = bench.session(&key);
    assert_eq!(bench.index().sessions, 0);

    /* The newest version's stored bytes are not what was archived. Verify finds it, and the
    row is made again from the copy as it is: unreadable, and still the copy that is read. */
    let segments = bench.segment_files_of(&key);
    let newest = segments.last().unwrap();
    let stored = fs::read(newest).unwrap();
    fs::write(newest, "not what was stored").unwrap();
    assert_eq!(bench.index().sessions, 0);
    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    assert_eq!(report.damaged.len(), 1);
    let indexed = bench.index();
    assert_eq!((indexed.sessions, indexed.held_back), (1, 0));
    assert_eq!(bench.versions(&key)[1].held_back, None);
    let row = bench.session(&key);
    assert!(
        matches!(row.ending, Some(Ending::Unreadable(_))),
        "{:?}",
        row.ending
    );
    assert!(row.unread && !row.held_back);
    assert_eq!(versions_of(&row), (Some(2), Some(2)));

    /* A damaged version is not one the owner chooses to read. */
    let damaged = bench.versions(&key).remove(1);
    assert!(damaged.damaged);
    assert!(matches!(
        bench.archive().catalogue().set_reading_version(
            &bench.session_id(&key),
            bench.item(&key).id,
            Some(damaged.id)
        ),
        Err(CatalogueError::Damaged(version)) if version == damaged.id
    ));

    /* The bytes are back, verify says so, and the row is as it was. */
    fs::write(newest, stored).unwrap();
    let report = bench.archive().verify(&CancelToken::new()).unwrap();
    assert_eq!(report.damaged.len(), 0);
    assert_eq!(bench.index().sessions, 1);
    assert_eq!(bench.session(&key), before);
}

#[test]
fn a_choice_of_an_older_version_holds_while_the_newest_grows() {
    let mut agent = FileAgent::new();
    let key = agent.add_session(8);
    let mut bench = Bench::new(agent);
    bench.sync();
    let first = bench.agent.content(&key);
    bench.agent.rewrite(&key);
    bench.sync();
    let (id, item) = (bench.session_id(&key), bench.item(&key).id);
    let older = bench.versions(&key).remove(0);
    bench
        .archive()
        .catalogue()
        .set_reading_version(&id, item, Some(older.id))
        .unwrap();
    bench.index_session(&key);

    /* An append is no version that was not there to choose from. */
    bench.agent.append(&key, 3);
    let report = bench.sync();
    assert_eq!((report.appended, report.new_versions), (1, 0));
    assert!(read(&bench, &key, &key) == first);
    assert_eq!(versions_of(&bench.session(&key)), (Some(1), Some(2)));
    assert_eq!(
        read_catalogue(bench.dir(), "SELECT version_id FROM reading_choice"),
        [older.id.to_string()]
    );
}
