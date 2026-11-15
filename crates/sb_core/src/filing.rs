//! Filing (PLAN §8): path normalisation, and matching a session's project path to a folder.
//!
//! This is the logic alone: paths and attached paths in, a decision out. The catalogue holds
//! the folders and says when the rules run.

use crate::location::LocationId;

/// Where a WSL distro mounts the Windows drives unless its `/etc/wsl.conf` says otherwise
/// (PLAN §6.3).
pub const WSL_DRIVE_MOUNT: &str = "/mnt/";

/// The hosts under which Windows shows a WSL distro's own files, as `\\<host>\<distro>\…`.
const WSL_HOSTS: [&str; 2] = ["wsl.localhost", "wsl$"];

/**
 * A path in the form paths are compared in (PLAN §8): its components joined by `/`, with no
 * trailing separator and nothing that names nothing (`//`, `/./`), and
 *
 * - a path on a Windows drive (`D:\Projects\app`, `D:/Projects/app`) in lower case, as Windows
 *   compares names without regard to case: `d:/projects/app`;
 * - a path under a WSL distro's drive mount (`/mnt/d/Projects/app`) as the drive's path, which
 *   is the same directory: `d:/projects/app`;
 * - a path on a Windows share (`\\Server\Share\app`, `//server/share/app`) in lower case behind
 *   `\\`: `\\server/share/app`. A WSL distro's files seen from Windows are such a path
 *   (`\\wsl.localhost\<distro>\home\user`), but their names are the distro's and keep their
 *   case;
 * - any other path as it is, case kept: `/home/user/code/app`.
 *
 * The path as it was typed or recorded is kept beside it, for display.
 */
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NormalPath(String);

