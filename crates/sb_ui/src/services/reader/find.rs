//! Find in session (DESIGN §4.2.9, PLAN §9): every match of a query in the open session's
//! parsed text, its subagents and its bodies included, collapsed or not, in the order the
//! column draws them.
//!
//! What is searched is what is drawn ([`Spot`]): a prompt's, a reply's, a summary's and a loaded
//! file's text as written (the view lights the matches in what Markdown made of it), a
//! thought's, each activity row's target, each body's texts as its kind shows them as Show all
//! shows them ([`super::shape`]'s text-only shapes: a terminal's text without its colours,
//! JSON printed again within its bound, an edit's text before and after, never the marks), and
//! a web search's result titles. So a match's range is where the body draws it.
//!
//! It runs off the UI thread, over the transcript's content, so it is bounded:
//! - each body text is read up to [`PART_BYTES`], and matching is linear
//!   ([`sb_core::search::find_all`]);
//! - it stops at [`MATCHES`] matches, the rest of the session unread;
//! - it searches at most [`SUBAGENTS`] subagents, and says so when there are more; those it
//!   reads itself are kept for the open session ([`Read`]), so the next keystroke doesn't read
//!   them again;
//! - it asks whether it is still wanted before each block and each text, so a search
//!   superseded by the next keystroke stops there.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use sb_core::provider::{Provider, SessionItems};
use sb_core::search::find_all;
use sb_core::transcript::{Content, Conversation, Output, Stored, Subagent, ToolCall};

use super::shape::{self, Renderer};
use super::{BlockRef, SubagentRead};

/// The most matches found: past them, the count says there are more.
pub const MATCHES: usize = 10_000;
/// The most of one body text read to search it.
pub const PART_BYTES: u64 = 1024 * 1024;
/// The most subagents one search searches.
pub const SUBAGENTS: usize = 500;

/// The subagents a search read itself, by item, kept while their session is open.
pub type Read = Arc<Mutex<HashMap<Arc<str>, Option<Arc<Conversation>>>>>;

/// Where a match is: which text of which block, as the column draws it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Spot {
    /// A prompt's, a reply's, a thought's, a compaction's summary's or a loaded file's text.
    Prose(BlockRef),
    /// What an activity row shows the call was about.
    Target(BlockRef),
    /// A call's input (`None`) or a part of its output, as its body shows it.
    Part(BlockRef, Option<usize>),
    /// An edit's diff.
    Diff(BlockRef),
    /// A web fetch's result, as Markdown.
    Fetched(BlockRef),
    /// The title of one of a web search's results.
    Result(BlockRef, usize),
}

impl Spot {
    pub fn block(&self) -> &BlockRef {
        match self {
            Self::Prose(at)
            | Self::Target(at)
            | Self::Part(at, _)
            | Self::Diff(at)
            | Self::Fetched(at)
            | Self::Result(at, _) => at,
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

/**
 * What a search found: its matches in the column's order, whether it stopped at [`MATCHES`]
 * with more to find, and whether it left subagents past [`SUBAGENTS`] unsearched.
 */
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Findings {
    pub matches: Vec<Found>,
    pub capped: bool,
    pub subagents_left: bool,
}

/// What a search reads of the open session.
pub struct Source {
    pub main: Option<Arc<Conversation>>,
    /// The subagents the reader has read, by item; the rest are read for the search.
    pub subagents: HashMap<Arc<str>, Arc<SubagentRead>>,
    pub unattached: Vec<Arc<str>>,
    /// Those searches read before, kept for the session.
    pub read: Read,
    /// The session's project folder, which targets are written from.
    pub root: Option<String>,
}

/// Why a search stopped before the session's end.
enum Stop {
    /// It is no longer wanted: what it found is dropped.
    Unwanted,
    /// It found [`MATCHES`]: what it found stands.
    Capped,
}

type Going = Result<(), Stop>;

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
        subagents: 0,
        turn: None,
    };
    let walked = (|| {
        if let Some(main) = &source.main {
            search.seen.insert(main.item.clone());
            search.conversation(main, true)?;
        }
        search.turn = None;
        for item in &source.unattached {
            search.subagent(item)?;
        }
        Ok(())
    })();
    match walked {
        Err(Stop::Unwanted) => None,
        Ok(()) | Err(Stop::Capped) => Some(search.findings),
    }
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
    /// How many subagents it searched.
    subagents: usize,
    /// The turn of the session's own conversation being searched.
    turn: Option<usize>,
}

impl Search<'_> {
    fn wanted(&self) -> Going {
        if (self.going)() {
            Ok(())
        } else {
            Err(Stop::Unwanted)
        }
    }

    /// Finds the query in `text` at `spot`; stops once the matches reach [`MATCHES`].
    fn text(&mut self, spot: Spot, text: &str) -> Going {
        self.wanted()?;
        let left = MATCHES - self.findings.matches.len();
        let found = find_all(text, self.query, left + 1);
        let capped = found.len() > left;
        let turn = self.turn;
        self.findings
            .matches
            .extend(found.into_iter().take(left).map(|range| Found {
                spot: spot.clone(),
                range,
                turn,
            }));
        if capped {
            self.findings.capped = true;
            return Err(Stop::Capped);
        }
        Ok(())
    }

