//! The sanitiser every body goes through before `TextView` sees it (PLAN D11), lifted from dv's
//! (D15).

use markdown::mdast::Node;

/**
 * Neutralises the constructs that make `TextView` fetch anything or render markup:
 *
 * - **Images** (`![alt](url)` and reference-style `![alt][ref]`) become an inert literal
 *   `[image: alt]` (or `[image]`). An image at a web address becomes a link to it with that
 *   text, so the owner can open it in their browser; nothing is fetched to show it.
 * - **Raw HTML** (inline tags in any casing, and whole HTML blocks) is entity-escaped and
 *   shows as literal text.
 *
 * The edits are made at the spans `markdown::to_mdast` reports, the parser `TextView` uses
 * itself, so code is left alone: text in fenced and indented blocks and inline code spans
 * lives under `Code` and `InlineCode` nodes, never `Image` or `Html` ones. Everything else
 * (links, emphasis, tables, lists) passes through. When the parse fails or a span doesn't map
 * onto the source, the whole body is escaped coarsely instead: uglier, still inert.
 */
pub fn sanitize(source: &str) -> String {
    let Ok(ast) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) else {
        return coarse_escape(source);
    };
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    if !collect_unsafe_spans(&ast, source, &mut edits) {
        return coarse_escape(source);
    }
    if edits.is_empty() {
        return source.to_string();
    }
    edits.sort_by_key(|&(start, _, _)| start);
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end, replacement) in edits {
        /* Spans never nest (a replaced node isn't descended into); an overlap is skipped
        rather than emitting text twice. */
        if start < cursor {
            continue;
        }
        out.push_str(&source[cursor..start]);
        out.push_str(&replacement);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    out
}

/// Collects a `(start, end, replacement)` byte-span edit for every image and raw-HTML node
/// under `node`. `false` when a node's span can't be mapped onto `source` (no position, out of
/// range, off a char boundary), which makes the caller escape the whole body.
fn collect_unsafe_spans(
    node: &Node,
    source: &str,
    edits: &mut Vec<(usize, usize, String)>,
) -> bool {
    let span_of = |node: &Node| -> Option<(usize, usize)> {
        let position = node.position()?;
        let (start, end) = (position.start.offset, position.end.offset);
        source.get(start..end).map(|_| (start, end))
    };
    let mut edit = |node: &Node, replacement: &dyn Fn(&str) -> String| match span_of(node) {
        Some((start, end)) => {
            edits.push((start, end, replacement(&source[start..end])));
            true
        }
        None => false,
    };
    match node {
        Node::Image(image) => edit(node, &|_| {
            let text = image_placeholder(&image.alt);
            match web_address(&image.url) {
                Some(url) => format!("[{text}](<{url}>)"),
                None => format!("[{text}]"),
            }
        }),
        Node::ImageReference(image) => {
            edit(node, &|_| format!("[{}]", image_placeholder(&image.alt)))
        }
        Node::Html(_) => edit(node, &coarse_escape),
        other => match other.children() {
            Some(children) => children
                .iter()
                .all(|child| collect_unsafe_spans(child, source, edits)),
            None => true,
        },
    }
}

/// `url` when it is a web address that can sit in a link's `<…>` as it is.
fn web_address(url: &str) -> Option<&str> {
    let web = url.starts_with("https://") || url.starts_with("http://");
    let plain = !url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '\\'));
    (web && plain).then_some(url)
}

/// The text an image becomes, without its brackets: its alt text, stripped of every character
/// that could reopen Markdown or HTML syntax once spliced back into the source.
fn image_placeholder(alt: &str) -> String {
    let clean: String = alt
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '[' | ']' | '(' | ')' | '<' | '>' | '`' | '!' | '&' | '\\'
            )
        })
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        "image".to_string()
    } else {
        format!("image: {clean}")
    }
}

/// Escapes HTML (`&`, `<`) and breaks image syntax (`![` to `!\[`), so the result can only
/// render as literal text: for a raw-HTML node's span (an image inside an HTML block must not
/// come back to life once the tags are escaped) and for a whole body when span edits aren't
/// possible.
fn coarse_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace("![", "!\\[")
}

#[cfg(test)]
mod tests {
    use markdown::mdast::Node;
    use markdown::{ParseOptions, to_mdast};

    use super::{coarse_escape, sanitize};

    /// After sanitising, a parse of the result holds no `Image`, `ImageReference` or `Html`
    /// node anywhere: exactly the nodes `TextView` turns into fetches or markup.
    fn assert_inert(sanitized: &str) {
        fn walk(node: &Node) {
            match node {
                Node::Image(_) | Node::ImageReference(_) | Node::Html(_) => {
                    panic!("sanitised output still has a fetchable node: {node:?}")
                }
                other => other.children().into_iter().flatten().for_each(walk),
            }
        }
        walk(&to_mdast(sanitized, &ParseOptions::gfm()).expect("sanitised output parses"));
    }

