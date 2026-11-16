//! Where scrollback keeps its files (PLAN D3): `--data-dir` / `SCROLLBACK_DATA_DIR` when given
//! and not empty (`SCROLLBACK_DATA_DIR=` is how a shell unsets it for one command);
//! under automation without one, a temporary folder, so a scripted run never reaches the real
//! one; otherwise the platform's local data folder plus `scrollback` (DESIGN §4.10:
//! `C:\Users\user\AppData\Local\scrollback`, `/Users/user/Library/Application
//! Support/scrollback`).
//!
//! The archive is its `archive` folder, made on the first run (PLAN D3, §4.4).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/**
 * The data folder's name in the platform's local data folder. On Windows the installer's folder
 * sits beside it, under another name (`script/package`'s pack id).
 */
pub const FOLDER: &str = "scrollback";

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
        dir: data.join(FOLDER),
        source: Source::Platform,
    })
}

/**
 * Whether `place` may be the data dir of a run with the development override
 * (`--dev-location-home`), which archives generated data under real distros and hosts: only
 * a data dir named for it, and never scrollback's own data folder, so that nothing of the
 * kind ever lands in the owner's archive.
 */
#[cfg(feature = "automation")]
pub fn for_development(place: &Place) -> Result<()> {
    let own = dirs::data_local_dir().map(|data| data.join(FOLDER));
    let paths = if cfg!(windows) {
        Paths::Windows
    } else {
        Paths::Unix
    };
    refuse_own(place, own.as_deref(), paths)
}

/// How a platform spells one folder: Windows in any case, with either slash.
#[cfg(feature = "automation")]
#[derive(Clone, Copy)]
enum Paths {
    Unix,
    Windows,
}

#[cfg(feature = "automation")]
impl Paths {
    /**
     * `path` as one spelling of its folder under these rules: no `.` steps, no doubled or
     * trailing separators, and on Windows `/` for `\` and lower case.
     */
    fn spelling(self, path: &Path) -> String {
        let path = path.to_string_lossy();
        let path = match self {
            Self::Unix => path.into_owned(),
            Self::Windows => path.replace('\\', "/").to_lowercase(),
        };
        let rooted = path.starts_with('/');
        let steps: Vec<&str> = path
            .split('/')
            .filter(|step| !step.is_empty() && *step != ".")
            .collect();
        format!("{}{}", if rooted { "/" } else { "" }, steps.join("/"))
    }

    /**
     * Whether `a` and `b` are one folder: as the file system resolves them where both exist
     * (links, `..`), and otherwise by their spellings, as when the owner's folder isn't made
     * yet.
     */
    fn same(self, a: &Path, b: &Path) -> bool {
        match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => self.spelling(&a) == self.spelling(&b),
            _ => self.spelling(a) == self.spelling(b),
        }
    }
}

#[cfg(feature = "automation")]
fn refuse_own(place: &Place, own: Option<&Path>, paths: Paths) -> Result<()> {
    anyhow::ensure!(
        place.source == Source::Explicit && !own.is_some_and(|own| paths.same(&place.dir, own)),
        "--dev-location-home archives generated data: it needs a scratch --data-dir of its own, \
         never scrollback's own data folder"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[cfg(feature = "automation")]
    #[test]
    fn the_development_override_needs_a_data_dir_that_is_not_the_owners() {
        let own = Path::new("/Users/user/Library/Application Support/scrollback");
        let at = |dir: &Path, source| Place {
            dir: dir.to_path_buf(),
            source,
        };
        let refused = |dir: &str, source| {
            refuse_own(&at(Path::new(dir), source), Some(own), Paths::Unix).is_err()
        };
        assert!(!refused("/tmp/scratch", Source::Explicit));
        assert!(refused(
            "/Users/user/Library/Application Support/scrollback",
            Source::Explicit
        ));
        assert!(refused(
            "/Users/user/Library/Application Support/scrollback/",
            Source::Explicit
        ));
        assert!(refused(
            "/Users/user/Library/Application Support/./scrollback",
            Source::Explicit
        ));
        assert!(refused(
            "/Users/user/Library/Application Support/scrollback",
            Source::Platform
        ));
        assert!(refused("/tmp/x", Source::Automation));
    }

    /// On Windows a folder is the same in any case and with either slash, made or not.
    #[cfg(feature = "automation")]
    #[test]
    fn the_owners_data_folder_on_windows_is_known_in_any_spelling() {
        let own = Path::new(r"C:\Users\user\AppData\Local\scrollback");
        let refused = |dir: &str| {
            let place = Place {
                dir: PathBuf::from(dir),
                source: Source::Explicit,
            };
            refuse_own(&place, Some(own), Paths::Windows).is_err()
        };
        assert!(refused(r"C:\Users\user\AppData\Local\scrollback"));
        assert!(refused(r"c:\users\user\appdata\local\SCROLLBACK"));
        assert!(refused("C:/Users/user/AppData/Local/scrollback/"));
        assert!(refused(r"C:\Users\user\AppData\Local\.\scrollback\"));
        assert!(!refused(r"D:\Projects\scratch"));
        assert!(!refused(r"C:\Users\user\AppData\Local\scrollback-dev"));
    }

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
