//! The catalogue's schema (PLAN §4.3), as `toolbox_store` migrations: append-only, a shipped
//! step is never edited. Tables that later phases own arrive as further steps with those
//! phases.

use toolbox_store::Schema;

pub(super) const SCHEMA: Schema = Schema {
    domain: "scrollback",
    steps: &[
        SYNC,
        LIBRARY,
        LOCATIONS,
        SEARCH_AND_DELETING,
        REMOVED,
        CHECKS,
        SETTINGS,
        COPIES,
    ],
};

/// What sync writes (PLAN §2.2): locations, items, versions and segments. Times are Unix
/// milliseconds; hashes are SHA-256 as hex.
const SYNC: &str = "
-- The id of the archive the catalogue belongs to, as in its archive.json: one row.
CREATE TABLE archive (
    id TEXT NOT NULL
) STRICT;

CREATE TABLE location (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_attempt_at INTEGER,
    last_success_at INTEGER,
    last_error TEXT
) STRICT;

-- A provider whose agent was found at a location, and how long that agent keeps transcripts
-- there, as last read.
CREATE TABLE location_provider (
    location_id TEXT NOT NULL REFERENCES location (id),
    provider TEXT NOT NULL,
    retention_days INTEGER,
    PRIMARY KEY (location_id, provider)
) STRICT, WITHOUT ROWID;

-- item_id is the item's folder under files/: the first 32 hex digits of the SHA-256 of its key.
-- seen_length and seen_stamp are the source's state the last time the item was fully dealt with.
CREATE TABLE item (
    id INTEGER PRIMARY KEY,
    location_id TEXT NOT NULL REFERENCES location (id),
    provider TEXT NOT NULL,
    key TEXT NOT NULL,
    item_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    session TEXT,
    seen_length INTEGER NOT NULL,
    seen_stamp TEXT NOT NULL,
    gone_since INTEGER,
    UNIQUE (location_id, provider, key),
    UNIQUE (location_id, provider, item_id)
) STRICT;

-- length and hash cover the version's content so far: its segments, in order.
CREATE TABLE version (
    id INTEGER PRIMARY KEY,
    item_id INTEGER NOT NULL REFERENCES item (id),
    number INTEGER NOT NULL,
    reason TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    length INTEGER NOT NULL,
    hash TEXT NOT NULL,
    held_back INTEGER NOT NULL,
    damaged INTEGER NOT NULL DEFAULT 0,
    export_version INTEGER,
    UNIQUE (item_id, number)
) STRICT;

-- An item the last pass of its provider at its location could not archive (it kept changing,
-- or could not be read), since the pass that first could not. It need not be in item.
CREATE TABLE waiting (
    location_id TEXT NOT NULL REFERENCES location (id),
    provider TEXT NOT NULL,
    key TEXT NOT NULL,
    since INTEGER NOT NULL,
    reason TEXT NOT NULL,
    PRIMARY KEY (location_id, provider, key)
) STRICT, WITHOUT ROWID;

CREATE TABLE segment (
    version_id INTEGER NOT NULL REFERENCES version (id),
    offset INTEGER NOT NULL,
    length INTEGER NOT NULL,
    hash TEXT NOT NULL,
    stored_at INTEGER NOT NULL,
    PRIMARY KEY (version_id, offset)
) STRICT, WITHOUT ROWID;
";

/// What the library is made of (PLAN §2.2). `folder`, `folder_path`, `session_state` and
/// `reading_choice` are the owner's and only here; `session` is derived from the archive and can
/// be emptied and rebuilt, so the owner's tables name a session by its identity (location,
/// provider and the provider's id for it) and never by its row in `session`.
const LIBRARY: &str = "
-- Held back because its transcript no longer parses where the copy being read does (PLAN §4.2),
-- whatever its length: held_back stays set for as long as this is.
ALTER TABLE version ADD COLUMN held_by_parse INTEGER NOT NULL DEFAULT 0
    CHECK (held_by_parse = 0 OR held_back = 1);

