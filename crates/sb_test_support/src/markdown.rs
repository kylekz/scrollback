//! Generated Markdown (docs/PLAN.md D11, §12): a corpus with one section per construct an
//! assistant reply can hold, and replies of a chosen size for measuring. Everything here is
//! made up; nothing comes from a real transcript.

use std::fmt::Write as _;

/// One part of the corpus: a name for screenshots and reports, and its Markdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: &'static str,
    pub markdown: String,
}

/// The corpus, in the order a review reads it.
pub fn corpus() -> Vec<Section> {
    let section = |name, markdown: &str| Section {
        name,
        markdown: markdown.to_string(),
    };
    vec![
        section("headings", HEADINGS),
        section("inline", INLINE),
        section("lists", LISTS),
        section("quotes", QUOTES),
        section("tables", TABLES),
        Section {
            name: "code",
            markdown: code_languages(),
        },
        Section {
            name: "code-long",
            markdown: long_code_block(2_000),
        },
        section("blocks", BLOCKS),
        section("unicode", UNICODE),
        Section {
            name: "nested",
            markdown: deeply_nested(),
        },
        section("malformed", MALFORMED),
    ]
}

const HEADINGS: &str = "\
# Heading one

A paragraph under the first heading, long enough to wrap at the reading column so the line \
height can be measured against the design: fifteen pixels on a twenty-four pixel line.

## Heading two

Text under the second heading.

### Heading three

Text under the third heading.

#### Heading four

Text under the fourth heading.

##### Heading five

Text under the fifth heading.

###### Heading six

Text under the sixth heading.

Setext heading one
==================

Setext heading two
------------------

## A heading with `inline code`, *emphasis* and a [link](https://example.com/heading)
";

const INLINE: &str = "\
Plain text with **bold**, *italic*, ***bold italic***, ~~strikethrough~~ and `inline code`. \
A [link with a title](https://example.com/docs \"Example docs\"), a [reference link][ref], an \
autolink <https://example.com/auto>, a bare URL https://example.com/bare?query=1 and a mail \
address <user@example.com>.

Inline code next to punctuation: (`Vec<T>`), `a | b`, ``a `tick` inside``, and a long one \
`crates/sb_core/src/provider/some_provider/a_rather_long_module_name/transcript_reader.rs` that \
has to wrap or overflow.

