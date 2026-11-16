//! The sanitiser every body goes through before `TextView` sees it (PLAN D11), first lifted
//! from dv's (D15).
//!
//! It parses as the view parses ([`options`]: GFM with math), and treats an inline math span as
//! the view does when nothing claims it: as prose. Its edits are made at the spans the parser
//! reports, so code is left alone. Then the result is parsed again the view's way, and if any
//! image or raw HTML is left in it, the whole body is shown literally instead.

use markdown::ParseOptions;
use markdown::mdast::Node;

/// How the view parses Markdown: GFM, and math in text and in its own blocks.
pub fn options() -> ParseOptions {
    let mut options = ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options
}

/// The same without math: how the view reads an inline math span nothing claims.
fn prose_options() -> ParseOptions {
    let mut options = options();
    options.constructs.math_text = false;
    options.constructs.math_flow = false;
    options
}

/**
 * `source` with nothing in it that makes `TextView` fetch anything or render markup:
 *
 * - **Images** (`![alt](url)` and `![alt][ref]`) become their alt text in brackets, `[image:
 *   alt]`; one at a web address becomes a link to it with that text, so the owner can open it
 *   in their browser. Nothing is fetched to show it. A `!` before the image is escaped, so the
 *   brackets can't join it into an image again.
 * - **Raw HTML** is shown as the text it is: a block of HTML as a fenced code block, a tag in a
 *   line as its text ([`untagged`]). Either reads literally, and no link runs into it.
 * - **A hard break of trailing spaces** becomes the backslash break the view keeps.
 *
 * If parsing fails, a span doesn't map onto the source, or the result still parses to an image
 * or HTML, the whole body is shown as a code block: uglier, still inert, and every character
 * there.
 */
pub fn sanitize(source: &str) -> String {
    let edited = match edit(source, &options()) {
        Some(edited) => edited,
        None => return literal(source),
    };
    if inert(&edited) {
        edited
    } else {
        literal(source)
    }
}

/// The edits of [`sanitize`] made to `source` parsed with `options`; `None` when they can't be.
fn edit(source: &str, options: &ParseOptions) -> Option<String> {
    if !source.contains(['<', '!', '$']) && !source.contains("  \n") {
        return Some(source.to_string());
    }
    let root = markdown::to_mdast(source, options).ok()?;
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    /* Each node with whether its parent is the document itself, where HTML is a block. */
    let mut stack: Vec<(&Node, bool)> = root
        .children()
        .into_iter()
        .flatten()
        .map(|node| (node, true))
        .collect();
    while let Some((node, top)) = stack.pop() {
        let span = || {
            let position = node.position()?;
            let (start, end) = (position.start.offset, position.end.offset);
            source.get(start..end).map(|text| (start, end, text))
        };
        match node {
            Node::Image(image) => {
                let (start, end, _) = span()?;
                let text = placeholder(&image.alt);
                let link = match web_address(&image.url) {
                    Some(url) => format!("[{text}](<{url}>)"),
                    None => format!("[{text}]"),
                };
                edits.push(after_bang(source, start, end, link));
            }
            Node::ImageReference(image) => {
                let (start, end, _) = span()?;
                let text = format!("[{}]", placeholder(&image.alt));
                edits.push(after_bang(source, start, end, text));
            }
            Node::Html(_) => {
                let (start, end, text) = span()?;
                edits.push((start, end, if top { fence(text) } else { untagged(text) }));
            }
            Node::InlineMath(_) => {
                let (start, end, text) = span()?;
                let prose = edit(text, &prose_options())?;
                if prose != text {
                    edits.push((start, end, prose));
                }
            }
            Node::Break(_) => {
                let (start, end, text) = span()?;
                if text.starts_with(' ') {
                    edits.push((start, end, "\\\n".to_string()));
                }
            }
            other => stack.extend(other.children().into_iter().flatten().map(|it| (it, false))),
        }
    }
    edits.sort_by_key(|&(start, _, _)| start);
    let mut out = String::with_capacity(source.len() + 64);
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
    Some(out)
}

