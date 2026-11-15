//! The catalogue's schema (PLAN §4.3), as `toolbox_store` migrations: append-only, a shipped
//! step is never edited. Tables that later phases own (folders, session state, tombstones, the
//! session index and its search text, settings) arrive as further steps with those phases.

use toolbox_store::Schema;

pub(super) const SCHEMA: Schema = Schema {
    domain: "scrollback",
    steps: &[SYNC, LOCATIONS],
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

/// WSL and SSH locations (PLAN §6), and a location that can't be reached (PLAN §5).
const LOCATIONS: &str = "
-- What a location's kind connects to: the WSL distro's name or the host as ssh takes it. NULL
-- for this machine.
ALTER TABLE location ADD COLUMN detail TEXT;
-- Set while the last sync could not connect: since which try, and the launcher's words.
ALTER TABLE location ADD COLUMN unreachable_since INTEGER;
ALTER TABLE location ADD COLUMN unreachable_reason TEXT;
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
            steps: &[SYNC],
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
}
