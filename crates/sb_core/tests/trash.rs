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
    deleting_for_good_leaves_no_text_of_the_session_in_any_file,
    a_removal_that_fails_stays_pending_and_is_no_stray,
    what_was_set_aside_of_a_deleted_item_goes_with_it,
    a_damaged_segment_of_a_deleted_session_goes_too,
    a_read_that_holds_a_log_neither_stalls_a_delete_nor_keeps_its_text,
    a_session_that_changed_since_it_was_shown_is_not_deleted,
    a_session_of_the_same_key_at_another_location_is_left_alone,
    a_session_merged_with_one_in_trash_stays_in_trash,
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

    let deleted = bench.archive().delete_for_good(&deletion).unwrap();
    /* Its items are removed in the background, and are pending until they are. */
    assert_eq!(
        (deleted.deletion, deleted.pending),
        (deletion.clone(), deletion.files)
    );
    bench.archive().wait_for_removals();
    assert_eq!(bench.archive().pending_removals().unwrap(), 0);

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
    assert!(tombstones[0].deleted > 0);

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
        bench.delete_for_good(&id),
        Err(ArchiveError::Catalogue(CatalogueError::NotInTrash(_)))
    ));
    bench.archive().catalogue().trash(&id).unwrap();

    /* A pass of its location is under way. */
    let refused = AtomicBool::new(false);
    let archive = bench.archive();
    let shown = archive.catalogue().deletion(&id).unwrap();
    bench
        .pass_with(bench.meddling(Meddle::at(When::AfterScan, || {
            refused.store(
                matches!(
                    archive.delete_for_good(&shown),
                    Err(ArchiveError::Syncing(_))
                ),
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
    assert!(bench.delete_for_good(&nothing).is_err());
    assert!(bench.delete_for_good(&id).is_ok());
}

fn a_session_deleted_for_good_stays_gone_until_its_tombstone_is_removed<A: Agent + Default>() {
    let mut agent = A::default();
    let keys = [agent.add_session(3), agent.add_session(3)];
    let mut bench = Bench::new(agent);
    bench.sync();
    let id = trashed(&bench, &keys[0]);
    let content = bench.agent.content(&keys[0]);
    bench.delete_for_good(&id).unwrap();

    /* The next sync skips it, and so does one after the agent went on with it. */
    let synced = bench.sync();
    assert_eq!((synced.deleted, synced.new_items), (1, 0));
    assert_eq!(synced.deleted_sessions, std::slice::from_ref(&id));
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
    bench.delete_for_good(&id).unwrap();
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
    bench.delete_for_good(&id).unwrap();
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
    bench.delete_for_good(&id).unwrap();

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
    let deletion = bench.delete_for_good(&id).unwrap().deletion;
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
    bench.delete_for_good(&id).unwrap();
    /* Its transcript and the new subagent's, with its metadata. */
    session.subagent(3);
    let synced = bench.sync();
    assert_eq!((synced.deleted, synced.new_items), (3, 0));
    assert!(bench.items().is_empty());
}

/// The files under `dir` that hold `bytes`, a segment's once it is decompressed.
fn held_anywhere(dir: &Path, bytes: &[u8]) -> Vec<PathBuf> {
    common::files_under(dir)
        .into_iter()
        .filter(|file| holds(file, bytes))
        .collect()
}

/// Whether the file `file` holds `bytes`; a segment, once it is decompressed too. The archive's
/// own lock is passed over where it can't be read: on Windows a file locked whole keeps every
/// other reader out for as long as the archive is open, and nothing is ever written to it.
fn holds(file: &Path, bytes: &[u8]) -> bool {
    let read = match fs::read(file) {
        Ok(read) => read,
        Err(_) if file.file_name().is_some_and(|name| name == "lock") => return false,
        Err(error) => panic!("{}: {error}", file.display()),
    };
    let decompressed = file
        .extension()
        .is_some_and(|extension| extension == "zst")
        .then(|| zstd::decode_all(&read[..]).unwrap_or_default());
    [Some(read), decompressed]
        .into_iter()
        .flatten()
        .any(|content| content.windows(bytes.len()).any(|run| run == bytes))
}

/**
 * A session is often deleted because it holds a secret, and often it is typed first. Once the
 * session is deleted for good, no file of the archive holds it, its segments decompressed, nor
 * a run of three characters of it the search index was made of; not the catalogue, its log or
 * the tombstone, though with no custom title the session's title was its secret. Only a backup
 * of the catalogue made before the delete holds it, and one made after does not.
 */
fn deleting_for_good_leaves_no_text_of_the_session_in_any_file<A: Agent + Default>() {
    let mut agent = A::default();
    let kept = agent.add_session(2);
    let doomed = agent.add_session(0);
    let secret = "qzxjvkwqzxjvkw";
    agent.exchange(
        &doomed,
        &format!("{secret} is the staging key; rotate it"),
        &format!("export TOKEN={secret}"),
        "",
        &format!("Done: {secret} is set."),
    );
    let mut bench = Bench::new(agent);
    bench.sync();
    /* Today's backup is made at a start. */
    bench.reopen();
    let dir = bench.dir().to_path_buf();
    let backups = common::files_under(&dir.join("backups"));
    assert_eq!(backups.len(), 1);
    assert!(holds(&backups[0], secret.as_bytes()));
    let id = trashed(&bench, &doomed);
    assert!(bench.session(&doomed).title.unwrap().starts_with(secret));
    assert!(!held_anywhere(&dir.join("files"), secret.as_bytes()).is_empty());

    bench.delete_for_good(&id).unwrap();
    let holding: Vec<PathBuf> = held_anywhere(&dir, secret.as_bytes())
        .into_iter()
        .filter(|file| !backups.contains(file))
        .collect();
    assert_eq!(holding, Vec::<PathBuf>::new());
    /* Nor does the search index keep a run of three characters of it, nor its log. */
    let search: Vec<PathBuf> = common::files_under(&dir)
        .into_iter()
        .filter(|file| {
            file.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("search.db"))
        })
        .collect();
    assert!(!search.is_empty());
    for run in secret.as_bytes().windows(3) {
        for file in &search {
            assert!(
                !holds(file, run),
                "{} in {}",
                String::from_utf8_lossy(run),
                file.display()
            );
        }
    }
    let tombstone = &bench.archive().catalogue().tombstones().unwrap()[0];
    assert_eq!(tombstone.session, id);

    /* A backup made after the delete does not hold it. */
    fs::remove_file(&backups[0]).unwrap();
    bench.reopen();
    let after = common::files_under(&dir.join("backups"));
    assert_eq!(after.len(), 1);
    assert!(!holds(&after[0], secret.as_bytes()));
    bench.assert_mirrors(&[kept]);
}

/**
 * A deleted item's folder that can't be removed (another program holds a file in it) stays
 * pending: verify says so apart from what the catalogue doesn't record, setting strays aside
 * leaves it, the delete says how many files are left, and the next start removes it.
 */
fn a_removal_that_fails_stays_pending_and_is_no_stray<A: Agent + Default>() {
    let mut agent = A::default();
    let key = agent.add_session(3);
    let mut bench = Bench::new(agent);
    bench.sync();
    let id = trashed(&bench, &key);
    /* Where the item's folder is, a file: no removal of a folder removes it. */
    let segment = bench.segment_files_of(&key).remove(0);
    let item = segment.parent().unwrap().parent().unwrap().to_path_buf();
    fs::remove_dir_all(&item).unwrap();
    fs::write(&item, b"held").unwrap();

    bench.delete_for_good(&id).unwrap();
    assert_eq!(bench.archive().pending_removals().unwrap(), 1);
    let report = bench
        .archive()
        .verify(&sb_core::CancelToken::new())
        .unwrap();
    assert_eq!(report.uncatalogued, Vec::<PathBuf>::new());
    assert_eq!(report.removing, [item.strip_prefix(bench.dir()).unwrap()]);
    assert_eq!(
        bench.archive().set_aside_uncatalogued().unwrap(),
        Vec::<PathBuf>::new()
    );
    assert!(item.exists() && bench.orphans().is_empty());

    /* The other program lets go: the next start removes it. */
    fs::remove_file(&item).unwrap();
    bench.reopen();
    bench.assert_verifies();
    assert!(bench.segment_files().is_empty());

    /* Where the operating system refuses the removal of the very bytes, they stay too, until
    it lets them go; and then the next start removes them. And nothing is removed through a
    link: the item stays to be removed, and what the link leads to stays as it is. */
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut agent = A::default();
        let keys = [agent.add_session(3), agent.add_session(3)];
        let mut bench = Bench::new(agent);
        bench.sync();
        let segment = bench.segment_files_of(&keys[0]).remove(0);
        let folder = segment.parent().unwrap().to_path_buf();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o500)).unwrap();
        bench.delete_for_good(&trashed(&bench, &keys[0])).unwrap();
        assert!(bench.archive().pending_removals().unwrap() == 1 && segment.exists());
        assert_eq!(
            bench.archive().set_aside_uncatalogued().unwrap(),
            Vec::<PathBuf>::new()
        );
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
        bench.reopen();
        assert!(!segment.exists() && bench.orphans().is_empty());
        bench.assert_verifies();

        let segment = bench.segment_files_of(&keys[1]).remove(0);
        let folder = segment.parent().unwrap().to_path_buf();
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("v0001");
        fs::rename(&folder, &target).unwrap();
        std::os::unix::fs::symlink(&target, &folder).unwrap();
        bench.delete_for_good(&trashed(&bench, &keys[1])).unwrap();
        assert_eq!(bench.archive().pending_removals().unwrap(), 1);
        assert!(target.join(segment.file_name().unwrap()).is_file());
        let report = bench
            .archive()
            .verify(&sb_core::CancelToken::new())
            .unwrap();
        assert_eq!(report.removing.len(), 1);
        fs::remove_file(&folder).unwrap();
        bench.reopen();
        assert!(target.join(segment.file_name().unwrap()).is_file());
        bench.assert_verifies();
    }
}

