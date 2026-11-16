//! Fenced code's highlights (PLAN §9), kept for the app by the code's text ([`Kept`]), and the
//! highlighter every `TextView` in the app draws fenced code with ([`init`]).
//!
//! The reader draws a fence its scan of a body found with its own code view, and asks the
//! reader service for the highlights, made as a job off the UI thread. A fence the scan missed
//! is drawn by `TextView`, inside a piece of Markdown, with the highlighter installed for the
//! app. gpui-component installs its own whenever it pushes a theme down to gpui-base, and that
//! one parses the whole block on the UI thread with no deadline: a minute, on some code. So
//! ours takes its place each time. It draws the highlights kept, else the code plain, and asks
//! the reader service for them through a channel, as it runs without a context; when they
//! land, a new highlighter is installed ([`renew`]), so `TextView` asks again and draws them.
//! Nothing else highlights code.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use gpui::{App, Global, HighlightStyle};
use gpui_base::TextViewDefaults;
use gpui_base::text::CodeBlock;

use super::{Code, Highlights};
use crate::syntax::MAX_HIGHLIGHT_BYTES;
use crate::theme::ActiveTheme as _;

/**
 * The most the kept highlights take, their code's text with them; past it, those asked for
 * least lately go. A piece of code is at most [`MAX_HIGHLIGHT_BYTES`], its highlights at most
 * a token a byte.
 */
pub const KEPT_BYTES: usize = 8 * 1024 * 1024;

/// The highlights made lately, by the code's text, the least lately asked for let go first.
#[derive(Default)]
pub struct Kept {
    by_code: HashMap<Code, (Highlights, u64)>,
    /// The codes by when they were last asked for.
    by_age: BTreeMap<u64, Code>,
    bytes: usize,
    /// Counts the asks, for `by_age`.
    asks: u64,
}

impl Kept {
    /// The highlights of `code`, when they have been made; asked for, they are kept longer.
    pub fn get(&mut self, code: &Code) -> Option<Highlights> {
        self.asks += 1;
        let (highlights, asked) = self.by_code.get_mut(code)?;
        let code = self.by_age.remove(asked)?;
        *asked = self.asks;
        self.by_age.insert(self.asks, code);
        Some(highlights.clone())
    }

    pub fn insert(&mut self, code: Code, highlights: Highlights) {
        self.asks += 1;
        self.bytes += size(&code, &highlights);
        if let Some((before, asked)) = self.by_code.insert(code.clone(), (highlights, self.asks)) {
            self.by_age.remove(&asked);
            self.bytes -= size(&code, &before);
        }
        self.by_age.insert(self.asks, code);
        while self.bytes > KEPT_BYTES
            && let Some((_, oldest)) = self.by_age.pop_first()
            && let Some((highlights, _)) = self.by_code.remove(&oldest)
        {
            self.bytes -= size(&oldest, &highlights);
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// What `code`'s highlights take kept.
fn size(code: &Code, highlights: &Highlights) -> usize {
    code.language.len() + code.text.len() + size_of_val(&**highlights)
}

type Highlighter = dyn Fn(&CodeBlock) -> Vec<(Range<usize>, HighlightStyle)> + Send + Sync;

/// The highlights kept, the channel `TextView`'s highlighter asks through, and the highlighter.
struct Shared {
    kept: Arc<Mutex<Kept>>,
    asks: async_channel::Sender<Code>,
    asked: async_channel::Receiver<Code>,
    current: Arc<Highlighter>,
}

impl Global for Shared {}

/**
 * Installs our highlighter for every `TextView` (the module docs), and again each time
 * gpui-component installs its own: with each theme it pushes to gpui-base. Call after the theme
 * is applied.
 */
pub fn init(cx: &mut App) {
    let (asks, asked) = async_channel::unbounded();
    cx.set_global(Shared {
        kept: Arc::default(),
        asks,
        asked,
        current: Arc::new(|_| Vec::new()),
    });
    renew(cx);
    cx.observe_global::<gpui_base::Theme>(renew).detach();
}

/**
 * Installs a new highlighter, in the theme's colours: each `TextView` highlights its blocks
 * again with it, as it would with another theme.
 */
pub fn renew(cx: &mut App) {
    let syntax = cx.sb_theme().colors.syntax;
    let shared = cx.global::<Shared>();
    let (kept, asks) = (shared.kept.clone(), shared.asks.clone());
    let current: Arc<Highlighter> = Arc::new(move |block: &CodeBlock| {
        let (Some(language), text) = (block.lang(), block.code()) else {
            return Vec::new();
        };
        if language.is_empty() || text.len() > MAX_HIGHLIGHT_BYTES {
            return Vec::new();
        }
        let code = Code {
            language: language.as_ref().into(),
            text: text.as_ref().into(),
        };
        let found = kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&code);
        match found {
            Some(highlights) => crate::syntax::styles(&code.text, &highlights, &syntax),
            None => {
                /* Only when the reader service is gone does this fail, and then nothing is
                highlighted. */
                asks.try_send(code).ok();
                Vec::new()
            }
        }
    });
    let installed = current.clone();
    TextViewDefaults::global(cx)
        .with_code_block_highlighter(move |block| installed(block))
        .install(cx);
    cx.global_mut::<Shared>().current = current;
}

/// The highlighter installed: a `TextView` built with it keeps its blocks' highlights while it
/// stays the one installed.
pub fn current(cx: &App) -> Arc<Highlighter> {
    cx.global::<Shared>().current.clone()
}

/// The highlights kept, which the reader service makes.
pub fn kept(cx: &App) -> Arc<Mutex<Kept>> {
    cx.global::<Shared>().kept.clone()
}

/// What `TextView`'s highlighter asks for, for the reader service to make.
pub fn asked(cx: &App) -> async_channel::Receiver<Code> {
    cx.global::<Shared>().asked.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::TokenKind;

    fn code(n: usize, bytes: usize) -> Code {
        Code {
            language: "rust".into(),
            text: format!("{n:0bytes$}").into(),
        }
    }

    #[test]
    fn the_kept_are_bounded_by_bytes_and_the_least_lately_asked_for_go_first() {
        let mut kept = Kept::default();
        let token: Highlights = Arc::from(vec![(0..1, TokenKind::Keyword); 1_000]);
        let each = size(&code(0, 16 * 1024), &token);
        let fit = KEPT_BYTES / each;
        for n in 0..fit {
            kept.insert(code(n, 16 * 1024), token.clone());
        }
        assert_eq!(kept.bytes(), fit * each);
        /* The first, asked for again, outlives the second. */
        assert!(kept.get(&code(0, 16 * 1024)).is_some());
        kept.insert(code(fit, 16 * 1024), token.clone());
        assert!(kept.bytes() <= KEPT_BYTES);
        assert!(kept.get(&code(0, 16 * 1024)).is_some());
        assert!(kept.get(&code(1, 16 * 1024)).is_none());
        assert!(kept.get(&code(fit, 16 * 1024)).is_some());
        /* Made again, counted once. */
        let before = kept.bytes();
        kept.insert(code(fit, 16 * 1024), token);
        assert_eq!(kept.bytes(), before);
        assert_eq!(kept.by_code.len(), kept.by_age.len());
    }
}
