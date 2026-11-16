//! Where a session opens in the reader when it is opened at a search match (DESIGN §4.9
//! "Keys", PLAN §10 "A result carries the session, the turn and the block").
//!
//! The palette asks for it with [`crate::services::LibraryService::open_at`]. Each request is
//! new, with a number of its own ([`ReadAt::request`]): opening the same match twice asks twice.
//! A session opened any other way (a card, a row, `Enter` on a page) has none and opens at its
//! start; opening another session, or closing the reader, drops a request not yet taken.
//!
//! **What the reader does with one:**
//! 1. It takes it, once, with [`crate::services::LibraryService::take_read_at`], when it has
//!    parsed the open session far enough to place it, and only if [`ReadAt::session`] is the
//!    session open. Taking it is what makes it one-shot: a parse again (a sync appended to the
//!    session, the version read changed) never moves the reader back to it.
//! 2. It scrolls to [`ReadAt::at`] in the session's own conversation: the block at its turn,
//!    or the subagent at that place of `Transcript::unattached`. For a match in a subagent's
//!    text it expands the call at `at`, then the subagent whose conversation is the item
//!    [`InSubagent::item`] (found by that key among the subagents under the call, at any
//!    depth), and scrolls to the turn and block there.
//! 3. It fills Find in session with [`ReadAt::find`], without stepping to its first match.
//! 4. It flashes [`ReadAt::flash`] (`hl.cur` for a second) in the block it scrolled to.
//!
//! **The places are positional.** `at` and the subagent's turn and block are places in the
//! conversation as the search index parsed it. When the session was synced or re-parsed since,
//! they may point elsewhere. So the reader first checks that the block at the place holds
//! `flash` (as Find matches: any case, Latin accents folded); when it doesn't, it falls back
//! to Find's first match of `find`, and flashes that.

use sb_core::catalogue::SessionId;
use sb_core::search::{Match, MatchKind};
use sb_core::transcript::{At, InSubagent};

/// What the reader does with a session opened at a match (the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadAt {
    /// Counts the requests the library was given, from 1: a request is a new one even for
    /// the same match.
    pub request: u64,
    /// The session it is for.
    pub session: SessionId,
    /**
     * Where in the session's own conversation: the block the match is in, with its turn
     * (`At::Block`); for a match in a subagent's text, the call that started the outermost
     * subagent; or a subagent no call is known to have started (`At::Unattached`). `None`
     * for a match in the title, and for a session row: the session opens at its start.
     */
    pub at: Option<At>,
    /// For a match in a subagent's text, which subagent's conversation it is in (the
    /// innermost, when subagents nest) and its turn and block there.
    pub subagent: Option<InSubagent>,
    /// What the match is: the kind column's prompt, reply, command, path or title.
    pub kind: MatchKind,
    /// What Find in session is filled with: the query's terms, without its filter tokens.
    pub find: String,
    /// The text to flash: the match's first hit as the span holds it, in its own case.
    /// `None` with no match to flash.
    pub flash: Option<String>,
}

impl ReadAt {
    /// Where opening `found`, a match of the session `session`, takes the reader, for a
    /// query whose terms are `find`. The library numbers it when it is asked for.
    pub fn of(session: SessionId, found: &Match, find: &str) -> Self {
        let snippet = &found.snippet;
        Self {
            request: 0,
            session,
            at: found.at,
            subagent: found.subagent.clone(),
            kind: found.kind,
            find: find.to_string(),
            flash: snippet
                .hits
                .first()
                .and_then(|hit| snippet.text.get(hit.clone()))
                .map(str::to_string),
        }
    }
}
