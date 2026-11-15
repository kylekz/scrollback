//! The library the design boards draw (`design/project/E-Folder`, `E-All-Sessions`,
//! `E-Unsorted`; docs/DESIGN.md §6), built as an archive: three locations, the boards' folders
//! with their attached paths, and 2,318 sessions whose counts are the boards'. The sessions a
//! board names have its title, prompt, model, day and tool calls; the rest are filler, spread
//! over the months before the ones the boards show, so that those months hold exactly the
//! cards drawn.
//!
//! Each location's sessions are written as an agent's transcripts and synced into the archive
//! through the real engine, then the folders are made and their paths attached, which files
//! the sessions as it would for the owner. The three locations are all this machine's kind
//! until the others exist (docs/PLAN.md §13, phase 5), so their tags share one colour.
//!
//! Everything here is illustrative: made-up titles in placeholder names.

use std::path::Path;

use anyhow::{Context as _, Result, bail, ensure};
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::catalogue::SessionId;
use sb_core::filing::FolderId;
use sb_core::location::{LocationId, LocationKind};
use sb_core::provider;
use sb_core::sync::sync_location;
use sb_test_support::{Outline, ProjectsTree};

/// The locations, by their place in [`LOCATIONS`].
const PC: usize = 0;
const WSL: usize = 1;
const MAC: usize = 2;
const LOCATIONS: [&str; 3] = ["This PC", "Ubuntu-24.04", "user@macbook"];

const OPUS: &str = "claude-opus-5-5";
const OPUS_46: &str = "claude-opus-4-6";
const OPUS_45: &str = "claude-opus-4-5";
const SONNET: &str = "claude-sonnet-5";
const FABLE: &str = "claude-fable-5-2";

/// What the boards count (`E-Folder`'s sidebar and header, `E-All-Sessions`' header): the
/// sessions and the tool calls of the archive and of fc, and the starred sessions.
const STARRED: usize = 14;
const CALLS: u64 = 241_706;
const FC_CALLS: u64 = 52_914;

/// A session a board names: title, prompt, model, day, tool calls, and whether it is starred.
type Named = (
    &'static str,
    &'static str,
    &'static str,
    (i32, u32, u32),
    u32,
    bool,
);

/// The sessions that started in one project path at one location.
struct Bucket {
    location: usize,
    project: &'static str,
    /// How many there are, the named among them.
    sessions: usize,
    named: &'static [Named],
    /// The days the filler sessions are spread over, counted from 3 Nov 2025.
    filler_days: (u32, u32),
    /// Whether its tool calls count towards fc's total.
    fc: bool,
}

/// The months before the ones the boards show: 3 Nov 2025 to 30 Aug 2026.
const BEFORE: (u32, u32) = (0, 300);

const fn bucket(location: usize, project: &'static str, sessions: usize) -> Bucket {
    Bucket {
        location,
        project,
        sessions,
        named: &[],
        filler_days: BEFORE,
        fc: false,
    }
}

