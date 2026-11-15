//! A generated Claude Code `projects/` tree in a temp dir (PLAN §7.3, §12): sessions of chosen
//! sizes with subagents, saved tool results and bookkeeping files, and transcripts written
//! record by record with whatever a test needs in them. Everything in it is made up here;
//! nothing comes from a real transcript.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sb_core::provider::{self, Provider};
use sb_core::transport::LocalTransport;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::Agent;

/// A home folder holding `.claude/projects/`.
pub struct ProjectsTree {
    home: TempDir,
    sessions: usize,
    record_bytes: usize,
}

impl Default for ProjectsTree {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectsTree {
    pub fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".claude/projects")).unwrap();
        Self {
            home,
            sessions: 0,
            record_bytes: 160,
        }
    }

    /// Makes each record [`Session::append`] writes from now on carry about `bytes` bytes of
    /// text.
    pub fn record_bytes(&mut self, bytes: usize) {
        self.record_bytes = bytes;
    }

    /// The location's home folder.
    pub fn home(&self) -> &Path {
        self.home.path()
    }

    /// This tree as a location.
    pub fn transport(&self) -> LocalTransport {
        LocalTransport::with_env(self.home().to_str().unwrap(), [])
    }

    /// The file an item's key names.
    pub fn path(&self, key: &str) -> PathBuf {
        self.home.path().join(".claude").join(key)
    }

    /// Writes the agent's `settings.json`.
    pub fn write_settings(&self, json: &str) {
        fs::write(self.path("settings.json"), json).unwrap();
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
        let root = self.home.path().join(".claude");
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
        let key = format!("projects/{}/memory/{name}.md", project_folder(project));
        write(&self.path(&key), text.as_bytes());
        key
    }
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
    /// Appends `records` records to the transcript, as the agent does while the session runs.
    pub fn append(&mut self, records: usize) {
        self.transcript.filler(records, self.record_bytes);
    }

    /// A subagent's transcript of `records` records, with its metadata file. Returns the
    /// transcript's key.
    pub fn subagent(&mut self, records: usize) -> String {
        let call = format!("toolu_{:024x}", self.extras + 1);
        let (key, mut transcript) = self.subagent_transcript(
            "",
            Some(json!({
                "agentType": "general-purpose",
                "description": "Look something up",
                "toolUseId": call,
            })),
        );
        transcript.filler(records, self.record_bytes);
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
    /// The uuid of the conversation record the next one follows.
    parent: Option<String>,
    /// The filler's call that its next record answers.
    unanswered: Option<Call>,
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
            parent: None,
            unanswered: None,
        }
    }

    /// Makes the next record follow the record `uuid` and not the last one written: the
    /// conversation is rewound to there, or the next record answers one of several calls made
    /// at once.
    pub fn follow(&mut self, uuid: &str) {
        self.parent = Some(uuid.to_string());
    }

    /// Appends `line` and a newline as they are.
    pub fn raw(&mut self, line: &str) {
        let mut file = File::options().append(true).open(&self.path).unwrap();
        writeln!(file, "{line}").unwrap();
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

    /// When the next record is written: a second after the one before it.
    fn time(&self) -> String {
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

    /// `records` records of filler, each with about `bytes` bytes of text: a prompt, a reply
    /// that calls a tool, the tool's result, and a bookkeeping record, over and over.
    fn filler(&mut self, records: usize, bytes: usize) {
        for _ in 0..records {
            let index = self.written;
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
                        uuid: self.assistant(index, MODEL, blocks, json!({})),
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

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
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
        Self {
            tree: ProjectsTree::new(),
            sessions: HashMap::new(),
        }
    }

    pub fn tree(&mut self) -> &mut ProjectsTree {
        &mut self.tree
    }

    /// The session whose transcript is `key`.
    pub fn session(&mut self, key: &str) -> &mut Session {
        self.sessions.get_mut(key).unwrap()
    }

    /// Writes `text` at the end of the transcript `key` as it is: with no newline, the start
    /// of a record the agent has not finished writing.
    pub fn append_raw(&self, key: &str, text: &str) {
        File::options()
            .append(true)
            .open(self.tree.path(key))
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
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
            .find(|provider| provider.id() == "claude-code")
            .unwrap()
    }

    fn transport(&self) -> LocalTransport {
        self.tree.transport()
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
        fs::write(self.tree.path(key), lines.concat()).unwrap();
        self.session(key).append(1);
    }

    fn truncate(&mut self, key: &str, records: usize) {
        let lines = self.lines(key);
        fs::write(self.tree.path(key), lines[..lines.len() - records].concat()).unwrap();
    }

    fn touch(&mut self, key: &str) {
        let file = File::options()
            .write(true)
            .open(self.tree.path(key))
            .unwrap();
        let modified = file.metadata().unwrap().modified().unwrap();
        file.set_modified(modified + Duration::from_secs(1))
            .unwrap();
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
}
