//! A shell's code, highlighted by hand: `bash`, `sh`, `shell` and `zsh` fences, and `console`,
//! a terminal session of prompts and what they printed. Tree-sitter's Bash grammar aborts the
//! process on some input (the module docs above), and shell is among the code transcripts hold
//! most.
//!
//! One pass over the bytes, whatever they are: linear in the code, never panicking, holding
//! only the tokens it finds. It recurses only into `$(…)`, at most [`MAX_DEPTH`] deep; past
//! that the rest of the code is plain. It colours what a reader looks for, not all a shell
//! reads: comments, strings, expansions (`$var`, `${…}`, `$(…)`, backquotes), keywords, the
//! first word of each command, operators and redirections, and heredocs' bodies.

use std::ops::Range;

use super::TokenKind;

/// The deepest `$(…)` lexed inside another; past it the rest is plain.
const MAX_DEPTH: usize = 16;
/// The most heredocs one line may open: those past it are lexed as their lines.
const MAX_HEREDOCS: usize = 16;

/// The words a shell reads as keywords where a command may start.
const KEYWORDS: &[&[u8]] = &[
    b"if",
    b"then",
    b"else",
    b"elif",
    b"fi",
    b"for",
    b"while",
    b"until",
    b"do",
    b"done",
    b"case",
    b"esac",
    b"function",
    b"in",
    b"select",
    b"return",
    b"export",
    b"local",
    b"{",
    b"}",
    b"!",
];

/// What a fence's language names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dialect {
    /// A script.
    Script,
    /// A session: a line that starts with a prompt (`$ ` or `# `) is a command, any other what
    /// it printed.
    Console,
}

/// The dialect a fence's language names, if it is a shell's.
pub(super) fn dialect(language: &str) -> Option<Dialect> {
    match language.to_ascii_lowercase().as_str() {
        "bash" | "sh" | "shell" | "zsh" => Some(Dialect::Script),
        "console" => Some(Dialect::Console),
        _ => None,
    }
}

/// The non-plain tokens of `code` (the module docs), as byte ranges in order.
pub(super) fn highlight(code: &str, dialect: Dialect) -> Vec<(Range<usize>, TokenKind)> {
    let mut lexer = Lexer {
        code: code.as_bytes(),
        at: 0,
        end: code.len(),
        tokens: Vec::new(),
        heredocs: Vec::new(),
        stopped: false,
    };
    match dialect {
        Dialect::Script => lexer.commands(0),
        Dialect::Console => lexer.console(),
    }
    lexer.tokens
}

/// What the next word of a command is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// A command's first word, a keyword, or an assignment before it.
    Command,
    /// The variable of `for`, `select` or `case`; `in` follows.
    Name,
    /// `in`, after a `for`, `select` or `case`'s name.
    In,
    /// The name of a `function`.
    Function,
    /// What `export` or `local` declares: assignments, or names.
    Declare,
    /// Arguments.
    Args,
}

struct Lexer<'a> {
    code: &'a [u8],
    at: usize,
    /// Where the code being lexed ends: the whole code's, or a console's command line's.
    end: usize,
    tokens: Vec<(Range<usize>, TokenKind)>,
    /// The heredocs opened on the line being lexed, whose bodies follow it: each delimiter, and
    /// whether its lines may be indented with tabs (`<<-`).
    heredocs: Vec<(Range<usize>, bool)>,
    /// Set past [`MAX_DEPTH`]: nothing more is lexed.
    stopped: bool,
}

