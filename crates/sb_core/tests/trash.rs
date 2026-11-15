//! Trash and deleting for good (PLAN §4.5, D14, §12). Deleting for good is the one act in the
//! app that can't be undone, so these tests compare the whole of `files/`, every table of the
//! catalogue and the location's own folder before and after it. The cases run against both
//! agents; what only the file-tree agent keeps (subagents, saved tool output, sidecars, files
//! of a project) has a case of its own.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use pretty_assertions::assert_eq;
use sb_core::archive::ArchiveError;
use sb_core::catalogue::{CatalogueError, Deletion, Filed, SessionId, Shelf};
use sb_core::hash::Hash;
use sb_core::provider::ItemKind;
use sb_test_support::{Agent, FileAgent, ProjectsTree, read_catalogue};

use common::{Bench, Meddle, When, for_both_agents};

for_both_agents!(
    a_session_in_trash_is_on_no_page_but_trash_and_is_still_synced,
    a_restored_session_goes_back_to_its_folder_or_to_the_rules,
    deleting_for_good_removes_the_sessions_files_and_rows_and_nothing_else,
    only_a_session_in_trash_is_deleted_for_good_and_not_while_its_location_syncs,
    a_session_deleted_for_good_stays_gone_until_its_tombstone_is_removed,
    a_tombstone_outlasts_a_rebuilt_index_and_a_classifier_that_renames_sessions,
    a_catalogue_restored_from_before_a_delete_has_the_rows_and_not_the_files,
);

/// Every file under `dir` with its bytes and modification time, by its path in `dir`; and
/// every folder.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Option<(Vec<u8>, SystemTime)>> {
    let mut found = BTreeMap::new();
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(dir).unwrap().to_path_buf();
            if path.is_dir() {
                found.insert(relative, None);
                folders.push(path);
            } else {
                let modified = fs::metadata(&path).unwrap().modified().unwrap();
                found.insert(relative, Some((fs::read(&path).unwrap(), modified)));
            }
        }
    }
    found
}

/// The catalogue's tables, but those of the search index's own storage, each as its rows.
fn tables(dir: &Path) -> BTreeMap<String, BTreeSet<String>> {
    read_catalogue(
        dir,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
           AND name NOT LIKE 'session_text%'",
    )
    .into_iter()
    .map(|table| {
        let rows = rows(dir, &table, "true");
        (table, rows)
    })
    .collect()
}

/// The rows of `table` for which `condition` holds, each as one line of text.
fn rows(dir: &Path, table: &str, condition: &str) -> BTreeSet<String> {
    let columns = read_catalogue(
        dir,
        &format!("SELECT name FROM pragma_table_info('{table}')"),
    );
    let row = columns
        .iter()
        .map(|column| format!("quote({column})"))
        .collect::<Vec<_>>()
        .join(" || '|' || ");
    read_catalogue(dir, &format!("SELECT {row} FROM {table} WHERE {condition}"))
        .into_iter()
        .collect()
}

/// `text` as an SQL literal.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The rows of each table that are the session `id`'s, as the catalogue holds them now.
fn rows_of(dir: &Path, id: &SessionId) -> BTreeMap<&'static str, BTreeSet<String>> {
    let identity = format!(
        "location_id = {} AND provider = {} AND session = {}",
        literal(id.location.as_str()),
        literal(&id.provider),
        literal(&id.key)
    );
    let items = format!("SELECT id FROM item WHERE {identity}");
    [
        ("item", identity.clone()),
        ("version", format!("item_id IN ({items})")),
        (
            "segment",
            format!("version_id IN (SELECT id FROM version WHERE item_id IN ({items}))"),
        ),
        ("session", identity.clone()),
        ("session_state", identity.clone()),
        (
            "span",
            format!("id >> 24 IN (SELECT id FROM session WHERE {identity})"),
        ),
    ]
    .into_iter()
    .map(|(table, condition)| (table, rows(dir, table, &condition)))
    .collect()
}