const BUCKETS: &[Bucket] = &[
    // fc, as `E-Folder` draws it: twelve cards in three months, and three path chips.
    Bucket {
        named: &[
            (
                "Portrait layout for vertical cams",
                "Vertical fancams letterbox into a tiny strip. Detect 9:16 and lay the player out side by side with the list instead of stacking it.",
                OPUS,
                (2026, 11, 2),
                118,
                false,
            ),
            (
                "Stickers as baked SVG, not rotated divs",
                "gpui can't rotate a div. Bake the −4° tilt into the sticker SVGs and drop the transform we never had.",
                OPUS,
                (2026, 10, 17),
                28,
                false,
            ),
            (
                "Thumbnail cache eviction",
                "The thumbnail cache grew to 9 GB. Evict by last-viewed, keep starred items, and cap it at 2 GB.",
                SONNET,
                (2026, 9, 3),
                57,
                false,
            ),
        ],
        fc: true,
        ..bucket(PC, r"D:\Projects\fc", 96)
    },
    Bucket {
        fc: true,
        ..bucket(WSL, "/mnt/d/Projects/fc", 22)
    },
    Bucket {
        named: &[
            (
                "libmpv hwdec fallback on AV1",
                "On the Intel box AV1 4K drops to software decode and stutters. Fall back to a lower rung before giving up on hardware decode.",
                SONNET,
                (2026, 11, 9),
                37,
                false,
            ),
            (
                "Frame-step keys in the player",
                ", and . should step one frame while paused, like mpv. Hold for repeat at 12 fps.",
                OPUS,
                (2026, 11, 1),
                44,
                false,
            ),
            (
                "Line up four cams by their sound",
                "Cross-correlate the audio of up to four cams and offset each player so they play in sync.",
                OPUS,
                (2026, 10, 21),
                241,
                true,
            ),
            (
                "Inbox auto-accept threshold",
                "Accept the agent's match automatically above 0.92 and show why in the inbox row.",
                SONNET,
                (2026, 10, 6),
                68,
                false,
            ),
            (
                "Library health panel",
                "One place for background work: backfill, thumbnails, orphaned files and what each is waiting for.",
                OPUS_46,
                (2026, 9, 19),
                132,
                false,
            ),
            (
                "Drop the sqlez copy from fc_db",
                "fc_db carries three files from Zed's sqlez. Move to toolbox_store and delete them.",
                OPUS,
                (2026, 9, 8),
                196,
                false,
            ),
        ],
        fc: true,
        ..bucket(WSL, "/home/user/code/fc", 248)
    },
    Bucket {
        named: &[
            (
                "Downloads: paste a whole playlist",
                "A playlist URL should expand into its videos before matching, with a count on the paste badge so it is clear how many were queued.",
                FABLE,
                (2026, 11, 12),
                68,
                true,
            ),
            (
                "Hangul in mono paths",
                "Paths with 카리나 render as tofu in Plex Mono. Add Nanum Gothic Coding as the fallback.",
                OPUS,
                (2026, 10, 2),
                16,
                false,
            ),
            (
                "Romanized search for Hangul names",
                "Typing \"karina\" should find 카리나. Build a romanization index that search consults.",
                SONNET,
                (2026, 9, 28),
                88,
                false,
            ),
        ],
        fc: true,
        ..bucket(MAC, "/Users/user/code/fc", 81)
    },
    Bucket {
        fc: true,
        ..bucket(WSL, "/home/user/code/fc/crates/fc_player", 41)
    },
    // The other folders, with the sessions `E-All-Sessions` and `E-Edge-First-Sync` name.
    Bucket {
        named: &[(
            "Word-level diff highlighting",
            "Changed lines in dv highlight the whole line. Mark the words that changed inside it.",
            OPUS,
            (2026, 11, 9),
            120,
            false,
        )],
        ..bucket(WSL, "/home/user/code/dv", 420)
    },
    bucket(PC, r"D:\Projects\dv", 134),
    Bucket {
        named: &[(
            "WSL distro discovery without starting it",
            "wsl.exe --list --verbose prints UTF-16. Parse it and never start a stopped distro to look.",
            OPUS,
            (2026, 10, 30),
            73,
            false,
        )],
        ..bucket(WSL, "/home/user/code/dv/crates/dv_backend", 58)
    },
    Bucket {
        named: &[(
            "Thumbnail strip for folio albums",
            "The album header should show the first six photos as a strip that scrolls with the title.",
            OPUS,
            (2026, 11, 6),
            61,
            false,
        )],
        ..bucket(WSL, "/home/user/code/folio", 380)
    },
    Bucket {
        named: &[(
            "Mica behind the sidebar only",
            "The title bar should stay opaque when Mica is on, and the sidebar alone shows the desktop.",
            OPUS,
            (2026, 8, 22),
            34,
            false,
        )],
        ..bucket(PC, r"D:\Projects\folio", 126)
    },
    Bucket {
        named: &[(
            "Speaker notes pane resizes",
            "Dragging the notes divider jumps by 40 px on the first move. Find where the offset comes from.",
            OPUS,
            (2026, 10, 19),
            38,
            false,
        )],
        ..bucket(WSL, "/home/user/code/slide", 197)
    },
    Bucket {
        named: &[(
            "Audit fc and slide for code taken from Zed",
            "Read PLAN D15 and list what we can lift from each sibling and what has to stay out.",
            OPUS,
            (2026, 10, 7),
            412,
            true,
        )],
        ..bucket(WSL, "/home/user/code/toolbox-rs", 180)
    },
    Bucket {
        named: &[(
            "Process tree kill on Windows",
            "Cancelling a sync leaves ssh.exe running. Kill the whole process tree with a job object.",
            OPUS,
            (2026, 10, 2),
            96,
            false,
        )],
        ..bucket(PC, r"D:\Projects\toolbox-rs", 61)
    },
    Bucket {
        named: &[(
            "Locations sheet: stale warning",
            "A location that has not synced for 7 days should say so, and say when the agent's cleanup would start deleting.",
            OPUS,
            (2026, 11, 14),
            52,
            false,
        )],
        ..bucket(WSL, "/home/user/code/scrollback", 12)
    },
    bucket(WSL, "/home/user/dotfiles", 40),
    bucket(MAC, "/Users/user/dotfiles", 24),
    bucket(WSL, "/home/user/scratch", 100),
    bucket(MAC, "/Users/user/scratch", 61),
    // Unsorted, as `E-Unsorted` draws it: six paths, the one with the most sessions first.
    Bucket {
        named: &[
            (
                "Day 12: garden fences, part two",
                "Part one works but part two counts sides, not edges. Count corners instead.",
                OPUS_45,
                (2025, 12, 12),
                19,
                false,
            ),
            (
                "Day 9: disk compaction",
                "Defragment by whole files this time, right to left, without scanning the free list each move.",
                OPUS_45,
                (2025, 12, 9),
                27,
                false,
            ),
            (
                "Day 6: guard patrol loops",
                "Brute force takes 40 s. Only try obstacles on the path the guard walks.",
                OPUS_45,
                (2025, 12, 6),
                31,
                false,
            ),
            (
                "Day 5: page ordering",
                "Sort each update with the rules as the comparator and sum the middle pages.",
                OPUS_45,
                (2025, 12, 5),
                14,
                false,
            ),
            (
                "Day 4: word search, diagonals",
                "Count XMAS in all eight directions, then the X-shaped MAS for part two.",
                OPUS_45,
                (2025, 12, 4),
                11,
                false,
            ),
        ],
        filler_days: (28, 30),
        ..bucket(WSL, "/home/user/code/advent-2025", 14)
    },
    Bucket {
        named: &[
            (
                "Why is WSL clock drifting?",
                "date in WSL is 40 s behind Windows after sleep. Find out why and what resets it.",
                SONNET,
                (2026, 7, 24),
                12,
                false,
            ),
            (
                "Clean up ~/.cache",
                "What in ~/.cache is safe to delete? It is 22 GB and most of it is old toolchains.",
                SONNET,
                (2026, 6, 3),
                9,
                false,
            ),
            (
                "Rotate the backup SSH key",
                "Replace the key the backup job uses and update authorized_keys on the NAS.",
                SONNET,
                (2026, 5, 12),
                6,
                false,
            ),
            (
                "Why does apt hold back kernel updates?",
                "Three kernel packages are kept back on every upgrade. Explain and fix.",
                SONNET,
                (2026, 4, 2),
                4,
                false,
            ),
            (
                "Set up a systemd timer for restic",
                "Run restic nightly at 03:00 with a timer unit, and mail me when it fails.",
                SONNET,
                (2026, 2, 18),
                21,
                false,
            ),
        ],
        filler_days: (60, 90),
        ..bucket(WSL, "/home/user", 10)
    },
    Bucket {
        named: &[
            (
                "Astro 5 migration, image pipeline",
                "Move the blog to Astro 5 and switch images to the new asset pipeline without changing URLs.",
                SONNET,
                (2026, 8, 30),
                112,
                false,
            ),
            (
                "Draft: a year of agent transcripts",
                "Outline a post on what a year of transcripts looks like once it is all in one place.",
                SONNET,
                (2026, 7, 11),
                18,
                false,
            ),
            (
                "RSS feed drops the summary",
                "The feed's entries have no summary since the last upgrade. Put it back.",
                SONNET,
                (2026, 6, 2),
                9,
                false,
            ),
            (
                "Dark mode code blocks",
                "Code blocks keep the light theme in dark mode. Use the site's tokens.",
                SONNET,
                (2026, 4, 20),
                23,
                false,
            ),
            (
                "Move comments to giscus",
                "Replace the old comment embed with giscus and keep the existing threads.",
                SONNET,
                (2026, 3, 9),
                31,
                false,
            ),
            (
                "Lighthouse pass on the home page",
                "The home page scores 71 on mobile. Find the largest wins.",
                SONNET,
                (2026, 2, 14),
                16,
                false,
            ),
        ],
        ..bucket(MAC, "/Users/user/code/blog", 6)
    },
    Bucket {
        named: &[
            (
                "Parse Claude Code JSONL into turns",
                "Spike: read one transcript and print turns, one line per prompt, with the tool calls under it.",
                OPUS,
                (2026, 11, 9),
                74,
                false,
            ),
            (
                "Catalogue schema sketch",
                "Sketch the tables for items, versions and segments, and what is derived.",
                OPUS,
                (2026, 11, 7),
                22,
                false,
            ),
            (
                "Try zstd segment sizes",
                "Measure compression ratio and seek time for 1, 4 and 16 MB segments.",
                OPUS,
                (2026, 11, 5),
                41,
                false,
            ),
        ],
        ..bucket(WSL, "/home/user/code/scrollback-spike", 3)
    },
    Bucket {
        named: &[
            (
                "Flatten nested arrays with jq",
                "Turn the nested results array into one row per item with its parent's id.",
                SONNET,
                (2026, 3, 22),
                5,
                false,
            ),
            (
                "jq: group by day and sum",
                "Group the events by day and sum their durations, sorted by day.",
                SONNET,
                (2026, 3, 21),
                8,
                false,
            ),
            (
                "Why does jq -s read everything?",
                "Explain slurp and how to stream a 2 GB file instead.",
                SONNET,
                (2026, 3, 20),
                3,
                false,
            ),
        ],
        ..bucket(WSL, "/home/user/tmp/jq-playground", 3)
    },
    Bucket {
        named: &[(
            "Regex golf: match valid IPv4",
            "Shortest regex that matches a valid dotted quad and nothing else.",
            SONNET,
            (2026, 2, 14),
            7,
            false,
        )],
        ..bucket(WSL, "/home/user/tmp/regex-golf", 1)
    },
];

