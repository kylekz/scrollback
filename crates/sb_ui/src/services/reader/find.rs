//! Find in session (DESIGN §4.2.9, PLAN §9): every match of a query in the open session's
//! parsed text, its subagents and its bodies included, collapsed or not, in the order the
//! column draws them.
//!
//! What is searched is what is drawn ([`Spot`]): a prompt's, a reply's and a summary's text as
//! written (the view lights the matches in what Markdown made of it), a thought's, each
//! activity row's target, and each body's texts as its kind shapes them ([`super::shape`]) and
//! Show all shows them, so a match's range is where the body draws it.
//!
//! It runs off the UI thread, over the transcript's content, so it is bounded: each body text
//! is read up to [`PART_BYTES`], the matches stop at [`MATCHES`], matching is linear
//! ([`sb_core::search::find_all`]), and it asks whether it is still wanted before each block
//! and each text read, so a search superseded by the next keystroke stops there.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use sb_core::provider::{Provider, SessionItems};
use sb_core::search::find_all;
use sb_core::transcript::{Content, Conversation, Output, Stored, Subagent, ToolCall};

use super::shape::{self, Renderer};
use super::{BlockRef, SubagentRead};

/// The most matches found: past them, the count says there are more.
pub const MATCHES: usize = 10_000;
/// The most of one body text read to search it.
pub const PART_BYTES: u64 = 1024 * 1024;

/// Where a match is: which text of which block, as the column draws it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Spot {
    /// A prompt's, a reply's, a thought's or a compaction's summary's text.
    Prose(BlockRef),
    /// What an activity row shows the call was about.
    Target(BlockRef),
    /// A call's input (`None`) or a part of its output, as its body shows it.
    Part(BlockRef, Option<usize>),
    /// An edit's diff.
    Diff(BlockRef),
    /// A web fetch's result, as Markdown.
    Fetched(BlockRef),
}

impl Spot {
    pub fn block(&self) -> &BlockRef {
        match self {
            Self::Prose(at)
            | Self::Target(at)
            | Self::Part(at, _)
            | Self::Diff(at)
            | Self::Fetched(at) => at,
        }
    }
}

/// A match: where, the bytes of that text it covers, and the turn of the session's own
/// conversation it is in (for a subagent's, the turn of the call that started it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub spot: Spot,
    pub range: Range<usize>,
    pub turn: Option<usize>,
}

/// What a search found: its matches in the column's order, and whether there were more.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Findings {
    pub matches: Vec<Found>,
    pub capped: bool,
}

/// What a search reads of the open session.
pub struct Source {
    pub main: Option<Arc<Conversation>>,
    /// The subagents read so far, by item; the rest are read for the search.
    pub subagents: HashMap<Arc<str>, Arc<SubagentRead>>,
    pub unattached: Vec<Arc<str>>,
    /// The session's project folder, which targets are written from.
    pub root: Option<String>,
}

/**
 * Every match of `query` in `source`, the session's items read by `provider` through `items`,
 * while `going` says the search is still wanted; `None` once it isn't.
 */
pub fn search(
    query: &str,
    source: &Source,
    provider: &dyn Provider,
    items: &dyn SessionItems,
    going: &dyn Fn() -> bool,
) -> Option<Findings> {
    let mut search = Search {
        query,
        source,
        provider,
        items,
        going,
        findings: Findings::default(),
        seen: HashSet::new(),
        turn: None,
    };
    if let Some(main) = &source.main {
        search.seen.insert(main.item.clone());
        search.conversation(main, true)?;
    }
    search.turn = None;
    for item in &source.unattached {
        search.subagent(item)?;
    }
    Some(search.findings)
}

struct Search<'a> {
    query: &'a str,
    source: &'a Source,
    provider: &'a dyn Provider,
    items: &'a dyn SessionItems,
    going: &'a dyn Fn() -> bool,
    findings: Findings,
    /// The conversations searched, so a subagent claimed twice is searched once.
    seen: HashSet<Arc<str>>,
    /// The turn of the session's own conversation being searched.
    turn: Option<usize>,
}