-- The owner chose to read the version while it was held back. It is not held back again.
ALTER TABLE version ADD COLUMN accepted INTEGER NOT NULL DEFAULT 0
    CHECK (accepted = 0 OR held_back = 0);

-- The version of the provider's parser whose classifier last said what each archived item of
-- the provider at the location is (its kind and its session).
ALTER TABLE location_provider ADD COLUMN classified_by INTEGER;

-- A session's items, by key.
CREATE INDEX item_session ON item (location_id, provider, session, key);

-- position orders a folder among those with the same parent.
CREATE TABLE folder (
    id INTEGER PRIMARY KEY,
    parent_id INTEGER REFERENCES folder (id),
    name TEXT NOT NULL,
    position INTEGER NOT NULL
) STRICT;

-- path is the path as filing compares it, typed as the owner gave it. location_id limits the
-- path to one location's sessions; NULL is every location's.
CREATE TABLE folder_path (
    folder_id INTEGER NOT NULL REFERENCES folder (id),
    path TEXT NOT NULL,
    typed TEXT NOT NULL,
    location_id TEXT REFERENCES location (id)
) STRICT;

-- A path is attached once for each scope. NULLs are distinct to UNIQUE, so the scope of every
-- location is indexed as the empty text, which no location's id is.
CREATE UNIQUE INDEX folder_path_scope ON folder_path (path, COALESCE(location_id, ''));

-- by_hand: the owner put the session in its folder, and no rule moves it.
CREATE TABLE session_state (
    location_id TEXT NOT NULL REFERENCES location (id),
    provider TEXT NOT NULL,
    session TEXT NOT NULL,
    folder_id INTEGER REFERENCES folder (id),
    by_hand INTEGER NOT NULL DEFAULT 0 CHECK (by_hand = 0 OR folder_id IS NOT NULL),
    starred INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (location_id, provider, session)
) STRICT, WITHOUT ROWID;

CREATE INDEX session_state_folder ON session_state (folder_id);

-- The version of an item the owner chose to read in place of its newest. It goes when the
-- item gets a newer version than there was to choose from.
CREATE TABLE reading_choice (
    item_id INTEGER PRIMARY KEY REFERENCES item (id),
    version_id INTEGER NOT NULL REFERENCES version (id)
) STRICT;

-- Derived: what the index holds of a session (PLAN §7.5). id is what other derived tables name
-- the session by (the search text, PLAN §10). main_item is the session's own transcript, NULL
-- when its items hold none; reading_version and newest_version are the numbers of the version
-- of it that is read and of its newest. project is project_path as filing compares it. models
-- is a JSON array. The own_ counts are of the session's own conversation, the all_ counts of
-- it and its subagents. ending is how the main transcript ended (whole, mid_record,
-- unreadable), with why in unreadable. unread: some part of the session, its own conversation
-- or a subagent's or what goes with them, could not be read. held_back: the newest version of
-- one of its transcripts is held back. failure is why the last index of the session did not
-- parse it; the other derived columns are then what the index before it found.
-- parser_version, index_version and indexed_from say what the row was made by and from: a
-- session is indexed again when any of them is no longer so. retry: the archive could not be
-- read when the row was made, and an index of a start tries again.
CREATE TABLE session (
    id INTEGER PRIMARY KEY,
    location_id TEXT NOT NULL REFERENCES location (id),
    provider TEXT NOT NULL,
    session TEXT NOT NULL,
    main_item INTEGER REFERENCES item (id),
    reading_version INTEGER,
    newest_version INTEGER,
    title TEXT,
    first_prompt TEXT,
    project_path TEXT,
    project TEXT,
    branch TEXT,
    entrypoint TEXT,
    started_at INTEGER,
    ended_at INTEGER,
    models TEXT NOT NULL,
    own_calls INTEGER NOT NULL,
    own_input INTEGER NOT NULL,
    own_output INTEGER NOT NULL,
    own_cache_read INTEGER NOT NULL,
    own_cache_write INTEGER NOT NULL,
    all_calls INTEGER NOT NULL,
    all_input INTEGER NOT NULL,
    all_output INTEGER NOT NULL,
    all_cache_read INTEGER NOT NULL,
    all_cache_write INTEGER NOT NULL,
    ending TEXT,
    unreadable TEXT,
    unread INTEGER NOT NULL,
    held_back INTEGER NOT NULL,
    failure TEXT,
    retry INTEGER NOT NULL,
    parser_version INTEGER NOT NULL,
    index_version INTEGER NOT NULL,
    indexed_from TEXT NOT NULL,
    UNIQUE (location_id, provider, session)
) STRICT;
";
/// WSL and SSH locations (PLAN §6), and a location that can't be reached (PLAN §5).
const LOCATIONS: &str = "
-- What a location's kind connects to: the WSL distro's name or the host as ssh takes it. NULL
-- for this machine.
ALTER TABLE location ADD COLUMN detail TEXT;
-- Set while the last sync could not connect: since which try, and the launcher's words.
ALTER TABLE location ADD COLUMN unreachable_since INTEGER;
ALTER TABLE location ADD COLUMN unreachable_reason TEXT;
-- Where a WSL distro mounts the Windows drives, when it is not the usual place: as its
-- /etc/wsl.conf says, with a slash at each end. NULL for the usual place.
ALTER TABLE location ADD COLUMN drive_mount TEXT;
";

