//! Text in the user's place (PLAN §7.3): what was asked, and what Claude Code put there itself.
//! A `user` record, and a prompt queued as an attachment, hold both, told apart by the record's
//! `isMeta`, by its `origin`, and by the markup Claude Code wraps its own additions in.

use super::record::{Origin, Text};
use crate::transcript::Content;

/// The `origin` of the notice that a background task finished, which is also its tag.
const TASK_NOTICE: &str = "task-notification";

/// The tags Claude Code wraps what it puts in the user's place itself in. Each such element
/// is a note.
const WRAPPERS: [&str; 7] = [
    "system-reminder",
    "side-sessions-event",
    "local-command-stdout",
    "local-command-caveat",
    "artifact-view-context",
    "fork-boilerplate",
    TASK_NOTICE,
];

/// The tags around a message from another session, whose `from-name` or `from` is its sender.
const MESSAGES: [&str; 2] = ["cross-session-message", "agent-message"];

/// What Claude Code writes in the user's place itself, in brackets, and the kind of note each
/// is.
const BRACKETED: [(&str, &str); 2] = [
    ("[Request interrupted by user", "interrupted"),
    ("[Workflow harness", "workflow-harness"),
];

/// Text in the user's place, as the model has it: notes, and a prompt with who it is from.
pub(super) struct Said {
    pub notes: Vec<Content>,
    pub prompt: Option<(String, Option<String>)>,
}

/**
 * Sorts text in the user's place into what was asked and what Claude Code put there.
 *
 * - Leading elements in the tags Claude Code wraps its own additions in are notes, each of
 *   its tag's kind with the element's content as its text. What follows them is taken on its
 *   own, so a prompt behind a reminder is still a prompt.
 * - With an `origin` other than `human` the rest came from elsewhere. A finished background
 *   task's notice is a note. Anything else (another session, a coordinator) is a prompt from
 *   that sender, and its text is the message alone, without the markup around it.
 * - On a record marked `isMeta`, or starting with one of the bracketed notices Claude Code
 *   writes itself (an interruption), the rest is a note.
 * - A slash command is a prompt: the command as it was typed.
 */
pub(super) fn said(text: &str, meta: bool, origin: Option<&Origin>) -> Said {
    let note = |kind: &str, text: &str| Content::Note {
        kind: kind.to_string(),
        text: text.trim().to_string(),
    };
    let mut notes = Vec::new();
    let mut rest = text;
    while let Some(element) = element(rest).filter(|element| WRAPPERS.contains(&element.tag)) {
        notes.push(note(element.tag, element.content));
        rest = element.rest;
    }
    let rest = rest.trim();
    let message = element(rest).filter(|element| MESSAGES.contains(&element.tag));
    let prompt = match origin.filter(|origin| origin.kind != "human") {
        _ if rest.is_empty() => None,
        Some(origin) if origin.kind == TASK_NOTICE => {
            notes.push(note(TASK_NOTICE, rest));
            None
        }
        Some(origin) => Some((
            match (&origin.body, &message) {
                (Some(Text(body)), _) => body.trim(),
                (None, Some(message)) => message.content.trim(),
                (None, None) => rest,
            }
            .to_string(),
            Some(
                origin
                    .name
                    .as_ref()
                    .or(origin.from.as_ref())
                    .map_or(origin.kind.as_str(), |Text(sender)| sender.as_str())
                    .to_string(),
            ),
        )),
        None if meta => {
            notes.push(note("meta", rest));
            None
        }
        None => match BRACKETED.iter().find(|(start, _)| rest.starts_with(start)) {
            Some((_, kind)) => {
                notes.push(note(kind, rest));
                None
            }
            None => Some(match (command(rest), message) {
                (Some(command), _) => (command, None),
                (None, Some(message)) => (
                    message.content.trim().to_string(),
                    Some(
                        attribute(message.attributes, "from-name")
                            .or(attribute(message.attributes, "from"))
                            .unwrap_or(message.tag)
                            .to_string(),
                    ),
                ),
                (None, None) => (rest.to_string(), None),
            }),
        },
    };
    Said { notes, prompt }
}

/// An element at the start of text: `<tag attributes>content</tag>`, and the text after it.
struct Element<'a> {
    tag: &'a str,
    attributes: &'a str,
    content: &'a str,
    rest: &'a str,
}

/// The element `text` starts with, if it starts with one. One that is never closed runs to
/// the end of the text.
fn element(text: &str) -> Option<Element<'_>> {
    let open = text.trim_start().strip_prefix('<')?;
    let (open, after) = open.split_once('>')?;
    let (tag, attributes) = open.split_once(char::is_whitespace).unwrap_or((open, ""));
    let named = !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    let (content, rest) = after
        .split_once(&format!("</{tag}>"))
        .unwrap_or((after, ""));
    named.then_some(Element {
        tag,
        attributes,
        content,
        rest,
    })
}

/// The value of the attribute `name` among an element's `attributes`.
fn attribute<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
    attributes
        .split_whitespace()
        .find_map(|attribute| attribute.strip_prefix(name)?.strip_prefix("=\""))
        .and_then(|value| {
            let start = value.as_ptr() as usize - attributes.as_ptr() as usize;
            attributes[start..].split('"').next()
        })
}

/**
 * The slash command `text` records, as it was typed: `/name arguments`. Claude Code writes one
 * as `<command-name>`, `<command-message>` and `<command-args>` elements, in an order that
 * has changed between releases.
 */
fn command(text: &str) -> Option<String> {
    let (mut name, mut message, mut arguments) = (None, None, None);
    let mut rest = text;
    while let Some(element) = element(rest) {
        match element.tag {
            "command-name" => name = Some(element.content.trim()),
            "command-message" => message = Some(element.content.trim()),
            "command-args" => arguments = Some(element.content.trim()),
            _ => break,
        }
        rest = element.rest;
    }
    let typed = [
        name.or(message)?,
        arguments.unwrap_or_default(),
        rest.trim(),
    ];
    Some(
        typed
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
    )
}
