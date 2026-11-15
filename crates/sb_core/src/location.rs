//! Locations (PLAN §6): the places transcripts are pulled from, which are this machine, a WSL
//! distro or a host over SSH. Only this machine exists so far; the other two come with the
//! shell transport (PLAN §13, phase 5).

use std::fmt;

use toolbox_store::rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use toolbox_store::rusqlite::{self, ToSql};

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

/// What kind of place a location is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocationKind {
    /// This machine's user.
    Local,
}

impl LocationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
        }
    }
}

impl ToSql for LocationKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for LocationKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "local" => Ok(Self::Local),
            _ => Err(FromSqlError::InvalidType),
        }
    }
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
    /// Why its last sync failed, in the failing tool's words; `None` once one succeeds.
    pub last_error: Option<String>,
}