/// Trash and deleting for good (PLAN §4.5). Search's text is not here but in a database of its
/// own (`crate::search`), so the catalogue and its backups hold none of it.
const SEARCH_AND_DELETING: &str = "
-- When the owner moved the session to Trash, while it is there.
ALTER TABLE session_state ADD COLUMN trashed_at INTEGER;

-- The owner's: a session deleted for good, when, and where and when it ran, for Settings to
-- name it: nothing of what was said in it. Sync archives nothing of it again (tombstone_item)
-- until the owner removes this.
CREATE TABLE tombstone (
    location_id TEXT NOT NULL REFERENCES location (id),
    provider TEXT NOT NULL,
    session TEXT NOT NULL,
    deleted_at INTEGER NOT NULL,
    project_path TEXT,
    started_at INTEGER,
    ended_at INTEGER,
    PRIMARY KEY (location_id, provider, session)
) STRICT, WITHOUT ROWID;

-- The keys of the items a tombstoned session had when it was deleted. Sync skips an item by its
-- key whatever session its provider now says it is of, and skips the new items of a session
-- one of these keys is now of, unless the archive holds that session.
CREATE TABLE tombstone_item (
    location_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    key TEXT NOT NULL,
    session TEXT NOT NULL,
    PRIMARY KEY (location_id, provider, key),
    FOREIGN KEY (location_id, provider, session) REFERENCES tombstone (location_id, provider, session)
) STRICT, WITHOUT ROWID;

CREATE INDEX tombstone_item_session ON tombstone_item (location_id, provider, session);

-- What the foreign keys of a deleted item and its versions are checked by: the session whose
-- main transcript the item was, and the owner's choice of the version to read.
CREATE INDEX session_main_item ON session (main_item);
CREATE INDEX reading_choice_version ON reading_choice (version_id);

-- A file of a session deleted for good whose removal from files/ is not done yet, in the folder
-- of an item that is no longer in the catalogue: a segment, by its version's number and its
-- offset, with the length and SHA-256 of what it holds; or with none of those, the item's
-- source.json. A file that no longer holds what it held when it was deleted is not removed.
CREATE TABLE removal (
    location_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    folder TEXT NOT NULL,
    version INTEGER,
    offset INTEGER,
    length INTEGER,
    hash TEXT,
    CHECK ((version IS NULL) = (offset IS NULL) AND (version IS NULL) = (length IS NULL)
           AND (version IS NULL) = (hash IS NULL))
) STRICT;