/// The edit that puts `replacement` at `start..end`, taking in a `!` right before it and
/// escaping it, so that the brackets that begin `replacement` can't make an image with it.
fn after_bang(
    source: &str,
    start: usize,
    end: usize,
    replacement: String,
) -> (usize, usize, String) {
    if source[..start].ends_with('!') {
        (start - 1, end, format!("\\!{replacement}"))
    } else {
        (start, end, replacement)
    }
}

/// Whether `text`, parsed as the view parses it, holds no image and no HTML: an inline math
/// span is looked into as the view reads one nothing claims.
pub(crate) fn inert(text: &str) -> bool {
    fn walk(node: &Node, source: &str) -> bool {
        match node {
            Node::Image(_) | Node::ImageReference(_) | Node::Html(_) => false,
            Node::InlineMath(_) => {
                let Some(span) = node
                    .position()
                    .and_then(|at| source.get(at.start.offset..at.end.offset))
                else {
                    return false;
                };
                match markdown::to_mdast(span, &prose_options()) {
                    Ok(prose) => walk(&prose, span),
                    Err(_) => false,
                }
            }
            other => other
                .children()
                .into_iter()
                .flatten()
                .all(|child| walk(child, source)),
        }
    }
    markdown::to_mdast(text, &options()).is_ok_and(|root| walk(&root, text))
}

/// The run of backticks longer than any in `text`, and at least `least` long.
fn ticks(text: &str, least: usize) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    "`".repeat((longest + 1).max(least))
}

/**
 * A tag in a line, shown as the text it is: a zero-width space after each `<` keeps it from
 * starting a tag, and reads as nothing. The `<` itself stays, so a web address written just
 * before it ends there, as it does before a tag (an escape or an entity would let the address
 * run on into the tag's text). An address inside the tag (`<img src="https://…">`) gets a
 * zero-width space too, so it doesn't become a link whose text runs on into the tag's.
 */
fn untagged(text: &str) -> String {
    text.replace('<', "<\u{200B}")
        .replace("://", ":\u{200B}//")
        .replace("www.", "www\u{200B}.")
}

/// `text` as a fenced code block, which shows it literally.
fn fence(text: &str) -> String {
    let fence = ticks(text, 3);
    format!("{fence}\n{}\n{fence}", text.trim_end_matches(['\n', '\r']))
}