/// An item's folder name: the start of the hash of its key (PLAN §4.1).
fn folder_of(key: &str) -> String {
    Hash::of(key.as_bytes()).to_hex()[..32].to_string()
}

/**
 * A bench whose agent has sessions until two of them have items whose folders are in the same
 * folder of `files/` (named by the first two digits of their own). Returns them, the second
 * the one to delete, and the other sessions.
 */
fn sharing<A: Agent + Default>() -> (Bench<A>, String, String, Vec<String>) {
    let mut agent = A::default();
    let mut keys: Vec<String> = Vec::new();
    loop {
        let key = agent.add_session(3);
        let shares = keys
            .iter()
            .position(|other| folder_of(other)[..2] == folder_of(&key)[..2]);
        if let Some(other) = shares {
            let other = keys.remove(other);
            let bench = Bench::new(agent);
            bench.sync();
            return (bench, other, key, keys);
        }
        keys.push(key);
    }
}

fn trashed<A: Agent>(bench: &Bench<A>, key: &str) -> SessionId {
    let id = bench.session_id(key);
    bench.archive().catalogue().trash(&id).unwrap();
    id
}

fn listed<A: Agent>(bench: &Bench<A>, shelf: Shelf) -> Vec<String> {
    let mut keys: Vec<String> = bench
        .archive()
        .catalogue()
        .sessions(shelf)
        .unwrap()
        .into_iter()
        .map(|session| session.id.key)
        .collect();
    keys.sort();
    keys
}

fn a_session_in_trash_is_on_no_page_but_trash_and_is_still_synced<A: Agent + Default>() {
    let mut agent = A::default();
    let keys = [agent.add_session(3), agent.add_session(3)];
    let mut bench = Bench::new(agent);
    bench.sync();
    let catalogue = bench.archive().catalogue();
    let ids = keys.each_ref().map(|key| bench.session_id(key));
    let folder = catalogue.create_folder("Kept", None).unwrap();
    for id in &ids {
        catalogue.file_by_hand(id, folder).unwrap();
        catalogue.set_starred(id, true).unwrap();
    }
    catalogue.trash(&ids[0]).unwrap();
    let when = catalogue
        .session(&ids[0])
        .unwrap()
        .unwrap()
        .trashed
        .unwrap();
    /* In Trash once: trashing it again keeps when it went. */
    catalogue.trash(&ids[0]).unwrap();
    assert_eq!(
        catalogue.session(&ids[0]).unwrap().unwrap().trashed,
        Some(when)
    );

    let kept = vec![ids[1].key.clone()];
    for shelf in [Shelf::All, Shelf::Starred, Shelf::Folder(folder)] {
        assert_eq!(listed(&bench, shelf), kept, "{shelf:?}");
    }
    assert_eq!(listed(&bench, Shelf::Trash), [ids[0].key.as_str()]);
    let counts = catalogue.library_counts().unwrap();
    assert_eq!(
        (counts.all, counts.starred, counts.unsorted, counts.trash),
        (1, 1, 0, 1)
    );
    assert_eq!(counts.folders[&folder], 1);
    /* Nor is it among Unsorted when its folder goes. */
    catalogue.delete_folder(folder).unwrap();
    assert_eq!(listed(&bench, Shelf::Unsorted), kept);
    assert_eq!(catalogue.unsorted().unwrap().len(), 1);
    assert_eq!(catalogue.library_counts().unwrap().unsorted, 1);

    /* It is synced and indexed as before. */
    bench.agent.append(&keys[0], 2);
    let synced = bench.sync();
    assert_eq!(synced.appended, 1);
    assert_eq!(synced.index.sessions, 1);
    bench.assert_mirrors(&[keys[0].clone()]);
    assert_eq!(listed(&bench, Shelf::Trash), [ids[0].key.as_str()]);

    /* And it stays in Trash through a restart and an index built again. */
    bench.reopen();
    let catalogue = bench.archive().catalogue();
    catalogue.drop_index().unwrap();
    bench.index();
    assert_eq!(listed(&bench, Shelf::Trash), [ids[0].key.as_str()]);
    assert_eq!(listed(&bench, Shelf::All), kept);
}

