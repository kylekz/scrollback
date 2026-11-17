//! Where copies ran (PLAN §7.7): what the records an app keeps beside its copies say of each
//! conversation (`copy_record`), made again from the archive at each index, and where this
//! machine's `ssh` makes the hosts they name, written by each attribution.

use std::collections::HashMap;

use toolbox_store::rusqlite::{self, params};

use super::{Catalogue, Result, folder};
use crate::location::LocationId;
use crate::provider::CopyRecord;

/// The hosts an attribution is to resolve: those recorded at This PC's location, and those of
/// the SSH locations a copy can be attributed to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Hosts {
    /// Every distinct host recorded at a location of this machine, as recorded.
    pub recorded: Vec<String>,
    /// Each SSH location that is not removed, with its host, the first added first.
    pub locations: Vec<(LocationId, String)>,
}

impl Catalogue {
    /**
     * Records what `records` say, which `provider`'s records at `location` say now, in place
     * of what they said before. A conversation whose host is as it was keeps what the last
     * attribution made of it; one with another host is unresolved until the next.
     */
    pub(crate) fn record_copies(
        &self,
        location: &LocationId,
        provider: &str,
        records: &[CopyRecord],
    ) -> Result<()> {
        self.db.write(|tx| {
            let mut was: HashMap<String, Option<String>> = tx
                .prepare(
                    "SELECT conversation, host FROM copy_record
                     WHERE location_id = ?1 AND provider = ?2",
                )?
                .query_map(params![location, provider], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let mut upsert = tx.prepare(
                "INSERT INTO copy_record (location_id, provider, conversation, host, path, key)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (location_id, provider, conversation) DO UPDATE SET
                     path = excluded.path,
                     key = excluded.key,
                     resolved = CASE WHEN host IS excluded.host THEN resolved ELSE 0 END,
                     ran_at = CASE WHEN host IS excluded.host THEN ran_at END,
                     host = excluded.host",
            )?;
            for record in records {
                upsert.execute(params![
                    location,
                    provider,
                    record.conversation,
                    record.host,
                    record.path,
                    record.key
                ])?;
                was.remove(&record.conversation);
            }
            let mut forget = tx.prepare(
                "DELETE FROM copy_record
                 WHERE location_id = ?1 AND provider = ?2 AND conversation = ?3",
            )?;
            for conversation in was.keys() {
                forget.execute(params![location, provider, conversation])?;
            }
            Ok(())
        })
    }

    /// The hosts the next attribution resolves ([`Hosts`]).
    pub(crate) fn copy_hosts(&self) -> Result<Hosts> {
        self.db.read(|c| {
            let recorded = c
                .prepare(
                    "SELECT DISTINCT r.host FROM copy_record r
                     JOIN location l ON l.id = r.location_id AND l.kind = 'local'
                     WHERE r.host IS NOT NULL ORDER BY r.host",
                )?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let locations = c
                .prepare(
                    "SELECT id, detail FROM location
                     WHERE kind = 'ssh' AND removed_at IS NULL AND detail IS NOT NULL
                     ORDER BY created_at, id",
                )?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(Hosts {
                recorded,
                locations,
            })
        })
    }

    /**
     * Writes what an attribution made of each recorded host of `places`: `None` for one that
     * didn't resolve, else the location it is, if any. Records at a location other than This
     * PC's are never resolved. The sessions whose location moved are filed again by the rules
     * (PLAN §8), in the same transaction. Returns how many records changed.
     */
    pub(crate) fn attribute_copies(
        &self,
        places: &HashMap<String, Option<Option<LocationId>>>,
    ) -> Result<usize> {
        self.db.write(|tx| {
            let mut set = tx.prepare(
                "UPDATE copy_record SET resolved = ?2, ran_at = ?3
                 WHERE host = ?1 AND location_id IN (SELECT id FROM location WHERE kind = 'local')
                   AND (resolved IS NOT ?2 OR ran_at IS NOT ?3)",
            )?;
            let mut changed = 0;
            for (host, place) in places {
                changed += set.execute(params![host, place.is_some(), place.clone().flatten()])?;
            }
            /* What is not This PC's, or names no host, is never resolved. */
            changed += tx.execute(
                "UPDATE copy_record SET resolved = 0, ran_at = NULL
                 WHERE (resolved OR ran_at IS NOT NULL)
                   AND (host IS NULL
                        OR location_id NOT IN (SELECT id FROM location WHERE kind = 'local'))",
                [],
            )?;
            if changed > 0 {
                folder::refile(tx)?;
            }
            Ok(changed)
        })
    }
}