/// A folder: its name, the folder it is in, and its attached paths, each for one location or
/// for any.
type FolderSpec = (
    &'static str,
    Option<&'static str>,
    &'static [(Option<usize>, &'static str)],
);

/// The folders of `E-Folder`'s sidebar, in its order.
const FOLDERS: &[FolderSpec] = &[
    (
        "dv",
        None,
        &[(Some(WSL), "/home/user/code/dv"), (None, r"D:\Projects\dv")],
    ),
    (
        "fc",
        None,
        &[
            (None, r"D:\Projects\fc"),
            (Some(WSL), "/home/user/code/fc"),
            (Some(MAC), "/Users/user/code/fc"),
        ],
    ),
    (
        "folio",
        None,
        &[
            (Some(WSL), "/home/user/code/folio"),
            (None, r"D:\Projects\folio"),
        ],
    ),
    ("slide", None, &[(Some(WSL), "/home/user/code/slide")]),
    (
        "toolbox-rs",
        None,
        &[
            (Some(WSL), "/home/user/code/toolbox-rs"),
            (None, r"D:\Projects\toolbox-rs"),
        ],
    ),
    (
        "scrollback",
        None,
        &[(Some(WSL), "/home/user/code/scrollback")],
    ),
    (
        "dotfiles",
        None,
        &[
            (Some(WSL), "/home/user/dotfiles"),
            (Some(MAC), "/Users/user/dotfiles"),
        ],
    ),
    (
        "Scratch",
        None,
        &[
            (Some(WSL), "/home/user/scratch"),
            (Some(MAC), "/Users/user/scratch"),
        ],
    ),
    (
        "dv-wsl-backend",
        Some("dv"),
        &[(Some(WSL), "/home/user/code/dv/crates/dv_backend")],
    ),
    (
        "player",
        Some("fc"),
        &[(Some(WSL), "/home/user/code/fc/crates/fc_player")],
    ),
];

