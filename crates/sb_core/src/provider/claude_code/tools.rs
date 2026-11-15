//! Claude Code's tools as the common kinds (PLAN §7.4): their inputs as the model types them,
//! and the short form of their results.

use std::time::Duration;

use serde::Deserialize;
use serde_json::value::RawValue;

use super::record::{
    EditResult, Launched, Line, ReadResult, SearchResult, Text, WriteResult, string, typed,
};
use crate::transcript::{Brief, Todo, TodoState, ToolInput, ToolKind};

/// The tool that runs a workflow, whose subagents are filed by the run's id.
pub(super) const WORKFLOW: &str = "Workflow";

/// Which common kind the tool `name` is. `Task` is what earlier releases called `Agent`.
pub(super) fn kind(name: &str) -> ToolKind {
    match name {
        "Bash" => ToolKind::Shell,
        "Read" => ToolKind::Read,
        "Edit" => ToolKind::Edit,
        "Write" => ToolKind::Write,
        "Grep" => ToolKind::ContentSearch,
        "Glob" => ToolKind::FileSearch,
        "TodoWrite" => ToolKind::Todos,
        "Agent" | "Task" => ToolKind::Subagent,
        "WebSearch" => ToolKind::WebSearch,
        "WebFetch" => ToolKind::WebFetch,
        _ if name.starts_with("mcp__") => ToolKind::Mcp,
        _ => ToolKind::Other,
    }
}

#[derive(Deserialize)]
struct Bash {
    command: Text,
    description: Option<Text>,
}

#[derive(Deserialize)]
struct Read {
    file_path: Text,
    offset: Option<u64>,
    limit: Option<u64>,
}

#[derive(Deserialize)]
struct Edit<'a> {
    file_path: Text,
    #[serde(borrow)]
    old_string: &'a RawValue,
    #[serde(borrow)]
    new_string: &'a RawValue,
    replace_all: Option<bool>,
}

#[derive(Deserialize)]
struct Write<'a> {
    file_path: Text,
    #[serde(borrow)]
    content: &'a RawValue,
}

/// `Grep`'s input and `Glob`'s.
#[derive(Deserialize)]
struct Search {
    pattern: Text,
    path: Option<Text>,
}

#[derive(Deserialize)]
struct TodoWrite {
    todos: Vec<TodoItem>,
}

#[derive(Deserialize)]
struct TodoItem {
    content: Text,
    status: String,
}

#[derive(Deserialize)]
struct Agent<'a> {
    subagent_type: Option<Text>,
    description: Option<Text>,
    #[serde(borrow)]
    prompt: &'a RawValue,
}

#[derive(Deserialize)]
struct WebSearch {
    query: Text,
}

#[derive(Deserialize)]
struct WebFetch<'a> {
    url: Text,
    #[serde(borrow)]
    prompt: Option<&'a RawValue>,
}

