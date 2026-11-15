//! Locations (PLAN §6): the places transcripts are pulled from, which are this machine, a WSL
//! distro or a host over SSH.

use std::fmt;

use toolbox_process::CancelToken;
use toolbox_store::rusqlite::types::{FromSql, FromSqlResult, ToSqlOutput, ValueRef};
use toolbox_store::rusqlite::{self, ToSql};

use crate::transport::{Launcher, LocalTransport, ShellTransport, Transport};

/// A location's identity (PLAN §2.1): fixed when it is added, independent of its name. It names
/// the location's folder under the archive's `files/`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LocationId(String);

impl LocationId {
    pub(crate) fn new() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LocationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl ToSql for LocationId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl FromSql for LocationId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        String::column_result(value).map(Self)
    }
}

/// What kind of place a location is, with what it takes to connect to it. No credential is
/// among it: `ssh` brings the owner's own config, keys and agent (PLAN D7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocationKind {
    /// This machine's user.
    Local,
    /// A WSL distro's default user, by the distro's name (`Ubuntu-24.04`).
    Wsl { distro: String },
    /// A host as the owner names it to `ssh`: an alias from their `~/.ssh/config`, or
    /// `user@host`.
    Ssh { host: String },
}

impl LocationKind {
    /// The kind's name in the catalogue, and the distro or the host that goes with it.
    pub(crate) fn columns(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Local => ("local", None),
            Self::Wsl { distro } => ("wsl", Some(distro)),
            Self::Ssh { host } => ("ssh", Some(host)),
        }
    }

    /// The kind the catalogue's columns spell, if they spell one.
    pub(crate) fn from_columns(kind: &str, detail: Option<String>) -> Option<Self> {
        match (kind, detail) {
            ("local", None) => Some(Self::Local),
            ("wsl", Some(distro)) => Some(Self::Wsl { distro }),
            ("ssh", Some(host)) => Some(Self::Ssh { host }),
            _ => None,
        }
    }

    /// Whether `other` is the same place to pull from: for adding a location, where a place
    /// that is one already is not added again. `wsl.exe` takes a distro's name in any case,
    /// so two spellings of one are the same distro; a location keeps the spelling WSL lists
    /// ([`crate::transport::wsl::find`]).
    pub fn same_place(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Wsl { distro }, Self::Wsl { distro: other }) => {
                distro.eq_ignore_ascii_case(other)
            }
            _ => self == other,
        }
    }

    /// The transport that reads a location of this kind. Cancelling `cancel` stops what a
    /// WSL or SSH location's transport is running.
    pub fn transport(&self, cancel: &CancelToken) -> Box<dyn Transport> {
        let shell = |launcher| Box::new(ShellTransport::new(launcher).cancelled_by(cancel));
        match self {
            Self::Local => Box::new(LocalTransport),
            Self::Wsl { distro } => shell(Launcher::Wsl {
                distro: distro.clone(),
            }),
            Self::Ssh { host } => shell(Launcher::Ssh { host: host.clone() }),
        }
    }
}

/// A location that could not be connected to when it was last tried (PLAN §5): a machine that
/// is asleep or off the network. A normal state, not an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreachable {
    /// When a sync first could not reach it, of the tries since it was last reached.
    pub since: i64,
    /// What the launcher said the last time.
    pub reason: String,
}

/// A location as the catalogue holds it (PLAN §4.3). Times are Unix milliseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub id: LocationId,
    pub kind: LocationKind,
    pub name: String,
    /// When a sync of it last started.
    pub last_attempt: Option<i64>,
    /// When a sync of it last finished without an error.
    pub last_success: Option<i64>,
    /// Why the last sync that reached it failed, in the failing tool's words; `None` once one
    /// succeeds.
    pub last_error: Option<String>,
    /// Set while the last sync could not reach it. What is known from when it was last
    /// reached, `last_success` and `last_error`, stays as it was.
    pub unreachable: Option<Unreachable>,
    /// Where a WSL distro mounts the Windows drives, when its `/etc/wsl.conf` puts them
    /// somewhere other than `/mnt/` (PLAN §6.3), as the last sync that reached it read
    /// there: its sessions' project paths under there are the drives' (PLAN §8).
    pub drive_mount: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_distro_is_the_same_place_however_its_name_is_cased() {
        let wsl = |distro: &str| LocationKind::Wsl {
            distro: distro.to_string(),
        };
        let ssh = |host: &str| LocationKind::Ssh {
            host: host.to_string(),
        };
        assert!(wsl("Ubuntu-24.04").same_place(&wsl("ubuntu-24.04")));
        assert!(!wsl("Ubuntu-24.04").same_place(&wsl("Ubuntu-22.04")));
        /* A host is as it is typed: ssh's config tells names apart by case. */
        assert!(ssh("user@macbook").same_place(&ssh("user@macbook")));
        assert!(!ssh("user@macbook").same_place(&ssh("user@MacBook")));
        assert!(!wsl("macbook").same_place(&ssh("macbook")));
        assert!(LocationKind::Local.same_place(&LocationKind::Local));
    }
}