/// What filler sessions are called and ask: a task and what it is done to.
const TASKS: [&str; 12] = [
    "Fix",
    "Refactor",
    "Speed up",
    "Add tests for",
    "Document",
    "Simplify",
    "Rework",
    "Profile",
    "Tidy up",
    "Harden",
    "Port",
    "Review",
];
const THINGS: [&str; 13] = [
    "the settings store",
    "the import pipeline",
    "keyboard navigation",
    "the thumbnail cache",
    "error reporting",
    "the file watcher",
    "window state restore",
    "the search index",
    "drag and drop",
    "the update check",
    "the theme tokens",
    "startup time",
    "the sidebar tree",
];

/// The day `days` after 3 Nov 2025, which is in 2025 or 2026.
fn day_after(days: u32) -> (i32, u32, u32) {
    // The months from November 2025 on, with how many days each has.
    const MONTHS: [(i32, u32, u32); 14] = [
        (2025, 11, 30),
        (2025, 12, 31),
        (2026, 1, 31),
        (2026, 2, 28),
        (2026, 3, 31),
        (2026, 4, 30),
        (2026, 5, 31),
        (2026, 6, 30),
        (2026, 7, 31),
        (2026, 8, 31),
        (2026, 9, 30),
        (2026, 10, 31),
        (2026, 11, 30),
        (2026, 12, 31),
    ];
    let mut day = 3 + days;
    for (year, month, length) in MONTHS {
        if day <= length {
            return (year, month, day);
        }
        day -= length;
    }
    (2026, 12, 31)
}

