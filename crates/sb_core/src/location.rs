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
            /* ssh lowercases a host's name; a user's is as typed. */
            (Self::Ssh { host }, Self::Ssh { host: other }) => {
                let split = |host: &str| match host.rsplit_once('@') {
                    Some((user, host)) => (Some(user.to_string()), host.to_ascii_lowercase()),
                    None => (None, host.to_ascii_lowercase()),
                };
                split(host) == split(other)
            }
            _ => self == other,
        }
    }

    /// What starts the shell of a WSL or SSH location: what its connection test runs, and
    /// shows (DESIGN §4.12). `None` for this machine, which is read without one.
    pub fn launcher(&self) -> Option<Launcher> {
        match self {
            Self::Local => None,
            Self::Wsl { distro } => Some(Launcher::Wsl {
                distro: distro.clone(),
            }),
            Self::Ssh { host } => Some(Launcher::Ssh { host: host.clone() }),
        }
    }

    /// The transport that reads a location of this kind. Cancelling `cancel` stops what a
    /// WSL or SSH location's transport is running.
    pub fn transport(&self, cancel: &CancelToken) -> Box<dyn Transport> {
        match self.launcher() {
            None => Box::new(LocalTransport),
            Some(launcher) => Box::new(ShellTransport::new(launcher).cancelled_by(cancel)),
        }
    }
}

/// The most characters a host may have: a name as DNS allows it, with a user and brackets.
const HOST_CHARS: usize = 320;

/**
 * What is wrong with `host` as a host to hand `ssh` (PLAN §6.4), if anything, in a sentence
 * that names the character. Only what a host or an alias can hold is taken: `[A-Za-z0-9._-]`
 * and `:` for the host, the same and `+` for a user before an `@`, and an IPv6 address in
 * brackets with `%` only for its zone. Anything else could reach a shell through an older
 * ssh's `ProxyCommand` and `%h`, or show the owner one host and connect to another (an
 * invisible or right-to-left character). Every way to a host is checked by this: adding a
 * location, and starting its launcher.
 */
pub fn host_problem(host: &str) -> Option<String> {
    let refused = |c: char| {
        Some(if c == ' ' {
            "A host can't hold a space.".to_string()
        } else if c.is_ascii_graphic() {
            format!("A host can't hold \u{201c}{c}\u{201d}.")
        } else {
            format!("A host can't hold the character U+{:04X}.", u32::from(c))
        })
    };
    let name = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    if host.is_empty() {
        return Some("Type a host, as you would after ssh.".to_string());
    }
    if host.chars().count() > HOST_CHARS {
        return Some(format!("A host has at most {HOST_CHARS} characters."));
    }
    let (user, at) = match host.rsplit_once('@') {
        Some((user, at)) => (Some(user), at),
        None => (None, host),
    };
    if let Some(user) = user {
        if user.is_empty() {
            return Some("A user's name goes before the @.".to_string());
        }
        if let Some(c) = user.chars().find(|c| !(name(*c) || *c == '+')) {
            return refused(c);
        }
    }
    if host.starts_with('-') || at.starts_with('-') {
        return Some("A host can't start with a dash.".to_string());
    }
    if at.is_empty() {
        return Some("A host goes after the @.".to_string());
    }
    match at.strip_prefix('[') {
        Some(inside) => {
            let Some(inside) = inside.strip_suffix(']') else {
                return Some("An address in brackets ends with ].".to_string());
            };
            let (address, zone) = inside.split_once('%').unwrap_or((inside, ""));
            if address.is_empty() {
                return Some("An address goes between the brackets.".to_string());
            }
            address
                .chars()
                .find(|c| !(c.is_ascii_hexdigit() || ":.".contains(*c)))
                .or_else(|| zone.chars().find(|c| !name(*c)))
                .and_then(refused)
        }
        None => at
            .chars()
            .find(|c| !(name(*c) || *c == ':'))
            .and_then(refused),
    }
}

/// The most characters a location's name may have.
pub const NAME_CHARS: usize = 80;

