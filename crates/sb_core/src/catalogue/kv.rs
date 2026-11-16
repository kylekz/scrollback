//! What belongs to the archive rather than to the app that opens it (PLAN §4.3 `kv`): settings
//! the owner made for this archive, which go with it to any app that opens it, and what its
//! last verify found ([`LastVerify`]). App settings live with the app, outside the archive.

use toolbox_store::Kv;

use super::{Catalogue, Result};
use crate::archive::LastVerify;

const KV: Kv = Kv::new("kv");

/// The key of the last verify, kept as JSON.
const LAST_VERIFY: &str = "verify.last";

impl Catalogue {
    /// An archive setting's value as it was written; `None` when it never was.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.db.read(|c| Ok(KV.get(c, key)?))
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.db.write(|tx| Ok(KV.set(tx, key, value)?))
    }

    /// What the archive's last verify that ran to its end found; `None` before the first, or
    /// when what was kept can't be read.
    pub fn last_verify(&self) -> Result<Option<LastVerify>> {
        self.db
            .read(|c| Ok(KV.get_parsed(c, LAST_VERIFY, |json| serde_json::from_str(json).ok())?))
    }

    pub(crate) fn record_verify(&self, last: &LastVerify) -> Result<()> {
        let json = serde_json::to_string(last).expect("a verify's findings always serialize");
        self.set_setting(LAST_VERIFY, &json)
    }
}
