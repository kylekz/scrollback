//! What a view asks of the system: opening a link and writing to the clipboard.
//!
//! Every view goes through here, so a run that must not reach the person's browser or
//! clipboard (automation, tests) can [`record`] instead: with a [`Recorded`] global installed
//! nothing leaves the app, and the global holds what would have.

use gpui::{App, ClipboardItem, Global};

/// What [`open_url`] and [`copy_text`] would have done, newest last.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recorded {
    pub opened: Vec<String>,
    pub copied: Vec<String>,
}

impl Global for Recorded {}

/// From now on [`open_url`] and [`copy_text`] only record into [`Recorded`].
pub fn record(cx: &mut App) {
    cx.set_global(Recorded::default());
}

/// Opens a web address in the person's browser. Anything else (`file:`, a custom scheme, a
/// relative path) is ignored: a transcript is untrusted text, and a link in it must not start a
/// program.
pub fn open_url(url: &str, cx: &mut App) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    if cx.has_global::<Recorded>() {
        cx.global_mut::<Recorded>().opened.push(url.to_string());
    } else {
        cx.open_url(url);
    }
}

/// Puts `text` on the clipboard.
pub fn copy_text(text: String, cx: &mut App) {
    if cx.has_global::<Recorded>() {
        cx.global_mut::<Recorded>().copied.push(text);
    } else {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;

    use super::*;

    #[gpui::test]
    fn a_recording_run_keeps_links_and_copies_in_the_app(cx: &mut TestAppContext) {
        cx.update(|cx| {
            record(cx);
            open_url("https://example.com/docs", cx);
            copy_text("copied".to_string(), cx);
            assert_eq!(
                cx.global::<Recorded>(),
                &Recorded {
                    opened: vec!["https://example.com/docs".to_string()],
                    copied: vec!["copied".to_string()],
                }
            );
        });
    }

    #[gpui::test]
    fn only_web_addresses_open(cx: &mut TestAppContext) {
        cx.update(|cx| {
            record(cx);
            for url in [
                "file:///etc/hosts",
                "javascript:alert(1)",
                "./notes.md",
                "app://open",
            ] {
                open_url(url, cx);
            }
            assert_eq!(cx.global::<Recorded>().opened, Vec::<String>::new());
        });
    }
}
