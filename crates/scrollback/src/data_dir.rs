//! Where scrollback keeps its files (PLAN D3): `--data-dir` / `SCROLLBACK_DATA_DIR` when given
//! and not empty (`SCROLLBACK_DATA_DIR=` is how a shell unsets it for one command);
//! under automation without one, a temporary folder, so a scripted run never reaches the real
//! one; otherwise the platform's local data folder plus `scrollback` (DESIGN §4.10:
//! `C:\Users\user\AppData\Local\scrollback`, `/Users/user/Library/Application
//! Support/scrollback`).
//!
//! Nothing is written there yet: the archive is created in it by phase 1 (PLAN §13).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// How the data dir was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `--data-dir` or `SCROLLBACK_DATA_DIR`.
    Explicit,
    /// A temporary folder for this automation run.
    Automation,
    /// The platform's local data folder.
    Platform,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "--data-dir / SCROLLBACK_DATA_DIR",
            Self::Automation => "automation without --data-dir: a temporary folder",
            Self::Platform => "the platform's local data folder",
        }
    }
}

/// The data dir and how it was chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub dir: PathBuf,
    pub source: Source,
}

/// Where the data dir is for this process (see the module docs). Nothing is created here.
pub fn resolve(explicit: Option<PathBuf>, automation: bool) -> Result<Place> {
    choose(
        explicit,
        automation,
        std::process::id(),
        &std::env::temp_dir(),
        dirs::data_local_dir(),
    )
}

fn choose(
    explicit: Option<PathBuf>,
    automation: bool,
    pid: u32,
    temp: &Path,
    data: Option<PathBuf>,
) -> Result<Place> {
    if let Some(dir) = explicit.filter(|dir| !dir.as_os_str().is_empty()) {
        return Ok(Place {
            dir,
            source: Source::Explicit,
        });
    }
    if automation {
        return Ok(Place {
            dir: temp.join(format!("scrollback-automation-{pid}")),
            source: Source::Automation,
        });
    }
    let data = data.context(
        "this system has no local data folder for scrollback; pass --data-dir or set \
         SCROLLBACK_DATA_DIR",
    )?;
    Ok(Place {
        dir: data.join("scrollback"),
        source: Source::Platform,
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn an_explicit_dir_wins() {
        let place = choose(
            Some(PathBuf::from("D:/data")),
            true,
            7,
            Path::new("C:/Temp"),
            Some(PathBuf::from("C:/Users/user/AppData/Local")),
        )
        .unwrap();
        assert_eq!(place.dir, PathBuf::from("D:/data"));
        assert_eq!(place.source, Source::Explicit);
    }

    #[test]
    fn automation_never_uses_the_real_dir() {
        // Not given, and given but empty (`SCROLLBACK_DATA_DIR=`, `--data-dir ""`).
        for explicit in [None, Some(PathBuf::new())] {
            let place = choose(
                explicit,
                true,
                42,
                Path::new("C:/Temp"),
                Some(PathBuf::from("C:/Users/user/AppData/Local")),
            )
            .unwrap();
            assert_eq!(
                place.dir,
                Path::new("C:/Temp").join("scrollback-automation-42")
            );
            assert_eq!(place.source, Source::Automation);
        }
    }

    #[test]
    fn otherwise_the_local_data_folder() {
        for explicit in [None, Some(PathBuf::new())] {
            let place = choose(
                explicit,
                false,
                1,
                Path::new("C:/Temp"),
                Some(PathBuf::from("C:/Users/user/AppData/Local")),
            )
            .unwrap();
            assert_eq!(
                place.dir,
                Path::new("C:/Users/user/AppData/Local").join("scrollback")
            );
            assert_eq!(place.source, Source::Platform);
        }
        assert!(choose(None, false, 1, Path::new("C:/Temp"), None).is_err());
    }
}
