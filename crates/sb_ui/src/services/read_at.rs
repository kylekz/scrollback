//! Where a session opens in the reader when it is opened at a search match (DESIGN §4.9
//! "Keys", PLAN §10 "A result carries the session, the turn and the block"). The palette asks
//! for it with [`crate::services::LibraryService::open_at`]; the reader reads it from
//! [`crate::services::LibraryService::read_at`] once the session's conversation is parsed, and
//! it is cleared when another session opens or the reader closes. A session opened any other
//! way (a card, a row, `Enter` on a page) has none and opens at its start.

use sb_core::search::{Match, MatchKind};
use sb_core::transcript::At;

/// What the reader does with a session opened at a match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadAt {
    /**
     * Where in the session's own conversation: the block the match is in, with its turn
     * (`At::Block`); for a match in a subagent's conversation, the call that started the
     * subagent, which opens expanded (decision 9); or a subagent no call is known to have
     * started (`At::Unattached`, the place in `Transcript::unattached`). `None` for a match in
     * the title, and for a session row: the session opens at its start.
     */
    pub at: Option<At>,
    /// The match is in a subagent's conversation, inside the call at `at`.
    pub subagent: bool,
    /// What the match is: the kind column's prompt, reply, command, path or title.
    pub kind: MatchKind,
    /// What Find in session is filled with: the query's free text as typed, without its
    /// filter tokens, so the reader's own find steps through every match of it.
    pub find: String,
    /**
     * The text of the match to flash (`hl.cur` for a second): its first hit as the span holds
     * it, in the span's own case. The reader looks for it in the block at `at` (in the
     * subagent's conversation under it, for `subagent`). `None` with no match to flash.
     */
    pub flash: Option<String>,
}

impl ReadAt {
    /// Where opening `found` takes the reader, for the query whose free text is `find`.
    pub fn of(found: &Match, find: &str) -> Self {
        let snippet = &found.snippet;
        Self {
            at: found.at,
            subagent: found.subagent,
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
