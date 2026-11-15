//! [`Script`]: what one call writes to the shell's stdin: the functions of `shell.sh`, then
//! the call's requests, each a line that calls one of them. And [`Login`], the request that
//! asks the user's login shell what its profile sets.

use std::collections::HashMap;

use super::reply::{END, Reply};
use super::{LOGIN_SECONDS, remote};
use crate::transport::TransportError;

/// What runs at the location: a function per operation.
const FUNCTIONS: &str = include_str!("shell.sh");

/// The script of one call: the functions, then the requests between braces, so that the
/// shell has read all of it before it runs any of it.
pub(super) struct Script {
    /// The line the answer starts at: one that nothing else at the location can have printed.
    marker: String,
    requests: String,
}

impl Script {
    pub(super) fn new() -> Self {
        Self {
            marker: unique("answer"),
            requests: String::new(),
        }
    }

    pub(super) fn marker(&self) -> &str {
        &self.marker
    }

    /// Adds a request that names nothing of the location's.
    pub(super) fn line(mut self, request: &str) -> Self {
        self.requests.push_str(request);
        self.requests.push('\n');
        self
    }

    /// Adds a call of `function` with `numbers` and then `path`.
    pub(super) fn call(
        self,
        function: &str,
        numbers: &[u64],
        path: &str,
    ) -> Result<Self, TransportError> {
        let numbers: String = numbers.iter().map(|number| format!(" {number}")).collect();
        Ok(self.line(&format!("{function}{numbers} {}", anchored(path)?)))
    }

    /// Adds a call of `function` with `words`, each as one word, and then `paths`.
    pub(super) fn group<'a>(
        self,
        function: &str,
        words: &[String],
        paths: impl Iterator<Item = &'a str>,
    ) -> Result<Self, TransportError> {
        let mut request = function.to_string();
        for word in words {
            request.push(' ');
            request.push_str(&quoted(word));
        }
        for path in paths {
            request.push(' ');
            request.push_str(&anchored(path)?);
        }
        Ok(self.line(&request))
    }

    /// Adds the request that has the login shell say which of the variables `names` its
    /// profile leaves set, and to what ([`Login::read`] reads the answer).
    pub(super) fn login(self, login: &Login, names: &[&str]) -> Self {
        /* What `sh -c` runs inside the login shell: its output between two lines of its
        own, so that whatever the profile prints around it is told apart. */
        let mut inner = format!("printf '\\n%s\\n' '{}'", login.begin);
        for name in names {
            inner.push_str(&format!(
                "; [ -z \"${{{name}+set}}\" ] || printf 'V %s\\n%s\\0' {name} \"${name}\""
            ));
        }
        inner.push_str(&format!("; printf '%s\\n' '{}'", login.end));
        self.line(&format!(
            "as_login {} {} {} {LOGIN_SECONDS}",
            quoted(&inner),
            quoted(&login.done),
            quoted(&login.late)
        ))
    }

    /// The script with `need <tools>` as the first thing it does.
    pub(super) fn needing(self, tools: &str) -> Self {
        Self {
            requests: format!("need {tools}\n{}", self.requests),
            ..self
        }
    }

    /// What goes to the shell's stdin.
    pub(super) fn text(&self) -> String {
        format!(
            "{FUNCTIONS}{{\nprintf '%s\\n' '{}'\n{}printf '{END}\\n'\n}} </dev/null\n",
            self.marker, self.requests
        )
    }
}

/// The lines that mark off what a login shell's command printed from what its profile did.
pub(super) struct Login {
    /// Before and after the command's own output.
    begin: String,
    end: String,
    /// Before `done`, when the login shell was killed for taking too long.
    late: String,
    /// After the login shell has ended, however it ended.
    done: String,
}

/// What the user's login shell had to say for itself.
pub(super) enum Asked {
    /// The variables it has set, of those asked about.
    Answered(HashMap<String, String>),
    /// Nothing, and it would say as much another time: there is none, it takes no `-lc`
    /// (csh and tcsh don't), or its profile fails.
    Silent,
    /// It was still running when its time was up, and was killed: its profile waits for
    /// something, or the location is busy just now.
    Late,
}

impl Login {
    pub(super) fn new() -> Self {
        Self {
            begin: unique("begin"),
            end: unique("end"),
            late: unique("late"),
            done: unique("done"),
        }
    }

    /// Reads what the request added by [`Script::login`] answered.
    pub(super) fn read(&self, reply: &mut Reply<'_>) -> Result<Asked, TransportError> {
        let line = |line: &str| format!("{line}\n");
        let (begin, late, done) = (line(&self.begin), line(&self.late), line(&self.done));
        let mut vars = HashMap::new();
        let mut whole = false;
        let mut found = reply.skip_to(&[&begin, &late, &done])?;
        if found == 0 {
            whole = loop {
                let line = reply.line()?;
                let Some(name) = line.strip_prefix("V ") else {
                    break line == self.end;
                };
                /* As on this machine, a value that is not Unicode counts as not set. */
                if let Ok(value) = String::from_utf8(reply.text()?) {
                    vars.insert(name.to_string(), value);
                }
            };
            found = 1 + reply.skip_to(&[&late, &done])?;
        }
        if found == 1 {
            reply.skip_to(&[&done])?;
        }
        /* An answer that is whole stands, whatever kept the shell from ending after it. */
        Ok(match (whole, found) {
            (true, _) => Asked::Answered(vars),
            (false, 1) => Asked::Late,
            (false, _) => Asked::Silent,
        })
    }
}

/// A line that nothing at the location can print unless this call's script has it do so.
fn unique(what: &str) -> String {
    format!("scrollback {what} {}", uuid::Uuid::new_v4().simple())
}

/// `word` between single quotes, where a POSIX shell reads every character as itself.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// `path` as one word of a request. One that is not absolute is made to start with `./`: no
/// tool then takes one that begins with a dash for an option.
fn anchored(path: &str) -> Result<String, TransportError> {
    if path.contains('\0') {
        return Err(remote("name", path, "a path can't hold a NUL".to_string()));
    }
    Ok(quoted(&anchor(path)))
}

/// `path` as the tools at the location are given it.
pub(super) fn anchor(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("./{path}")
    }
}

/// Whether `name` can be an environment variable's name in a shell.
pub(super) fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|first: char| first.is_ascii_digit())
        && name
            .chars()
            .all(|letter| letter.is_ascii_alphanumeric() || letter == '_')
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_path_is_one_shell_word_whatever_is_in_it() {
        let script = Script::new()
            .call("range", &[3, 7], "/a b/it's \"here\"\n$HOME `x`")
            .unwrap()
            .call("file L", &[], "-rf")
            .unwrap()
            .group(
                "bulk",
                &["9".to_string(), "4 5 ".to_string()],
                ["/it's", "--"].into_iter(),
            )
            .unwrap();
        assert_eq!(
            script.requests,
            "range 3 7 '/a b/it'\\''s \"here\"\n$HOME `x`'\nfile L './-rf'\n\
             bulk '9' '4 5 ' '/it'\\''s' './--'\n"
        );
        assert!(Script::new().call("small", &[], "/a\0b").is_err());
        assert!(
            Script::new()
                .group("bulk", &[], ["/a", "/b\0"].into_iter())
                .is_err()
        );
    }

    #[test]
    fn only_a_variables_name_is_asked_about() {
        for name in ["HOME", "_x", "A1_b"] {
            assert!(is_identifier(name), "{name}");
        }
        for name in ["", "1A", "A B", "A;id", "A-B", "É"] {
            assert!(!is_identifier(name), "{name}");
        }
    }
}