/// A whole body shown literally, as a code block: nothing in it is parsed.
pub fn literal(text: &str) -> String {
    fence(text)
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
fn placeholder(alt: &str) -> String {
    let clean: String = alt
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '[' | ']' | '(' | ')' | '<' | '>' | '`' | '!' | '&' | '\\' | '$' | '*' | '_'
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

#[cfg(test)]
mod tests {
    use markdown::mdast::Node;
    use pretty_assertions::assert_eq;

    use super::*;

    /// Whether the view's own reading of `text` holds an image or HTML anywhere.
    fn assert_inert(sanitized: &str) {
        assert!(
            inert(sanitized),
            "the view would read markup in {sanitized:?}"
        );
    }

    /// The web addresses of the links in `markdown`, as the view parses it.
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
        walk(&markdown::to_mdast(markdown, &options()).unwrap(), &mut out);
        out
    }

    /// Bodies built to get markup past a sanitiser: every one must come out inert.
    const HOSTILE: &[&str] = &[
        "!![x](https://example.com/p.png)",
        "!!![x](https://example.com/p.png)",
        "\\![x](https://example.com/p.png)",
        "!![x][r]\n\n[r]: https://example.com/p.png",
        "!![x]\n\n[x]: https://example.com/p.png",
        "$x`$ <img src=\"https://example.com/p.png\"> `y",
        "$a <a href=\"file:///etc/passwd\">b</a> c$",
        "$![x](https://example.com/p.png)$",
        "$$\n<img src=x>\n$$",
        "a <img src=\"https://example.com/p.png\"> b",
        "a <IMG SRC=x> b <a href=\"javascript:alert(1)\">c</a>",
        "<div>\n<img src=x>\n![y](https://example.com/q.png)\n</div>\n",
        "<script>\n\nalert(1)\n\n</script>",
        "> <img src=x>\n> continued <b>bold</b>",
        "- item <img src=x>\n  - nested <img src=y>",
        "| a | <img src=x> |\n|---|---|\n| <b>c</b> | d |",
        "text `` <img> `` and <img src=x> and ```<img>``` `",
        "<!-- a comment ![x](https://example.com/p.png) -->",
        "<![CDATA[ <img src=x> ]]>",
        "line <img src=x\n# heading\nrest>",
        "[![badge](https://example.com/b.svg)](https://example.org)",
        "fetch('http://example.com/script');</script> after",
        "]()[ [[[]]] ()()( ![]( [text](",
        "*<img src=x>* _<b>x</b>_ ~~<i>y</i>~~",
        "![a<b>`c`[d]\\](./p.png)",
        "&lt;img src=x&gt; &#60;img&#62;",
    ];

    #[test]
    fn no_hostile_body_comes_out_with_an_image_or_html_the_view_would_read() {
        for source in HOSTILE {
            assert_inert(&sanitize(source));
        }
        for source in [
            "```rust\nfn main() {\n// never closed".to_string(),
            "*".repeat(500) + "text" + &"*".repeat(500),
            "a".repeat(200_000),
            "hello \0 world \u{7} \u{1b}[31m fake ansi".to_string(),
            String::new(),
        ] {
            assert_inert(&sanitize(&source));
        }
    }

    #[test]
    fn a_bang_before_an_image_cannot_make_its_replacement_an_image() {
        let out = sanitize("!![x](https://example.com/p.png)");
        assert_eq!(out, "\\![image: x](<https://example.com/p.png>)");
        assert_eq!(links(&out), ["https://example.com/p.png"]);
        assert_inert(&out);
    }

    #[test]
    fn html_inside_math_the_view_reads_as_prose_is_shown_as_its_text() {
        let out = sanitize("$x`$ <img src=\"https://example.com/p.png\"> `y");
        assert_inert(&out);
        assert!(
            out.contains("img src"),
            "the tag is still there to read: {out}"
        );
        // The address inside the tag is its text, not a link that runs on into it.
        assert_eq!(links(&out), Vec::<String>::new());
    }

    #[test]
    fn an_escaped_tag_reads_literally_and_no_link_runs_into_it() {
        let out = sanitize("fetch('http://example.com/script');</script> after");
        assert_eq!(
            out,
            "fetch('http://example.com/script');<\u{200B}/script> after"
        );
        assert!(
            links(&out).iter().all(|url| !url.contains("script>")),
            "{:?}",
            links(&out)
        );
        assert_inert(&out);
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
    fn a_block_of_html_is_a_code_block_and_a_tag_in_a_line_its_text() {
        assert_eq!(
            sanitize("<div>\n<b>x</b>\n</div>\n\ntext"),
            "```\n<div>\n<b>x</b>\n</div>\n```\n\ntext"
        );
        assert_eq!(sanitize("a <b>x</b> c"), "a <\u{200B}b>x<\u{200B}/b> c");
    }

    #[test]
    fn a_hard_break_of_trailing_spaces_is_kept_as_a_backslash_break() {
        assert_eq!(sanitize("one  \ntwo"), "one\\\ntwo");
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
    fn what_cannot_be_made_inert_in_place_is_shown_whole_as_code() {
        let out = literal("a ``` b\n<img>");
        assert!(
            out.starts_with("````\n") && out.ends_with("\n````"),
            "{out}"
        );
        assert_inert(&out);
    }
}