    /// The web addresses of the links in `markdown`.
    fn links(markdown: &str) -> Vec<String> {
        fn walk(node: &Node, out: &mut Vec<String>) {
            if let Node::Link(link) = node {
                out.push(link.url.clone());
            }
            for child in node.children().into_iter().flatten() {
                walk(child, out);
            }
        }
        let mut out = Vec::new();
        walk(&to_mdast(markdown, &ParseOptions::gfm()).unwrap(), &mut out);
        out
    }

    #[test]
    fn hostile_input_never_panics() {
        for source in [
            "```rust\nfn main() {\n// never closed".to_string(),
            "*".repeat(500) + "text" + &"*".repeat(500),
            "a".repeat(200_000),
            "]()[ [[[]]] ()()( ![]( [text](".to_string(),
            "hello \0 world \u{7} \u{1b}[31m fake ansi".to_string(),
            "héllo 🎉 \u{200b} zero-width \u{feff} bom-in-middle".to_string(),
            String::new(),
            "   \n\n\t  ".to_string(),
        ] {
            assert_inert(&sanitize(&source));
        }
    }

    #[test]
    fn plain_markdown_passes_through_unchanged() {
        let source =
            "## Title\n\nsome *emphasis*, a [link](https://example.com), and\n\n- a list\n";
        assert_eq!(sanitize(source), source);
    }

    #[test]
    fn an_image_at_a_web_address_becomes_a_link_to_it_and_any_other_an_inert_placeholder() {
        let out = sanitize("before ![tracking pixel](https://example.com/p.png) after");
        assert_eq!(
            out,
            "before [image: tracking pixel](<https://example.com/p.png>) after"
        );
        assert_eq!(links(&out), ["https://example.com/p.png"]);
        assert_inert(&out);

        for source in [
            "x ![](file:///etc/passwd) y",
            "x ![](./local.png) y",
            "x ![](data:image/png;base64,AAAA) y",
            "x ![](<https://example.com/a b.png>) y",
        ] {
            let out = sanitize(source);
            assert!(out.contains("[image]") && links(&out).is_empty(), "{out}");
            assert_inert(&out);
        }
    }

    #[test]
    fn reference_images_of_every_form_are_neutralised() {
        let out = sanitize("see ![alt text][pix]\n\n[pix]: https://example.com/p.png\n");
        assert!(out.contains("[image: alt text]"), "{out}");
        assert_inert(&out);
        assert_inert(&sanitize("![pix][]\n\n[pix]: https://example.com/a.png\n"));
        assert_inert(&sanitize("![pix]\n\n[pix]: https://example.com/a.png\n"));
    }

    #[test]
    fn inline_html_is_escaped_in_any_casing() {
        let out = sanitize("a <img src=\"https://example.com/p.png\"> b");
        assert!(out.contains("&lt;img"), "{out}");
        assert_inert(&out);
        let upper = sanitize("a <IMG SRC=\"https://example.com/p.png\"> b");
        assert!(upper.contains("&lt;IMG"), "{upper}");
        assert_inert(&upper);
    }

    #[test]
    fn an_html_block_is_escaped_with_the_image_inside_it() {
        let source = "<div>\n<img src=\"https://example.com/p.png\">\n![x](https://example.com/q.png)\n</div>\n";
        assert_inert(&sanitize(source));
    }

    #[test]
    fn images_nested_in_lists_and_links_are_neutralised() {
        let out = sanitize("- first\n- has ![pix](./p.png) inline\n- last");
        assert!(out.contains("- has [image: pix] inline"), "{out}");
        assert_inert(&out);
        assert_inert(&sanitize(
            "[![badge](https://example.com/b.svg)](https://example.org)",
        ));
    }

    #[test]
    fn code_stays_verbatim() {
        for source in [
            "intro\n\n```html\n![alt](https://example.com/x.png)\n<img src=\"y.png\">\n```\n\noutro",
            "use `![alt](url)` and `<img src=x>` literally",
            "para\n\n    ![alt](https://example.com/x.png)\n",
        ] {
            assert_eq!(sanitize(source), source);
        }
    }

    #[test]
    fn alt_text_cannot_reopen_syntax_through_the_placeholder() {
        let out = sanitize("![a<b>`c`[d]\\](./p.png)");
        assert_inert(&out);
        assert!(links(&out).is_empty(), "{out}");
    }

    #[test]
    fn the_coarse_fallback_is_inert_too() {
        assert_inert(&coarse_escape(
            "<img src=\"https://example.com/x\"> ![a](https://example.com/y)",
        ));
    }
}
