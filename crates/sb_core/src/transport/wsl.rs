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

/// The variable that has `wsl.exe` write its own messages in UTF-8 instead of UTF-16. An
/// older one does not know it, so what it writes is decoded either way ([`decode`]).
pub(super) const UTF8: &str = "WSL_UTF8";

/// How long the listing may take. It answers at once unless WSL itself is starting.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// What `wsl.exe` names its failure when no distro is installed, in any language.
const NO_DISTRO: &str = "WSL_E_DEFAULT_DISTRO_NOT_FOUND";

/// An installed distro.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Distro {
    /// Its name as WSL spells it. `wsl.exe` takes a name in any case, so a location is kept
    /// under this spelling ([`find`]).
    pub name: String,
    /// Whether it is running now. A distro in any other state (stopped, installing) is not.
    pub running: bool,
    /// Whether it is the one `wsl.exe` uses when none is named.
    pub default: bool,
    /// Whether it is one a container tool keeps for itself (Docker Desktop's, Rancher
    /// Desktop's): nobody works in it, so it is not one to offer as a location.
    pub utility: bool,
}

/**
 * The installed distros, without starting any; `None` when this machine has no WSL (`wsl.exe`
 * is not there, or this is not Windows), which is not an error. No distro installed is an
 * empty list. A `wsl.exe` that fails any other way and lists nothing is an error that
 * carries what it said: WSL not set up, or broken.
 */
pub fn distros(cancel: &CancelToken) -> Result<Option<Vec<Distro>>, TransportError> {
    if !cfg!(windows) {
        return Ok(None);
    }
    let listing = Command::new(WSL_EXE)
        .args(["--list", "--verbose"])
        .env(UTF8, "1");
    match run(&listing, LIST_TIMEOUT, cancel) {
        Ok(output) => {
            let distros = parse(&output.stdout);
            let said = format!("{}\n{}", decode(&output.stdout), decode(&output.stderr));
            if output.status.success() || !distros.is_empty() || said.contains(NO_DISTRO) {
                return Ok(Some(distros));
            }
            let said: Vec<&str> = said
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            Err(TransportError::Failed(if said.is_empty() {
                format!("{WSL_EXE} --list failed ({})", output.status)
            } else {
                said.join("\n")
            }))
        }
        Err(ProcessError::Start { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(ProcessError::Cancelled { .. }) => Err(TransportError::Cancelled),
        Err(error) => Err(TransportError::Failed(error.to_string())),
    }
}

/// The distro among `distros` that `wsl.exe` would take `name` for: it matches names without
/// regard to case.
pub fn find<'a>(distros: &'a [Distro], name: &str) -> Option<&'a Distro> {
    distros
        .iter()
        .find(|distro| distro.name.eq_ignore_ascii_case(name))
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
                utility: ["docker-desktop", "rancher-desktop"]
                    .iter()
                    .any(|tool| name.to_ascii_lowercase().starts_with(tool)),
            })
        })
        .collect()
}

/// Text `wsl.exe` printed: UTF-8 when it is that, as it is from a Linux program and from a
/// `wsl.exe` that knows [`UTF8`]; otherwise UTF-16, which its own messages are without it.
/// (UTF-16 has a NUL in every ASCII letter and line end, which UTF-8 text never holds.)
pub(super) fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => text.to_string(),
        _ => {
            let units: Vec<u16> = bytes
                .strip_prefix(&[0xFF, 0xFE])
                .unwrap_or(bytes)
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            String::from_utf16_lossy(&units)
        }
    }
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
            utility: false,
        }
    }

    const TABLE: &str = "  NAME                   STATE           VERSION\n\
                         * Ubuntu-24.04           Running         2\n\
                         \x20 Debian                 Stopped         2\n\
                         \x20 docker-desktop         Stopped         2\n\
                         \x20 docker-desktop-data    Stopped         2\n\
                         \x20 Alpine                 Installing      1\n";

    #[test]
    fn the_table_gives_each_distro_its_state_and_the_default() {
        let tool = |name| Distro {
            utility: true,
            ..distro(name, false, false)
        };
        let listed = [
            distro("Ubuntu-24.04", true, true),
            distro("Debian", false, false),
            tool("docker-desktop"),
            tool("docker-desktop-data"),
            distro("Alpine", false, false),
        ];
        /* As wsl.exe writes it, the same after a byte order mark, and as it writes it with
        WSL_UTF8 set. */
        assert_eq!(parse(&utf16(TABLE)), listed);
        assert_eq!(parse(&[&[0xFF, 0xFE][..], &utf16(TABLE)].concat()), listed);
        assert_eq!(parse(TABLE.replace('\n', "\r\n").as_bytes()), listed);
        /* A name is found as wsl.exe finds it, in any case, and kept as it is listed. */
        assert_eq!(find(&listed, "ubuntu-24.04"), Some(&listed[0]));
        assert_eq!(find(&listed, "ubuntu"), None);
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
             and 'wsl.exe --install <Distro>' to install.\n\
             Error code: Wsl/WSL_E_DEFAULT_DISTRO_NOT_FOUND\n",
            "The Windows Subsystem for Linux is not installed. You can install by running \
             'wsl.exe --install'.\nFor more information please visit https://aka.ms/wslinstall\n",
            "",
        ] {
            assert_eq!(parse(&utf16(said)), [], "{said}");
        }
    }

    #[test]
    fn its_words_are_read_in_either_encoding_and_any_script() {
        for said in [
            "There is no distribution with the supplied name.\r\n",
            "指定された名前のディストリビューションはありません。\r\n",
            "Дистрибутив с указанным именем не найден.\r\n",
        ] {
            let utf16: Vec<u8> = said.encode_utf16().flat_map(u16::to_le_bytes).collect();
            assert_eq!(decode(&utf16), said);
            assert_eq!(decode(said.as_bytes()), said);
        }
        assert_eq!(decode(b""), "");
    }
}
