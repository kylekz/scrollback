//! What the search and Trash scenarios add to the boards' library ([`crate::boards`]): the
//! sessions `E-Search` finds, with the prompts, replies, commands, paths and a subagent's text
//! its matches are in; and the three sessions `E-Trash` lists, moved to Trash. Everything here
//! is made up, in placeholder names.

use std::path::Path;

use anyhow::{Context as _, Result};
use sb_core::archive::Archive;
use sb_core::catalogue::{SessionId, Shelf};
use sb_test_support::{Call, Outline, ProjectsTree};
use serde_json::{Value, json};
use toolbox_store::rusqlite;

/// What a scenario adds to the boards' library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extra {
    Nothing,
    /// One of the subfolder player's sessions held back and filed by hand.
    HeldBack,
    /// `E-Search`'s sessions.
    Search,
    /// `E-Trash`'s sessions, in Trash.
    Trash,
}

/// Where fc's sessions from the distro started.
const FC: &str = "/home/user/code/fc";
const OPUS: &str = "claude-opus-5-5";

/// The record of a reply's text, at `at`.
fn text(text: &str) -> Value {
    json!([{ "type": "text", "text": text }])
}

/**
 * One of `E-Search`'s sessions at `day`: its prompt, then for each step a tool call (its tool,
 * its input and what it returned) and the reply after it, and with `subagent` a subagent
 * started to do a task, whose reply holds text of its own. Returns nothing: the session is
 * written to `tree`.
 */
fn searched(
    tree: &mut ProjectsTree,
    title: &str,
    day: (u32, u32),
    prompt: &str,
    steps: &[(&str, Value, &str)],
    subagent: Option<(&str, &str)>,
) {
    let mut session = tree.session(FC, 0);
    let at = |minute: usize| format!("2026-{:02}-{:02}T11:{minute:02}:00.000Z", day.0, day.1);
    let mut minute = 0;
    let mut next = || {
        minute += 1;
        at(minute)
    };
    session.transcript.user(
        json!(prompt),
        json!({ "origin": { "kind": "human" }, "timestamp": at(0) }),
    );
    for (message, (tool, input, reply)) in steps.iter().enumerate() {
        let id = format!("toolu_search_{message}");
        let uuid = session.transcript.assistant(
            message * 2 + 1,
            OPUS,
            json!([{ "type": "tool_use", "id": id, "name": tool, "input": input }]),
            json!({ "timestamp": next() }),
        );
        let call = Call { id, uuid };
        session.transcript.user(
            json!([{ "type": "tool_result", "tool_use_id": call.id, "content": "done" }]),
            json!({ "timestamp": next(), "toolUseResult": {} }),
        );
        if let Some((task, said)) = subagent.filter(|_| *tool == "Agent") {
            let mut records = session.subagent_of(&call, "general-purpose", task);
            records.user(json!(task), json!({ "timestamp": next() }));
            records.assistant(1, OPUS, text(said), json!({ "timestamp": next() }));
        }
        session.transcript.assistant(
            message * 2 + 2,
            OPUS,
            text(reply),
            json!({ "timestamp": next() }),
        );
    }
    session.transcript.title(title);
}

/// Writes `E-Search`'s sessions to the distro's tree: fc's, in March 2026.
pub fn search(tree: &mut ProjectsTree) {
    searched(
        tree,
        "Throttle yt-dlp metadata backfill",
        (3, 14),
        "Backfilling metadata for the whole library trips the rate limit. Throttle it: a few at \
         a time, a backoff on 429, and show progress in Library health.",
        &[
            (
                "Bash",
                json!({ "command": "rg -n \"backoff|429\" crates/fc_tools" }),
                "Nothing retries yet. HTTP Error 429 on stderr becomes FetchError::RateLimited; \
                 the loop waits with a doubling backoff.",
            ),
            (
                "Read",
                json!({ "file_path": "/home/user/code/fc/crates/fc_tools/src/backoff.rs" }),
                "The backoff starts at a second and doubles to a minute.",
            ),
            (
                "Agent",
                json!({
                    "subagent_type": "general-purpose",
                    "description": "Find the retries",
                    "prompt": "List every caller that retries a fetch.",
                }),
                "Three callers retry; each now shares the backoff.",
            ),
        ],
        Some((
            "List every caller that retries a fetch.",
            "Three callers retry a fetch on 429: the playlist expander, the thumbnailer and the \
             metadata backfill.",
        )),
    );
    searched(
        tree,
        "Back off on 429 only, not on 403",
        (3, 15),
        "A 403 means the cookies expired; a backoff just hides it. Only 429 should wait.",
        &[(
            "Edit",
            json!({
                "file_path": "/home/user/code/fc/crates/fc_tools/src/backoff.rs",
                "old_string": "is_retryable",
                "new_string": "is_rate_limited",
            }),
            "Backoff::wait now runs for 429 only; 403 surfaces as \"Sign in again\" in Library \
             health.",
        )],
        None,
    );
}

