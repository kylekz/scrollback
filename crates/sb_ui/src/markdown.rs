//! Markdown for prompts, replies and the other prose of a transcript (PLAN D11, DESIGN §4.2.2),
//! rendered by gpui-base's `TextView` in the theme's tokens.
//!
//! Every body is untrusted: it is whatever the agent saw or wrote. `TextView` fetches the URL of
//! an image (`![](url)`, a raw `<img>`) and renders raw HTML, so a body reaches it only as
//! [`Sanitized`], which has neither (PLAN D11). The sanitiser is dv's (D15).
//!
//! [`view`] is the one way a view renders a body, so every body gets the same style and the same
//! link and copy handling ([`crate::system`]: links open only for `http` and `https`). Fenced
//! code is drawn by the reader's own code view, highlighted off the UI thread
//! ([`crate::syntax`]). What `TextView` draws as fenced code (a fence the reader's scan
//! missed) is highlighted off the UI thread too, by the reader's highlighter, which takes
//! gpui-component's place for the whole app ([`crate::services::reader::highlights`]).
//!
//! **The view parses on the UI thread.** `TextView` parses a body of up to 4 KiB as it is handed
//! it, on the UI thread, and a larger one on gpui's background executor; the parser is far
//! slower than linear on some text. So a body reaches it only once that same parse has been
//! timed off the UI thread ([`Sanitized::measured`]): one that took long is shown as plain text
//! instead.
//!
//! A body's parsed document lives in a [`TextViewState`] its owner keeps ([`state`]). One takes
//! about half a megabyte, so the reader keeps them only for the rows near what is on screen
//! (D11).

mod sanitize;

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext as _, Entity, FontFeatures, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, Overflow, ParentElement as _, Pixels, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Styled as _, div, px, rems,
};
use gpui_base::text::{CodeBlock, TextView, TextViewState, TextViewStyle};

use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _, TextSpec};

#[cfg(test)]
pub(crate) use sanitize::inert;
pub use sanitize::{options, sanitize};

/**
 * The most the view's parse of a body may take, for each [`VIEW_PARSE_UNIT`] of the body
 * begun; past it, the body is plain text. Only the view's parse is timed against it: that is
 * what the UI thread pays, while the sanitiser runs in the background with the rest of a
 * body's preparation, bounded by its size and its budget ([`crate::services::reader::prose`]).
 *
 * Measured on an Apple M-series release build, the view parses a unit of ordinary prose in
 * under a quarter of a millisecond, dense but ordinary Markdown (a heading or a link every few
 * words, a long loose list) in about half a millisecond, and a table of a dozen columns in
 * 2 ms; runs of emphasis or of one-line blocks take 3 to 5 ms, and the inputs the parser is
 * slow on from 16 ms to 50 ms. So this keeps ordinary Markdown clear many times over, and the
 * densest twice over, for a slower machine or a busy one, and the slow inputs out.
 */
pub const VIEW_PARSE_LIMIT: Duration = Duration::from_millis(8);
/// gpui-base's `MAX_SYNC_FULL_REPLACE_BYTES`: the most of a body `TextView` parses on the UI
/// thread.
pub const VIEW_PARSE_UNIT: usize = 4 * 1024;

/// [`VIEW_PARSE_LIMIT`] for a body of `bytes`.
pub fn view_parse_limit(bytes: usize) -> Duration {
    let units = bytes.div_ceil(VIEW_PARSE_UNIT).max(1);
    VIEW_PARSE_LIMIT * u32::try_from(units).unwrap_or(u32::MAX)
}

/**
 * A body that has been through [`sanitize`], and whose parse by the view was timed: nothing in
 * it fetches anything or renders markup, and the view parses it quickly. The only way to make
 * one is [`Sanitized::measured`].
 */
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sanitized(SharedString);

impl Sanitized {
    /**
     * `text` sanitised, its bidirectional controls shown as what they are
     * ([`crate::hostile::bidi`]), then parsed as `TextView` parses it, the parse timed: `None`
     * when it took past [`VIEW_PARSE_LIMIT`]. Run it off the UI thread, on a thread with a
     * stack as deep as the body nests.
     *
     * A time not far past the limit is taken again, and the shorter kept: the thread may have
     * been made to wait.
     */
    pub fn measured(text: &str) -> Option<Self> {
        Self::gated(text, |sanitized| {
            let start = Instant::now();
            let parsed =
                gpui_base::text::editor_host::parse_markdown(sanitized, Default::default());
            drop(parsed.ok()?);
            Some(start.elapsed())
        })
    }

