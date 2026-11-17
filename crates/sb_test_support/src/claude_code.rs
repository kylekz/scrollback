//! A generated Claude Code `projects/` tree in a temp dir (PLAN §7.3, §12): sessions of chosen
//! sizes with subagents, saved tool results and bookkeeping files, and transcripts written
//! record by record with whatever a test needs in them. Everything in it is made up here;
//! nothing comes from a real transcript.
//!
//! Every file is given a modification time well in the past, a second later at each write, as
//! if the agent had written it a while ago: sync does not trust a file modified in the last
//! moments before it is scanned, which a file a test has just written would be.
//! [`ProjectsTree::modified_ago`] makes a file one of those.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sb_core::provider::{self, Provider};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::{Agent, HomeTransport};

/// A home folder holding the agent's root with its `projects/`.
pub struct ProjectsTree {
    home: PathBuf,
    /// The temp dir the home folder is, when the tree made its own.
    _temp: Option<TempDir>,
    root: PathBuf,
    /// The location's environment: the variable that says where the root is, when it is not
    /// where the agent puts it by default.
    vars: Vec<(String, String)>,
    sessions: usize,
    record_bytes: usize,
}

impl Default for ProjectsTree {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectsTree {
    /// A tree in the agent's default place, `.claude` under the home folder.
    pub fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".claude");
        Self::at(home.path().to_path_buf(), Some(home), root, Vec::new())
    }

    /// A tree somewhere else under the home folder, which the location's environment points
    /// the agent at.
    pub fn in_config_dir() -> Self {
        let home = tempfile::tempdir().unwrap();
        Self::configured(home.path().to_path_buf(), Some(home))
    }

    /// [`ProjectsTree::in_config_dir`] with `home` as the home folder, which stays when the
    /// tree is dropped: for a process started with [`ProjectsTree::vars`] to find.
    pub fn in_home(home: &Path) -> Self {
        Self::configured(home.to_path_buf(), None)
    }

    fn configured(home: PathBuf, temp: Option<TempDir>) -> Self {
        let root = home.join("elsewhere/agent");
        let vars = vec![(
            "CLAUDE_CONFIG_DIR".to_string(),
            root.to_str().unwrap().to_string(),
        )];
        Self::at(home, temp, root, vars)
    }

    fn at(
        home: PathBuf,
        temp: Option<TempDir>,
        root: PathBuf,
        vars: Vec<(String, String)>,
    ) -> Self {
        fs::create_dir_all(root.join("projects")).unwrap();
        Self {
            home,
            _temp: temp,
            root,
            vars,
            sessions: 0,
            record_bytes: 160,
        }
    }

    /// The location's environment: what points the agent, and its provider, at the tree.
    pub fn vars(&self) -> &[(String, String)] {
        &self.vars
    }

    /// Makes each record [`Session::append`] writes from now on carry about `bytes` bytes of
    /// text.
    pub fn record_bytes(&mut self, bytes: usize) {
        self.record_bytes = bytes;
    }

    /// The location's home folder.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// This tree as a location.
    pub fn transport(&self) -> HomeTransport {
        HomeTransport::new(self.home(), self.vars.clone())
    }

    /// The file an item's key names.
    pub fn path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    /// Writes the agent's `settings.json`.
    pub fn write_settings(&self, json: &str) {
        write(&self.path("settings.json"), json.as_bytes());
    }

    /// A new session started in `project` (a path such as `/home/user/code/app`), with a
    /// transcript of `records` records.
    pub fn session(&mut self, project: &str, records: usize) -> Session {
        self.sessions += 1;
        let id = format!("00000000-0000-4000-8000-{:012x}", self.sessions);
        self.session_with_id(project, id, records)
    }

    fn session_with_id(&self, project: &str, id: String, records: usize) -> Session {
        let folder = project_folder(project);
        let root = self.root.clone();
        let key = format!("projects/{folder}/{id}.jsonl");
        let mut session = Session {
            dir: format!("projects/{folder}/{id}"),
            transcript: Records::new(root.join(&key), &id, project, None, 0x1000_0000),
            root,
            key,
            id,
            record_bytes: self.record_bytes,
            extras: 0,
        };
        fs::create_dir_all(session.root.join(&session.dir)).unwrap();
        session.append(records);
        session
    }

    /// A memory file of the project at `project`. Returns its key.
    pub fn memory(&self, project: &str, name: &str, text: &str) -> String {
        self.project_file(project, &format!("memory/{name}.md"), text.as_bytes())
    }

    /// The file `path` in the folder of the project at `project`, holding `bytes`: any file
    /// the agent keeps there, whatever it is. Returns its key.
    pub fn project_file(&self, project: &str, path: &str, bytes: &[u8]) -> String {
        let key = format!("projects/{}/{path}", project_folder(project));
        write(&self.path(&key), bytes);
        key
    }

    /// Gives the file at `key` the modification time `modified`.
    pub fn set_modified(&self, key: &str, modified: SystemTime) {
        set_modified(&self.path(key), modified);
    }

    /// Gives the file at `key` a modification time `ago` before now: for a short `ago`, the
    /// time of a file the agent has just written.
    pub fn modified_ago(&self, key: &str, ago: Duration) {
        self.set_modified(key, SystemTime::now() - ago);
    }

    /// Writes the session `outline` describes: its prompt, one reply with its tool calls, and
    /// its title. Returns the provider's id for the session.
    pub fn outlined(&mut self, outline: &Outline) -> String {
        let mut session = self.session(outline.project, 0);
        let (year, month, day) = outline.day;
        let at = |hour: u32| format!("{year:04}-{month:02}-{day:02}T{hour:02}:00:00.000Z");
        session.transcript.user(
            json!(outline.prompt),
            json!({ "origin": { "kind": "human" }, "timestamp": at(11) }),
        );
        let blocks: Vec<Value> = std::iter::once(json!({ "type": "text", "text": "On it." }))
            .chain((0..outline.calls).map(|call| {
                json!({
                    "type": "tool_use",
                    "id": format!("toolu_{}_{call}", session.id),
                    "name": "Bash",
                    "input": { "command": "ls" },
                })
            }))
            .collect();
        session.transcript.assistant(
            1,
            outline.model,
            json!(blocks),
            json!({ "timestamp": at(12) }),
        );
        session.transcript.title(outline.title);
        session.session_key().to_string()
    }
}