fn a_restored_session_goes_back_to_its_folder_or_to_the_rules<A: Agent + Default>() {
    let mut agent = A::default();
    let keys = [agent.add_session(3), agent.add_session(3)];
    let bench = Bench::new(agent);
    bench.sync();
    let catalogue = bench.archive().catalogue();
    let ids = keys.each_ref().map(|key| bench.session_id(key));
    let kept = catalogue.create_folder("Kept", None).unwrap();
    let gone = catalogue.create_folder("Gone", None).unwrap();
    catalogue.file_by_hand(&ids[0], kept).unwrap();
    catalogue.file_by_hand(&ids[1], gone).unwrap();
    for id in &ids {
        catalogue.trash(id).unwrap();
    }

    /* Its folder is there: it goes back to it, filed as it was. */
    catalogue.restore(&ids[0]).unwrap();
    let session = catalogue.session(&ids[0]).unwrap().unwrap();
    assert_eq!(
        (session.filed, session.trashed),
        (Filed::ByHand(kept), None)
    );

    /* Its folder was deleted meanwhile: to Unsorted, and the rules file it. */
    catalogue.delete_folder(gone).unwrap();
    catalogue.restore(&ids[1]).unwrap();
    assert_eq!(
        catalogue.session(&ids[1]).unwrap().unwrap().filed,
        Filed::Unsorted
    );
    assert_eq!(listed(&bench, Shelf::Trash), Vec::<String>::new());
    assert_eq!(listed(&bench, Shelf::All).len(), 2);

    /* What is not in Trash is not restored. */
    assert!(matches!(
        catalogue.restore(&ids[1]),
        Err(CatalogueError::NotInTrash(_))
    ));
}