/**
 * A location's name as the owner typed it, fit to keep: without control characters and the
 * invisible ones that format text (a right-to-left override, a zero-width space), trimmed,
 * and no longer than [`NAME_CHARS`]. `None` when nothing is left.
 */
pub fn clean_name(name: &str) -> Option<String> {
    let invisible = |c: char| {
        matches!(
            u32::from(c),
            0xAD | 0x61C | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064
                | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB
        )
    };
    let kept: String = name
        .chars()
        .filter(|c| !c.is_control() && !invisible(*c))
        .collect();
    let kept: String = kept.trim().chars().take(NAME_CHARS).collect();
    let kept = kept.trim_end().to_string();
    (!kept.is_empty()).then_some(kept)
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
    /// When the owner removed it: it is synced no more, and what was archived from it stays
    /// (FEATURES: Locations). `None` while it is one of the owner's locations.
    pub removed: Option<i64>,
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
        assert!(ssh("user@macbook").same_place(&ssh("user@macbook")));
        assert!(!wsl("macbook").same_place(&ssh("macbook")));
        assert!(LocationKind::Local.same_place(&LocationKind::Local));
        /* ssh lowercases a host's name, and takes a user's as typed. */
        assert!(ssh("user@MacBook").same_place(&ssh("user@macbook")));
        assert!(ssh("MacBook").same_place(&ssh("macbook")));
        assert!(!ssh("User@macbook").same_place(&ssh("user@macbook")));
        assert!(!ssh("macbook").same_place(&ssh("user@macbook")));
    }

    #[test]
    fn a_host_holds_only_what_a_host_or_an_alias_can() {
        for good in [
            "macbook",
            "user@macbook",
            "user.name+tag@mac-book.local",
            "build_box",
            "10.0.0.7",
            "fe80::1",
            "user@[fe80::1%eth0]",
            "[::1]",
        ] {
            assert_eq!(host_problem(good), None, "{good}");
        }
        for (bad, says) in [
            ("", "Type a host, as you would after ssh."),
            ("-oProxyCommand=x", "A host can't start with a dash."),
            ("user@-x", "A host can't start with a dash."),
            ("@macbook", "A user's name goes before the @."),
            ("user@", "A host goes after the @."),
            ("mac book", "A host can't hold a space."),
            ("$(touch x)", "A host can't hold \u{201c}$\u{201d}."),
            ("mac`id`", "A host can't hold \u{201c}`\u{201d}."),
            ("a;b", "A host can't hold \u{201c};\u{201d}."),
            ("a|b", "A host can't hold \u{201c}|\u{201d}."),
            ("'mac'", "A host can't hold \u{201c}'\u{201d}."),
            ("mac%h", "A host can't hold \u{201c}%\u{201d}."),
            (
                "user@\u{202e}koob\u{202c}cam",
                "A host can't hold the character U+202E.",
            ),
            ("mac\u{200b}book", "A host can't hold the character U+200B."),
            ("mac\tbook", "A host can't hold the character U+0009."),
            ("[fe80::1", "An address in brackets ends with ]."),
            ("[fe80::g]", "A host can't hold \u{201c}g\u{201d}."),
        ] {
            assert_eq!(host_problem(bad).as_deref(), Some(says), "{bad:?}");
        }
        assert!(host_problem(&"a".repeat(HOST_CHARS + 1)).is_some());
    }

    #[test]
    fn a_name_is_kept_without_what_cant_be_seen_and_not_too_long() {
        assert_eq!(clean_name("  Work Mac "), Some("Work Mac".to_string()));
        assert_eq!(
            clean_name("Work\u{202e}Mac\u{200b}"),
            Some("WorkMac".to_string())
        );
        assert_eq!(clean_name("a\tb\n"), Some("ab".to_string()));
        assert_eq!(clean_name(" \u{200b} "), None);
        assert_eq!(
            clean_name(&"x".repeat(400)).map(|it| it.len()),
            Some(NAME_CHARS)
        );
    }
}
