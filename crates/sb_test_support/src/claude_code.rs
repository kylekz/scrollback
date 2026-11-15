//! A generated Claude Code `projects/` tree in a temp dir (PLAN §7.3, §12): sessions of chosen
//! sizes with subagents, saved tool results and bookkeeping files. Everything in it is made up
//! here; nothing comes from a real transcript.
//!
//! Every file is given a modification time well in the past, a second later at each write, as
//! if the agent had written it a while ago: sync does not trust a file modified in the last
//! moments before it is scanned, which a file a test has just written would be.
//! [`ProjectsTree::freshen`] makes a file one of those.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sb_core::provider::{self, Provider};
use serde_json::json;
use tempfile::TempDir;

use crate::{Agent, HomeTransport};

/// A home folder holding the agent's root with its `projects/`.
pub struct ProjectsTree {
    home: TempDir,
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
        Self::at(home, root, Vec::new())
    }

    /// A tree somewhere else under the home folder, which the location's environment points
    /// the agent at.
    pub fn in_config_dir() -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("elsewhere/agent");
        let vars = vec![(
            "CLAUDE_CONFIG_DIR".to_string(),
            root.to_str().unwrap().to_string(),
        )];
        Self::at(home, root, vars)
    }

    fn at(home: TempDir, root: PathBuf, vars: Vec<(String, String)>) -> Self {
        fs::create_dir_all(root.join("projects")).unwrap();
        Self {
            home,
            root,
            vars,
            sessions: 0,
            record_bytes: 160,
        }
    }

    /// Makes each record written from now on carry about `bytes` bytes of text.
    pub fn record_bytes(&mut self, bytes: usize) {
        self.record_bytes = bytes;
    }

    /// The location's home folder.
    pub fn home(&self) -> &Path {
        self.home.path()
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
        let mut session = Session {
            root: self.root.clone(),
            dir: format!("projects/{folder}/{id}"),
            key: format!("projects/{folder}/{id}.jsonl"),
            id,
            project: project.to_string(),
            record_bytes: self.record_bytes,
            records: 0,
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

    /// Gives the file at `key` a modification time of now, as a file the agent is writing has.
    pub fn freshen(&self, key: &str) {
        set_modified(&self.path(key), SystemTime::now());
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
    project: String,
    record_bytes: usize,
    records: usize,
    extras: usize,
}

impl Session {
    /// Appends `records` records to the transcript, as the agent does while the session runs.
    pub fn append(&mut self, records: usize) {
        let lines: String = (self.records..self.records + records)
            .map(|n| record(&self.id, &self.project, n, self.record_bytes, false))
            .collect();
        append(&self.root.join(&self.key), lines.as_bytes());
        self.records += records;
    }

    /// A subagent's transcript of `records` records, with its metadata file. Returns the
    /// transcript's key.
    pub fn subagent(&mut self, records: usize) -> String {
        self.extras += 1;
        let agent = format!("subagents/agent-a{:016x}", self.extras);
        let lines: String = (0..records)
            .map(|n| record(&self.id, &self.project, n, self.record_bytes, true))
            .collect();
        self.file(
            &format!("{agent}.meta.json"),
            json!({
                "agentType": "general-purpose",
                "description": "Look something up",
                "toolUseId": format!("toolu_{:024x}", self.extras),
            })
            .to_string()
            .as_bytes(),
        );
        self.file(&format!("{agent}.jsonl"), lines.as_bytes())
    }

    /// The file `path` in the session's own folder, holding `bytes`: any file the agent keeps
    /// there, whatever it is. Returns its key.
    pub fn file(&self, path: &str, bytes: &[u8]) -> String {
        let key = format!("{}/{path}", self.dir);
        write(&self.root.join(&key), bytes);
        key
    }

    /// Tool output of `bytes` bytes saved outside the transcript. Returns its key.
    pub fn tool_result(&mut self, bytes: usize) -> String {
        self.extras += 1;
        let key = format!("{}/tool-results/toolu_{:024x}.txt", self.dir, self.extras);
        write(&self.root.join(&key), filler(self.extras, bytes).as_bytes());
        key
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

/// The `index`th record of a transcript, as one line of JSON: a prompt, a reply that calls a
/// tool, the tool's result, and now and then one of the bookkeeping records the agent writes
/// between them.
fn record(session: &str, project: &str, index: usize, bytes: usize, sidechain: bool) -> String {
    let uuid = |n: usize| format!("10000000-0000-4000-8000-{n:012x}");
    let common = json!({
        "uuid": uuid(index + 1),
        "parentUuid": if index == 0 { json!(null) } else { json!(uuid(index)) },
        "timestamp": format!(
            "2026-01-01T{:02}:{:02}:{:02}.000Z",
            index / 3600 % 24,
            index / 60 % 60,
            index % 60
        ),
        "sessionId": session,
        "cwd": project,
        "gitBranch": "main",
        "version": "2.1.0",
        "entrypoint": "cli",
        "isSidechain": sidechain,
    });
    let text = filler(index, bytes);
    let tool_use = format!("toolu_{index:024x}");
    let mut record = match index % 4 {
        0 => json!({
            "type": "user",
            "message": { "role": "user", "content": text },
        }),
        1 => json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "model": "model-1",
                "content": [
                    { "type": "text", "text": text },
                    { "type": "tool_use", "id": tool_use, "name": "Bash",
                      "input": { "command": "ls" } },
                ],
                "usage": { "input_tokens": 100, "output_tokens": 20 },
            },
        }),
        2 => json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": format!("toolu_{:024x}", index - 1),
                    "content": text,
                }],
            },
        }),
        _ => {
            return json!({
                "type": "file-history-snapshot",
                "messageId": uuid(index + 1),
                "snapshot": { "note": text },
            })
            .to_string()
                + "\n";
        }
    };
    record
        .as_object_mut()
        .unwrap()
        .extend(common.as_object().unwrap().clone());
    record.to_string() + "\n"
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
        lines[0] = record("rewritten", PROJECT, 0, 64, false);
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
}