/// Shares `total` among `count` sessions: each gets the same, and the first few one more.
struct Share {
    each: u32,
    extra: usize,
    given: usize,
}

impl Share {
    fn new(total: u64, count: usize) -> Self {
        Self {
            each: (total / count as u64) as u32,
            extra: (total % count as u64) as usize,
            given: 0,
        }
    }

    fn next(&mut self) -> u32 {
        self.given += 1;
        self.each + u32::from(self.given <= self.extra)
    }
}

/// Writes the boards' sessions as transcripts, a location's at a time.
struct Writer {
    /// The tool calls still to share among fc's filler sessions, and among the others'.
    fc_calls: Share,
    other_calls: Share,
    /// How many filler sessions are still to be starred.
    filler_stars: usize,
    /// How many filler sessions are written.
    filler: usize,
}

impl Writer {
    fn new() -> Self {
        let named_calls = |fc: bool| -> u64 {
            BUCKETS
                .iter()
                .filter(|bucket| bucket.fc == fc)
                .flat_map(|bucket| bucket.named)
                .map(|named| u64::from(named.4))
                .sum()
        };
        let fillers = |fc: bool| -> usize {
            BUCKETS
                .iter()
                .filter(|bucket| bucket.fc == fc)
                .map(|bucket| bucket.sessions - bucket.named.len())
                .sum()
        };
        let named_stars = BUCKETS
            .iter()
            .flat_map(|bucket| bucket.named)
            .filter(|named| named.5)
            .count();
        Self {
            fc_calls: Share::new(FC_CALLS - named_calls(true), fillers(true)),
            other_calls: Share::new(CALLS - FC_CALLS - named_calls(false), fillers(false)),
            filler_stars: STARRED - named_stars,
            filler: 0,
        }
    }