/// `E-Trash`'s sessions: the location each is from (by its place in the boards' list), its
/// outline, when it was moved to Trash, and whether its location still has it.
const TRASHED: [(usize, Outline<'static>, &str, bool); 3] = [
    (
        1,
        Outline {
            project: "/home/user/scratch/tsbash",
            title: "Try tree-sitter-bash for command parsing",
            prompt: "Can tree-sitter-bash split a pipeline into commands so search can index \
                     each one on its own?",
            model: OPUS,
            day: (2026, 11, 12),
            calls: 34,
        },
        "2026-11-13 12:30:00",
        true,
    ),
    (
        0,
        Outline {
            project: r"C:\Users\user",
            title: "test",
            prompt: "test",
            model: OPUS,
            day: (2026, 11, 13),
            calls: 0,
        },
        "2026-11-13 12:00:00",
        false,
    ),
    (
        2,
        Outline {
            project: "/Users/user/dotfiles",
            title: "Paste API key into .env by mistake",
            prompt: "Here is my key sk-placeholder, put it in .env and run the deploy script.",
            model: OPUS,
            day: (2026, 9, 30),
            calls: 9,
        },
        "2026-10-02 12:00:00",
        true,
    ),
];

/// Writes `E-Trash`'s sessions of the location at `index` to its tree.
pub fn trash_sessions(index: usize, tree: &mut ProjectsTree) {
    for (_, outline, ..) in TRASHED.iter().filter(|it| it.0 == index) {
        tree.outlined(outline);
    }
}

/**
 * Moves `E-Trash`'s sessions to Trash, each on the day the board says, and marks the one its
 * location no longer has as gone there: its confirmation says it is the last copy.
 */
pub fn trash(archive_dir: &Path) -> Result<()> {
    let archive = Archive::open(archive_dir)?;
    let catalogue = archive.catalogue();
    let sessions = catalogue.sessions(Shelf::All)?;
    let mut moved: Vec<(SessionId, &str, bool)> = Vec::new();
    for (_, outline, when, there) in &TRASHED {
        let session = sessions
            .iter()
            .find(|it| {
                it.title.as_deref() == Some(outline.title)
                    && it.project_path.as_deref() == Some(outline.project)
            })
            .with_context(|| format!("{} is synced", outline.title))?;
        catalogue.trash(&session.id)?;
        moved.push((session.id.clone(), when, *there));
    }
    drop(archive);
    /* Only the catalogue says when a session was trashed and an item was last seen. */
    let db = rusqlite::Connection::open(archive_dir.join("scrollback.db"))?;
    for (id, when, there) in moved {
        let identity = rusqlite::params![id.location, id.provider, id.key, when];
        db.execute(
            "UPDATE session_state SET trashed_at = CAST(strftime('%s', ?4) AS INTEGER) * 1000
             WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            identity,
        )?;
        if !there {
            db.execute(
                "UPDATE item SET gone_since = CAST(strftime('%s', ?4) AS INTEGER) * 1000
                 WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                identity,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use sb_core::CancelToken;
    use sb_core::search::{Limit, MatchKind, Query, Searched};

    use super::*;
    use crate::boards;

    fn built(extra: Extra) -> (tempfile::TempDir, Archive) {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        boards::build(&archive_dir, &dir.path().join("locations"), extra).unwrap();
        let archive = Archive::open(&archive_dir).unwrap();
        (dir, archive)
    }

    #[test]
    fn the_search_boards_sessions_are_found_in_every_kind_of_text() {
        let (_dir, archive) = built(Extra::Search);
        let query = Query {
            terms: vec!["backoff".to_string(), "429".to_string()],
            ..Query::default()
        };
        let Searched::Results(results) = archive
            .search(&query, Limit::default(), &CancelToken::new())
            .unwrap()
        else {
            panic!("the terms are long enough");
        };
        let titles: Vec<&str> = (results.sessions.iter())
            .filter_map(|found| found.session.title.as_deref())
            .collect();
        assert_eq!(
            titles,
            [
                "Back off on 429 only, not on 403",
                "Throttle yt-dlp metadata backfill"
            ]
        );
        let throttle = &results.sessions[1];
        let kinds: Vec<(MatchKind, bool)> = (throttle.shown.iter())
            .map(|it| (it.kind, it.subagent))
            .collect();
        assert!(kinds.contains(&(MatchKind::Prompt, false)), "{kinds:?}");
        assert!(kinds.contains(&(MatchKind::Command, false)), "{kinds:?}");
        assert!(throttle.matches > throttle.shown.len(), "+n more");
    }

    #[test]
    fn the_trash_boards_sessions_are_in_trash_and_one_is_gone_at_its_location() {
        let (_dir, archive) = built(Extra::Trash);
        let catalogue = archive.catalogue();
        let mut trashed = catalogue.sessions(Shelf::Trash).unwrap();
        trashed.sort_by_key(|it| std::cmp::Reverse(it.trashed));
        assert_eq!(
            trashed
                .iter()
                .map(|it| it.title.as_deref())
                .collect::<Vec<_>>(),
            [
                Some("Try tree-sitter-bash for command parsing"),
                Some("test"),
                Some("Paste API key into .env by mistake")
            ]
        );
        assert_eq!(catalogue.library_counts().unwrap().all, 2318);
        let there: Vec<bool> = (trashed.iter())
            .map(|it| catalogue.deletion(&it.id).unwrap().at_location)
            .collect();
        assert_eq!(there, [true, false, true]);
    }
}
