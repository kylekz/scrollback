//! Syntax highlighting (PLAN §9): tree-sitter, through gpui-component's highlighter, producing
//! **token kinds** rather than colours, as dv's `syntax` does (lifted from it, PLAN D15).
//!
//! A kind resolves to a colour only when it paints ([`TokenKind::color`], from the theme's
//! [`SyntaxColors`]), so a theme change repaints without highlighting again. The highlighter's
//! capture names (`keyword.return`, `function.method`, `string.escape`, …) map to kinds through
//! one table ([`kind_for_capture`]); the highlighter reports styles, so the resolver hands it a
//! style that encodes the kind and the result is decoded back, losslessly.
//!
//! [`highlight`] blocks. `TextView` asks for a fenced block's highlights while it draws the
//! block, on the UI thread, so a block over [`MAX_HIGHLIGHT_BYTES`] isn't highlighted at all:
//! that bounds what one frame can spend on it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;

use gpui::{HighlightStyle, Hsla};
use gpui_component::Rope;
use gpui_component::highlighter::{LanguageRegistry, SyntaxHighlighter};
use gpui_component::input::HighlightStyleResolver;

use crate::theme::SyntaxColors;

/// Code larger than this, in bytes, isn't highlighted: a few milliseconds of tree-sitter.
pub const MAX_HIGHLIGHT_BYTES: usize = 64 * 1024;

/// What a piece of code is, as far as colouring goes (DESIGN §1.1, §1.2 "Syntax").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Keyword,
    Function,
    String,
    Number,
    Type,
    Property,
    Comment,
    Plain,
}

impl TokenKind {
    /// Every kind, in the order [`TokenKind::index`] numbers them.
    pub const ALL: [TokenKind; 8] = [
        Self::Keyword,
        Self::Function,
        Self::String,
        Self::Number,
        Self::Type,
        Self::Property,
        Self::Comment,
        Self::Plain,
    ];

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|kind| *kind == self)
            .unwrap_or(Self::ALL.len() - 1)
    }

    /// The colour this kind paints in; `None` for plain code, which keeps the text colour.
    pub fn color(self, syntax: &SyntaxColors) -> Option<Hsla> {
        match self {
            Self::Keyword => Some(syntax.keyword),
            Self::Function => Some(syntax.function),
            Self::String => Some(syntax.string),
            Self::Number => Some(syntax.number),
            Self::Type => Some(syntax.ty),
            Self::Property => Some(syntax.property),
            Self::Comment => Some(syntax.comment),
            Self::Plain => None,
        }
    }
}

/// The kind a tree-sitter capture name stands for; `None` for a name that says nothing about
/// colour (a query's private `_name` captures, unknown names), which leaves the enclosing
/// capture's kind in place.
///
/// A capture that names plain code explicitly (`variable`, `punctuation`, `embedded`, …) is
/// [`TokenKind::Plain`], so a variable interpolated into a string paints plain, not as the
/// string around it. By the name's first segment unless the full name is listed:
///
/// | Kind | Names |
/// |---|---|
/// | Keyword | `keyword*`, `conditional`, `repeat`, `include`, `import`, `exception`, `storageclass`, `preproc`, `variable.builtin`, `variable.special`, `tag*`, `title`, `boolean`, `label` (DESIGN §4.2.4: literals are `k`) |
/// | Function | `function*`, `method*`, `link_text` |
/// | String | `string*`, `character*`, `escape`, `text.literal`, `text.code.span`, `text.uri`, `link_uri` |
/// | Number | `number*`, `float`, `constant*` |
/// | Type | `type*`, `constructor` |
/// | Property | `property*`, `field`, `attribute`, `tag.attribute`, `variable.member` |
/// | Comment | `comment*` |
/// | Plain | `variable*` (others), `parameter`, `punctuation*`, `operator`, `delimiter`, `embedded`, `module*`, `namespace`, `emphasis*`, `text*` (others), `none`, `spell`, `nospell` |
pub fn kind_for_capture(name: &str) -> Option<TokenKind> {
    use TokenKind::*;
    if name.is_empty() || name.starts_with('_') {
        return None;
    }
    let exact = match name {
        "constructor" => Some(Type),
        "boolean" | "label" | "variable.builtin" | "variable.special" | "conditional"
        | "repeat" | "include" | "import" | "exception" | "storageclass" | "preproc" | "title" => {
            Some(Keyword)
        }
        "attribute" | "field" | "tag.attribute" | "variable.member" => Some(Property),
        "operator" | "delimiter" | "tag.delimiter" => Some(Plain),
        "escape" | "text.literal" | "text.code.span" | "text.uri" | "link_uri" => Some(String),
        "float" => Some(Number),
        "link_text" => Some(Function),
        _ => None,
    };
    if exact.is_some() {
        return exact;
    }
    match name.split('.').next().unwrap_or(name) {
        "keyword" | "tag" => Some(Keyword),
        "function" | "method" => Some(Function),
        "string" | "character" => Some(String),
        // Grammars capture number literals and other constants alike (`constant.builtin`).
        "number" | "constant" => Some(Number),
        "type" => Some(Type),
        "property" => Some(Property),
        "comment" => Some(Comment),
        "variable" | "parameter" | "punctuation" | "embedded" | "module" | "namespace"
        | "emphasis" | "text" | "none" | "spell" | "nospell" => Some(Plain),
        _ => None,
    }
}