    /// Writes the sessions of the location at `index` of [`LOCATIONS`] to `tree`. Returns
    /// the provider's ids of those that are starred.
    fn location(&mut self, index: usize, tree: &mut ProjectsTree) -> Vec<String> {
        let mut starred = Vec::new();
        for bucket in BUCKETS.iter().filter(|bucket| bucket.location == index) {
            for &(title, prompt, model, day, calls, star) in bucket.named {
                let session = tree.outlined(&Outline {
                    project: bucket.project,
                    title,
                    prompt,
                    model,
                    day,
                    calls,
                });
                if star {
                    starred.push(session);
                }
            }
            let count = bucket.sessions - bucket.named.len();
            let (from, to) = bucket.filler_days;
            for at in 0..count {
                self.filler += 1;
                let (task, thing) = (
                    TASKS[self.filler % TASKS.len()],
                    THINGS[self.filler % THINGS.len()],
                );
                let session = tree.outlined(&Outline {
                    project: bucket.project,
                    title: &format!("{task} {thing}"),
                    prompt: &format!(
                        "{task} {thing}: start from the failing case, keep the change small \
                         and say what you left out."
                    ),
                    model: [OPUS, OPUS, SONNET, OPUS_46][self.filler % 4],
                    day: day_after(from + (to - from) * at as u32 / count as u32),
                    calls: if bucket.fc {
                        self.fc_calls.next()
                    } else {
                        self.other_calls.next()
                    },
                });
                // The first filler session of the first folders' buckets is starred.
                if at == 0 && self.filler_stars > 0 && to == BEFORE.1 {
                    self.filler_stars -= 1;
                    starred.push(session);
                }
            }
        }
        starred
    }
}

/// Every location's sessions as one agent's transcripts in `tree`: a first sync with
/// thousands of sessions to copy and index, which land in Unsorted by their paths.
pub fn agent(tree: &mut ProjectsTree) {
    let mut writer = Writer::new();
    for index in 0..LOCATIONS.len() {
        writer.location(index, tree);
    }
}