Mixed: **bold with `code` and a [link](https://example.com/in-bold)**, *italic with ~~strike~~*, \
and an escaped \\*asterisk\\* and \\`backtick\\`.

A hard break with two spaces\x20\x20
continues here, and one with a backslash\\
continues here. A soft break
stays on the same paragraph.

[ref]: https://example.com/reference
";

const LISTS: &str = "\
Unordered:

- First item
- Second item with **bold** and `code`
  - Nested item
    - Third level
      - Fourth level, with a long sentence that wraps to a second line so the hanging indent \
of a nested item can be checked against its marker
  - Back at two
- Third item

Ordered:

1. One
2. Two
   1. Two point one
   2. Two point two
      - Mixed bullet inside ordered
3. Three

Ordered from another number:

9. Nine
10. Ten
11. Eleven

Task list:

- [x] Done item
- [ ] Open item with `code`
  - [x] Nested done
  - [ ] Nested open
- [ ] Another open item

A loose list with paragraphs and code:

1. First step.

   A second paragraph of the first step.

   ```sh
   cargo build --release
   ```

2. Second step.

   | Key | Value |
   |---|---|
   | a | 1 |
";

const QUOTES: &str = "\
> A block quote of one paragraph.

> A quote with **bold**, `code` and a [link](https://example.com/quote).
>
> A second paragraph in the same quote.

> Outer quote.
>
> > Nested quote.
> >
> > > Third level.

> A quote holding a list:
>
> - One
> - Two
>   1. Two point one
>
> And code:
>
> ```rust
> fn quoted() -> u32 {
>     42
> }
> ```

> [!NOTE]
> A GitHub alert, which is a quote with a marker.
";

const TABLES: &str = "\
A small table:

| Name | Kind | Count |
|---|---|---|
| alpha | file | 12 |
| beta | folder | 3 |

Alignment and inline formatting:

| Left | Centre | Right | Formatting |
|:---|:---:|---:|---|
| a | b | 1 | **bold** and *italic* |
| longer cell | mid | 1,024 | `code` and a [link](https://example.com/cell) |
| x | y | 16,777,216 | ~~struck~~ and a pipe \\| escaped |

A wide table:

| Column one | Column two | Column three | Column four | Column five | Column six | Column seven | Column eight | Column nine | Column ten |
|---|---|---|---|---|---|---|---|---|---|
| a long cell that would like to stay on one line | value | value | value | value | value | value | value | value | the last cell |
| short | value | value | value | value | value | value | value | value | end |

A table with long text in cells:

| Option | What it does |
|---|---|
| `--data-dir <PATH>` | Where the app keeps its files. A long description that has to wrap inside the cell, because the column cannot grow past the reading column. |
| `--theme <MODE>` | `system`, `light` or `dark`. |

A table without a body:

| Only | Header |
|---|---|
";

const BLOCKS: &str = "\
Text above a rule.

---

Text between rules.

***

Raw HTML inline: <kbd>Ctrl</kbd> + <kbd>F</kbd>, <b>bold tag</b>, <i>italic tag</i>, \
<code>code tag</code>, <mark>marked</mark>, <sub>sub</sub> and <sup>sup</sup>, and a line<br>break.

<div align=\"center\">A centred div block.</div>

<details>
<summary>A details summary</summary>

Hidden content with **bold** text and a list:

- One
- Two

</details>

<!-- an HTML comment -->

An image from a data URL: ![a red dot](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAUAAAAFCAYAAACNbyblAAAAHElEQVQI12P4//8/w38GIAXDIBKE0DHxgljNBAAO9TXL0Y4OHwAAAABJRU5ErkJggg==)

An image that cannot load: ![missing diagram](./missing-diagram.png)

A footnote reference[^one] and another[^long].

[^one]: The first footnote.
[^long]: A longer footnote with `code` in it.

Math-looking text: the cost is $5 and $10, inline $x^2 + y^2 = z^2$, and a block:

$$
\\sum_{i=1}^{n} i = \\frac{n(n+1)}{2}
$$

An indented code block:

    indented code
    second line

A definition-like line
: with a colon

Line one\\
Line two after a hard break\x20\x20
Line three after another.
";

const UNICODE: &str = "\
Emoji: 🎉 ✅ ❌ 🚀 ⚠️ 👍🏽 👨‍👩‍👧‍👦 🇯🇵 and in a list:

- ✅ Passed
- ❌ Failed
- ⚠️ Warning

CJK: 日本語のテキスト、中文文本，한국어 텍스트。 Mixed with Latin: the 設定 file and the `設定.toml` path.

これは折り返しを確認するための長い日本語の文章です。句読点のない長い文字列でも正しく折り返されるかどうかを確認します。これは折り返しを確認するための長い日本語の文章です。

Right-to-left: مرحبا بالعالم and שלום עולם.

Combining and wide: é (e + ◌́), Z̷a̷l̷g̷o̷, ｆｕｌｌｗｉｄｔｈ, and box drawing:

```
┌──────────┬────────┐
│ name     │ count  │
├──────────┼────────┤
│ 日本語   │ 12     │
└──────────┴────────┘
```

Arrows and symbols: → ← ↔ ⇒ ≤ ≥ ≠ ± × ÷ … — – “quotes” ‘single’.
";

const MALFORMED: &str = "\
Unbalanced **bold that never closes and *italic that never closes.

A stray ` backtick and a ~~strike that never closes.

A [link with no target] and a [broken link](https://example.com/unclosed and an ![image with \
no target].

| A table | with a ragged row |
|---|---|
| one |
| one | two | three |

- A list
    that changes indent
  - oddly
1. and switches kind

> A quote
that continues lazily

<div>An HTML block that never closes

#Not a heading, no space

####### Seven hashes

A fence that never closes:

```rust
fn unclosed() {
    let x = 1;
";

/// A fenced block per language a reply commonly holds, one with no language, one with a
/// language no grammar knows, and one with very long lines.
fn code_languages() -> String {
    let samples: [(&str, &str); 14] = [
        (
            "rust",
            "/// Adds two numbers.\npub fn add(a: u32, b: u32) -> u32 {\n    let total = a + b; // a comment\n    println!(\"{total}\");\n    total\n}",
        ),
        (
            "python",
            "def add(a: int, b: int) -> int:\n    \"\"\"Adds two numbers.\"\"\"\n    total = a + b  # a comment\n    print(f\"{total}\")\n    return total",
        ),
        (
            "typescript",
            "export function add(a: number, b: number): number {\n  const total = a + b; // a comment\n  console.log(`${total}`);\n  return total;\n}",
        ),
        (
            "tsx",
            "export function Row({ name }: { name: string }) {\n  return <li className=\"row\">{name}</li>;\n}",
        ),
        ("javascript", "const add = (a, b) => {\n  return a + b;\n};"),
        (
            "go",
            "func Add(a, b int) int {\n\ttotal := a + b // a comment\n\tfmt.Println(total)\n\treturn total\n}",
        ),
        (
            "bash",
            "#!/usr/bin/env bash\nset -euo pipefail\nfor f in *.rs; do\n  echo \"$f\" | wc -c\ndone",
        ),
        (
            "json",
            "{\n  \"name\": \"example\",\n  \"count\": 12,\n  \"nested\": { \"ok\": true, \"none\": null }\n}",
        ),
        (
            "toml",
            "[package]\nname = \"example\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = { version = \"1.0\", features = [\"derive\"] }",
        ),
        (
            "yaml",
            "name: example\nsteps:\n  - run: cargo test\n    env:\n      RUST_BACKTRACE: \"1\"",
        ),
        (
            "sql",
            "SELECT name, COUNT(*) AS n\nFROM sessions\nWHERE started_at > '2026-01-01'\nGROUP BY name\nORDER BY n DESC;",
        ),
        (
            "diff",
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n fn main() {\n-    old();\n+    new();\n }",
        ),
        (
            "c",
            "#include <stdio.h>\n\nint main(void) {\n    printf(\"%d\\n\", 42);\n    return 0;\n}",
        ),
        (
            "html",
            "<!doctype html>\n<html>\n  <body class=\"page\">\n    <p>Hello</p>\n  </body>\n</html>",
        ),
    ];
    let mut out = String::from("Fenced code, one block per language:\n\n");
    for (language, code) in samples {
        let _ = write!(out, "```{language}\n{code}\n```\n\n");
    }
    out.push_str("No language:\n\n```\nplain text in a fence\n  with indentation\n```\n\n");
    out.push_str(
        "A language no grammar knows:\n\n```brainfudge\n++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]\n```\n\n",
    );
    out.push_str(
        "A fence with an info string:\n\n```rust title=\"src/lib.rs\" {2}\nfn main() {}\n```\n\n",
    );
    out.push_str("Tildes:\n\n~~~python\nprint(\"tildes\")\n~~~\n\n");
    out.push_str("Very long lines:\n\n```json\n");
    let _ = writeln!(
        out,
        "{{\"minified\": [{}]}}",
        (0..400)
            .map(|n| format!("{{\"id\": {n}, \"name\": \"item-{n}\"}}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    out.push_str("```\n\n```\n");
    out.push_str(&"x".repeat(1_200));
    out.push_str("\n```\n");
    out
}

/// One fenced Rust block of `lines` lines, with a sentence either side.
pub fn long_code_block(lines: usize) -> String {
    let mut out = format!("A code block of {lines} lines:\n\n```rust\n");
    for n in 0..lines {
        let _ = match n % 5 {
            0 => writeln!(out, "/// Item {n}."),
            1 => writeln!(out, "pub fn item_{n}(input: &str) -> Option<usize> {{"),
            2 => writeln!(
                out,
                "    let found = input.find(\"needle-{n}\")?; // offset"
            ),
            3 => writeln!(out, "    Some(found + {n})"),
            _ => writeln!(out, "}}"),
        };
    }
    out.push_str("```\n\nText after the block.\n");
    out
}

/// Structures nested as deeply as replies ever nest them: a quote in a list in a quote, a list
/// ten levels deep, a table and code inside a list inside a quote.
fn deeply_nested() -> String {
    let mut out = String::from("A list ten levels deep:\n\n");
    for depth in 0..10 {
        let _ = writeln!(out, "{}- Level {}", "  ".repeat(depth), depth + 1);
    }
    out.push_str("\nQuotes six levels deep:\n\n");
    for depth in 1..=6 {
        let _ = writeln!(
            out,
            "{} Level {depth}\n{}",
            ">".repeat(depth),
            ">".repeat(depth)
        );
    }
    out.push_str(
        "\nA quote in a list in a quote, with a table and code:\n\n\
> 1. An item in a quote.\n\
>\n\
>    > A quote in the item.\n\
>    >\n\
>    > - A list in that quote.\n\
>    >\n\
>    >   ```python\n\
>    >   print(\"five containers deep\")\n\
>    >   ```\n\
>\n\
>    | In | A | Table |\n\
>    |---|---|---|\n\
>    | 1 | 2 | 3 |\n\
>\n\
> 2. The next item.\n",
    );
    out
}

/// A deterministic stream of numbers for [`reply`]: a linear congruential generator, so the
/// crate needs no dependency for it.
struct Sequence(u64);

impl Sequence {
    fn next(&mut self, below: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % below
    }
}

const WORDS: [&str; 32] = [
    "the",
    "archive",
    "session",
    "reader",
    "parser",
    "sync",
    "location",
    "provider",
    "version",
    "catalogue",
    "folder",
    "transcript",
    "index",
    "offset",
    "block",
    "render",
    "layout",
    "measure",
    "cache",
    "thread",
    "result",
    "error",
    "path",
    "branch",
    "token",
    "search",
    "match",
    "filter",
    "column",
    "theme",
    "window",
    "frame",
];

fn sentence(sequence: &mut Sequence, out: &mut String) {
    let words = 6 + sequence.next(14);
    for n in 0..words {
        let word = WORDS[sequence.next(WORDS.len())];
        match (n, sequence.next(18)) {
            (0, _) => {
                let mut letters = word.chars();
                if let Some(first) = letters.next() {
                    out.extend(first.to_uppercase());
                    out.push_str(letters.as_str());
                }
            }
            (_, 0) => {
                let _ = write!(out, " **{word}**");
            }
            (_, 1) => {
                let _ = write!(out, " `{word}_{n}`");
            }
            (_, 2) => {
                let _ = write!(out, " [{word}](https://example.com/{word})");
            }
            (_, 3) => {
                let _ = write!(out, " *{word}*");
            }
            _ => {
                let _ = write!(out, " {word}");
            }
        }
    }
    out.push_str(". ");
}

/// A reply of about `target_bytes` of Markdown in the mix replies have: mostly paragraphs and
/// lists, with headings, fenced code, tables and quotes among them. The same `seed` and size
/// give the same text.
pub fn reply(seed: u64, target_bytes: usize) -> String {
    let mut sequence = Sequence(seed.wrapping_mul(2_654_435_761).wrapping_add(1));
    let mut out = String::with_capacity(target_bytes + 512);
    while out.len() < target_bytes {
        match sequence.next(20) {
            0 => {
                let _ = write!(out, "## ");
                sentence(&mut sequence, &mut out);
                out.push_str("\n\n");
            }
            1..=3 => {
                for _ in 0..(2 + sequence.next(5)) {
                    out.push_str("- ");
                    sentence(&mut sequence, &mut out);
                    out.push('\n');
                }
                out.push('\n');
            }
            4 => {
                for n in 0..(2 + sequence.next(4)) {
                    let _ = write!(out, "{}. ", n + 1);
                    sentence(&mut sequence, &mut out);
                    out.push('\n');
                }
                out.push('\n');
            }
            5 | 6 => {
                let language =
                    ["rust", "python", "typescript", "bash", "json", ""][sequence.next(6)];
                let _ = writeln!(out, "```{language}");
                for n in 0..(3 + sequence.next(20)) {
                    let _ = writeln!(
                        out,
                        "let {}_{n} = {}({}); // {}",
                        WORDS[sequence.next(WORDS.len())],
                        WORDS[sequence.next(WORDS.len())],
                        sequence.next(1_000),
                        WORDS[sequence.next(WORDS.len())],
                    );
                }
                out.push_str("```\n\n");
            }
            7 => {
                out.push_str("| Name | Kind | Count |\n|---|---|---:|\n");
                for _ in 0..(2 + sequence.next(6)) {
                    let _ = writeln!(
                        out,
                        "| {} | `{}` | {} |",
                        WORDS[sequence.next(WORDS.len())],
                        WORDS[sequence.next(WORDS.len())],
                        sequence.next(10_000),
                    );
                }
                out.push('\n');
            }
            8 => {
                out.push_str("> ");
                sentence(&mut sequence, &mut out);
                out.push_str("\n\n");
            }
            _ => {
                for _ in 0..(1 + sequence.next(4)) {
                    sentence(&mut sequence, &mut out);
                }
                out.push_str("\n\n");
            }
        }
    }
    out
}

/// The sizes of `count` replies as sessions have them: most under a kilobyte or two, a tail of
/// tens of kilobytes. The same `seed` gives the same sizes.
pub fn reply_sizes(seed: u64, count: usize) -> Vec<usize> {
    let mut sequence = Sequence(seed.wrapping_add(7));
    (0..count)
        .map(|_| match sequence.next(100) {
            0 => 20_000 + sequence.next(40_000),
            1..=5 => 5_000 + sequence.next(10_000),
            6..=30 => 1_500 + sequence.next(3_000),
            _ => 100 + sequence.next(1_200),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_corpus_has_a_section_per_construct() {
        let names: Vec<_> = corpus().iter().map(|section| section.name).collect();
        assert_eq!(
            names,
            [
                "headings",
                "inline",
                "lists",
                "quotes",
                "tables",
                "code",
                "code-long",
                "blocks",
                "unicode",
                "nested",
                "malformed"
            ]
        );
    }

    #[test]
    fn a_reply_is_about_the_size_asked_for_and_repeats() {
        for size in [1_000, 10_000, 100_000] {
            let text = reply(3, size);
            assert!(
                text.len() >= size && text.len() < size + 4_000,
                "{}",
                text.len()
            );
            assert_eq!(text, reply(3, size));
        }
        assert_ne!(reply(1, 2_000), reply(2, 2_000));
    }

    #[test]
    fn the_long_code_block_has_the_lines_asked_for() {
        let text = long_code_block(2_000);
        let code = text
            .lines()
            .skip_while(|line| *line != "```rust")
            .skip(1)
            .take_while(|line| *line != "```");
        assert_eq!(code.count(), 2_000);
    }
}