/**
 * Everything in a deleted item's folder goes while the catalogue records no item there,
 * whatever its bytes: a damaged segment of the session is the session's all the same.
 */
fn a_damaged_segment_of_a_deleted_session_goes_too<A: Agent + Default>() {
    let mut agent = A::default();
    let key = agent.add_session(3);
    let bench = Bench::new(agent);
    bench.sync();
    let segment = bench.segment_files_of(&key).remove(0);
    let mut bytes = fs::read(&segment).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(&segment, bytes).unwrap();
    bench.delete_for_good(&trashed(&bench, &key)).unwrap();
    assert_eq!(bench.archive().pending_removals().unwrap(), 0);
    assert!(!segment.exists() && bench.orphans().is_empty());
    bench.assert_verifies();
    assert_eq!(
        bench.archive().set_aside_uncatalogued().unwrap(),
        Vec::<PathBuf>::new()
    );
}

/// What verify's strays were set aside from a deleted item's folder before the delete are
/// that session's bytes: they go with it, wherever setting aside numbered their path. What
/// was set aside of another item stays.
fn what_was_set_aside_of_a_deleted_item_goes_with_it<A: Agent + Default>() {
    let mut agent = A::default();
    let keys = [agent.add_session(3), agent.add_session(3)];
    let bench = Bench::new(agent);
    bench.sync();
    let orphans = bench.dir().join("orphans");
    /* A copy of the item's segment set aside under `orphans/`, with its path's part at
    `numbered` (counted from files/) given the number `number`. */
    let set_aside = |key: &str, numbered: usize, number: &str| {
        let segment = bench.segment_files_of(key).remove(0);
        let relative = segment.strip_prefix(bench.dir()).unwrap();
        let mut copy = orphans.clone();
        for (at, part) in relative.iter().enumerate() {
            let mut part = part.to_os_string();
            if at == numbered {
                part.push(number);
            }
            copy.push(part);
        }
        fs::create_dir_all(copy.parent().unwrap()).unwrap();
        fs::copy(&segment, &copy).unwrap();
    };
    set_aside(&keys[0], 4, "");
    set_aside(&keys[0], 4, ".1");
    set_aside(&keys[0], 0, ".2");
    set_aside(&keys[0], 1, ".1");
    set_aside(&keys[0], 6, ".3");
    set_aside(&keys[1], 4, "");
    assert_eq!(bench.orphans().len(), 6);
    let id = trashed(&bench, &keys[0]);
    bench.delete_for_good(&id).unwrap();
    let left = bench.orphans();
    assert_eq!(left.len(), 1);
    let other = Hash::of(keys[1].as_bytes()).to_hex();
    assert!(left[0].iter().any(|part| part == &other[..32]));
}