/// A session as the library shows one: where it started, what it is called and asked, the
/// model that answered, the day it ran, and how many tool calls its reply made.
pub struct Outline<'a> {
    pub project: &'a str,
    pub title: &'a str,
    pub prompt: &'a str,
    pub model: &'a str,
    /// Year, month and day. The session runs from 11:00 to 12:00 UTC, so the day is the same
    /// in most of the world's time zones.
    pub day: (i32, u32, u32),
    pub calls: u32,
}

/// One session in a [`ProjectsTree`].
pub struct Session {
    root: PathBuf,
    /// The key prefix of the session's own folder.
    dir: String,
    /// The transcript's key.
    pub key: String,
    pub id: String,
    /// The session's main transcript.
    pub transcript: Records,
    record_bytes: usize,
    extras: usize,
}

impl Session {
    /// The provider's id for the session: its project's folder and its own id.
    pub fn session_key(&self) -> &str {
        self.dir
            .strip_prefix("projects/")
            .expect("a session's folder is under projects/")
    }

    /// Appends `records` records to the transcript, as the agent does while the session runs.
    pub fn append(&mut self, records: usize) {
        self.transcript.filler(records, self.record_bytes);
    }

    /**
     * A subagent's transcript of `records` records, with its metadata file, and in the
     * session's transcript the call that started it and the call's result, as a real session
     * has them. Returns the subagent transcript's key.
     */
    pub fn subagent(&mut self, records: usize) -> String {
        let call = self.transcript.call(
            "Agent",
            json!({
                "subagent_type": "general-purpose",
                "description": "Look something up",
                "prompt": "Look it up.",
            }),
        );
        let (key, mut transcript) = self.subagent_transcript(
            "",
            Some(json!({
                "agentType": "general-purpose",
                "description": "Look something up",
                "toolUseId": call.id,
            })),
        );
        transcript.filler(records, self.record_bytes);
        let started = json!({ "agentId": transcript.agent, "status": "completed" });
        self.transcript
            .result_with(&call, json!("Looked up."), false, started);
        key
    }

    /// The file `path` in the session's own folder, holding `bytes`: any file the agent keeps
    /// there, whatever it is. Returns its key.
    pub fn file(&self, path: &str, bytes: &[u8]) -> String {
        let key = format!("{}/{path}", self.dir);
        write(&self.root.join(&key), bytes);
        key
    }