    /// A body text read up to [`PART_BYTES`], as `shape` shows it, searched at `spot`.
    fn stored(&mut self, spot: Spot, stored: &Stored, shape: impl Fn(String) -> String) -> Going {
        self.wanted()?;
        match stored.load_head(self.items, PART_BYTES) {
            Ok((text, _)) => self.text(spot, &shape(text)),
            Err(_) => Ok(()),
        }
    }

    fn conversation(&mut self, conversation: &Conversation, own: bool) -> Going {
        for (index, turn) in conversation.turns().iter().enumerate() {
            if own {
                self.turn = Some(index);
            }
            for &id in &turn.blocks {
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
                        summary: Some(stored),
                        ..
                    }
                    | Content::ContextFile { text: stored, .. } => {
                        self.stored(Spot::Prose(at), stored, |text| text)?
                    }
                    Content::ToolCall(call) => self.call(at, call)?,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// A call: its target, the subagents it started, then its body, as the rows go.
    fn call(&mut self, at: BlockRef, call: &ToolCall) -> Going {
        self.text(
            Spot::Target(at.clone()),
            &crate::hostile::label(&shape::target(call, self.source.root.as_deref())),
        )?;
        for subagent in &call.subagents {
            match subagent {
                Subagent::Read { conversation, .. } => {
                    if self.seen.insert(conversation.item.clone()) && self.counted() {
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
        let first_text = |this: &Self| {
            texts().next().and_then(|(_, stored)| {
                stored
                    .load_head(this.items, shape::RESULT_BYTES)
                    .ok()
                    .map(|(text, _)| text)
            })
        };
        let outputs = |this: &mut Self| -> Going {
            for (index, stored) in texts() {
                this.stored(Spot::Part(at.clone(), Some(index)), stored, |text| {
                    shape::output_text(call, text)
                })?;
            }
            Ok(())
        };
        let input = |this: &mut Self| {
            this.stored(Spot::Part(at.clone(), None), &call.raw_input, |text| {
                shape::input_text(call, text)
            })
        };
        let failed = call
            .result
            .as_ref()
            .is_some_and(|result| result.status == sb_core::transcript::ToolStatus::Failed);
        match shape::renderer(call) {
            Renderer::Shell | Renderer::Read | Renderer::Search | Renderer::Files => outputs(self),
            Renderer::Diff => {
                self.wanted()?;
                if let Some(text) = shape::edit_text(call, self.items) {
                    self.text(Spot::Diff(at.clone()), &text)?;
                }
                if failed { outputs(self) } else { Ok(()) }
            }
            Renderer::Todos if failed => outputs(self),
            Renderer::Todos => Ok(()),
            Renderer::WebSearch => {
                self.wanted()?;
                let results = first_text(self).map(|text| shape::results(&text));
                match results.filter(|results| !results.is_empty()) {
                    Some(results) => {
                        for (index, found) in results.iter().enumerate() {
                            self.text(Spot::Result(at.clone(), index), &found.title)?;
                        }
                        Ok(())
                    }
                    None => {
                        input(self)?;
                        outputs(self)
                    }
                }
            }
            Renderer::WebFetch => {
                self.wanted()?;
                match first_text(self) {
                    Some(text) => self.text(Spot::Fetched(at.clone()), &text),
                    None => {
                        input(self)?;
                        outputs(self)
                    }
                }
            }
            Renderer::Mcp => {
                input(self)?;
                match texts().next() {
                    Some((index, stored)) => {
                        self.stored(Spot::Part(at.clone(), Some(index)), stored, |text| {
                            shape::output_text(call, text)
                        })
                    }
                    None => Ok(()),
                }
            }
            Renderer::Generic => {
                input(self)?;
                outputs(self)
            }
        }
    }

    /// Whether another subagent may be searched ([`SUBAGENTS`]); counts it if so.
    fn counted(&mut self) -> bool {
        if self.subagents == SUBAGENTS {
            self.findings.subagents_left = true;
            return false;
        }
        self.subagents += 1;
        true
    }

    /// The subagent `item`'s conversation, as the reader read it, or a search before did, or
    /// read now; once.
    fn subagent(&mut self, item: &Arc<str>) -> Going {
        if !self.seen.insert(item.clone()) || !self.counted() {
            return Ok(());
        }
        self.wanted()?;
        if let Some(read) = self.source.subagents.get(item) {
            return self.conversation(&read.conversation, false);
        }
        let kept = self
            .source
            .read
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(item)
            .cloned();
        let conversation = match kept {
            Some(conversation) => conversation,
            None => {
                let read = match self.provider.subagent(self.items, item) {
                    Subagent::Read { conversation, .. } => Some(Arc::new(*conversation)),
                    Subagent::Unread { .. } => None,
                };
                self.source
                    .read
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(item.clone(), read.clone());
                read
            }
        };
        match conversation {
            Some(conversation) => self.conversation(&conversation, false),
            None => Ok(()),
        }
    }
}