impl NormalPath {
    /**
     * Normalises `path`. `drive_mount` is where the location the path is from mounts the
     * Windows drives, when it is a WSL distro; for any other location it is `None`, and a path
     * like `/mnt/d/…` is only itself.
     *
     * `None` for a path that names nothing (an empty one, `.`, `/`): attached to a folder it
     * would claim every session.
     */
    pub fn new(path: &str, drive_mount: Option<&str>) -> Option<Self> {
        let named = |component: &&str| !component.is_empty() && *component != ".";
        let bytes = path.as_bytes();
        if bytes.len() >= 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes.get(2).is_none_or(|byte| matches!(byte, b'\\' | b'/'))
        {
            let components: Vec<&str> = path.split(['\\', '/']).filter(named).collect();
            return Some(Self(components.join("/").to_lowercase()));
        }
        if path.starts_with(r"\\") || (path.starts_with("//") && !path.starts_with("///")) {
            let mut components = path.split(['\\', '/']).filter(named);
            let host = components.next()?.to_lowercase();
            let rest = components.collect::<Vec<_>>().join("/");
            let share = if WSL_HOSTS.contains(&host.as_str()) {
                format!(r"\\{}/{rest}", WSL_HOSTS[0])
            } else {
                format!(r"\\{host}/{}", rest.to_lowercase())
            };
            return Some(Self(share.trim_end_matches('/').to_string()));
        }
        let components: Vec<&str> = path.split('/').filter(named).collect();
        /* Under the drive mount, the next component is the drive's letter. */
        let drive = drive_mount.and_then(|mount| {
            let mount: Vec<&str> = mount.split('/').filter(named).collect();
            let (letter, rest) = components.strip_prefix(&mount[..])?.split_first()?;
            (letter.len() == 1 && letter.as_bytes()[0].is_ascii_alphabetic()).then(|| {
                format!("{letter}:/{}", rest.join("/"))
                    .trim_end_matches('/')
                    .to_lowercase()
            })
        });
        match drive {
            Some(drive) => Some(Self(drive)),
            None if components.is_empty() => None,
            None if path.starts_with('/') => Some(Self(format!("/{}", components.join("/")))),
            None => Some(Self(components.join("/"))),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `other` is this path or one under it. `/home/user/code/app` contains
    /// `/home/user/code/app/crates` and not `/home/user/code/app2`.
    pub fn contains(&self, other: &NormalPath) -> bool {
        other
            .0
            .strip_prefix(&self.0)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }
}

/// A folder's identity in the catalogue (PLAN §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FolderId(pub i64);

/// A project path attached to a folder (PLAN §2.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachedPath {
    pub folder: FolderId,
    pub path: NormalPath,
    /// The one location whose sessions it files; `None` files every location's.
    pub location: Option<LocationId>,
}

/**
 * The attached path that files a session started in `project` at `location`, if one does
 * (PLAN §8). The candidates are the attached paths that contain the project path and are for
 * this location or for all; the longest wins, so a subfolder's more specific path claims a
 * session before its parent's. Of the same path attached twice, the one for this location wins.
 */
pub fn file<'a>(
    project: &NormalPath,
    location: &LocationId,
    attached: &'a [AttachedPath],
) -> Option<&'a AttachedPath> {
    attached
        .iter()
        .filter(|candidate| {
            candidate.path.contains(project)
                && candidate
                    .location
                    .as_ref()
                    .is_none_or(|only| only == location)
        })
        /* Every candidate is a prefix of the one project path, so the longest text has the
        most components. */
        .max_by_key(|candidate| (candidate.path.0.len(), candidate.location.is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wsl(path: &str) -> NormalPath {
        NormalPath::new(path, Some(WSL_DRIVE_MOUNT)).unwrap()
    }

    fn plain(path: &str) -> NormalPath {
        NormalPath::new(path, None).unwrap()
    }

    #[test]
    fn paths_normalise_to_one_form() {
        for (path, mount, normal) in [
            /* POSIX paths keep their case and lose what names nothing. */
            ("/home/user/code/app", None, "/home/user/code/app"),
            ("/home/user/code/app/", None, "/home/user/code/app"),
            ("/home/user//code/./app///", None, "/home/user/code/app"),
            ("///home/user", None, "/home/user"),
            ("/Users/user/Code/App", None, "/Users/user/Code/App"),
            ("code/app/", None, "code/app"),
            (r"/home/user/odd\name", None, r"/home/user/odd\name"),
            /* Windows paths fold case and take either separator. */
            (r"D:\Projects\App", None, "d:/projects/app"),
            (r"d:\projects\app\", None, "d:/projects/app"),
            ("D:/Projects/App", None, "d:/projects/app"),
            (r"D:\Projects/App\\crates", None, "d:/projects/app/crates"),
            (r"C:\Users\user\Code", None, "c:/users/user/code"),
            (r"D:\", None, "d:"),
            ("D:", None, "d:"),
            (r"\\Server\Share\App\", None, r"\\server/share/app"),
            (r"\\Server", None, r"\\server"),
            ("//Server/Share/App", None, r"\\server/share/app"),
            /* A WSL distro's files seen from Windows keep the case of their names. */
            (
                r"\\wsl.localhost\Ubuntu-24.04\home\user\Code",
                None,
                r"\\wsl.localhost/Ubuntu-24.04/home/user/Code",
            ),
            (
                r"\\WSL$\Ubuntu-24.04\home\user\Code\",
                None,
                r"\\wsl.localhost/Ubuntu-24.04/home/user/Code",
            ),
            /* A name that only starts like a drive is not one. */
            ("D:ata/app", None, "D:ata/app"),
            ("/D:/app", None, "/D:/app"),
            /* Under a WSL distro's drive mount, a path is the drive's. */
            ("/mnt/d/Projects/App", Some("/mnt/"), "d:/projects/app"),
            ("/mnt/D/Projects/App/", Some("/mnt/"), "d:/projects/app"),
            ("/mnt//d/Projects/App", Some("/mnt/"), "d:/projects/app"),
            ("/mnt/./d/Projects/App", Some("/mnt/"), "d:/projects/app"),
            ("/mnt/c", Some("/mnt/"), "c:"),
            ("/mnt/c/", Some("/mnt"), "c:"),
            ("/mnt/data/app", Some("/mnt/"), "/mnt/data/app"),
            ("/mnt", Some("/mnt/"), "/mnt"),
            ("/mntd/app", Some("/mnt/"), "/mntd/app"),
            ("/home/user/Code/App", Some("/mnt/"), "/home/user/Code/App"),
            /* The distro's own mount root, when it sets one. */
            ("/d/Projects/App", Some("/"), "d:/projects/app"),
            ("/windows/d/Projects", Some("/windows/"), "d:/projects"),
            ("/mnt/d/Projects", Some("/windows/"), "/mnt/d/Projects"),
            /* Anywhere but in a WSL distro, the mount is only a directory. */
            ("/mnt/d/Projects/App", None, "/mnt/d/Projects/App"),
        ] {
            assert_eq!(
                NormalPath::new(path, mount).unwrap().as_str(),
                normal,
                "{path:?} with the drive mount {mount:?}"
            );
        }
    }

    #[test]
    fn a_path_that_names_nothing_is_refused() {
        for path in ["", ".", "./", "/", "//", "/./", r"\\"] {
            assert_eq!(NormalPath::new(path, None), None, "{path:?}");
            assert_eq!(
                NormalPath::new(path, Some(WSL_DRIVE_MOUNT)),
                None,
                "{path:?}"
            );
        }
        assert_eq!(
            NormalPath::new("/mnt/", Some(WSL_DRIVE_MOUNT)),
            Some(plain("/mnt"))
        );
    }

    #[test]
    fn the_same_directory_seen_from_windows_and_from_wsl_is_one_path() {
        assert_eq!(wsl("/mnt/d/Projects/app"), plain(r"D:\Projects\app"));
        assert_eq!(wsl("/mnt/d/Projects/app"), plain("d:/projects/APP/"));
        assert_ne!(wsl("/home/user/app"), plain("/home/user/App"));
        assert_ne!(
            plain(r"\\wsl.localhost\Ubuntu-24.04\home\user\app"),
            plain(r"\\wsl.localhost\Ubuntu-24.04\home\user\App")
        );
    }

    #[test]
    fn a_path_contains_itself_and_what_is_under_it() {
        for (outer, inner, contains) in [
            ("/home/user/code/app", "/home/user/code/app", true),
            (
                "/home/user/code/app",
                "/home/user/code/app/crates/core",
                true,
            ),
            ("/home/user/code/app", "/home/user/code/app2", false),
            ("/home/user/code/app", "/home/user/code", false),
            ("/home/user/code/app", "/home/user/code/App", false),
            ("/home", "/home/user", true),
            ("/home", r"D:\home", false),
            ("home", "/home/user", false),
            (r"\\Server\share", r"\\server\Share\app", true),
            (r"\\Server\share", "/server/share/app", false),
            (r"D:\Projects\app", r"d:\projects\APP\crates", true),
            (r"D:\Projects\app", r"D:\Projects\application", false),
            (r"D:\", r"D:\Projects", true),
            (r"D:\", r"E:\Projects", false),
            (r"D:\Projects", "/d:/projects", false),
        ] {
            assert_eq!(
                plain(outer).contains(&plain(inner)),
                contains,
                "{outer:?} and {inner:?}"
            );
        }
        assert!(plain(r"D:\Projects\app").contains(&wsl("/mnt/d/Projects/app/crates")));
        assert!(wsl("/mnt/d/Projects").contains(&plain(r"D:\Projects\app")));
    }

    #[test]
    fn the_longest_attached_path_files_a_session() {
        let (pc, laptop) = (LocationId::new(), LocationId::new());
        let attach = |folder, path, location: Option<&LocationId>| AttachedPath {
            folder: FolderId(folder),
            path: plain(path),
            location: location.cloned(),
        };
        let attached = [
            attach(1, "/home/user/code", None),
            attach(2, "/home/user/code/app", None),
            attach(3, "/home/user/code/app/crates/ui", None),
            attach(4, "/home/user/code/app", Some(&laptop)),
            attach(5, "/home/user/code/app/docs", Some(&laptop)),
            attach(6, r"D:\Projects\app", None),
            attach(7, "/home/user/code/application", None),
        ];
        for (project, location, folder) in [
            /* Under a path counts, and the most specific path wins. */
            (plain("/home/user/code/app"), &pc, Some(2)),
            (plain("/home/user/code/app/crates/core"), &pc, Some(2)),
            (plain("/home/user/code/app/crates/ui/src"), &pc, Some(3)),
            (plain("/home/user/code/other"), &pc, Some(1)),
            /* Components match, not text. */
            (plain("/home/user/code/app2"), &pc, Some(1)),
            (plain("/home/user/code/application/x"), &pc, Some(7)),
            (plain("/home/user/codes"), &pc, None),
            (plain("/home/user"), &pc, None),
            (plain("/srv/x"), &pc, None),
            /* A path limited to a location files only that location's sessions, and there it
            wins over the same path attached for all. */
            (plain("/home/user/code/app"), &laptop, Some(4)),
            (plain("/home/user/code/app/docs/plan"), &laptop, Some(5)),
            (plain("/home/user/code/app/docs/plan"), &pc, Some(2)),
            (plain("/home/user/code/app/crates/ui"), &laptop, Some(3)),
            /* Windows and WSL views of one directory, whatever the case. */
            (plain(r"d:\projects\APP\crates"), &pc, Some(6)),
            (wsl("/mnt/d/Projects/app"), &pc, Some(6)),
            (plain("/mnt/d/Projects/app"), &laptop, None),
            (plain(r"D:\Projects\apple"), &pc, None),
        ] {
            assert_eq!(
                file(&project, location, &attached).map(|path| path.folder),
                folder.map(FolderId),
                "{project:?}"
            );
        }
        assert_eq!(file(&plain("/home/user/code/app"), &pc, &[]), None);
    }
}