    /// The transcript of the subagent that the tool call `call` started, with a metadata file
    /// that names the call.
    pub fn subagent_of(&mut self, call: &Call, kind: &str, description: &str) -> Records {
        self.subagent_transcript(
            "",
            Some(json!({
                "agentType": kind,
                "description": description,
                "toolUseId": call.id,
            })),
        )
        .1
    }

    /// The transcript of a subagent with no metadata file: only the result of the call that
    /// started it, which carries the subagent's id ([`Records::agent`]), ties them.
    pub fn subagent_without_meta(&mut self) -> Records {
        self.subagent_transcript("", None).1
    }

    /// The transcript of a subagent that the workflow run `run` started. Its metadata names no
    /// call.
    pub fn workflow_subagent(&mut self, run: &str, kind: &str, description: &str) -> Records {
        self.subagent_transcript(
            &format!("workflows/{run}/"),
            Some(json!({ "agentType": kind, "description": description })),
        )
        .1
    }

    /// A new subagent's transcript in `folder` under `subagents/`, with the metadata `meta`.
    fn subagent_transcript(&mut self, folder: &str, meta: Option<Value>) -> (String, Records) {
        self.extras += 1;
        let agent = format!("a{:016x}", self.extras);
        let stem = format!("{}/subagents/{folder}agent-{agent}", self.dir);
        let key = format!("{stem}.jsonl");
        if let Some(meta) = meta {
            write(
                &self.root.join(format!("{stem}.meta.json")),
                meta.to_string().as_bytes(),
            );
        }
        let records = Records::new(
            self.root.join(&key),
            &self.id,
            &self.transcript.project,
            Some(agent),
            0x2000_0000 + self.extras,
        );
        (key, records)
    }

    /// Tool output of `bytes` bytes saved outside the transcript. Returns its key.
    pub fn tool_result(&mut self, bytes: usize) -> String {
        self.extras += 1;
        let key = format!("{}/tool-results/toolu_{:024x}.txt", self.dir, self.extras);
        write(&self.root.join(&key), filler(self.extras, bytes).as_bytes());
        key
    }

    /// Saves `output` outside the transcript as `tool-results/<name>`, as the agent does with
    /// output too large for it. Returns the file's key and the note the transcript holds in
    /// the output's place, which is what the tool's result is given as.
    pub fn saved_output(&self, name: &str, output: &[u8]) -> (String, String) {
        let key = format!("{}/tool-results/{name}", self.dir);
        write(&self.root.join(&key), output);
        let note = format!(
            "<persisted-output>\nOutput too large ({} bytes). Full output saved to: {}\n\n\
             Preview (first 16 bytes):\n{}\n</persisted-output>",
            output.len(),
            self.root.join(&key).display(),
            String::from_utf8_lossy(&output[..output.len().min(16)]),
        );
        (key, note)
    }

    /// The session's `custom-title.json`. Returns its key.
    pub fn title(&self, title: &str) -> String {
        let key = format!("{}/custom-title.json", self.dir);
        write(
            &self.root.join(&key),
            json!({ "customTitle": title }).to_string().as_bytes(),
        );
        key
    }
}

/// What [`with_subagents`] wrote: the keys of the session's items, for a test to find them by.
pub struct WithSubagents {
    /// The session's own transcript.
    pub key: String,
    /// The subagent the session's first call started, the subagent that one started, and the
    /// subagent no call is known to have started.
    pub subagent: String,
    pub nested: String,
    pub lone: String,
}

/**
 * A session in `project` called `title`, whose subagents hold words to find. Its first turn asks
 * a subagent, which answers "A widget holds a frobnicator." and asks another, which answers
 * "Deep inside sits a gizmotron."; then come `turns` turns that say nothing of note, and a last
 * one that asks "Where is the zanzibar setting?" and is answered "It sits in the quokka file.".
 * One subagent no call is known to have started answers "The quuxifier hums.".
 */