/// Builds the archive in `archive_dir`, writing each location's transcripts under `scratch`
/// on the way, which is removed again.
pub fn build(archive_dir: &Path, scratch: &Path) -> Result<()> {
    let providers = provider::builtin();
    let provider = providers
        .first()
        .context("scrollback ships a provider")?
        .id();
    let archive = Archive::create(archive_dir)?;
    let catalogue = archive.catalogue();

    let mut writer = Writer::new();
    let mut locations: Vec<LocationId> = Vec::new();
    for (index, name) in LOCATIONS.into_iter().enumerate() {
        let mut tree = ProjectsTree::in_home(&scratch.join(index.to_string()));
        let starred = writer.location(index, &mut tree);
        let location = catalogue.add_location(LocationKind::Local, name)?;
        let passes = sync_location(
            &archive,
            &location.id,
            &tree.transport(),
            &providers,
            &CancelToken::new(),
            &mut |_| {},
        )?;
        for pass in passes {
            let report = pass.outcome?;
            let index = pass.index?;
            ensure!(
                report.failed.is_empty() && report.deferred.is_empty() && index.failed.is_empty(),
                "{name} did not sync whole: {report:?}, {index:?}"
            );
        }
        for key in starred {
            let session = SessionId {
                location: location.id.clone(),
                provider: provider.as_str().to_string(),
                key,
            };
            catalogue.set_starred(&session, true)?;
        }
        locations.push(location.id);
    }

    let mut made: Vec<(&str, FolderId)> = Vec::new();
    for &(name, parent, paths) in FOLDERS {
        let parent = match parent {
            Some(parent) => Some(
                made.iter()
                    .find(|(name, _)| *name == parent)
                    .with_context(|| format!("{parent} is made before {name}"))?
                    .1,
            ),
            None => None,
        };
        let folder = catalogue.create_folder(name, parent)?;
        for (location, path) in paths {
            catalogue.attach_path(folder, path, location.map(|at| &locations[at]))?;
        }
        made.push((name, folder));
    }
    if writer.filler_stars != 0 {
        bail!(
            "{} of the boards' starred sessions were not starred",
            writer.filler_stars
        );
    }
    drop(archive);
    std::fs::remove_dir_all(scratch)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use pretty_assertions::assert_eq;
    use sb_core::catalogue::Shelf;

    use super::*;

    /// The sidebar's counts on `E-Folder`.
    const SESSIONS: usize = 2318;
    const UNSORTED: usize = 37;
    const FC_SESSIONS: usize = 488;

    #[test]
    fn the_buckets_add_up_to_the_boards_counts() {
        let sessions: usize = BUCKETS.iter().map(|bucket| bucket.sessions).sum();
        assert_eq!(sessions, SESSIONS);
        let fc: usize = BUCKETS
            .iter()
            .filter(|bucket| bucket.fc)
            .map(|bucket| bucket.sessions)
            .sum();
        assert_eq!(fc, FC_SESSIONS);
        assert_eq!(day_after(0), (2025, 11, 3));
        assert_eq!(day_after(28), (2025, 12, 1));
        assert_eq!(day_after(300), (2026, 8, 30));
    }

    #[test]
    fn the_archive_holds_the_library_the_boards_draw() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        build(&archive_dir, &dir.path().join("locations")).unwrap();
        let archive = Archive::open(&archive_dir).unwrap();
        let catalogue = archive.catalogue();

        // `E-Folder`'s sidebar.
        let counts = catalogue.library_counts().unwrap();
        assert_eq!(
            (counts.all, counts.starred, counts.unsorted),
            (SESSIONS, STARRED, UNSORTED)
        );
        let folders = catalogue.folders().unwrap();
        let by_name: HashMap<&str, usize> = folders
            .iter()
            .map(|folder| (folder.name.as_str(), counts.folders[&folder.id]))
            .collect();
        assert_eq!(
            by_name,
            HashMap::from([
                ("dv", 612),
                ("dv-wsl-backend", 58),
                ("fc", 488),
                ("player", 41),
                ("folio", 506),
                ("slide", 197),
                ("toolbox-rs", 241),
                ("scrollback", 12),
                ("dotfiles", 64),
                ("Scratch", 161),
            ])
        );

        // Its header and its first cards.
        let fc = folders.iter().find(|folder| folder.name == "fc").unwrap();
        let sessions = catalogue.sessions(Shelf::Folder(fc.id)).unwrap();
        assert_eq!(sessions.len(), FC_SESSIONS);
        let calls: u64 = sessions.iter().map(|it| u64::from(it.all.calls)).sum();
        assert_eq!(calls, FC_CALLS);
        let titles: Vec<&str> = sessions[..5]
            .iter()
            .filter_map(|session| session.title.as_deref())
            .collect();
        assert_eq!(
            titles,
            [
                "Downloads: paste a whole playlist",
                "libmpv hwdec fallback on AV1",
                "Portrait layout for vertical cams",
                "Frame-step keys in the player",
                "Line up four cams by their sound",
            ]
        );
        assert_eq!(sessions[0].models, ["claude-fable-5-2"]);
        assert!(sessions[0].starred);

        // `E-All-Sessions`' header.
        let all = catalogue.sessions(Shelf::All).unwrap();
        let calls: u64 = all.iter().map(|it| u64::from(it.all.calls)).sum();
        assert_eq!(calls, CALLS);

        // `E-Unsorted`: six paths, the one with the most sessions first once sorted so.
        let mut groups = catalogue.unsorted().unwrap();
        groups.sort_by_key(|group| std::cmp::Reverse(group.sessions.len()));
        let sizes: Vec<(&str, usize)> = groups
            .iter()
            .map(|group| (group.spellings[0].as_str(), group.sessions.len()))
            .collect();
        assert_eq!(
            sizes,
            [
                ("/home/user/code/advent-2025", 14),
                ("/home/user", 10),
                ("/Users/user/code/blog", 6),
                ("/home/user/code/scrollback-spike", 3),
                ("/home/user/tmp/jq-playground", 3),
                ("/home/user/tmp/regex-golf", 1),
            ]
        );
        assert_eq!(
            groups[0].sessions[0].title.as_deref(),
            Some("Day 12: garden fences, part two")
        );
    }
}
