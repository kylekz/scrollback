//! What belongs to the archive rather than to the app that opens it (PLAN §4.3 `kv`): settings
//! the owner made for this archive, which go with it to any app that opens it, and what its
//! last verify found ([`LastVerify`]). App settings live with the app, outside the archive.

use serde_json::{Value, json};
use toolbox_store::Kv;

use super::{Catalogue, Result};
use crate::archive::{LAST_VERIFY_VERSION, LastVerify};

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

    /**
     * What the archive's last verify that ran to its end found; `None` before the first, or
     * when what was kept is another version's record or can't be read, which is logged.
     */
    pub fn last_verify(&self) -> Result<Option<LastVerify>> {
        self.db.read(|c| {
            Ok(KV.get_parsed(c, LAST_VERIFY, |json| {
                let read = serde_json::from_str::<Value>(json)
                    .map_err(|error| error.to_string())
                    .and_then(|mut kept| {
                        let version = kept
                            .as_object_mut()
                            .and_then(|kept| kept.remove("v"))
                            .and_then(|version| version.as_u64())
                            .ok_or_else(|| "it has no version".to_string())?;
                        if version != u64::from(LAST_VERIFY_VERSION) {
                            return Err(format!("it was kept as version {version}"));
                        }
                        serde_json::from_value(kept).map_err(|error| error.to_string())
                    });
                read.map_err(|why| log::warn!("the last verify isn't read: {why}"))
                    .ok()
            })?)
        })
    }

    pub(crate) fn record_verify(&self, last: &LastVerify) -> Result<()> {
        let mut kept = serde_json::to_value(last).expect("a verify's findings always serialize");
        if let Some(fields) = kept.as_object_mut() {
            fields.insert("v".to_string(), json!(LAST_VERIFY_VERSION));
        }
        self.set_setting(LAST_VERIFY, &kept.to_string())
    }
}