pub fn with_subagents(
    tree: &mut ProjectsTree,
    project: &str,
    title: &str,
    turns: usize,
) -> WithSubagents {
    let mut session = tree.session(project, 0);
    let ask = |description: &str| json!({ "subagent_type": "expert", "description": description, "prompt": "Explain." });
    session.transcript.prompt("Ask the widget expert.");
    let call = session.transcript.call("Agent", ask("Ask"));
    let (subagent, mut first) = session.subagent_transcript(
        "",
        Some(json!({ "agentType": "expert", "description": "Ask", "toolUseId": call.id })),
    );
    first.prompt("Explain widgets.");
    first.reply("A widget holds a frobnicator.");
    let inner = first.call("Agent", ask("Look deeper"));
    let (nested, mut second) = session.subagent_transcript(
        "",
        Some(json!({ "agentType": "expert", "description": "Look deeper", "toolUseId": inner.id })),
    );
    second.prompt("Look deeper.");
    second.reply("Deep inside sits a gizmotron.");
    let started = |records: &Records| json!({ "agentId": records.agent, "status": "completed" });
    first.result_with(&inner, json!("Looked."), false, started(&second));
    first.reply("Widgets explained.");
    session
        .transcript
        .result_with(&call, json!("Explained."), false, started(&first));
    session.transcript.reply("The expert explained widgets.");
    for turn in 1..=turns {
        session
            .transcript
            .prompt(&format!("Carry on with step {turn}."));
        session.transcript.reply(&format!("Step {turn} is done."));
    }
    session.transcript.prompt("Where is the zanzibar setting?");
    session.transcript.reply("It sits in the quokka file.");
    let (lone, mut alone) = session.subagent_transcript("", None);
    alone.prompt("Nobody asked.");
    alone.reply("The quuxifier hums.");
    session.title(title);
    WithSubagents {
        key: session.key,
        subagent,
        nested,
        lone,
    }
}

/// What [`nested_subagents`] wrote: the keys of the session's items.
pub struct NestedSubagents {
    /// The session's own transcript.
    pub key: String,
    /// The subagents from the one the session's call started down to the innermost, each
    /// started by the one before.
    pub chain: Vec<String>,
    /// The subagents the outermost started besides the next of the chain.
    pub siblings: Vec<String>,
}

/**
 * A session in `project` called `title` whose call starts a chain of `depth` subagents, each
 * started by the one before; the outermost also starts `siblings` subagents of its own, before
 * the next of the chain. The innermost answers "The needle is a quillwort.", each sibling
 * "Leaf n.".
 */
pub fn nested_subagents(
    tree: &mut ProjectsTree,
    project: &str,
    title: &str,
    depth: usize,
    siblings: usize,
) -> NestedSubagents {
    let mut session = tree.session(project, 0);
    let ask = |description: &str| json!({ "subagent_type": "expert", "description": description, "prompt": "Go." });
    let meta = |call: &Call| {
        Some(json!({ "agentType": "expert", "description": "Go", "toolUseId": call.id }))
    };
    let started = |records: &Records| json!({ "agentId": records.agent, "status": "completed" });
    session.transcript.prompt("Go down the chain.");
    let mut call = session.transcript.call("Agent", ask("Level 1"));
    let mut chain: Vec<(String, Records, Call)> = Vec::new();
    let mut leaves = Vec::new();
    for level in 1..=depth {
        let (key, mut records) = session.subagent_transcript("", meta(&call));
        records.prompt(&format!("Level {level}."));
        if level == 1 {
            for leaf in 0..siblings {
                let asked = records.call("Agent", ask(&format!("Leaf {leaf}")));
                let (key, mut sibling) = session.subagent_transcript("", meta(&asked));
                sibling.prompt("Look.");
                sibling.reply(&format!("Leaf {leaf}."));
                records.result_with(&asked, json!("Looked."), false, started(&sibling));
                leaves.push(key);
            }
        }
        if level == depth {
            records.reply("The needle is a quillwort.");
        } else {
            let next = records.call("Agent", ask(&format!("Level {}", level + 1)));
            chain.push((key, records, call));
            call = next;
            continue;
        }
        chain.push((key, records, call.clone()));
    }
    /* Each call's result names the subagent it started, from the innermost out. */
    for at in (1..chain.len()).rev() {
        let inner = started(&chain[at].1);
        let starting = chain[at].2.clone();
        chain[at - 1]
            .1
            .result_with(&starting, json!("Done."), false, inner);
    }
    if let Some((_, outermost, first)) = chain.first() {
        let outer = started(outermost);
        session
            .transcript
            .result_with(first, json!("Done."), false, outer);
    }
    session.transcript.reply("The chain is done.");
    session.title(title);
    NestedSubagents {
        key: session.key,
        chain: chain.into_iter().map(|(key, ..)| key).collect(),
        siblings: leaves,
    }
}