CREATE INDEX removal_item ON removal (location_id, provider, folder);

-- searchable: the session's text in search.db (crate::search) is of the row as it is. A row
-- made again is not, until the second step of an index says it is. Only those that are not are
-- indexed, so an index with nothing to do reads none.
ALTER TABLE session ADD COLUMN searchable INTEGER NOT NULL DEFAULT 0;
CREATE INDEX session_unsearchable ON session (location_id, provider) WHERE searchable = 0;

-- Sessions whose text is to be taken out of search.db: their rows went (deleted for good, no
-- session any more, or the index emptied), in the transaction that took them.
CREATE TABLE text_gone (
    location_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    session TEXT NOT NULL,
    PRIMARY KEY (location_id, provider, session)
) STRICT, WITHOUT ROWID;
";

/// Locations the owner removed (FEATURES: Locations), and where each agent keeps its data at a
/// location (DESIGN §4.11).
const REMOVED: &str = "
-- When the owner removed the location: it is synced no more, and everything archived from it
-- stays. NULL while it is one of the owner's locations.
ALTER TABLE location ADD COLUMN removed_at INTEGER;
-- Where the agent keeps its data there, as the provider names it, from the last pass.
ALTER TABLE location_provider ADD COLUMN root TEXT;
";

/// The last connection test of each WSL and SSH location (DESIGN §4.11, §4.12).
const CHECKS: &str = "
-- What the location's shell said the last time it was tested: when, the sh that answered, the
-- login shell when it answered (NULL when it didn't), the tools it lacks, one per line (empty
-- for none), and how long the test took. Kept when the location is removed.
CREATE TABLE location_check (
    location_id TEXT PRIMARY KEY REFERENCES location (id),
    checked_at INTEGER NOT NULL,
    shell TEXT NOT NULL,
    login_shell TEXT,
    missing TEXT NOT NULL,
    took_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
";

/// What belongs to the archive rather than to the app that opens it (PLAN §4.3 `kv`): the
/// owner's settings for it, and what the last verify found.
const SETTINGS: &str = "
-- One row per key; the value is text that the code owning the key reads.
CREATE TABLE kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT, WITHOUT ROWID;
";

/// Copies of a session kept elsewhere (PLAN §7.7): what the library matches them to their
/// originals by, what it shows a copy as, and where a copy ran as the records an app keeps
/// beside its copies say.
const COPIES: &str = "
-- Derived: the agent's own id for the conversation, which its copies share; NULL when the
-- session's items don't say. copy_label, copy_origin and copy_ran_on are the provider's words
-- for a copy of a session that ran elsewhere, NULL for an original.
ALTER TABLE session ADD COLUMN conversation TEXT;
ALTER TABLE session ADD COLUMN copy_label TEXT;
ALTER TABLE session ADD COLUMN copy_origin TEXT CHECK ((copy_label IS NULL) = (copy_origin IS NULL));
-- The provider's words before the name of the machine a copy ran on, when that is known.
ALTER TABLE session ADD COLUMN copy_ran_on TEXT;

-- The sessions of one conversation, which the library shows once.
CREATE INDEX session_conversation ON session (provider, conversation)
    WHERE conversation IS NOT NULL;

-- The owner decided this while the session was a copy shown alone: it goes to the copy's
-- original when that is archived. What was decided of a session before it was known to be a
-- copy stays with it.
ALTER TABLE session_state ADD COLUMN as_copy INTEGER NOT NULL DEFAULT 0;

-- The conversation a session deleted for good was of, and what it was a copy as when it was
-- one: no session of that conversation is archived again, from any location. Its identity
-- alone, as the rest of a tombstone.
ALTER TABLE tombstone ADD COLUMN conversation TEXT;
ALTER TABLE tombstone ADD COLUMN copy_label TEXT;
CREATE INDEX tombstone_conversation ON tombstone (provider, conversation)
    WHERE conversation IS NOT NULL;

-- Derived from the location's register items at each index: the host a conversation ran on
-- over SSH and its transcript's path there, as the app at the location recorded them, and the
-- key the provider makes of that path (the original's item's key where it ran). resolved
-- and ran_at are the last attribution's: whether this machine's ssh resolved the host, and
-- the SSH location it is the same place as. sources: the keys of the register items that say
-- it, as JSON, which are deleted for good with the conversation. cwd is where the session
-- started there, as recorded, and wsl the WSL location that makes it, when it ran in a distro
-- reached over SSH at localhost: the only one, or the one whose home holds cwd.
CREATE TABLE copy_record (
    location_id TEXT NOT NULL REFERENCES location (id),
    provider TEXT NOT NULL,
    conversation TEXT NOT NULL,
    host TEXT,
    path TEXT,
    key TEXT,
    sources TEXT NOT NULL DEFAULT '[]',
    cwd TEXT,
    wsl TEXT REFERENCES location (id),
    resolved INTEGER NOT NULL DEFAULT 0,
    ran_at TEXT REFERENCES location (id),
    PRIMARY KEY (location_id, provider, conversation)
) STRICT, WITHOUT ROWID;
";

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use toolbox_store::{Database, OpenOptions};

    use super::*;
    use crate::catalogue::{Catalogue, DATABASE};
    use crate::location::LocationKind;

    #[test]
    fn a_catalogue_from_before_wsl_and_ssh_keeps_its_location() {
        const BEFORE: Schema = Schema {
            domain: SCHEMA.domain,
            steps: &[SYNC, LIBRARY],
        };
        let dir = tempfile::tempdir().unwrap();
        Database::open(&dir.path().join(DATABASE), &OpenOptions::new(&BEFORE))
            .unwrap()
            .write(|tx| {
                tx.execute(
                    "INSERT INTO location (id, kind, name, created_at, last_error)
                     VALUES ('a1', 'local', 'This machine', 1, 'it failed')",
                    [],
                )?;
                Ok::<_, toolbox_store::Error>(())
            })
            .unwrap();

        let catalogue = Catalogue::open(dir.path()).unwrap();
        let locations = catalogue.locations().unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(
            (&locations[0].kind, locations[0].last_error.as_deref()),
            (&LocationKind::Local, Some("it failed"))
        );
        assert_eq!(locations[0].unreachable, None);

        /* And takes the new kinds, each with what it connects to. */
        let added = [
            LocationKind::Wsl {
                distro: "Ubuntu-24.04".to_string(),
            },
            LocationKind::Ssh {
                host: "user@macbook".to_string(),
            },
        ]
        .map(|kind| catalogue.add_location(kind, "another").unwrap());
        for added in added {
            assert_eq!(catalogue.location(&added.id).unwrap(), Some(added));
        }
        assert_eq!(catalogue.locations().unwrap().len(), 3);
    }

    /**
     * A catalogue from before search and Trash keeps its sessions: none is in Trash, and the
     * row of each, made by an earlier index without its spans, is made again by the next
     * index (its index version is older) and goes as any row does.
     */
    #[test]
    fn a_catalogue_from_before_search_and_trash_keeps_its_sessions() {
        const BEFORE: Schema = Schema {
            domain: SCHEMA.domain,
            steps: &[SYNC, LIBRARY, LOCATIONS],
        };
        let dir = tempfile::tempdir().unwrap();
        Database::open(&dir.path().join(DATABASE), &OpenOptions::new(&BEFORE))
            .unwrap()
            .write(|tx| {
                tx.execute_batch(
                    "INSERT INTO location (id, kind, name, created_at) VALUES ('a1', 'local', 'here', 1);
                     INSERT INTO session (location_id, provider, session, models, own_calls,
                         own_input, own_output, own_cache_read, own_cache_write, all_calls,
                         all_input, all_output, all_cache_read, all_cache_write, unread,
                         held_back, retry, parser_version, index_version, indexed_from)
                     VALUES ('a1', 'fixture', 's', '[]', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                             1, 1, '');
                     INSERT INTO session_state (location_id, provider, session, starred)
                     VALUES ('a1', 'fixture', 's', 1);",
                )?;
                Ok::<_, toolbox_store::Error>(())
            })
            .unwrap();

        let catalogue = Catalogue::open(dir.path()).unwrap();
        let sessions = catalogue.sessions(crate::catalogue::Shelf::All).unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].starred && sessions[0].trashed.is_none());
        assert_eq!(catalogue.library_counts().unwrap().trash, 0);
        catalogue.drop_index().unwrap();
        assert!(
            catalogue
                .sessions(crate::catalogue::Shelf::All)
                .unwrap()
                .is_empty()
        );
    }

    /**
     * A catalogue from before copies (PLAN §7.7) keeps its sessions as originals, until the
     * next index makes their rows again (its index version is older).
     */
    #[test]
    fn a_catalogue_from_before_copies_keeps_its_sessions_as_originals() {
        const BEFORE: Schema = Schema {
            domain: SCHEMA.domain,
            steps: &[
                SYNC,
                LIBRARY,
                LOCATIONS,
                SEARCH_AND_DELETING,
                REMOVED,
                CHECKS,
                SETTINGS,
            ],
        };
        let dir = tempfile::tempdir().unwrap();
        Database::open(&dir.path().join(DATABASE), &OpenOptions::new(&BEFORE))
            .unwrap()
            .write(|tx| {
                tx.execute_batch(
                    "INSERT INTO location (id, kind, name, created_at) VALUES ('a1', 'local', 'here', 1);
                     INSERT INTO session (location_id, provider, session, models, own_calls,
                         own_input, own_output, own_cache_read, own_cache_write, all_calls,
                         all_input, all_output, all_cache_read, all_cache_write, unread,
                         held_back, retry, parser_version, index_version, indexed_from)
                     VALUES ('a1', 'fixture', 's', '[]', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                             1, 2, '');",
                )?;
                Ok::<_, toolbox_store::Error>(())
            })
            .unwrap();

        let catalogue = Catalogue::open(dir.path()).unwrap();
        let sessions = catalogue.sessions(crate::catalogue::Shelf::All).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].copy, None);
        assert_eq!(sessions[0].ran_at(), Some(&sessions[0].id.location));
        assert!(catalogue.kin_read(&sessions[0].id).unwrap().is_empty());
    }

    /// A catalogue from before the archive's own settings keeps what it holds, and takes them.
    #[test]
    fn a_catalogue_from_before_settings_keeps_its_locations_and_takes_settings() {
        const BEFORE: Schema = Schema {
            domain: SCHEMA.domain,
            steps: &[
                SYNC,
                LIBRARY,
                LOCATIONS,
                SEARCH_AND_DELETING,
                REMOVED,
                CHECKS,
            ],
        };
        let dir = tempfile::tempdir().unwrap();
        Database::open(&dir.path().join(DATABASE), &OpenOptions::new(&BEFORE))
            .unwrap()
            .write(|tx| {
                tx.execute(
                    "INSERT INTO location (id, kind, name, created_at)
                     VALUES ('a1', 'local', 'here', 1)",
                    [],
                )?;
                Ok::<_, toolbox_store::Error>(())
            })
            .unwrap();

        let catalogue = Catalogue::open(dir.path()).unwrap();
        assert_eq!(catalogue.locations().unwrap().len(), 1);
        assert_eq!(catalogue.setting("sync.stale_after_days").unwrap(), None);
        catalogue.set_setting("sync.stale_after_days", "7").unwrap();
        assert_eq!(
            catalogue
                .setting("sync.stale_after_days")
                .unwrap()
                .as_deref(),
            Some("7")
        );
        assert_eq!(catalogue.last_verify().unwrap(), None);
    }
}