fn deleting_for_good_removes_the_sessions_files_and_rows_and_nothing_else<A: Agent + Default>() {
    let (bench, kept, deleted, others) = sharing::<A>();
    let catalogue = bench.archive().catalogue();
    let id = trashed(&bench, &deleted);
    catalogue.set_starred(&id, true).unwrap();
    let neighbour = bench.session_id(&kept);
    catalogue.trash(&neighbour).unwrap();

    let deleted_items: Vec<_> = bench
        .items()
        .into_iter()
        .filter(|item| item.session.as_deref() == Some(&id.key))
        .collect();
    let deletion = catalogue.deletion(&id).unwrap();
    assert_eq!(
        deletion,
        Deletion {
            session: id.clone(),
            files: deleted_items.len(),
            versions: 1,
            bytes: deleted_items
                .iter()
                .map(|item| bench
                    .versions(&item.key)
                    .iter()
                    .map(|v| v.length)
                    .sum::<u64>())
                .sum(),
            at_location: true,
        }
    );
    let files_before = snapshot(&bench.dir().join("files"));
    let source_before = snapshot(bench.agent.home());
    let tables_before = tables(bench.dir());
    let its_rows = rows_of(bench.dir(), &id);
    assert!(
        its_rows.values().all(|rows| !rows.is_empty()),
        "{its_rows:#?}"
    );
    let unchanged = ["reading_choice", "waiting", "removal", "folder", "location"];
    assert!(
        unchanged
            .iter()
            .all(|table| tables_before.contains_key(*table))
    );

    assert_eq!(bench.archive().delete_for_good(&id).unwrap(), deletion);

    /* Of files/, exactly the deleted items' folders are gone; the folder they shared with
    another session's is not. */
    let folders: Vec<String> = deleted_items
        .iter()
        .map(|item| folder_of(&item.key))
        .collect();
    let files_after = snapshot(&bench.dir().join("files"));
    let expected: BTreeMap<_, _> = files_before
        .iter()
        .filter(|(path, _)| {
            !path
                .iter()
                .any(|part| folders.iter().any(|folder| part == folder.as_str()))
        })
        .map(|(path, file)| (path.clone(), file.clone()))
        .collect();
    assert_eq!(
        files_after.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>()
    );
    assert!(files_after == expected, "a file that stays was changed");
    let shared = files_after
        .keys()
        .any(|path| path.iter().any(|part| part == &folder_of(&kept)[..2]));
    assert!(shared);

    /* Of the catalogue, exactly the session's rows are gone, and a tombstone is new. */
    let tables_after = tables(bench.dir());
    for (table, before) in &tables_before {
        let after = &tables_after[table];
        let removed: BTreeSet<_> = before.difference(after).cloned().collect();
        let added: BTreeSet<_> = after.difference(before).cloned().collect();
        let expected = its_rows.get(table.as_str()).cloned().unwrap_or_default();
        assert_eq!(removed, expected, "removed from {table}");
        match table.as_str() {
            "tombstone" => assert_eq!(added.len(), 1),
            "tombstone_item" => assert_eq!(added.len(), deleted_items.len()),
            _ => assert_eq!(added, BTreeSet::new(), "added to {table}"),
        }
    }
    assert_eq!(rows(bench.dir(), "removal", "true"), BTreeSet::new());
    let tombstones = catalogue.tombstones().unwrap();
    assert_eq!(tombstones.len(), 1);
    assert_eq!(tombstones[0].session, id);
    assert!(tombstones[0].title.is_some());

    /* The location is as it was, the rest of the archive verifies, and the others read. */
    assert!(
        snapshot(bench.agent.home()) == source_before,
        "the location was changed"
    );
    bench.assert_verifies();
    bench.assert_mirrors(&[&others[..], std::slice::from_ref(&kept)].concat());
    assert_eq!(listed(&bench, Shelf::Trash), [neighbour.key.as_str()]);
    assert_eq!(catalogue.library_counts().unwrap().trash, 1);
    assert!(catalogue.session(&id).unwrap().is_none());
    assert!(matches!(
        catalogue.deletion(&id),
        Err(CatalogueError::NotInTrash(_))
    ));
}

fn only_a_session_in_trash_is_deleted_for_good_and_not_while_its_location_syncs<
    A: Agent + Default,
>() {
    let mut agent = A::default();
    let key = agent.add_session(3);
    let bench = Bench::new(agent);
    bench.sync();
    let id = bench.session_id(&key);
    let files = snapshot(&bench.dir().join("files"));
    assert!(matches!(
        bench.archive().delete_for_good(&id),
        Err(ArchiveError::Catalogue(CatalogueError::NotInTrash(_)))
    ));
    bench.archive().catalogue().trash(&id).unwrap();

    /* A pass of its location is under way. */
    let refused = AtomicBool::new(false);
    let archive = bench.archive();
    bench
        .pass_with(bench.meddling(Meddle::at(When::AfterScan, || {
            refused.store(
                matches!(archive.delete_for_good(&id), Err(ArchiveError::Syncing(_))),
                Ordering::Relaxed,
            );
        })))
        .unwrap();
    assert!(refused.load(Ordering::Relaxed));
    assert!(snapshot(&bench.dir().join("files")) == files);

    /* Nor is what is not archived. */
    let nothing = SessionId {
        key: "no such session".to_string(),
        ..id.clone()
    };
    assert!(bench.archive().delete_for_good(&nothing).is_err());
    assert!(bench.archive().delete_for_good(&id).is_ok());
}