/// The non-plain tokens of `code` in `language`, a name or alias gpui-component's registry
/// knows (`rust`, `ts`, `sh`, …), as byte ranges of `code` in order. Empty for a language
/// without a grammar and for code over [`MAX_HIGHLIGHT_BYTES`].
///
/// Each thread keeps one highlighter per language, so a grammar's queries are compiled once.
pub fn highlight(language: &str, code: &str) -> Vec<(Range<usize>, TokenKind)> {
    thread_local! {
        static HIGHLIGHTERS: RefCell<HashMap<String, SyntaxHighlighter>> =
            RefCell::new(HashMap::new());
    }
    if code.len() > MAX_HIGHLIGHT_BYTES
        || LanguageRegistry::singleton().language(language).is_none()
    {
        return Vec::new();
    }
    HIGHLIGHTERS.with(|highlighters| {
        let mut highlighters = highlighters.borrow_mut();
        let highlighter = highlighters
            .entry(language.to_string())
            .or_insert_with(|| SyntaxHighlighter::new(language));
        // No edit: the text before is another block's, so nothing of its parse is kept.
        highlighter.update(None, &Rope::from_str(code), None);
        highlighter
            .styles(&(0..code.len()), &KindResolver)
            .into_iter()
            .filter_map(|(range, style)| {
                let kind = decode(&style);
                (kind != TokenKind::Plain).then_some((range, kind))
            })
            .collect()
    })
}

/// Hands the highlighter a style per capture that carries the capture's kind in its colour's
/// hue (the kind's index), which [`decode`] reads back. The highlighter only copies colours
/// from one capture to the run it covers, never blends them, so nothing is lost.
struct KindResolver;

impl HighlightStyleResolver for KindResolver {
    fn style(&self, name: &str) -> Option<HighlightStyle> {
        kind_for_capture(name).map(encode)
    }
}

fn encode(kind: TokenKind) -> HighlightStyle {
    HighlightStyle {
        color: Some(Hsla {
            h: kind.index() as f32,
            s: 0.,
            l: 0.,
            a: 1.,
        }),
        ..Default::default()
    }
}

fn decode(style: &HighlightStyle) -> TokenKind {
    style
        .color
        .and_then(|color| {
            let index = color.h as usize;
            (color.h == index as f32)
                .then(|| TokenKind::ALL.get(index).copied())
                .flatten()
        })
        .unwrap_or(TokenKind::Plain)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn kinds(language: &str, code: &str) -> Vec<(&'static str, TokenKind)> {
        // Leaked so the pairs can borrow the code for the assertion's lifetime.
        let code: &'static str = code.to_string().leak();
        highlight(language, code)
            .into_iter()
            .map(|(range, kind)| (&code[range], kind))
            .collect()
    }

    #[test]
    fn rust_is_highlighted_into_kinds() {
        let tokens = kinds("rust", "fn add(a: u32) -> u32 { \"s\"; 12 } // c");
        for expected in [
            ("fn", TokenKind::Keyword),
            ("add", TokenKind::Function),
            ("u32", TokenKind::Type),
            ("\"s\"", TokenKind::String),
            ("12", TokenKind::Number),
            ("// c", TokenKind::Comment),
        ] {
            assert!(tokens.contains(&expected), "{expected:?} in {tokens:?}");
        }
    }

    #[test]
    fn an_alias_names_its_language() {
        assert!(!highlight("ts", "const a: number = 1;").is_empty());
        assert!(!highlight("sh", "echo \"$HOME\" # c").is_empty());
    }

    #[test]
    fn an_unknown_language_and_no_code_are_plain() {
        assert_eq!(highlight("brainfudge", "++++[>++<-]"), Vec::new());
        assert_eq!(highlight("rust", ""), Vec::new());
    }

    #[test]
    fn every_kind_survives_the_round_trip_through_a_style() {
        for kind in TokenKind::ALL {
            assert_eq!(decode(&encode(kind)), kind);
        }
        assert_eq!(decode(&HighlightStyle::default()), TokenKind::Plain);
    }
}
