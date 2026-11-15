//! WSL seen from Windows (PLAN §6.3): which distros are installed, which run, and which is the
//! default, from `wsl.exe --list --verbose`. Listing never starts a distro; syncing one does
//! ([`super::Launcher::Wsl`]).
//!
//! On any system but Windows there is no WSL, and nothing is run to find that out.

use std::io;
use std::time::Duration;

use toolbox_process::{CancelToken, Command, ProcessError, run};

use super::TransportError;

/// The program every WSL command runs through.
pub(super) const WSL_EXE: &str = "wsl.exe";

/// How long the listing may take. It answers at once unless WSL itself is starting.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// An installed distro.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Distro {
    pub name: String,
    /// Whether it is running now. A distro in any other state (stopped, installing) is not.
    pub running: bool,
    /// Whether it is the one `wsl.exe` uses when none is named.
    pub default: bool,
}

/**
 * The installed distros, without starting any; `None` when this machine has no WSL (`wsl.exe`
 * is not there, or this is not Windows), which is not an error. A `wsl.exe` that is there
 * and lists nothing (no distro installed, or WSL itself not set up yet) gives none.
 */
pub fn distros(cancel: &CancelToken) -> Result<Option<Vec<Distro>>, TransportError> {
    if !cfg!(windows) {
        return Ok(None);
    }
    let listing = Command::new(WSL_EXE).args(["--list", "--verbose"]);
    match run(&listing, LIST_TIMEOUT, cancel) {
        Ok(output) => Ok(Some(parse(&output.stdout))),
        Err(ProcessError::Start { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(ProcessError::Cancelled { .. }) => Err(TransportError::Cancelled),
        Err(error) => Err(TransportError::Failed(error.to_string())),
    }
}

/**
 * The distros in what `wsl.exe --list --verbose` printed: a table of the default's mark,
 * NAME, STATE and VERSION. A row is a line that ends in a version number; the header and
 * anything `wsl.exe` says instead of a table are not rows. A distro's name has no spaces.
 */
fn parse(listing: &[u8]) -> Vec<Distro> {
    decode(listing)
        .lines()
        .filter_map(|line| {
            let row = line.trim();
            let (default, row) = match row.strip_prefix('*') {
                Some(row) => (true, row),
                None => (false, row),
            };
            let mut columns = row.split_whitespace();
            let name = columns.next()?;
            let state = columns.next()?;
            columns.next()?.parse::<u32>().ok()?;
            Some(Distro {
                name: name.to_string(),
                running: state == "Running",
                default,
            })
        })
        .collect()
}

/// Text `wsl.exe` printed: its own messages are UTF-16LE (a byte order mark, or a NUL in every
/// other byte of ASCII text); with `WSL_UTF8` set, and from a Linux program, it is UTF-8.
pub(super) fn decode(bytes: &[u8]) -> String {
    let utf16 = |bytes: &[u8]| {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest);
    }
    let nuls = bytes.iter().filter(|byte| **byte == 0).count();
    if bytes.len() >= 4 && nuls > bytes.len() / 4 {
        return utf16(bytes).replace('\0', "");
    }
    String::from_utf8_lossy(bytes).replace('\0', "")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// `text` as `wsl.exe` writes it to a pipe: UTF-16LE, lines ended `\r\n`.
    fn utf16(text: &str) -> Vec<u8> {
        text.replace('\n', "\r\n")
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn distro(name: &str, running: bool, default: bool) -> Distro {
        Distro {
            name: name.to_string(),
            running,
            default,
        }
    }

    const TABLE: &str = "  NAME              STATE           VERSION\n\
                         * Ubuntu-24.04      Running         2\n\
                         \x20 Debian            Stopped         2\n\
                         \x20 docker-desktop    Stopped         2\n\
                         \x20 Alpine            Installing      1\n";

    #[test]
    fn the_table_gives_each_distro_its_state_and_the_default() {
        let listed = [
            distro("Ubuntu-24.04", true, true),
            distro("Debian", false, false),
            distro("docker-desktop", false, false),
            distro("Alpine", false, false),
        ];
        /* As wsl.exe writes it, the same after a byte order mark, and as it writes it with
        WSL_UTF8 set. */
        assert_eq!(parse(&utf16(TABLE)), listed);
        assert_eq!(parse(&[&[0xFF, 0xFE][..], &utf16(TABLE)].concat()), listed);
        assert_eq!(parse(TABLE.replace('\n', "\r\n").as_bytes()), listed);
    }

    #[test]
    fn a_default_that_is_stopped_and_a_table_with_one_row() {
        assert_eq!(
            parse(&utf16(
                "  NAME            STATE           VERSION\n* Ubuntu-24.04    Stopped         2\n"
            )),
            [distro("Ubuntu-24.04", false, true)]
        );
    }

    #[test]
    fn what_wsl_says_instead_of_a_table_lists_nothing() {
        for said in [
            "Windows Subsystem for Linux has no installed distributions.\n\
             You can resolve this by installing a distribution with the instructions below:\n\n\
             Use 'wsl.exe --list --online' to list available distributions\n\
             and 'wsl.exe --install <Distro>' to install.\n",
            "The Windows Subsystem for Linux is not installed. You can install by running \
             'wsl.exe --install'.\nFor more information please visit https://aka.ms/wslinstall\n",
            "",
        ] {
            assert_eq!(parse(&utf16(said)), [], "{said}");
        }
    }
}
