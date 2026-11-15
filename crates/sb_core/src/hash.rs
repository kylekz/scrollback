//! SHA-256 (PLAN §4.1): the hash of every segment and of every version's content. SHA-256,
//! because a remote location can compute it with standard tools (PLAN §6.2).

use std::fmt;

use sha2::{Digest as _, Sha256};
use toolbox_store::rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use toolbox_store::rusqlite::{self, ToSql};

/// A SHA-256 digest. The catalogue stores it as 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash([u8; 32]);

impl Hash {
    pub fn of(bytes: &[u8]) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(bytes);
        hasher.finish()
    }

    /// The digest `hex` spells, as `sha256sum` prints one.
    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() != 64 || !hex.is_ascii() {
            return None;
        }
        let mut bytes = [0; 32];
        for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(Self(bytes))
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl ToSql for Hash {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.to_hex()))
    }
}

impl FromSql for Hash {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        Self::from_hex(value.as_str()?).ok_or(FromSqlError::InvalidType)
    }
}

/// A SHA-256 in progress. Cloning it keeps the state so far, which is how a version's running
/// hash is read without ending it.
#[derive(Clone, Default)]
pub struct Hasher(Sha256);

impl Hasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finish(self) -> Hash {
        Hash(self.0.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_survives_its_hex_form() {
        let hash = Hash::of(b"abc");
        assert_eq!(
            hash.to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(Hash::from_hex(&hash.to_hex()), Some(hash));
        assert_eq!(Hash::from_hex("ba78"), None);
        assert_eq!(Hash::from_hex(&"zz".repeat(32)), None);
    }
}