    /**
     * [`Sanitized::measured`] without the clock: kept when the view parses it, however long that
     * took. For the tests of what a body is made into, which a busy machine mustn't change.
     */
    #[cfg(test)]
    pub(crate) fn untimed(text: &str) -> Option<Self> {
        Self::gated(text, |sanitized| {
            gpui_base::text::editor_host::parse_markdown(sanitized, Default::default())
                .ok()
                .map(|_| Duration::ZERO)
        })
    }

    /**
     * [`Sanitized::measured`]'s gate, with `parse` for the timed parse of the sanitised text:
     * how long it took, or `None` when it failed. The tests hand it the times to decide on.
     */
    fn gated(text: &str, mut parse: impl FnMut(&str) -> Option<Duration>) -> Option<Self> {
        let sanitized = sanitize(&crate::hostile::bidi(text));
        let limit = view_parse_limit(sanitized.len());
        let took = match parse(&sanitized)? {
            took if took > limit && took < limit * 4 => took.min(parse(&sanitized)?),
            took => took,
        };
        (took <= limit).then(|| Self(sanitized.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The size a body is set at (DESIGN §1.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    /// Transcript prose, 15/24.
    Reading,
    /// Prose inside a nested panel or a tool's result, 13/20.
    Nested,
}

/// The parsed state of a body.
pub fn state(body: &Sanitized, cx: &mut App) -> Entity<TextViewState> {
    cx.new(|cx| TextViewState::markdown(&body.0, cx))
}

/// `state`'s body at `size`, as wide as its container.
pub fn view(state: &Entity<TextViewState>, size: Size, cx: &App) -> TextView {
    let theme = cx.sb_theme();
    let header = CodeHeader {
        mono: theme.fonts.mono.clone(),
        height: theme.sizes.call_header,
        fill: theme.colors.codehead,
        rule: theme.colors.line_subtle,
        label: theme.colors.fg3,
        hover: theme.colors.fg2,
    };
    let text = match size {
        Size::Reading => &theme.text.reading,
        Size::Nested => &theme.text.reading_nested,
    };
    TextView::new(state)
        .style(style(theme))
        .text_spec(text)
        .shared_code_block_highlighter(crate::services::reader::highlights::current(cx))
        .code_block_actions(move |block, _window, _cx| code_block_header(block, header.clone()))
        .on_link_click(|url, _event, _window, cx| crate::system::open_url(url, cx))
        // Defence in depth: a sanitised body has no image, and none is ever fetched.
        .image_source(|_| gpui::ImageSource::Image(Arc::new(gpui::Image::empty())))
}

/// The room between a fenced block's code and its edges, and the room it leaves for its
/// header (`codeblk`: 12 at the sides, 8 above and below the code).
const CODE_PADDING: Pixels = px(12.);
const CODE_PADDING_Y: Pixels = px(8.);

/// The theme's [`TextSpec`] as a style refinement.
fn refined(spec: &TextSpec) -> StyleRefinement {
    StyleRefinement::default()
        .font_family(spec.family.clone())
        .text_size(spec.size)
        .line_height(spec.line_height)
        .font_weight(spec.weight)
}

/// DESIGN §4.2.2's Markdown in `theme`'s tokens, as far as `TextView`'s style reaches.
pub fn style(theme: &SbTheme) -> TextViewStyle {
    let c = &theme.colors;
    let radius = theme.radii.row;
    let (fg, fg2) = (c.fg, c.fg2);
    let headings = [
        refined(&theme.text.heading1),
        refined(&theme.text.heading2),
        refined(&theme.text.heading3),
    ];
    TextViewStyle::default()
        .with_foreground(c.fg)
        .with_muted_foreground(c.fg3)
        .with_link(c.accent_text)
        .with_selection(c.accent_soft)
        .with_code_background(c.code)
        .with_border(c.line)
        .with_dark(theme.appearance == crate::theme::Appearance::Dark)
        // 12 between blocks, at gpui-component's 16px rem.
        .with_paragraph_gap(rems(0.75))
        .with_heading(move |level| {
            let at = usize::from(level.clamp(1, 3)) - 1;
            headings[at]
                .clone()
                .text_color(if level > 3 { fg2 } else { fg })
        })
        .with_inline_code_style(
            StyleRefinement::default()
                .font_family(theme.text.inline_code.family.clone())
                .font_features(FontFeatures::disable_ligatures())
                .text_size(theme.text.inline_code.size)
                .bg(c.s3)
                .rounded(theme.radii.small)
                .px(px(5.))
                .py(px(1.)),
        )
        .with_code_block(
            refined(&theme.text.code_block)
                .font_features(FontFeatures::disable_ligatures())
                .font_weight(FontWeight::NORMAL)
                .text_color(c.fg2)
                .rounded(radius)
                // Clips the header, which is drawn wider than any block (`code_block_header`),
                // and a line too long for the block, which doesn't wrap (`codeblk pre`).
                .overflow_hidden()
                .whitespace_nowrap()
                .px(CODE_PADDING)
                .pt(theme.sizes.call_header + CODE_PADDING_Y + px(1.))
                .pb(CODE_PADDING_Y),
        )
        .with_blockquote_style(StyleRefinement::default().text_color(c.fg2))
        .with_table({
            let mut table = StyleRefinement::default()
                .rounded(radius)
                .bg(c.bg_app)
                .text_size(theme.text.body.size);
            // A table wider than the column scrolls sideways rather than lose its columns.
            table.overflow.x = Some(Overflow::Scroll);
            table
        })
        .with_table_head(
            StyleRefinement::default()
                .bg(c.s1)
                .text_color(c.fg2)
                .text_size(theme.text.secondary.size)
                .font_weight(FontWeight::SEMIBOLD),
        )
        .with_table_cell(
            StyleRefinement::default()
                .border_color(c.line_subtle)
                .px(px(10.))
                .py(theme.spacing.sm),
        )
}

/// What [`code_block_header`] draws with.
#[derive(Clone)]
struct CodeHeader {
    mono: SharedString,
    height: Pixels,
    fill: Hsla,
    rule: Hsla,
    label: Hsla,
    hover: Hsla,
}

/**
 * The header of a fenced code block: a `codehead` strip with the language in mono 11px `fg3`
 * and a Copy button.
 *
 * `TextView` has no header: it places this element in the block's top right corner, 8px in.
 * So the strip is drawn far wider than a block can be, anchored at the block's right edge and
 * clipped by the block, and its content sits at its right end: the language can't be at the
 * left, where DESIGN has it, because nothing here knows how wide the block is (D11).
 */
fn code_block_header(block: &CodeBlock, header: CodeHeader) -> impl IntoElement + use<> {
    let code = block.code();
    div()
        // Back out of `TextView`'s 8px corner inset, to just inside the block's border.
        .mt(px(-7.))
        .mr(px(-7.))
        .w(px(4_000.))
        .h(header.height)
        .flex()
        .items_center()
        .justify_end()
        .gap(px(8.))
        .pr(px(4.))
        .bg(header.fill)
        .border_b_1()
        .border_color(header.rule)
        .text_size(px(11.))
        .line_height(px(14.))
        .text_color(header.label)
        .children(
            block
                .lang()
                .filter(|lang| !lang.is_empty())
                .map(|lang| div().font_family(header.mono).child(lang)),
        )
        .child(
            div()
                .id("copy")
                .px(px(6.))
                .py(px(2.))
                .rounded(px(5.))
                .cursor_pointer()
                .hover(move |this| this.text_color(header.hover))
                .on_click(move |_, _, cx| crate::system::copy_text(code.to_string(), cx))
                .child("Copy"),
        )
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::services::reader::Code;
    use crate::theme::{Appearance, Material};

    /**
     * [`Sanitized::measured`]'s gate, on the times it is handed in place of the clock's: a body
     * the view parsed within its limit is Markdown and any other plain; one a little past it is
     * timed again and the shorter time decides, one far past it is not; the limit grows with the
     * body. How long the view takes on each body the reader makes is measured, in a release
     * build, by `every_piece_the_view_is_handed_is_one_it_parses_quickly`.
     */
    #[test]
    fn a_body_is_markdown_only_when_the_view_parsed_it_within_its_limit() {
        /* What the gate kept, given `times` in turn, and how many it asked for. */
        let gate = |text: &str, times: &[Option<Duration>]| {
            let sanitized = sanitize(&crate::hostile::bidi(text));
            let mut asked = 0;
            let kept = Sanitized::gated(text, |parsed| {
                assert_eq!(parsed, sanitized);
                asked += 1;
                times[asked - 1]
            });
            (kept.map(|it| it.as_str().to_string()), asked)
        };
        let text = "A *short* body.";
        let markdown = Some(sanitize(text));
        let limit = VIEW_PARSE_LIMIT;
        let over = limit + Duration::from_micros(1);
        assert_eq!(gate(text, &[Some(limit / 2)]), (markdown.clone(), 1));
        assert_eq!(gate(text, &[Some(limit)]), (markdown.clone(), 1));
        /* A little past it, as when the thread was made to wait: timed again. */
        assert_eq!(
            gate(text, &[Some(over), Some(limit)]),
            (markdown.clone(), 2)
        );
        assert_eq!(gate(text, &[Some(limit * 3), Some(over)]), (None, 2));
        /* Far past it: plain, without another try. */
        assert_eq!(gate(text, &[Some(limit * 4)]), (None, 1));
        /* A parse that fails is plain. */
        assert_eq!(gate(text, &[None]), (None, 1));
        assert_eq!(gate(text, &[Some(over), None]), (None, 2));
        /* Three units begun: three times the limit. */
        let long = "word ".repeat(2 * VIEW_PARSE_UNIT / 5 + 100);
        let long_limit = view_parse_limit(sanitize(&long).len());
        assert_eq!(long_limit, limit * 3);
        assert!(gate(&long, &[Some(long_limit)]).0.is_some());
        let past = long_limit + Duration::from_micros(1);
        assert_eq!(gate(&long, &[Some(past), Some(past)]).0, None);
    }

    /// A view of one body, as the reader's (`markdown::view`) or as any other `TextView`.
    struct Body {
        state: Entity<TextViewState>,
        ours: bool,
    }

    impl gpui::Render for Body {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            if self.ours {
                view(&self.state, Size::Reading, cx).into_any_element()
            } else {
                /* In the text colour a window's root sets, as gpui-component's does. */
                div()
                    .text_color(gpui_component::ActiveTheme::theme(&**cx).foreground)
                    .child(TextView::new(&self.state))
                    .into_any_element()
            }
        }
    }

    /**
     * A fence the reader's scan missed, drawn by `TextView` (a fence in a list item that a
     * tree-sitter parse takes a minute on), by the reader's view or any other, before and after
     * gpui-component puts its own highlighter back with a theme: never highlighted on the UI
     * thread, only asked for of the reader service.
     */
    #[gpui::test]
    fn a_fence_textview_draws_is_highlighted_by_ours_off_the_ui_thread(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| crate::init(cx).unwrap());
        let asked = cx.update(|cx| crate::services::reader::highlights::asked(cx));
        let fence = |n: usize| {
            format!(
                "- a\n\n    ```swift\n    {}\n\n    {}\n    ```\n",
                "/*".repeat(1_500 + n),
                "/*".repeat(1_500)
            )
        };
        for (n, ours) in [(0, true), (1, false), (2, true), (3, false)] {
            if n == 2 {
                cx.update(|cx| {
                    gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx)
                });
            }
            let body = fence(n);
            let start = Instant::now();
            let (_, window) = cx.add_window_view(|_, cx| Body {
                state: cx.new(|cx| TextViewState::markdown(&body, cx)),
                ours,
            });
            window.run_until_parked();
            window.update(|window, _| window.refresh());
            window.run_until_parked();
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "{n}: {:?}",
                start.elapsed()
            );
            /* The windows before ask again for theirs once the highlighter is renewed. */
            let codes: Vec<Code> = std::iter::from_fn(|| asked.try_recv().ok()).collect();
            let opener = format!("{}\n", "/*".repeat(1_500 + n));
            assert!(
                codes
                    .iter()
                    .any(|code| &*code.language == "swift" && code.text.starts_with(&opener)),
                "{n}: the code was asked for"
            );
        }
    }

    #[test]
    fn the_style_is_drawn_from_the_tokens() {
        let theme = SbTheme::new(Appearance::Dark, Material::Opaque);
        let style = style(&theme);
        let c = &theme.colors;
        assert_eq!(style.foreground(), c.fg);
        assert_eq!(style.link(), c.accent_text);
        assert_eq!(style.border(), c.line);
        assert_eq!(style.code_background(), c.code);
        // DESIGN §1.3: h2 17/24, h3 14/20, both 600.
        assert_eq!(
            style.heading(2),
            StyleRefinement::default()
                .font_family(theme.fonts.ui.clone())
                .text_size(px(17.))
                .line_height(px(24.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(c.fg)
        );
        assert_eq!(style.heading(3).text.font_size, Some(px(14.).into()));
        assert_eq!(style.inline_code_style().background, Some(c.s3.into()));
        assert_eq!(style.table_head().background, Some(c.s1.into()));
    }
}
