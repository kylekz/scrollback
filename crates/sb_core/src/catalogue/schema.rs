//! The catalogue's schema (PLAN §4.3), as `toolbox_store` migrations: append-only, a shipped
//! step is never edited. Tables that later phases own (tombstones, the search text, settings)
//! arrive as further steps with those phases.

use toolbox_store::Schema;

pub(super) const SCHEMA: Schema = Schema {
    domain: "scrollback",
    steps: &[SYNC, LIBRARY],
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

/// What the library is made of (PLAN §2.2). `folder`, `folder_path` and `session_state` are the
/// owner's and only here; `session` is derived from the archive and can be emptied and rebuilt,
/// so the owner's tables name a session by its identity (location, provider and the provider's
/// id for it) and never by its row in `session`.
const LIBRARY: &str = "
-- Held back because its transcript no longer parses where the copy being read does (PLAN §4.2),
-- whatever its length: held_back stays set for as long as this is.
ALTER TABLE version ADD COLUMN held_by_parse INTEGER NOT NULL DEFAULT 0;

CREATE INDEX item_session ON item (location_id, provider, session);

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

-- by_hand: the owner put the session in its folder, and no rule moves it. reading_version is
-- the version the owner chose to read of one of the session's items, the main transcript.
CREATE TABLE session_state (
    location_id TEXT NOT NULL REFERENCES location (id),
    provider TEXT NOT NULL,
    session TEXT NOT NULL,
    folder_id INTEGER REFERENCES folder (id),
    by_hand INTEGER NOT NULL DEFAULT 0 CHECK (by_hand = 0 OR folder_id IS NOT NULL),
    starred INTEGER NOT NULL DEFAULT 0,
    reading_version INTEGER REFERENCES version (id),
    PRIMARY KEY (location_id, provider, session)
) STRICT, WITHOUT ROWID;

CREATE INDEX session_state_folder ON session_state (folder_id);

-- Derived: what the index holds of a session (PLAN §7.5). id is what other derived tables name
-- the session by (the search text, PLAN §10). main_item is the session's own transcript, NULL
-- when its items hold none. project is project_path as filing compares it.
-- models is a JSON array. The own_ counts are of the session's own conversation, the all_
-- counts of it and its subagents. ending is how the main transcript ended (whole, mid_record,
-- unreadable), with why in unreadable. failure is why the session could not be parsed at all;
-- the other derived columns are then empty. parser_version, index_version and indexed_from
-- say what the row was made by and from: a session is indexed again when any of them is no
-- longer so.
CREATE TABLE session (
    id INTEGER PRIMARY KEY,
    location_id TEXT NOT NULL REFERENCES location (id),
    provider TEXT NOT NULL,
    session TEXT NOT NULL,
    main_item INTEGER REFERENCES item (id),
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
    failure TEXT,
    parser_version INTEGER NOT NULL,
    index_version INTEGER NOT NULL,
    indexed_from TEXT NOT NULL,
    UNIQUE (location_id, provider, session)
) STRICT;
";