/// A tool call written to a transcript.
#[derive(Clone, Debug)]
pub struct Call {
    /// The call's `tool_use` id.
    pub id: String,
    /// The uuid of the record that holds it.
    pub uuid: String,
}

/// An image as a content block of a prompt or of a tool's result.
pub fn image_block(media_type: &str, bytes: &[u8]) -> Value {
    json!({
        "type": "image",
        "source": { "type": "base64", "media_type": media_type, "data": STANDARD.encode(bytes) },
    })
}

/// A transcript being written a record at a time, as the agent appends to it: a session's main
/// transcript or a subagent's. Each record follows the one written before it unless
/// [`Records::follow`] says otherwise.
pub struct Records {
    path: PathBuf,
    session: String,
    project: String,
    /// The subagent's id, in a subagent's transcript.
    pub agent: Option<String>,
    /// What the ids of this transcript's records and calls start with, so that no two
    /// transcripts of a session share one.
    prefix: usize,
    /// How many records were written. Ids and times count from it.
    written: usize,
    /// How many of them were filler.
    filled: usize,
    /// The uuid of the conversation record the next one follows.
    parent: Option<String>,
    /// The filler's call that its next record answers.
    unanswered: Option<Call>,
    /// The time the records written from now on carry, when one is set ([`Records::set_time`]).
    clock: Option<String>,
    /// What the conversation's records written from now on carry in place of the usual
    /// ([`Records::carry`]).
    carried: Vec<(String, Value)>,
}

impl Records {
    fn new(
        path: PathBuf,
        session: &str,
        project: &str,
        agent: Option<String>,
        prefix: usize,
    ) -> Self {
        write(&path, b"");
        Self {
            path,
            session: session.to_string(),
            project: project.to_string(),
            agent,
            prefix,
            written: 0,
            filled: 0,
            parent: None,
            unanswered: None,
            clock: None,
            carried: Vec::new(),
        }
    }

    /// Makes the conversation's records written from now on carry `value` as their `field`
    /// (`gitBranch`, `cwd`), in place of what they otherwise do.
    pub fn carry(&mut self, field: &str, value: Value) {
        self.carried.retain(|(name, _)| name != field);
        self.carried.push((field.to_string(), value));
    }

    /// Makes the records written from now on carry the time `timestamp` (`2026-11-09T09:12:00Z`),
    /// in place of one a second after the record before.
    pub fn set_time(&mut self, timestamp: &str) {
        self.clock = Some(timestamp.to_string());
    }

    /// Makes the next record follow the record `uuid` and not the last one written: the
    /// conversation is rewound to there, or the next record answers one of several calls made
    /// at once.
    pub fn follow(&mut self, uuid: &str) {
        self.parent = Some(uuid.to_string());
    }

    /// Appends `line` and a newline as they are.
    pub fn raw(&mut self, line: &str) {
        append(&self.path, format!("{line}\n").as_bytes());
        self.written += 1;
    }

    /// A record outside the conversation: `fields` and the session's id, with no uuid.
    pub fn bookkeeping(&mut self, kind: &str, mut fields: Value) {
        let record = fields.as_object_mut().unwrap();
        record.insert("type".to_string(), json!(kind));
        record.insert("sessionId".to_string(), json!(self.session));
        self.raw(&fields.to_string());
    }

    /// A record of the conversation of type `kind`: `fields` and what every such record
    /// carries. Returns its uuid.
    pub fn record(&mut self, kind: &str, mut fields: Value) -> String {
        let uuid = format!(
            "{:08x}-0000-4000-8000-{:012x}",
            self.prefix,
            self.written + 1
        );
        let record = fields.as_object_mut().unwrap();
        for (name, value) in &self.carried {
            record.entry(name).or_insert(value.clone());
        }
        for (name, value) in [
            ("type", json!(kind)),
            ("uuid", json!(uuid)),
            ("parentUuid", json!(self.parent)),
            ("timestamp", json!(self.time())),
            ("sessionId", json!(self.session)),
            ("cwd", json!(self.project)),
            ("gitBranch", json!("main")),
            ("version", json!("2.1.0")),
            ("entrypoint", json!("cli")),
            ("isSidechain", json!(self.agent.is_some())),
        ] {
            record.entry(name).or_insert(value);
        }
        if let Some(agent) = &self.agent {
            record.insert("agentId".to_string(), json!(agent));
        }
        self.raw(&fields.to_string());
        self.parent = Some(uuid.clone());
        uuid
    }