fn a_session_deleted_for_good_stays_gone_until_its_tombstone_is_removed<A: Agent + Default>() {
    let mut agent = A::default();
    let keys = [agent.add_session(3), agent.add_session(3)];
    let mut bench = Bench::new(agent);
    bench.sync();
    let id = trashed(&bench, &keys[0]);
    let content = bench.agent.content(&keys[0]);
    bench.archive().delete_for_good(&id).unwrap();

    /* The next sync skips it, and so does one after the agent went on with it. */
    let synced = bench.sync();
    assert_eq!((synced.deleted, synced.new_items), (1, 0));
    bench.agent.append(&keys[0], 2);
    let synced = bench.sync();
    assert_eq!(
        (synced.deleted, synced.new_items, synced.appended),
        (1, 0, 0)
    );
    assert!(
        bench
            .archive()
            .catalogue()
            .item(&bench.location, &id.provider, &keys[0])
            .unwrap()
            .is_none()
    );
    assert_eq!(listed(&bench, Shelf::All), [bench.session_id(&keys[1]).key]);
    assert!(
        snapshot(&bench.dir().join("files"))
            .keys()
            .all(|path| !path.iter().any(|part| part == folder_of(&keys[0]).as_str()))
    );
    bench.assert_verifies();

    /* Removing the tombstone lets the next sync archive it again, as the location has it. */
    let catalogue = bench.archive().catalogue();
    catalogue.remove_tombstone(&id).unwrap();
    assert!(matches!(
        catalogue.remove_tombstone(&id),
        Err(CatalogueError::NoTombstone(_))
    ));
    assert_eq!(catalogue.tombstones().unwrap(), []);
    let synced = bench.sync();
    assert_eq!((synced.deleted, synced.new_items), (0, 1));
    assert_ne!(bench.agent.content(&keys[0]), content);
    bench.assert_mirrors(&keys);
    assert_eq!(listed(&bench, Shelf::All).len(), 2);
    assert_eq!(bench.session(&keys[0]).trashed, None);

    /* The deletion survives a restart. */
    let id = trashed(&bench, &keys[0]);
    bench.archive().delete_for_good(&id).unwrap();
    bench.reopen();
    assert_eq!(bench.sync().deleted, 1);
    assert_eq!(listed(&bench, Shelf::All).len(), 1);
}

fn a_tombstone_outlasts_a_rebuilt_index_and_a_classifier_that_renames_sessions<
    A: Agent + Default,
>() {
    let mut agent = A::default();
    let keys = [agent.add_session(3), agent.add_session(3)];
    let bench = Bench::new(agent);
    bench.sync();
    let id = trashed(&bench, &keys[0]);
    bench.archive().delete_for_good(&id).unwrap();
    let catalogue = bench.archive().catalogue();
    catalogue.drop_index().unwrap();
    bench.index();
    assert_eq!(catalogue.tombstones().unwrap().len(), 1);
    assert_eq!(bench.sync().deleted, 1);

    /* A newer parser gives every session another id: the deleted one's items are still
    skipped by their keys, and the other session is archived on under its new id. */
    let renamed = || {
        bench.meddling(Meddle::Classifier(|_, (kind, session)| {
            (kind, session.map(|session| format!("renamed/{session}")))
        }))
    };
    bench.index_with(renamed());
    let pass = bench.synced_with(renamed());
    assert_eq!((pass.deleted, pass.new_items), (1, 0));
    let sessions: Vec<_> = catalogue
        .sessions(Shelf::All)
        .unwrap()
        .into_iter()
        .map(|session| session.id.key)
        .collect();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].starts_with("renamed/"));
    assert_eq!(catalogue.tombstones().unwrap()[0].session, id);
}

