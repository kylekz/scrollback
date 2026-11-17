//! Where copies ran (PLAN §7.7): what the records an app keeps beside its copies say of each
//! conversation (`copy_record`), made again from the archive at each index, and where this
//! machine's `ssh` makes the hosts they name, written by each attribution.

use std::collections::HashMap;

use toolbox_store::rusqlite::{self, params};

use super::session::SHOWN;
use super::{Catalogue, Result, folder};
use crate::location::LocationId;
use crate::provider::CopyRecord;

/// The hosts an attribution is to resolve: those recorded at This PC's location, and those of
/// the SSH locations a copy can be attributed to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Hosts {
    /// Every distinct host recorded at a location of this machine for a copy shown alone, as
    /// recorded.
    pub recorded: Vec<String>,
    /// Each SSH location that is not removed, with its host, the first added first.
    pub locations: Vec<(LocationId, String)>,
}

/// The `kv` key of what the copy records of `provider` at `location` were read from.
fn made_key(location: &LocationId, provider: &str) -> String {
    format!("copy_records.{}.{provider}", location.as_str())
}

/// What an attribution makes of a recorded host (PLAN §7.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// `ssh` makes no machine of it: the copy is the app's, with no host.
    Unresolved,
    /// A machine, and the SSH location it is, if one is: the copy is that location's, or
    /// shows the host.
    Machine(Option<LocationId>),
    /// This machine itself (`localhost`): the WSL distro its sshd is, which each record
    /// names (`copy_record.wsl`), or none, and never a host to show.
    ThisMachine,
}

impl Catalogue {
    /**
     * Records what `records` say, which `provider`'s records at `location` say now, in place
     * of what they said before, and that they were read from the register items `made` (a
     * fingerprint, [`Catalogue::copy_records_made`]). A conversation whose host is as it was
     * keeps what the last attribution made of it; one with another host is unresolved until
     * the next. `wsl` is, for each record, the WSL location it ran in if its host is this
     * machine's own (the only one, or the one whose home holds its cwd), for the attribution
     * to take. With `whole` false, some record could not be read: what is not among `records`
     * is kept, and `made` is not, so the next index reads them all again. A copy whose
     * location this takes away is filed again by the rules (PLAN §8), in the same transaction.
     */
    pub(crate) fn record_copies(
        &self,
        location: &LocationId,
        provider: &str,
        records: &[CopyRecord],
        wsl: &[Option<LocationId>],
        whole: bool,
        made: &str,
    ) -> Result<()> {
        self.db.write(|tx| {
            let mut was: HashMap<String, (Option<String>, bool)> = tx
                .prepare(
                    "SELECT conversation, host, ran_at IS NOT NULL FROM copy_record
                     WHERE location_id = ?1 AND provider = ?2",
                )?
                .query_map(params![location, provider], |row| {
                    Ok((row.get(0)?, (row.get(1)?, row.get(2)?)))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let mut upsert = tx.prepare(
                "INSERT INTO copy_record
                     (location_id, provider, conversation, host, path, key, sources, cwd, wsl)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (location_id, provider, conversation) DO UPDATE SET
                     path = excluded.path,
                     key = excluded.key,
                     sources = excluded.sources,
                     cwd = excluded.cwd,
                     wsl = excluded.wsl,
                     resolved = CASE WHEN host IS excluded.host THEN resolved ELSE 0 END,
                     ran_at = CASE WHEN host IS excluded.host THEN ran_at END,
                     host = excluded.host",
            )?;
            let mut moved = false;
            for (record, wsl) in records.iter().zip(wsl) {
                upsert.execute(params![
                    location,
                    provider,
                    record.conversation,
                    record.host,
                    record.path,
                    record.key,
                    serde_json::to_string(&record.sources).expect("keys always serialize"),
                    record.cwd,
                    wsl
                ])?;
                if let Some((host, attributed)) = was.remove(&record.conversation) {
                    moved |= attributed && host != record.host;
                }
            }
            if whole {
                let mut forget = tx.prepare(
                    "DELETE FROM copy_record
                     WHERE location_id = ?1 AND provider = ?2 AND conversation = ?3",
                )?;
                for (conversation, (_, attributed)) in &was {
                    forget.execute(params![location, provider, conversation])?;
                    moved |= *attributed;
                }
                tx.execute(
                    "INSERT INTO kv (key, value) VALUES (?1, ?2)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    params![made_key(location, provider), made],
                )?;
            }
            if moved {
                folder::refile(tx)?;
            }
            Ok(())
        })
    }

    /// What the copy records of `provider` at `location` were last read from, whole: a
    /// fingerprint of its register items and the parser's version ([`Catalogue::record_copies`]).
    pub(crate) fn copy_records_made(
        &self,
        location: &LocationId,
        provider: &str,
    ) -> Result<Option<String>> {
        self.setting(&made_key(location, provider))
    }

    /// The WSL locations not removed, the first added first, each with where `provider`'s
    /// agent keeps its data there, as its last pass said.
    pub(crate) fn wsl_roots(&self, provider: &str) -> Result<Vec<(LocationId, Option<String>)>> {
        self.db.read(|c| {
            Ok(c.prepare(
                "SELECT l.id, lp.root FROM location l
                 LEFT JOIN location_provider lp ON lp.location_id = l.id AND lp.provider = ?1
                 WHERE l.kind = 'wsl' AND l.removed_at IS NULL
                 ORDER BY l.created_at, l.id",
            )?
            .query_map([provider], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The hosts the next attribution resolves ([`Hosts`]).
    pub(crate) fn copy_hosts(&self) -> Result<Hosts> {
        self.db.read(|c| {
            /* Only of copies shown alone: nothing shows where a hidden one ran. */
            let recorded = c
                .prepare(&format!(
                    "SELECT DISTINCT r.host FROM copy_record r
                     JOIN location l ON l.id = r.location_id AND l.kind = 'local'
                     JOIN session s ON s.location_id = r.location_id AND s.provider = r.provider
                      AND s.conversation = r.conversation AND s.copy_label IS NOT NULL
                     WHERE r.host IS NOT NULL AND {SHOWN} ORDER BY r.host"
                ))?
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
     * Writes what an attribution made of each recorded host of `places` ([`Place`]); a host
     * not among them keeps what it had. Records at a location other than This PC's are never
     * resolved. The sessions whose location moved are filed again by the rules (PLAN §8), in
     * the same transaction. Returns how many records changed.
     */
    pub(crate) fn attribute_copies(&self, places: &HashMap<String, Place>) -> Result<usize> {
        self.db.write(|tx| {
            let mut set = tx.prepare(
                "UPDATE copy_record SET resolved = ?2, ran_at = ?3
                 WHERE host = ?1 AND location_id IN (SELECT id FROM location WHERE kind = 'local')
                   AND (resolved IS NOT ?2 OR ran_at IS NOT ?3)",
            )?;
            let mut local = tx.prepare(
                "UPDATE copy_record SET resolved = 0, ran_at = wsl
                 WHERE host = ?1 AND location_id IN (SELECT id FROM location WHERE kind = 'local')
                   AND (resolved OR ran_at IS NOT wsl)",
            )?;
            let mut changed = 0;
            for (host, place) in places {
                changed += match place {
                    Place::Unresolved => set.execute(params![host, false, None::<LocationId>])?,
                    Place::Machine(at) => set.execute(params![host, true, at])?,
                    Place::ThisMachine => local.execute([host])?,
                };
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