    /// When the next record is written: the time set, else a second after the one before it.
    fn time(&self) -> String {
        if let Some(clock) = &self.clock {
            return clock.clone();
        }
        format!(
            "2026-01-01T{:02}:{:02}:{:02}.000Z",
            self.written / 3600 % 24,
            self.written / 60 % 60,
            self.written % 60
        )
    }

    /// A `user` record whose content is `content`: text, or a list of blocks.
    pub fn user(&mut self, content: Value, mut fields: Value) -> String {
        fields["message"] = json!({ "role": "user", "content": content });
        self.record("user", fields)
    }

    /// An `assistant` record: content blocks of the reply `message`, by `model`. The agent
    /// writes most replies as a record per block, all with the reply's id and usage.
    pub fn assistant(
        &mut self,
        message: usize,
        model: &str,
        blocks: Value,
        mut fields: Value,
    ) -> String {
        fields["message"] = json!({
            "id": format!("msg_{message:024x}"),
            "role": "assistant",
            "model": model,
            "content": blocks,
            "usage": { "input_tokens": 100, "output_tokens": 20,
                       "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 10 },
        });
        self.record("assistant", fields)
    }

    /// What the user typed.
    pub fn prompt(&mut self, text: &str) -> String {
        self.user(json!(text), json!({ "origin": { "kind": "human" } }))
    }

    /// The agent's answer.
    pub fn reply(&mut self, text: &str) -> String {
        self.reply_by(MODEL, text)
    }

    pub fn reply_by(&mut self, model: &str, text: &str) -> String {
        let message = self.written;
        self.assistant(
            message,
            model,
            json!([{ "type": "text", "text": text }]),
            json!({}),
        )
    }

    pub fn thinking(&mut self, text: &str, millis: u64) -> String {
        let message = self.written;
        self.assistant(
            message,
            MODEL,
            json!([{ "type": "thinking", "thinking": text, "signature": "c2ln" }]),
            json!({ "thinkingDurationMs": millis }),
        )
    }

    /// A call of the tool `name`.
    pub fn call(&mut self, name: &str, input: Value) -> Call {
        let message = self.written;
        self.calls(message, &[(name, input)]).remove(0)
    }

    /// Calls made at once: one reply, `message`, written as a record per call, each following
    /// the one before. Their results follow their own calls ([`Records::follow`]).
    pub fn calls(&mut self, message: usize, calls: &[(&str, Value)]) -> Vec<Call> {
        calls
            .iter()
            .map(|(name, input)| {
                let id = self.call_id();
                let block = json!({ "type": "tool_use", "id": id, "name": name, "input": input });
                Call {
                    uuid: self.assistant(message, MODEL, json!([block]), json!({})),
                    id,
                }
            })
            .collect()
    }

    /// The `tool_use` id of a call in the next record.
    fn call_id(&self) -> String {
        format!("toolu_{:08x}{:016x}", self.prefix, self.written)
    }

    /// The result of `call`: text, or a list of blocks.
    pub fn result(&mut self, call: &Call, content: Value) -> String {
        self.result_with(call, content, false, json!({}))
    }

    /// The result of a call that failed.
    pub fn failure(&mut self, call: &Call, text: &str) -> String {
        self.result_with(call, json!(text), true, json!({}))
    }

    /// The result of `call`, with the agent's structured copy of it as `toolUseResult`.
    pub fn result_with(
        &mut self,
        call: &Call,
        content: Value,
        failed: bool,
        tool_use_result: Value,
    ) -> String {
        self.user(
            json!([{
                "type": "tool_result",
                "tool_use_id": call.id,
                "content": content,
                "is_error": failed,
            }]),
            json!({ "toolUseResult": tool_use_result }),
        )
    }

    /// A compaction: the boundary, which starts the tree anew and names the record it follows,
    /// and the summary the agent goes on with.
    pub fn compact(&mut self, summary: &str, before: u64, after: u64) {
        let last = self.parent.take();
        self.record(
            "system",
            json!({
                "subtype": "compact_boundary",
                "content": "Conversation compacted",
                "logicalParentUuid": last,
                "compactMetadata": { "trigger": "auto", "preTokens": before, "postTokens": after },
            }),
        );
        self.user(json!(summary), json!({ "isCompactSummary": true }));
    }

    /// A `system` record that says `text`.
    pub fn system(&mut self, subtype: &str, text: &str) -> String {
        self.record(
            "system",
            json!({ "subtype": subtype, "content": text, "level": "info" }),
        )
    }

    /// The session's title, as a record.
    pub fn title(&mut self, title: &str) {
        self.bookkeeping("custom-title", json!({ "customTitle": title }));
    }

    /**
     * A subfolder's memory file loaded as the agent started working in that subfolder: the
     * file at `path`, shown as `shown`, whose text was `text`. `differs` says the text loaded
     * isn't the file's text on disk. Returns its uuid.
     */
    pub fn nested_memory(&mut self, path: &str, shown: &str, text: &str, differs: bool) -> String {
        self.record(
            "attachment",
            json!({ "attachment": {
                "type": "nested_memory",
                "path": path,
                "displayPath": shown,
                "content": {
                    "path": path,
                    "type": "Project",
                    "content": text,
                    "contentDiffersFromDisk": differs,
                },
            } }),
        )
    }

    /// `records` records of filler, each with about `bytes` bytes of text: a prompt, a reply
    /// that calls a tool, the tool's result, and a bookkeeping record, over and over.
    fn filler(&mut self, records: usize, bytes: usize) {
        for _ in 0..records {
            let index = self.filled;
            self.filled += 1;
            let text = filler(index, bytes);
            match index % 4 {
                0 => {
                    self.user(json!(text), json!({}));
                }
                1 => {
                    let id = self.call_id();
                    let blocks = json!([
                        { "type": "text", "text": text },
                        { "type": "tool_use", "id": id, "name": "Bash",
                          "input": { "command": "ls" } },
                    ]);
                    self.unanswered = Some(Call {
                        uuid: self.assistant(self.written, MODEL, blocks, json!({})),
                        id,
                    });
                }
                2 => {
                    let call = self.unanswered.take().unwrap();
                    self.result(&call, json!(text));
                }
                _ => self.bookkeeping(
                    "file-history-snapshot",
                    json!({ "messageId": index, "snapshot": { "note": text } }),
                ),
            }
        }
    }
}

/// The model the generated replies name.
const MODEL: &str = "model-1";

/// The folder Claude Code keeps a project's sessions in: the path the sessions started in,
/// with its punctuation replaced by `-`.
fn project_folder(project: &str) -> String {
    project
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Replaces the file at `path` with `bytes`, as written a second after it was last written.
fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let modified = next_modified(path);
    fs::write(path, bytes).unwrap();
    set_modified(path, modified);
}

/// Adds `bytes` to the end of the file at `path`, as written a second after it was last
/// written.
fn append(path: &Path, bytes: &[u8]) {
    let modified = next_modified(path);
    File::options()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
    set_modified(path, modified);
}

/// The time the next write to `path` is given: a second after its last, or an hour ago for a
/// new file.
fn next_modified(path: &Path) -> SystemTime {
    match fs::metadata(path) {
        Ok(metadata) => metadata.modified().unwrap() + Duration::from_secs(1),
        Err(_) => SystemTime::now() - Duration::from_secs(3600),
    }
}

fn set_modified(path: &Path, modified: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

/// `bytes` bytes of words, different for each `seed`.
fn filler(seed: usize, bytes: usize) -> String {
    const WORDS: [&str; 8] = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    ];
    let mut text = String::with_capacity(bytes + 8);
    let mut at = seed;
    while text.len() < bytes {
        text.push_str(WORDS[at % WORDS.len()]);
        text.push(' ');
        at = at.wrapping_mul(31).wrapping_add(7);
    }
    text.truncate(bytes);
    text
}

/// Where [`FileAgent`]'s sessions start.
const PROJECT: &str = "/home/user/code/app";

/// A [`ProjectsTree`] behind [`Agent`].
pub struct FileAgent {
    tree: ProjectsTree,
    sessions: HashMap<String, Session>,
}

impl Default for FileAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl FileAgent {
    pub fn new() -> Self {
        Self::on(ProjectsTree::new())
    }