/**
 * What is deleted is what the confirmation showed: a session that gained bytes since is not
 * deleted, and says so; shown again, it is. What the confirmation shows is read from the
 * session's items, so it is the same while its index row is missing.
 */
fn a_session_that_changed_since_it_was_shown_is_not_deleted<A: Agent + Default>() {
    let mut agent = A::default();
    let key = agent.add_session(3);
    let mut bench = Bench::new(agent);
    bench.sync();
    let id = trashed(&bench, &key);
    let catalogue = bench.archive().catalogue();
    let shown = catalogue.deletion(&id).unwrap();
    assert_eq!(shown.versions, 1);
    catalogue.drop_index().unwrap();
    assert_eq!(catalogue.deletion(&id).unwrap(), shown);
    bench.index();

    bench.agent.append(&key, 2);
    bench.sync();
    let files = snapshot(&bench.dir().join("files"));
    assert!(matches!(
        bench.archive().delete_for_good(&shown),
        Err(ArchiveError::Catalogue(CatalogueError::DeletionChanged(_)))
    ));
    assert!(snapshot(&bench.dir().join("files")) == files);
    assert!(bench.archive().catalogue().tombstones().unwrap().is_empty());
    let shown = bench.archive().catalogue().deletion(&id).unwrap();
    assert!(bench.archive().delete_for_good(&shown).is_ok());
    bench.archive().wait_for_removals();
    assert!(bench.segment_files().is_empty());
}