/// The input `raw` of a call of the tool `name`, in the record `line`.
pub(super) fn input(name: &str, raw: &RawValue, line: &Line) -> ToolInput {
    let text = |text: Option<Text>| text.map(|Text(text)| text);
    match kind(name) {
        ToolKind::Shell => typed(raw).map(|input: Bash| ToolInput::Shell {
            command: input.command.0,
            description: text(input.description),
        }),
        ToolKind::Read => typed(raw).map(|input: Read| ToolInput::Read {
            path: input.file_path.0,
            first_line: input.offset,
            lines: input.limit,
        }),
        ToolKind::Edit => typed(raw).map(|input: Edit| ToolInput::Edit {
            path: input.file_path.0,
            old: line.text(input.old_string),
            new: line.text(input.new_string),
            all: input.replace_all.unwrap_or(false),
        }),
        ToolKind::Write => typed(raw).map(|input: Write| ToolInput::Write {
            path: input.file_path.0,
            content: line.text(input.content),
        }),
        ToolKind::ContentSearch => typed(raw).map(|input: Search| ToolInput::ContentSearch {
            pattern: input.pattern.0,
            path: text(input.path),
        }),
        ToolKind::FileSearch => typed(raw).map(|input: Search| ToolInput::FileSearch {
            pattern: input.pattern.0,
            path: text(input.path),
        }),
        ToolKind::Todos => typed(raw).map(|input: TodoWrite| {
            ToolInput::Todos(
                input
                    .todos
                    .into_iter()
                    .map(|todo| Todo {
                        text: todo.content.0,
                        state: match todo.status.as_str() {
                            "completed" => TodoState::Done,
                            "in_progress" => TodoState::InProgress,
                            _ => TodoState::Open,
                        },
                    })
                    .collect(),
            )
        }),
        ToolKind::Subagent => typed(raw).map(|input: Agent| ToolInput::Subagent {
            kind: text(input.subagent_type),
            description: text(input.description),
            prompt: line.text(input.prompt),
        }),
        ToolKind::WebSearch => typed(raw).map(|input: WebSearch| ToolInput::WebSearch {
            query: input.query.0,
        }),
        ToolKind::WebFetch => typed(raw).map(|input: WebFetch| ToolInput::WebFetch {
            url: input.url.0,
            prompt: input.prompt.and_then(string),
        }),
        /* `mcp__<server>__<tool>`. */
        ToolKind::Mcp => name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
            .map(|(server, tool)| ToolInput::Mcp {
                server: server.to_string(),
                tool: tool.to_string(),
            }),
        ToolKind::Other => None,
    }
    .unwrap_or(ToolInput::Other)
}

/**
 * The short form of the result of a call of a tool of the kind `kind`, where the record has
 * one. `text` is the result's first text, and `detail` its `toolUseResult`.
 *
 * - A command's exit status is only in the text of a failed one, which starts `Exit code <n>`.
 * - The rest is read from `toolUseResult`: the lines a `Read` returned, the `+` and `-` lines
 *   of an `Edit`'s patch, whether a `Write` made the file and the lines of what it wrote, the
 *   matches of a `Grep` or the files of a `Glob`, and what a finished subagent's run came to.
 */
pub(super) fn brief(
    kind: ToolKind,
    text: Option<&RawValue>,
    detail: Option<&RawValue>,
) -> Option<Brief> {
    match kind {
        ToolKind::Shell => {
            let status = text?.get().strip_prefix("\"Exit code ")?;
            let digits = status.len()
                - status
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .len();
            status[..digits].parse().ok().map(Brief::Exit)
        }
        ToolKind::Read => typed(detail?).map(|read: ReadResult| Brief::Lines(read.file.num_lines)),
        ToolKind::Edit => typed(detail?).map(|edit: EditResult| {
            let starting = |mark| {
                edit.structured_patch
                    .iter()
                    .flat_map(|hunk| &hunk.lines)
                    .filter(|line| line.get().as_bytes().get(1) == Some(&mark))
                    .count() as u64
            };
            Brief::Changed {
                added: starting(b'+'),
                removed: starting(b'-'),
            }
        }),
        ToolKind::Write => typed(detail?).map(|written: WriteResult| Brief::Written {
            new: written.kind == "create",
            lines: string(written.content).map_or(0, |content| content.lines().count() as u64),
        }),
        ToolKind::ContentSearch | ToolKind::FileSearch => {
            let found: SearchResult = typed(detail?)?;
            found.num_matches.or(found.num_files).map(Brief::Matches)
        }
        ToolKind::Subagent => {
            let ran: Launched = typed(detail?)?;
            (ran.resolved_model.is_some() || ran.total_tool_use_count.is_some()).then(|| {
                Brief::Ran {
                    model: ran.resolved_model,
                    calls: ran.total_tool_use_count,
                    duration: ran.total_duration_ms.map(Duration::from_millis),
                }
            })
        }
        _ => None,
    }
}