    /// The agent whose data is `tree`.
    pub fn on(tree: ProjectsTree) -> Self {
        Self {
            tree,
            sessions: HashMap::new(),
        }
    }

    pub fn tree(&self) -> &ProjectsTree {
        &self.tree
    }

    pub fn tree_mut(&mut self) -> &mut ProjectsTree {
        &mut self.tree
    }

    /// The session whose transcript is `key`.
    pub fn session(&mut self, key: &str) -> &mut Session {
        self.sessions.get_mut(key).unwrap()
    }

    /// Writes `text` at the end of the file `key` as it is: without a newline, the start of a
    /// record the agent has not finished writing.
    pub fn append_raw(&self, key: &str, text: &str) {
        append(&self.tree.path(key), text.as_bytes());
    }

    /// Replaces what the file `key` holds with `bytes`, as the agent rewriting it would.
    pub fn overwrite(&self, key: &str, bytes: &[u8]) {
        write(&self.tree.path(key), bytes);
    }

    /// Replaces what the file `key` holds with `bytes` and leaves its modification time as it
    /// was: a change a scan can only see in the bytes.
    pub fn overwrite_unseen(&self, key: &str, bytes: &[u8]) {
        let path = self.tree.path(key);
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, bytes).unwrap();
        set_modified(&path, modified);
    }