/**
 * Another location can hold a session of the same key: the same agent's session ids at two
 * machines. Deleting one leaves the other's rows and files as they were, and the other's
 * syncs go on.
 */
fn a_session_of_the_same_key_at_another_location_is_left_alone<A: Agent + Default>() {
    let mut agent = A::default();
    let key = agent.add_session(3);
    let bench = Bench::new(agent);
    let mut there = A::default();
    assert_eq!(there.add_session(3), key);
    let elsewhere = bench
        .archive()
        .catalogue()
        .add_location(sb_core::location::LocationKind::Local, "elsewhere")
        .unwrap()
        .id;
    let sync_there = |bench: &Bench<A>, there: &A| {
        sb_core::sync::sync_location(
            bench.archive(),
            &elsewhere,
            &there.transport(),
            &[there.provider()],
            &sb_core::CancelToken::new(),
            &mut |_| {},
        )
        .unwrap()
    };
    bench.sync();
    sync_there(&bench, &there);
    let other = SessionId {
        location: elsewhere.clone(),
        ..bench.session_id(&key)
    };
    let its_rows = rows_of(bench.dir(), &other);
    let its_files: BTreeMap<_, _> = snapshot(&bench.dir().join("files"))
        .into_iter()
        .filter(|(path, _)| path.starts_with(elsewhere.as_str()))
        .collect();
    assert!(!its_files.is_empty());

    let id = trashed(&bench, &key);
    bench.delete_for_good(&id).unwrap();
    assert_eq!(rows_of(bench.dir(), &other), its_rows);
    let files: BTreeMap<_, _> = snapshot(&bench.dir().join("files"))
        .into_iter()
        .filter(|(path, _)| path.starts_with(elsewhere.as_str()))
        .collect();
    assert!(files == its_files);
    assert!(
        bench
            .archive()
            .catalogue()
            .session(&other)
            .unwrap()
            .is_some()
    );

    there.append(&key, 2);
    let passes = sync_there(&bench, &there);
    let report = passes[0].outcome.as_ref().unwrap();
    assert_eq!((report.appended, report.deleted), (1, 0));
    assert_eq!(bench.sync().deleted, 1);
    bench.assert_verifies();
}

/// When a newer parser makes one session of two and one of them was in Trash, the one there
/// is now is in Trash: out of sight until the owner says otherwise.
fn a_session_merged_with_one_in_trash_stays_in_trash<A: Agent + Default>() {
    let mut agent = A::default();
    let keys = [agent.add_session(3), agent.add_session(3)];
    let bench = Bench::new(agent);
    bench.sync();
    let catalogue = bench.archive().catalogue();
    catalogue
        .set_starred(&bench.session_id(&keys[0]), true)
        .unwrap();
    trashed(&bench, &keys[1]);
    let merged = || {
        bench.meddling(Meddle::Classifier(|_, (kind, session)| {
            (kind, session.map(|_| "merged".to_string()))
        }))
    };
    bench.index_with(merged());
    let sessions = catalogue.sessions(Shelf::Trash).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id.key, "merged");
    assert!(catalogue.sessions(Shelf::All).unwrap().is_empty());
}

/**
 * A read under way holds a database's log, which a delete for good empties so its text is in
 * no file: the delete does not wait on it for more than a moment, and the next index, once the
 * read is over, empties the log.
 */
fn a_read_that_holds_a_log_neither_stalls_a_delete_nor_keeps_its_text<A: Agent + Default>() {
    use toolbox_store::rusqlite::Connection;
    let mut agent = A::default();
    let kept = agent.add_session(2);
    let doomed = agent.add_session(0);
    let secret = "qzxjvkwqzxjvkw";
    agent.exchange(&doomed, &format!("{secret} again"), "true", "", "Fine.");
    let bench = Bench::new(agent);
    bench.sync();
    let id = trashed(&bench, &doomed);
    let dir = bench.dir().to_path_buf();
    let readers = ["scrollback.db", "search.db"].map(|name| {
        let reader = Connection::open(dir.join(name)).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let _: i64 = reader
            .query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
            .unwrap();
        reader
    });
    let started = std::time::Instant::now();
    bench.delete_for_good(&id).unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    drop(readers);
    bench.index();
    for name in ["scrollback.db-wal", "search.db-wal"] {
        let log = dir.join(name);
        assert!(
            !log.exists() || fs::metadata(&log).unwrap().len() == 0,
            "{name}"
        );
    }
    assert_eq!(
        held_anywhere(&dir, secret.as_bytes()),
        Vec::<PathBuf>::new()
    );
    bench.assert_mirrors(&[kept]);
}
