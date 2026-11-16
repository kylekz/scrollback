//! Markdown for prompts, replies and the other prose of a transcript (PLAN D11, DESIGN §4.2.2),
//! rendered by gpui-base's `TextView` in the theme's tokens.
//!
//! Every body is untrusted: it is whatever the agent saw or wrote. `TextView` fetches the URL of
//! an image (`![](url)`, a raw `<img>`) and renders raw HTML, so a body reaches it only as
//! [`Sanitized`], which has neither (PLAN D11). The sanitiser is dv's (D15).
//!
//! [`view`] is the one way a view renders a body, so every body gets the same style, the same
//! link and copy handling ([`crate::system`]: links open only for `http` and `https`) and the
//! same highlighter: fenced code is highlighted by [`crate::syntax`] into token kinds, coloured
//! from the theme here.
//!
//! A body's parsed document lives in a [`TextViewState`] its owner keeps ([`state`]). One takes
//! about half a megabyte, so the reader keeps them only for the rows near what is on screen
//! (D11). A body over a few kilobytes is parsed on a background thread.

mod sanitize;

use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Entity, FontFeatures, FontWeight, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement, Overflow, ParentElement as _, Pixels, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Styled as _, div, px, rems,
};
use gpui_base::text::{CodeBlock, TextView, TextViewState, TextViewStyle};

use crate::syntax;
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _, SyntaxColors, TextSpec};

#[cfg(test)]
pub(crate) use sanitize::inert;
pub use sanitize::{options, sanitize};

/// A body that has been through [`sanitize`]: nothing in it fetches anything or renders
/// markup. The only way to make one is to sanitise text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sanitized(SharedString);

impl Sanitized {
    /// `text` sanitised, its bidirectional controls shown as what they are
    /// ([`crate::hostile::bidi`]).
    pub fn new(text: &str) -> Self {
        Self(sanitize(&crate::hostile::bidi(text)).into())
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
        .shared_code_block_highlighter(highlighter(theme.colors.syntax))
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

/// What `TextView` asks for a code block's colours: byte ranges of its code and their styles.
type Highlighter = Arc<dyn Fn(&CodeBlock) -> Vec<(Range<usize>, HighlightStyle)> + Send + Sync>;

/// The highlighter for `colors`, the same `Arc` for as long as the colours are: a code block
/// keeps its highlights only while its highlighter is the same one.
fn highlighter(colors: SyntaxColors) -> Highlighter {
    thread_local! {
        static CURRENT: RefCell<Option<(SyntaxColors, Highlighter)>> =
            const { RefCell::new(None) };
    }
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        match &*current {
            Some((shared_colors, shared)) if *shared_colors == colors => shared.clone(),
            _ => {
                let shared: Highlighter = Arc::new(move |block: &CodeBlock| {
                    let Some(language) = block.lang() else {
                        return Vec::new();
                    };
                    syntax::highlight(&language, &block.code())
                        .into_iter()
                        .filter_map(|(range, kind)| {
                            Some((
                                range,
                                HighlightStyle {
                                    color: Some(kind.color(&colors)?),
                                    ..Default::default()
                                },
                            ))
                        })
                        .collect()
                });
                *current = Some((colors, shared.clone()));
                shared
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::theme::{Appearance, Material};

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