    fn lines(&self, key: &str) -> Vec<String> {
        fs::read_to_string(self.tree.path(key))
            .unwrap()
            .split_inclusive('\n')
            .map(str::to_string)
            .collect()
    }
}

impl Agent for FileAgent {
    fn provider(&self) -> Box<dyn Provider> {
        provider::builtin()
            .into_iter()
            .find(|provider| provider.id().as_str() == "claude-code")
            .unwrap()
    }

    fn transport(&self) -> HomeTransport {
        self.tree.transport()
    }

    fn home(&self) -> &Path {
        self.tree.home()
    }

    fn add_session(&mut self, records: usize) -> String {
        let session = self.tree.session(PROJECT, records);
        let key = session.key.clone();
        self.sessions.insert(key.clone(), session);
        key
    }

    fn append(&mut self, key: &str, records: usize) {
        self.session(key).append(records);
    }

    fn rewrite(&mut self, key: &str) {
        let mut lines = self.lines(key);
        let mut first: Value = serde_json::from_str(&lines[0]).unwrap();
        first["message"]["content"] = json!(filler(1, 64));
        lines[0] = first.to_string() + "\n";
        self.overwrite(key, lines.concat().as_bytes());
        self.session(key).append(1);
    }

    fn rewrite_same_size(&mut self, key: &str) {
        let mut bytes = self.content(key);
        /* Inside the first record's text, where any letter will do. */
        let at = bytes.iter().position(|byte| *byte == b' ').unwrap() - 1;
        bytes[at] = if bytes[at] == b'x' { b'y' } else { b'x' };
        self.overwrite(key, &bytes);
    }

    fn truncate(&mut self, key: &str, records: usize) {
        let lines = self.lines(key);
        self.overwrite(key, lines[..lines.len() - records].concat().as_bytes());
    }

    fn touch(&mut self, key: &str) {
        let path = self.tree.path(key);
        set_modified(&path, next_modified(&path));
    }

    fn delete(&mut self, key: &str) {
        fs::remove_file(self.tree.path(key)).unwrap();
        let session = self.sessions.remove(key).unwrap();
        fs::remove_dir_all(session.root.join(session.dir)).unwrap();
    }

    fn recreate(&mut self, key: &str, records: usize) {
        let id = Path::new(key).file_stem().unwrap().to_str().unwrap();
        let session = self.tree.session_with_id(PROJECT, id.to_string(), records);
        assert_eq!(session.key, key);
        self.sessions.insert(session.key.clone(), session);
    }

    fn content(&self, key: &str) -> Vec<u8> {
        fs::read(self.tree.path(key)).unwrap()
    }

    fn exchange(&mut self, key: &str, prompt: &str, command: &str, output: &str, reply: &str) {
        let records = &mut self.session(key).transcript;
        records.prompt(prompt);
        let call = records.call("Bash", json!({ "command": command }));
        records.result(&call, json!(output));
        records.reply(reply);
    }
}
