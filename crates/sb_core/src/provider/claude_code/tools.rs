//! Claude Code's tools as the common kinds (PLAN §7.4), and their inputs as the model types them.

use serde::Deserialize;
use serde_json::value::RawValue;

use super::record::{Line, string, typed};
use crate::transcript::{Todo, TodoState, ToolInput, ToolKind};

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
    command: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct Read {
    file_path: String,
    offset: Option<u64>,
    limit: Option<u64>,
}

#[derive(Deserialize)]
struct Edit<'a> {
    file_path: String,
    #[serde(borrow)]
    old_string: &'a RawValue,
    #[serde(borrow)]
    new_string: &'a RawValue,
    replace_all: Option<bool>,
}

#[derive(Deserialize)]
struct Write<'a> {
    file_path: String,
    #[serde(borrow)]
    content: &'a RawValue,
}

/// `Grep`'s input and `Glob`'s.
#[derive(Deserialize)]
struct Search {
    pattern: String,
    path: Option<String>,
}

#[derive(Deserialize)]
struct TodoWrite {
    todos: Vec<TodoItem>,
}

#[derive(Deserialize)]
struct TodoItem {
    content: String,
    status: String,
}

#[derive(Deserialize)]
struct Agent<'a> {
    subagent_type: Option<String>,
    description: Option<String>,
    #[serde(borrow)]
    prompt: &'a RawValue,
}

#[derive(Deserialize)]
struct WebSearch {
    query: String,
}

#[derive(Deserialize)]
struct WebFetch<'a> {
    url: String,
    #[serde(borrow)]
    prompt: Option<&'a RawValue>,
}

/// The input `raw` of a call of the tool `name`, in the record `line`.
pub(super) fn input(name: &str, raw: &RawValue, line: &Line) -> ToolInput {
    match kind(name) {
        ToolKind::Shell => typed(raw).map(
            |Bash {
                 command,
                 description,
             }| ToolInput::Shell {
                command,
                description,
            },
        ),
        ToolKind::Read => typed(raw).map(|input: Read| ToolInput::Read {
            path: input.file_path,
            first_line: input.offset,
            lines: input.limit,
        }),
        ToolKind::Edit => typed(raw).map(|input: Edit| ToolInput::Edit {
            path: input.file_path,
            old: line.text(input.old_string),
            new: line.text(input.new_string),
            all: input.replace_all.unwrap_or(false),
        }),
        ToolKind::Write => typed(raw).map(|input: Write| ToolInput::Write {
            path: input.file_path,
            content: line.text(input.content),
        }),
        ToolKind::ContentSearch => {
            typed(raw).map(|Search { pattern, path }| ToolInput::ContentSearch { pattern, path })
        }
        ToolKind::FileSearch => {
            typed(raw).map(|Search { pattern, path }| ToolInput::FileSearch { pattern, path })
        }
        ToolKind::Todos => typed(raw).map(|input: TodoWrite| {
            ToolInput::Todos(
                input
                    .todos
                    .into_iter()
                    .map(|todo| Todo {
                        text: todo.content,
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
            kind: input.subagent_type,
            description: input.description,
            prompt: line.text(input.prompt),
        }),
        ToolKind::WebSearch => typed(raw).map(|WebSearch { query }| ToolInput::WebSearch { query }),
        ToolKind::WebFetch => typed(raw).map(|input: WebFetch| ToolInput::WebFetch {
            url: input.url,
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
