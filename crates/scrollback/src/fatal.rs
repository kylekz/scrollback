//! Reporting what stops scrollback from starting, and its `--help` and `--version` (dv's
//! `fatal.rs`): `main` reports what fails before the app starts, `app` what fails inside it (the
//! window can't open, the fonts don't load).
//!
//! It is always logged. The shipped app on Windows is a windows-subsystem program (`main.rs`):
//! started from the Start menu it has no console, so there what it would print is shown in a
//! native message box instead ([`may_show_boxes`]). Its output still goes to a console or pipe
//! it was given (`scrollback.exe --version` in a terminal, `$(…)` in a script), which a box
//! would block. A build with automation never shows one, as a modal box would hang the driver,
//! and nor does a test.

use std::io::Write as _;

/**
 * Whether this build reports in message boxes at all: the windows-subsystem build alone; never
 * one with automation (the scenarios'), nor a test build.
 */
const fn may_show_boxes() -> bool {
    cfg!(all(windows, not(feature = "automation"), not(test)))
}

/// Which of the process's standard streams a text goes to.
#[derive(Clone, Copy)]
enum Stream {
    Out,
    Err,
}

/// Logs `error` and, where nothing would see the log, shows it in a blocking message box.
pub fn report(error: &anyhow::Error) {
    log::error!("scrollback couldn't start: {error:#}");
    if may_show_boxes() && !attached(Stream::Err) {
        show_box(&message(error), Icon::Error);
    }
}

/// The message box text: what failed, then the chain of causes on separate lines.
fn message(error: &anyhow::Error) -> String {
    let mut text = String::from("scrollback couldn't start.\n");
    for cause in error.chain() {
        text.push('\n');
        text.push_str(&cause.to_string());
    }
    text
}

/// A command line that doesn't parse: on stderr, or in a message box where there is none.
pub fn show_parse_error(text: &str) {
    show(text, Stream::Err, Icon::Error);
}

/// `--help` and `--version`'s text: on stdout, or in a message box where there is none.
pub fn show_info(text: &str) {
    show(text, Stream::Out, Icon::Information);
}

fn show(text: &str, stream: Stream, icon: Icon) {
    if may_show_boxes() && !attached(stream) {
        show_box(text, icon);
        return;
    }
    /* Nowhere to report a stream that fails. */
    let _ = match stream {
        Stream::Out => std::io::stdout().write_all(text.as_bytes()),
        Stream::Err => std::io::stderr().write_all(text.as_bytes()),
    };
}

/**
 * Whether the process was given `stream`: a windows-subsystem program started from Explorer
 * or the Start menu has none.
 */
#[cfg(windows)]
fn attached(stream: Stream) -> bool {
    use std::os::windows::io::AsRawHandle as _;
    let handle = match stream {
        Stream::Out => std::io::stdout().as_raw_handle(),
        Stream::Err => std::io::stderr().as_raw_handle(),
    };
    !handle.is_null()
}

#[cfg(not(windows))]
fn attached(_: Stream) -> bool {
    true
}

enum Icon {
    Error,
    Information,
}

/// A blocking message box of `text` (Windows only; elsewhere [`may_show_boxes`] is false).
#[cfg(windows)]
fn show_box(text: &str, icon: Icon) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MessageBoxW,
    };

    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain([0]).collect() };
    let (text, caption) = (wide(text), wide("scrollback"));
    let icon = match icon {
        Icon::Error => MB_ICONERROR,
        Icon::Information => MB_ICONINFORMATION,
    };
    /* SAFETY: both strings are NUL-terminated UTF-16 that outlive the call; a null owner makes
    the box top-level. */
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | icon | MB_SETFOREGROUND,
        );
    }
}

#[cfg(not(windows))]
fn show_box(text: &str, _: Icon) {
    eprintln!("{text}");
}

#[cfg(test)]
mod tests {
    use anyhow::Context as _;

    use super::*;

    #[test]
    fn the_message_lists_every_cause() {
        let error = Err::<(), _>(std::io::Error::other("access denied"))
            .context("creating the data folder D:\\data")
            .unwrap_err();
        assert_eq!(
            message(&error),
            "scrollback couldn't start.\n\
             \ncreating the data folder D:\\data\
             \naccess denied"
        );
    }

    /// No box ever opens in a test, whatever the shape.
    #[test]
    fn a_test_build_never_shows_a_box() {
        assert!(!may_show_boxes());
    }
}