impl Lexer<'_> {
    fn byte(&self, at: usize) -> Option<u8> {
        (at < self.end).then(|| self.code[at])
    }

    /// Paints `range`, merged with the token before when it is the same kind and touches it.
    fn paint(&mut self, range: Range<usize>, kind: TokenKind) {
        if range.is_empty() || kind == TokenKind::Plain {
            return;
        }
        match self.tokens.last_mut() {
            Some((last, same)) if last.end == range.start && *same == kind => last.end = range.end,
            _ => self.tokens.push((range, kind)),
        }
    }

    /// Each line of a session that starts with a prompt, lexed as a command line.
    fn console(&mut self) {
        let mut start = 0;
        while start < self.code.len() && !self.stopped {
            let end = (self.code[start..].iter().position(|&b| b == b'\n'))
                .map_or(self.code.len(), |at| start + at);
            let line = &self.code[start..end];
            if matches!(line, [b'$' | b'#'] | [b'$' | b'#', b' ', ..]) {
                (self.at, self.end) = (start + line.len().min(2), end);
                self.heredocs.clear();
                self.commands(0);
            }
            start = end + 1;
        }
    }

    /**
     * Commands, from `at` to the end, or inside a `$(…)` `depth` deep, to the `)` that closes
     * it, which is left for the caller.
     */
    fn commands(&mut self, depth: usize) {
        let mut expect = Expect::Command;
        while let Some(byte) = self.byte(self.at)
            && !self.stopped
        {
            match byte {
                b'\n' => {
                    self.at += 1;
                    self.bodies();
                    expect = Expect::Command;
                }
                b' ' | b'\t' | b'\r' => self.at += 1,
                b'#' => {
                    let start = self.at;
                    while self.byte(self.at).is_some_and(|b| b != b'\n') {
                        self.at += 1;
                    }
                    self.paint(start..self.at, TokenKind::Comment);
                }
                b')' if depth > 0 => return,
                b'|' | b'&' | b';' | b'(' | b')' => {
                    let start = self.at;
                    self.at += 1;
                    if self.byte(self.at).is_some_and(|next| {
                        matches!(
                            (byte, next),
                            (b'|' | b'&', b'|' | b'&' | b'>') | (b';', b';' | b'&')
                        )
                    }) {
                        self.at += 1;
                    }
                    self.paint(start..self.at, TokenKind::Keyword);
                    expect = Expect::Command;
                }
                b'<' | b'>' => self.redirect(depth),
                _ => expect = self.next_word(expect, depth),
            }
        }
    }

    /// A word, what `expect` says it is; what the next one is.
    fn next_word(&mut self, expect: Expect, depth: usize) -> Expect {
        if matches!(expect, Expect::Command | Expect::Declare) && self.assignment() {
            self.word(depth);
            return expect;
        }
        let (range, literal) = self.word(depth);
        let text = &self.code[range.clone()];
        let keyword = literal && KEYWORDS.contains(&text);
        match expect {
            Expect::Command if keyword => {
                self.paint(range, TokenKind::Keyword);
                match text {
                    b"for" | b"select" | b"case" => Expect::Name,
                    b"function" => Expect::Function,
                    b"export" | b"local" => Expect::Declare,
                    b"return" | b"fi" | b"done" | b"esac" | b"}" => Expect::Args,
                    _ => Expect::Command,
                }
            }
            Expect::Command => {
                if literal {
                    self.paint(range, TokenKind::Function);
                }
                Expect::Args
            }
            /* Its body follows: `{`, or `()` and then `{`. */
            Expect::Function => {
                if literal {
                    self.paint(range, TokenKind::Function);
                }
                Expect::Command
            }
            Expect::Name => Expect::In,
            Expect::In if literal && text == b"in" => {
                self.paint(range, TokenKind::Keyword);
                Expect::Args
            }
            Expect::Declare => Expect::Declare,
            Expect::In | Expect::Args => Expect::Args,
        }
    }

    /// Paints an assignment's name (`name=`, `name+=`) and moves past its `=`, if one starts
    /// at `at`.
    fn assignment(&mut self) -> bool {
        let start = self.at;
        let mut at = start;
        while self
            .byte(at)
            .is_some_and(|b| b == b'_' || b.is_ascii_alphanumeric())
        {
            at += 1;
        }
        let name = at > start && !self.code[start].is_ascii_digit();
        let equals = match (self.byte(at), self.byte(at + 1)) {
            (Some(b'='), _) => at + 1,
            (Some(b'+'), Some(b'=')) => at + 2,
            _ => return false,
        };
        if name {
            self.paint(start..at, TokenKind::Property);
            self.at = equals;
        }
        name
    }

    /**
     * A word from `at`: up to a blank or an operator, its quotes and expansions painted as it
     * goes. Its range, and whether it is literal (none of those in it).
     */
    fn word(&mut self, depth: usize) -> (Range<usize>, bool) {
        let start = self.at;
        let mut literal = true;
        while let Some(byte) = self.byte(self.at)
            && !self.stopped
        {
            match byte {
                b' ' | b'\t' | b'\r' | b'\n' | b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')' => {
                    break;
                }
                b'\\' => self.at = self.end.min(self.at + 2),
                b'\'' => {
                    literal = false;
                    self.single_quoted();
                }
                b'"' => {
                    literal = false;
                    self.double_quoted(depth);
                }
                b'$' => {
                    literal = false;
                    self.dollar(depth);
                }
                b'`' => {
                    literal = false;
                    self.backquoted();
                }
                _ => self.at += 1,
            }
        }
        (start..self.at, literal)
    }

    /// `'…'`, to the closing quote or the end.
    fn single_quoted(&mut self) {
        let start = self.at;
        self.at += 1;
        while self.byte(self.at).is_some_and(|b| b != b'\'') {
            self.at += 1;
        }
        self.at = self.end.min(self.at + 1);
        self.paint(start..self.at, TokenKind::String);
    }

    /// `"…"`, to the closing quote or the end, with the expansions in it.
    fn double_quoted(&mut self, depth: usize) {
        let mut run = self.at;
        self.at += 1;
        while let Some(byte) = self.byte(self.at)
            && !self.stopped
        {
            match byte {
                b'"' => break,
                b'\\' => self.at = self.end.min(self.at + 2),
                b'$' | b'`' => {
                    self.paint(run..self.at, TokenKind::String);
                    if byte == b'$' {
                        self.dollar(depth);
                    } else {
                        self.backquoted();
                    }
                    run = self.at;
                }
                _ => self.at += 1,
            }
        }
        self.at = self.end.min(self.at + 1);
        self.paint(run..self.at, TokenKind::String);
    }

    /// An expansion at a `$`: `$(…)`, `${…}`, `$'…'`, `$name`, `$1`, `$?`; a `$` on its own is
    /// plain.
    fn dollar(&mut self, depth: usize) {
        let start = self.at;
        self.at += 1;
        match self.byte(self.at) {
            Some(b'(') => {
                self.at += 1;
                self.paint(start..self.at, TokenKind::Property);
                self.substitution(depth);
            }
            Some(b'{') => {
                let mut open = 0usize;
                while let Some(byte) = self.byte(self.at)
                    && byte != b'\n'
                {
                    self.at += 1;
                    match byte {
                        b'{' => open += 1,
                        b'}' => {
                            open -= 1;
                            if open == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                self.paint(start..self.at, TokenKind::Property);
            }
            Some(b'\'') => {
                self.at += 1;
                while let Some(byte) = self.byte(self.at)
                    && byte != b'\''
                {
                    self.at = self.end.min(self.at + if byte == b'\\' { 2 } else { 1 });
                }
                self.at = self.end.min(self.at + 1);
                self.paint(start..self.at, TokenKind::String);
            }
            Some(b) if b == b'_' || b.is_ascii_alphabetic() => {
                while self
                    .byte(self.at)
                    .is_some_and(|b| b == b'_' || b.is_ascii_alphanumeric())
                {
                    self.at += 1;
                }
                self.paint(start..self.at, TokenKind::Property);
            }
            Some(b'0'..=b'9' | b'@' | b'*' | b'#' | b'?' | b'$' | b'!' | b'-') => {
                self.at += 1;
                self.paint(start..self.at, TokenKind::Property);
            }
            _ => {}
        }
    }

    /// The commands of a `$(…)` or `<(…)` whose opening is painted, and its `)`. Past
    /// [`MAX_DEPTH`], nothing more is lexed.
    fn substitution(&mut self, depth: usize) {
        if depth >= MAX_DEPTH {
            self.stopped = true;
            return;
        }
        self.commands(depth + 1);
        if !self.stopped && self.byte(self.at) == Some(b')') {
            self.paint(self.at..self.at + 1, TokenKind::Property);
            self.at += 1;
        }
    }

    /// `` `…` ``, to the closing backquote or the end: an old command substitution, painted
    /// whole.
    fn backquoted(&mut self) {
        let start = self.at;
        self.at += 1;
        while let Some(byte) = self.byte(self.at)
            && byte != b'`'
        {
            self.at = self.end.min(self.at + if byte == b'\\' { 2 } else { 1 });
        }
        self.at = self.end.min(self.at + 1);
        self.paint(start..self.at, TokenKind::Property);
    }

    /**
     * A redirection at `<` or `>`: its operator, and for a heredoc (`<<`, `<<-`) its delimiter,
     * whose body follows the line; `<(…)` and `>(…)` are substitutions.
     */
    fn redirect(&mut self, depth: usize) {
        let start = self.at;
        let first = self.code[start];
        self.at += 1;
        match (first, self.byte(self.at)) {
            (_, Some(b'(')) => {
                self.at += 1;
                self.paint(start..self.at, TokenKind::Property);
                self.substitution(depth);
            }
            (b'<', Some(b'<')) if self.byte(self.at + 1) == Some(b'<') => {
                self.at += 2;
                self.paint(start..self.at, TokenKind::Keyword);
                self.target(depth);
            }
            (b'<', Some(b'<')) => {
                self.at += 1;
                let strip = self.byte(self.at) == Some(b'-');
                if strip {
                    self.at += 1;
                }
                self.paint(start..self.at, TokenKind::Keyword);
                self.delimiter(strip);
            }
            (_, next) => {
                if matches!(next, Some(b'>' | b'&' | b'|')) {
                    self.at += 1;
                }
                self.paint(start..self.at, TokenKind::Keyword);
                self.target(depth);
            }
        }
    }

    /// The word a redirection reads or writes, if one follows: a file, or a descriptor.
    fn target(&mut self, depth: usize) {
        while self.byte(self.at).is_some_and(|b| b == b' ' || b == b'\t') {
            self.at += 1;
        }
        if self.byte(self.at).is_some_and(|b| {
            !matches!(
                b,
                b'\n' | b'#' | b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')'
            )
        }) {
            self.word(depth);
        }
    }

    /// A heredoc's delimiter word, painted as a string; its body is lexed after the line.
    fn delimiter(&mut self, strip: bool) {
        while self.byte(self.at).is_some_and(|b| b == b' ' || b == b'\t') {
            self.at += 1;
        }
        let start = self.at;
        let delimiter = match self.byte(self.at) {
            Some(quote @ (b'\'' | b'"')) => {
                self.at += 1;
                let from = self.at;
                while self.byte(self.at).is_some_and(|b| b != quote && b != b'\n') {
                    self.at += 1;
                }
                let delimiter = from..self.at;
                if self.byte(self.at) == Some(quote) {
                    self.at += 1;
                }
                delimiter
            }
            _ => {
                if self.byte(self.at) == Some(b'\\') {
                    self.at += 1;
                }
                let from = self.at;
                while self.byte(self.at).is_some_and(|b| {
                    !matches!(
                        b,
                        b' ' | b'\t'
                            | b'\r'
                            | b'\n'
                            | b'|'
                            | b'&'
                            | b';'
                            | b'<'
                            | b'>'
                            | b'('
                            | b')'
                    )
                }) {
                    self.at += 1;
                }
                from..self.at
            }
        };
        self.paint(start..self.at, TokenKind::String);
        if !delimiter.is_empty() && self.heredocs.len() < MAX_HEREDOCS {
            self.heredocs.push((delimiter, strip));
        }
    }

    /// The bodies of the heredocs the line just ended opened, each to the line that is its
    /// delimiter (that line included), or to the end: strings.
    fn bodies(&mut self) {
        for (delimiter, strip) in std::mem::take(&mut self.heredocs) {
            let start = self.at;
            while self.at < self.end {
                let line_end = (self.code[self.at..self.end]
                    .iter()
                    .position(|&b| b == b'\n'))
                .map_or(self.end, |at| self.at + at);
                let mut line = &self.code[self.at..line_end];
                if strip {
                    while let [b'\t', rest @ ..] = line {
                        line = rest;
                    }
                }
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                self.at = self.end.min(line_end + 1);
                if line == &self.code[delimiter.clone()] {
                    break;
                }
            }
            self.paint(start..self.at, TokenKind::String);
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// The tokens of `code` as `(text, kind)`.
    fn tokens(code: &str, dialect: Dialect) -> Vec<(&str, TokenKind)> {
        highlight(code, dialect)
            .into_iter()
            .map(|(range, kind)| (&code[range], kind))
            .collect()
    }

    fn script(code: &str) -> Vec<(&str, TokenKind)> {
        tokens(code, Dialect::Script)
    }

    use TokenKind::{Comment, Function, Keyword, Property, String};

    #[test]
    fn a_command_its_strings_expansions_and_comment() {
        assert_eq!(
            script("echo 'a b' \"x $HOME ${PATH:-/bin} $(id -u) y\" $1 # done"),
            vec![
                ("echo", Function),
                ("'a b'", String),
                ("\"x ", String),
                ("$HOME", Property),
                (" ", String),
                ("${PATH:-/bin}", Property),
                (" ", String),
                ("$(", Property),
                ("id", Function),
                (")", Property),
                (" y\"", String),
                ("$1", Property),
                ("# done", Comment),
            ]
        );
    }

    #[test]
    fn keywords_operators_and_each_commands_first_word() {
        assert_eq!(
            script("if [ -f a ]; then\n  cat a | grep b && x=1 make >> log 2>&1\nfi"),
            vec![
                ("if", Keyword),
                ("[", Function),
                (";", Keyword),
                ("then", Keyword),
                ("cat", Function),
                ("|", Keyword),
                ("grep", Function),
                ("&&", Keyword),
                ("x", Property),
                ("make", Function),
                (">>", Keyword),
                (">&", Keyword),
                ("fi", Keyword),
            ]
        );
        assert_eq!(
            script("for f in *.rs; do\n  echo \"$f\"\ndone"),
            vec![
                ("for", Keyword),
                ("in", Keyword),
                (";", Keyword),
                ("do", Keyword),
                ("echo", Function),
                ("\"", String),
                ("$f", Property),
                ("\"", String),
                ("done", Keyword),
            ]
        );
        assert_eq!(
            script("function greet { local n=$1; return 0; }"),
            vec![
                ("function", Keyword),
                ("greet", Function),
                ("{", Keyword),
                ("local", Keyword),
                ("n", Property),
                ("$1", Property),
                (";", Keyword),
                ("return", Keyword),
                (";", Keyword),
                ("}", Keyword),
            ]
        );
    }

    #[test]
    fn a_heredocs_body_is_a_string_to_its_delimiter() {
        assert_eq!(
            script("cat <<'EOF' > out\nline $x\nEOF\nls\n\tcat <<-END\n\tbody\n\tEND\npwd"),
            vec![
                ("cat", Function),
                ("<<", Keyword),
                ("'EOF'", String),
                (">", Keyword),
                ("line $x\nEOF\n", String),
                ("ls", Function),
                ("cat", Function),
                ("<<-", Keyword),
                ("END", String),
                ("\tbody\n\tEND\n", String),
                ("pwd", Function),
            ]
        );
    }

    #[test]
    fn a_consoles_prompts_start_its_commands_and_the_rest_is_what_they_printed() {
        assert_eq!(
            tokens(
                "$ cargo test --release\nrunning 3 tests\n# rm -rf target # tidy\n$\n",
                Dialect::Console
            ),
            vec![("cargo", Function), ("rm", Function), ("# tidy", Comment),]
        );
    }

    #[test]
    fn substitutions_nested_past_the_cap_leave_the_rest_plain() {
        let code = "echo $(".repeat(MAX_DEPTH + 4) + "x" + &")".repeat(MAX_DEPTH + 4) + "\nls";
        let found = script(&code);
        assert_eq!(
            found.iter().filter(|(text, _)| *text == "echo").count(),
            MAX_DEPTH + 1
        );
        assert!(!found.contains(&("ls", Function)));
        /* Deeper than any stack would take, were it not capped. */
        let deep = "$(".repeat(1_000_000);
        assert!(!highlight(&deep, Dialect::Script).is_empty());
    }

    #[test]
    fn the_fences_that_name_a_shell() {
        for name in ["bash", "sh", "shell", "zsh", "Bash"] {
            assert_eq!(dialect(name), Some(Dialect::Script), "{name}");
        }
        assert_eq!(dialect("console"), Some(Dialect::Console));
        assert_eq!(dialect("fish"), None);
    }
}
