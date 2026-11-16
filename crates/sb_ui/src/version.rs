//! What this build of scrollback is (PLAN §13, phase 7): a release `script/package` built, with
//! its version and channel baked in at compile time (`SCROLLBACK_VERSION`,
//! `SCROLLBACK_CHANNEL`), or a development build of the workspace's version. `--version` and
//! Settings say which; only a packaged build ever looks for updates (`crate::services::updates`).

use std::sync::OnceLock;

/// The channel `script/package` built this for (`release`), `None` in a development build.
const CHANNEL: Option<&str> = match option_env!("SCROLLBACK_CHANNEL") {
    Some(channel) if !channel.is_empty() => Some(channel),
    _ => None,
};

/// This build's version: a packaged release's own, else the workspace's.
pub const VERSION: &str = match option_env!("SCROLLBACK_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};

/// Whether `script/package` built this.
pub const fn is_packaged() -> bool {
    CHANNEL.is_some()
}

/**
 * What `--version` prints after `scrollback `: the version, and `(development build)` unless
 * this is a packaged one.
 */
pub fn text() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        if is_packaged() {
            VERSION.to_string()
        } else {
            format!("{VERSION} (development build)")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests are built by cargo, never by `script/package`.
    #[test]
    fn a_test_build_is_a_development_build() {
        assert!(!is_packaged());
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            text(),
            format!("{} (development build)", env!("CARGO_PKG_VERSION"))
        );
    }
}