impl Search<'_> {
    /// Finds the query in `text` at `spot`; `None` once the search is no longer wanted.
    fn text(&mut self, spot: Spot, text: &str) -> Option<()> {
        let left = MATCHES - self.findings.matches.len();
        if left == 0 {
            self.findings.capped = true;
            return Some(());
        }
        let found = find_all(text, self.query, left + 1);
        self.findings.capped |= found.len() > left;
        let turn = self.turn;
        self.findings
            .matches
            .extend(found.into_iter().take(left).map(|range| Found {
                spot: spot.clone(),
                range,
                turn,
            }));
        Some(())
    }

    /// A body text read up to [`PART_BYTES`], shaped by `shape`, searched at `spot`.
    fn stored(
        &mut self,
        spot: Spot,
        stored: &Stored,
        shape: impl Fn(String) -> String,
    ) -> Option<()> {
        (self.going)().then_some(())?;
        if let Ok((text, _)) = stored.load_head(self.items, PART_BYTES) {
            self.text(spot, &shape(text))?;
        }
        Some(())
    }

    fn conversation(&mut self, conversation: &Conversation, own: bool) -> Option<()> {
        for (index, turn) in conversation.turns().iter().enumerate() {
            if own {
                self.turn = Some(index);
            }
            for &id in &turn.blocks {
                (self.going)().then_some(())?;
                let at = BlockRef {
                    conversation: conversation.item.clone(),
                    block: id,
                };
                match &conversation[id].content {
                    Content::Prompt { text, .. } | Content::Reply { text } => {
                        self.text(Spot::Prose(at), text)?
                    }
                    Content::Thinking { text, .. } => {
                        self.text(Spot::Prose(at), &crate::hostile::bidi(text))?
                    }
                    Content::Compaction {
                        summary: Some(summary),
                        ..
                    } => self.stored(Spot::Prose(at), summary, |text| text)?,
                    Content::ToolCall(call) => self.call(at, call)?,
                    _ => {}
                }
            }
        }
        Some(())
    }

    /// A call: its target, the subagents it started, then its body, as the rows go.
    fn call(&mut self, at: BlockRef, call: &ToolCall) -> Option<()> {
        self.text(
            Spot::Target(at.clone()),
            &crate::hostile::label(&shape::target(call, self.source.root.as_deref())),
        )?;
        for subagent in &call.subagents {
            match subagent {
                Subagent::Read { conversation, .. } => {
                    if self.seen.insert(conversation.item.clone()) {
                        self.conversation(conversation, false)?;
                    }
                }
                Subagent::Unread { item } => self.subagent(item)?,
            }
        }
        let texts = || {
            call.result
                .iter()
                .flat_map(|result| result.output.iter().enumerate())
                .filter_map(|(index, part)| match part {
                    Output::Text(stored) => Some((index, stored)),
                    Output::Image(_) => None,
                })
        };
        let first = || texts().next();
        let first_text = |this: &Self| {
            first().and_then(|(_, stored)| {
                stored
                    .load_head(this.items, shape::RESULT_BYTES)
                    .ok()
                    .map(|(text, _)| text)
            })
        };
        let outputs = |this: &mut Self| -> Option<()> {
            for (index, stored) in texts() {
                this.stored(Spot::Part(at.clone(), Some(index)), stored, |text| {
                    shape::output(call, text).text
                })?;
            }
            Some(())
        };
        let generic = |this: &mut Self| -> Option<()> {
            this.stored(Spot::Part(at.clone(), None), &call.raw_input, |text| {
                shape::input(call, text).text
            })?;
            outputs(this)
        };
        let failed = call
            .result
            .as_ref()
            .is_some_and(|result| result.status == sb_core::transcript::ToolStatus::Failed);
        match shape::renderer(call) {
            Renderer::Shell | Renderer::Read | Renderer::Search | Renderer::Files => outputs(self),
            Renderer::Diff => {
                (self.going)().then_some(())?;
                if let Some(Ok(diff)) = shape::edit(call, self.items) {
                    self.text(Spot::Diff(at.clone()), &diff.text)?;
                }
                if failed {
                    outputs(self)?;
                }
                Some(())
            }
            Renderer::Todos => {
                if failed {
                    outputs(self)?;
                }
                Some(())
            }
            Renderer::WebSearch => match first_text(self) {
                Some(text) if !shape::results(&text).is_empty() => Some(()),
                _ => generic(self),
            },
            Renderer::WebFetch => match first_text(self) {
                Some(text) => self.text(Spot::Fetched(at.clone()), &text),
                None => generic(self),
            },
            Renderer::Mcp => {
                self.stored(Spot::Part(at.clone(), None), &call.raw_input, |text| {
                    shape::input(call, text).text
                })?;
                match first() {
                    Some((index, stored)) => {
                        self.stored(Spot::Part(at.clone(), Some(index)), stored, |text| {
                            shape::output(call, text).text
                        })
                    }
                    None => Some(()),
                }
            }
            Renderer::Generic => generic(self),
        }
    }

    /// The subagent `item`'s conversation, as read, or read now; once.
    fn subagent(&mut self, item: &Arc<str>) -> Option<()> {
        if !self.seen.insert(item.clone()) {
            return Some(());
        }
        (self.going)().then_some(())?;
        if let Some(read) = self.source.subagents.get(item) {
            return self.conversation(&read.conversation, false);
        }
        match self.provider.subagent(self.items, item) {
            Subagent::Read { conversation, .. } => self.conversation(&conversation, false),
            Subagent::Unread { .. } => Some(()),
        }
    }
}
