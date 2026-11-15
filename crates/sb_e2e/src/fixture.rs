//! Generated sessions for the scenarios' agent trees (docs/PLAN.md §12). Everything here is
//! made up: titles, prompts and paths in placeholder names.

use sb_test_support::{Outline, ProjectsTree};

/// An agent's transcripts on this machine: two sessions of one project and one of another,
/// for the first run to find and Unsorted to group.
pub fn agent(tree: &mut ProjectsTree) {
    for outline in [
        Outline {
            project: "/home/user/code/app",
            title: "Fix the flaky upload test",
            prompt: "The upload test fails one run in five. Find the race and fix it.",
            model: "claude-opus-5-5",
            day: (2026, 11, 12),
            calls: 7,
        },
        Outline {
            project: "/home/user/code/app",
            title: "Add retries to the fetcher",
            prompt: "Retry a failed fetch three times with backoff before giving up.",
            model: "claude-sonnet-5",
            day: (2026, 11, 10),
            calls: 3,
        },
        Outline {
            project: "/home/user/notes",
            title: "Summarise this week's notes",
            prompt: "Read the notes from this week and write a summary for Friday.",
            model: "claude-opus-5-5",
            day: (2026, 10, 30),
            calls: 0,
        },
    ] {
        tree.outlined(&outline);
    }
}