fn a_catalogue_restored_from_before_a_delete_has_the_rows_and_not_the_files<A: Agent + Default>() {
    let mut agent = A::default();
    let key = agent.add_session(3);
    let mut bench = Bench::new(agent);
    bench.sync();
    let id = trashed(&bench, &key);
    let catalogue = bench.dir().join("scrollback.db");
    let backup = bench.dir().join("before-the-delete.db");
    bench.close();
    fs::copy(&catalogue, &backup).unwrap();
    bench.reopen();
    bench.archive().delete_for_good(&id).unwrap();

    /* The owner puts the older catalogue back: the session's rows are there, in Trash,
    and its files are not; verify says so, and the next sync, as the location still has the
    session, fetches it whole again. */
    bench.close();
    fs::copy(&backup, &catalogue).unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(bench.dir().join(format!("scrollback.db{suffix}")));
    }
    bench.reopen();
    let session = bench.session(&key);
    assert!(session.trashed.is_some());
    assert!(bench.archive().catalogue().tombstones().unwrap().is_empty());
    let report = bench
        .archive()
        .verify(&sb_core::CancelToken::new())
        .unwrap();
    assert_eq!(report.damaged.len(), 1);
    let synced = bench.synced_with(bench.agent.provider());
    assert_eq!((synced.deleted, synced.new_versions), (0, 1));
    let newest = bench.versions(&key).pop().unwrap();
    assert_eq!(bench.read(&newest), bench.agent.content(&key));
}

/**
 * Every kind of item a session has goes with it: its transcript, a subagent's, saved tool
 * output, its title file and a sidecar beside its transcript. What belongs to its project
 * and to no session, and the other session of the project, stay.
 */
#[test]
fn deleting_for_good_takes_every_item_of_the_session_and_none_of_its_project() {
    let mut tree = ProjectsTree::new();
    let project = "/home/user/code/app";
    let mut deleted = tree.session(project, 4);
    let mut keys = vec![
        deleted.key.clone(),
        deleted.subagent(3),
        deleted.tool_result(500),
        deleted.title("Doomed"),
        tree.project_file(project, &format!("{}.sidecar.json", deleted.id), b"{}"),
    ];
    let kept = tree.session(project, 4);
    let memory = tree.memory(project, "notes", "Remember this.");
    let bench = Bench::new(FileAgent::on(tree));
    bench.sync();
    let id = bench.session_id(&deleted.key);
    let mut kinds: Vec<ItemKind> = Vec::new();
    for item in bench.items() {
        if item.session.as_deref() == Some(&id.key) && !kinds.contains(&item.kind) {
            kinds.push(item.kind);
        }
    }
    assert_eq!(kinds.len(), 4, "{kinds:?}");
    keys.sort();
    let source = snapshot(bench.agent.home());
    bench.archive().catalogue().trash(&id).unwrap();
    let items = bench
        .items()
        .into_iter()
        .filter(|item| item.session.as_deref() == Some(&id.key))
        .count();
    /* The subagent's transcript has its metadata beside it. */
    assert_eq!(items, keys.len() + 1);
    let deletion = bench.archive().delete_for_good(&id).unwrap();
    assert_eq!(deletion.files, items);

    let left: Vec<String> = bench.items().into_iter().map(|item| item.key).collect();
    assert_eq!(left, {
        let mut left = vec![kept.key.clone(), memory.clone()];
        left.sort();
        left
    });
    let files = snapshot(&bench.dir().join("files"));
    for key in &keys {
        assert!(
            files
                .keys()
                .all(|path| !path.iter().any(|part| part == folder_of(key).as_str())),
            "{key}"
        );
    }
    assert!(snapshot(bench.agent.home()) == source);
    bench.assert_mirrors(&[kept.key.clone(), memory]);
    /* Nothing of it comes back, the subagent and the rest included. */
    let synced = bench.sync();
    assert_eq!((synced.deleted, synced.new_items), (items, 0));
}

/**
 * A new item that turns up later under a session deleted for good (a subagent the agent
 * starts when the session is resumed) is skipped with the rest.
 */
#[test]
fn what_turns_up_later_under_a_deleted_session_is_skipped() {
    let mut tree = ProjectsTree::new();
    let mut session = tree.session("/home/user/code/app", 4);
    let bench = Bench::new(FileAgent::on(tree));
    bench.sync();
    let id = trashed(&bench, &session.key);
    bench.archive().delete_for_good(&id).unwrap();
    /* Its transcript and the new subagent's, with its metadata. */
    session.subagent(3);
    let synced = bench.sync();
    assert_eq!((synced.deleted, synced.new_items), (3, 0));
    assert!(bench.items().is_empty());
}
