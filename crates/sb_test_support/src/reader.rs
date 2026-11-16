//! Sessions for the reader (docs/PLAN.md §9, §12), written into a [`ProjectsTree`]: the
//! sessions the design boards read (`E-Reader`, `E-Reader-Calls-1` and `-2`,
//! `E-Reader-Subagent`, `E-Edge-Source-Gone`) with the boards' own words, so that the reader's
//! screenshots compare one to one (docs/DESIGN.md §6); one that holds every construct the reader
//! draws; one with no conversation of its own; and a large one to measure the reader's budgets
//! against (PLAN §14). Everything here is made up, in placeholder names.

use std::fs::File;
use std::io::{Cursor, Write as _};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use crate::claude_code::image_block;
use crate::markdown;
use crate::{Call, ProjectsTree, Records, Session};

pub const OPUS_46: &str = "claude-opus-4-6";
pub const OPUS_55: &str = "claude-opus-5-5";
pub const FABLE: &str = "claude-fable-5-2";
pub const HAIKU: &str = "claude-haiku-4-5";

/// Where the boards' fc sessions started, and the boards' toolbox-rs's and the garden's.
pub const FC: &str = "/home/user/code/fc";
pub const TOOLBOX: &str = "/home/user/code/toolbox-rs";
pub const ADVENT: &str = "/home/user/code/advent-2025";

/// A transcript written at the times the boards show, with calls that name their model.
pub struct Script<'a> {
    pub records: &'a mut Records,
    /// The day its records are on, `2026-11-09`.
    day: &'static str,
    /// Counts the replies and calls, for their ids.
    count: usize,
    /// What the ids of this transcript's calls start with.
    prefix: &'static str,
}

impl<'a> Script<'a> {
    pub fn new(records: &'a mut Records, day: &'static str, prefix: &'static str) -> Self {
        Self {
            records,
            day,
            count: 0,
            prefix,
        }
    }

    /// The records written from now on are at `hms` (`09:12:04`) of the day.
    pub fn at(&mut self, hms: &str) -> &mut Self {
        self.records.set_time(&format!("{}T{hms}.000Z", self.day));
        self
    }

    fn next(&mut self) -> usize {
        self.count += 1;
        self.count
    }

    pub fn prompt(&mut self, text: &str) -> String {
        self.records.prompt(text)
    }

    /// A prompt with images after its text.
    pub fn prompt_with(&mut self, text: &str, images: &[Vec<u8>]) -> String {
        let blocks: Vec<Value> = std::iter::once(json!({ "type": "text", "text": text }))
            .chain(images.iter().map(|png| image_block("image/png", png)))
            .collect();
        self.records
            .user(json!(blocks), json!({ "origin": { "kind": "human" } }))
    }

    pub fn reply(&mut self, model: &str, text: &str) -> String {
        let message = self.next();
        self.records.assistant(
            message,
            model,
            json!([{ "type": "text", "text": text }]),
            json!({}),
        )
    }

    pub fn thinking(&mut self, model: &str, text: &str, millis: u64) -> String {
        let message = self.next();
        self.records.assistant(
            message,
            model,
            json!([{ "type": "thinking", "thinking": text, "signature": "c2ln" }]),
            json!({ "thinkingDurationMs": millis }),
        )
    }

    pub fn call(&mut self, model: &str, name: &str, input: Value) -> Call {
        let message = self.next();
        let id = format!("toolu_{}{message:08}", self.prefix);
        let block = json!({ "type": "tool_use", "id": id, "name": name, "input": input });
        Call {
            uuid: self
                .records
                .assistant(message, model, json!([block]), json!({})),
            id,
        }
    }

    /// The result of `call`: text or blocks, with the agent's structured copy of it.
    pub fn result(&mut self, call: &Call, content: Value, detail: Value) -> String {
        self.records.result_with(call, content, false, detail)
    }

    pub fn failure(&mut self, call: &Call, text: &str) -> String {
        self.records.result_with(call, json!(text), true, json!({}))
    }
}

/// `count` lines of `line`'s output, numbered.
fn numbered(count: usize, line: impl Fn(usize) -> String) -> String {
    (1..=count).map(line).collect::<Vec<_>>().join("\n")
}

/**
 * A PNG of `width` × `height`: a window's grey and a few bars, as a screenshot the agent was
 * shown might look, with `seed` moving the bars.
 */
pub fn screenshot(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let picture = image::RgbImage::from_fn(width, height, |x, y| {
        let bar = (y / 18 + seed) % 5;
        let inset = width / 12;
        let length = width * (3 + bar) / 9;
        if y % 18 < 8 && x > inset && x < inset + length && y > height / 6 {
            image::Rgb([138, 184, 240])
        } else if y < height / 8 {
            image::Rgb([36, 40, 45])
        } else {
            image::Rgb([23, 25, 28])
        }
    });
    let mut png = Vec::new();
    picture
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("a generated image encodes");
    png
}

/// `E-Reader`: the backfill session of fc, its first turn as the board draws it, then its other
/// three turns from the board's facts. Returns the session's key.
pub fn backfill(tree: &mut ProjectsTree) -> String {
    let mut session = tree.session(FC, 0);
    let explore = {
        let mut s = Script::new(&mut session.transcript, "2026-11-09", "a");
        s.at("09:12:04").prompt_with(
            "The metadata backfill hammers YouTube on a fresh library: 1,729 cams means 1,729 \
             `yt-dlp --dump-json` processes queued at once, and we start getting 429s after \
             ~200. Throttle it: a few at a time, back off on 429, and show progress in Library \
             health instead of the spinner.",
            &[screenshot(640, 360, 1)],
        );
        s.at("09:12:10").thinking(
            OPUS_46,
            "If it spawns one task per cam, a semaphore around spawn still leaves 1,729 futures \
             holding their args, and the 429s come from how many run at once, not how many \
             exist. A bounded stream is the simpler fix.",
            14_000,
        );
        s.at("09:12:24").reply(
            OPUS_46,
            "Let me find where the backfill is scheduled first.",
        );
        let grep = s.at("09:12:25").call(
            OPUS_46,
            "Grep",
            json!({ "pattern": "backfill", "path": "crates", "type": "rust", "output_mode": "content" }),
        );
        s.at("09:12:25").result(
            &grep,
            json!("crates/fc_tools/src/backfill.rs\n  14  pub struct Backfill {\n  41  pub fn spawn_backfill(library: &Library, cx: &mut App) -> Task<()> {\n  58          cx.background_spawn(fetch_metadata(cam.clone(), tools.clone()))\ncrates/fc_ui/src/health/mod.rs\n  88      let pending = state.backfill.pending();"),
            json!({ "mode": "content", "numMatches": 5, "numLines": 5, "numFiles": 2 }),
        );
        let read = s.at("09:12:27").call(
            OPUS_46,
            "Read",
            json!({ "file_path": "/home/user/code/fc/crates/fc_tools/src/backfill.rs", "offset": 36, "limit": 37 }),
        );
        s.at("09:12:27").result(
            &read,
            json!(numbered(37, |n| format!("{:>4}\t    // line {} of the backfill", n + 35, n + 35))),
            json!({ "type": "text", "file": { "filePath": "/home/user/code/fc/crates/fc_tools/src/backfill.rs", "numLines": 37, "startLine": 36, "totalLines": 188 } }),
        );
        let todos = s.at("09:12:30").call(
            OPUS_46,
            "TodoWrite",
            json!({ "todos": [
                { "content": "Find where the backfill is scheduled", "status": "completed", "activeForm": "Finding it" },
                { "content": "Bound concurrency to 4 with a queue", "status": "in_progress", "activeForm": "Bounding it" },
                { "content": "Back off on HTTP 429", "status": "pending", "activeForm": "Backing off" },
                { "content": "Show progress in Library health", "status": "pending", "activeForm": "Showing it" },
            ] }),
        );
        s.at("09:12:30").result(
            &todos,
            json!("Todos have been modified successfully."),
            json!({}),
        );
        s.at("09:12:33").call(
            OPUS_46,
            "Agent",
            json!({
                "subagent_type": "Explore",
                "description": "How does Library health show background work?",
                "prompt": "Find how Library health in fc shows work that runs in the background: the type of a health item, how progress is drawn, and what sets it. Report the names and where they are.",
            }),
        )
    };
    let mut sub = session.subagent_of(
        &explore,
        "Explore",
        "How does Library health show background work?",
    );
    let agent = sub.agent.clone();
    {
        let mut s = Script::new(&mut sub, "2026-11-09", "b");
        s.at("09:12:34").prompt(
            "Find how Library health in fc shows work that runs in the background: the type of a \
             health item, how progress is drawn, and what sets it. Report the names and where \
             they are.",
        );
        let calls: [(&str, Value, Value, Value); 9] = [
            (
                "Grep",
                json!({ "pattern": "HealthItem", "path": "crates/fc_ui" }),
                json!("crates/fc_ui/src/health/mod.rs\ncrates/fc_ui/src/health/row.rs"),
                json!({ "mode": "content", "numMatches": 7 }),
            ),
            (
                "Read",
                json!({ "file_path": "/home/user/code/fc/crates/fc_ui/src/health/mod.rs", "offset": 60, "limit": 81 }),
                json!(numbered(81, |n| format!("{:>4}\t// health {n}", n + 59))),
                json!({ "file": { "numLines": 81 } }),
            ),
            (
                "Read",
                json!({ "file_path": "/home/user/code/fc/crates/fc_core/src/health.rs" }),
                json!(numbered(40, |n| format!("{n:>4}\t// core {n}"))),
                json!({ "file": { "numLines": 40 } }),
            ),
            (
                "Grep",
                json!({ "pattern": "set_progress", "path": "crates" }),
                json!("crates/fc_ui/src/health/mod.rs:102"),
                json!({ "mode": "content", "numMatches": 1 }),
            ),
            (
                "Read",
                json!({ "file_path": "/home/user/code/fc/crates/fc_ui/src/health/row.rs" }),
                json!(numbered(30, |n| format!("{n:>4}\t// row {n}"))),
                json!({ "file": { "numLines": 30 } }),
            ),
            (
                "Glob",
                json!({ "pattern": "crates/fc_ui/src/health/*.rs" }),
                json!("crates/fc_ui/src/health/mod.rs\ncrates/fc_ui/src/health/row.rs"),
                json!({ "numFiles": 2 }),
            ),
            (
                "Grep",
                json!({ "pattern": "HealthCheck::", "path": "crates" }),
                json!("crates/fc_core/src/health.rs:12"),
                json!({ "mode": "content", "numMatches": 4 }),
            ),
            (
                "Bash",
                json!({ "command": "git log --oneline -3 -- crates/fc_ui/src/health" }),
                json!(
                    "a1b2c3d Draw progress in health rows\n4e5f6a7 Health checks\n8b9c0d1 Library health"
                ),
                json!({}),
            ),
            (
                "Read",
                json!({ "file_path": "/home/user/code/fc/crates/fc_tools/src/backfill.rs", "offset": 1, "limit": 20 }),
                json!(numbered(20, |n| format!("{n:>4}\t// backfill {n}"))),
                json!({ "file": { "numLines": 20 } }),
            ),
        ];
        for (index, (name, input, output, detail)) in calls.into_iter().enumerate() {
            let call = s
                .at(&format!("09:12:{:02}", 36 + index * 4))
                .call(HAIKU, name, input);
            s.result(&call, output, detail);
        }
        s.at("09:13:14").reply(
            HAIKU,
            "Library health renders a `HealthItem` per check; each has `progress: Option<(usize, \
             usize)>`, already drawn as a bar by `health_row`. Nothing sets it for the backfill: \
             `HealthCheck::Backfill` only ever reports done or failed.",
        );
    }
    let mut s = Script::new(&mut session.transcript, "2026-11-09", "c");
    s.at("09:13:15").result(
        &explore,
        json!("Library health renders a HealthItem per check."),
        json!({ "agentId": agent, "status": "completed", "resolvedModel": HAIKU, "totalToolUseCount": 9, "totalDurationMs": 41_000 }),
    );
    s.at("09:13:20").reply(
        OPUS_46,
        "Library health already draws progress, so this is a scheduling change plus one \
         `set_progress` call.",
    );
    let edits = [
        ("crates/fc_tools/src/backfill.rs", 12, 5),
        ("crates/fc_tools/src/fetch.rs", 9, 1),
        ("crates/fc_ui/src/health/mod.rs", 4, 2),
    ];
    for (index, (path, added, removed)) in edits.into_iter().enumerate() {
        let edit = s.at(&format!("09:13:{:02}", 30 + index * 10)).call(
            OPUS_46,
            "Edit",
            json!({ "file_path": format!("{FC}/{path}"), "old_string": "let tasks: Vec<_> = missing.iter()", "new_string": "let health = HealthState::global(cx).clone();" }),
        );
        let lines: Vec<String> = (0..removed)
            .map(|n| format!("-old line {n}"))
            .chain((0..added).map(|n| format!("+new line {n}")))
            .collect();
        s.result(
            &edit,
            json!(format!("The file {FC}/{path} has been updated.")),
            json!({ "filePath": format!("{FC}/{path}"), "structuredPatch": [{ "oldStart": 39, "oldLines": 6, "newStart": 39, "newLines": 13, "lines": lines }] }),
        );
    }
    let test = s.at("09:14:02").call(
        OPUS_46,
        "Bash",
        json!({ "command": "cargo test -p fc_tools backfill", "description": "Run the backfill tests" }),
    );
    s.at("09:14:17").result(
        &test,
        json!("running 4 tests\ntest backfill::tests::bounded ... ok\ntest backfill::tests::backs_off_on_429 ... ok\ntest backfill::tests::reports_progress ... ok\ntest backfill::tests::resumes ... ok\n\ntest result: ok. 4 passed; 0 failed"),
        json!({}),
    );
    s.at("09:14:20").reply(
        OPUS_46,
        "## Backfill is throttled\n\nThe backfill now runs through a bounded queue instead of \
         spawning one task per cam, so a fresh library never has more than **4** `yt-dlp` \
         processes alive.\n\n- **Concurrency:** `buffer_unordered(CONCURRENCY)`; nothing is \
         spawned up front.\n- **429s:** `HTTP Error 429` on stderr becomes \
         `FetchError::RateLimited`; the loop waits with a doubling backoff.\n- **Progress:** \
         \"Fetching metadata · 212 of 1,729\" through the existing \
         [progress bar](https://example.com/fc/health).\n\n| Setting | Default | Where |\n|---|---|---|\n\
         | `backfill.concurrency` | 4 | `settings.toml` |\n| `backfill.backoff_start` | 30 s | \
         `settings.toml` |\n| `backfill.backoff_max` | 8 min | `settings.toml` |\n\n```toml\n\
         [backfill]\nconcurrency = 4\nbackoff_start = \"30s\"\nbackoff_max = \"8m\"\n```",
    );
    for (time, prompt, answer) in [
        (
            "09:30:02",
            "Back off on 429 only, not on 403",
            "Done: a 403 now fails the cam at once and is reported in Library health; only a 429 waits and retries.",
        ),
        (
            "09:38:40",
            "Run the tests and show me",
            "All 4 backfill tests pass, and the whole workspace's 212.",
        ),
        (
            "09:49:11",
            "Yes, add the fields to Downloads",
            "Downloads now shows the metadata state per cam: fetched, waiting, or failed with why.",
        ),
    ] {
        s.at(time).prompt(prompt);
        s.reply(OPUS_46, answer);
    }
    session.title("Throttle yt-dlp metadata backfill");
    session.session_key().to_string()
}

/// `E-Reader-Calls-1` and `-2`: the playlist session of fc, a call of every kind. Returns the
/// session's key.
pub fn playlist(tree: &mut ProjectsTree) -> String {
    let mut session = tree.session(FC, 0);
    let mut s = Script::new(&mut session.transcript, "2026-11-08", "d");
    s.at("14:26:00")
        .prompt("The test fails on the Mix URL, fix it.");
    let test = s.at("14:26:05").call(
        FABLE,
        "Bash",
        json!({ "command": "cd /home/user/code/fc && cargo test -p fc_downloads playlist" }),
    );
    s.at("14:26:06").failure(
        &test,
        "Exit code 101\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.94s\n     Running unittests src/lib.rs (target/debug/deps/fc_downloads-3f1c0a)\ntest playlist::tests::expands_plain_playlist ... ok\ntest playlist::tests::keeps_order ... ok\ntest playlist::tests::expands_mix_url ... FAILED\n\n---- playlist::tests::expands_mix_url stdout ----\nthread 'playlist::tests::expands_mix_url' panicked at crates/fc_downloads/src/playlist.rs:142:9:\nassertion `left == right` failed\n  left: 0\n right: 25",
    );
    let read = s.at("14:26:09").call(
        FABLE,
        "Read",
        json!({ "file_path": "/home/user/code/fc/crates/fc_downloads/src/playlist.rs", "offset": 28, "limit": 17 }),
    );
    s.result(
        &read,
        json!(numbered(17, |n| format!(
            "{:>4}\tlet line_{} = expand(url, tools)?;",
            n + 27,
            n + 27
        ))),
        json!({ "file": { "numLines": 17 } }),
    );
    let edit = s.at("14:26:20").call(
        FABLE,
        "Edit",
        json!({ "file_path": "/home/user/code/fc/crates/fc_downloads/src/playlist.rs", "old_string": "Some((_, id)) if id.starts_with(\"PL\") =>", "new_string": "Some((_, id)) if LIST_PREFIXES.iter().any(|p| id.starts_with(p)) =>" }),
    );
    s.result(
        &edit,
        json!("The file has been updated."),
        json!({ "structuredPatch": [{ "lines": [" let list = match", "-        Some((_, id)) if id.starts_with(\"PL\") =>", "+        // Mixes (\"RD…\") expand too; yt-dlp caps them at 25 entries.", "+        Some((_, id)) if LIST_PREFIXES.iter().any(|p| id.starts_with(p)) =>", "+            id.into_owned()"] }] }),
    );
    let mix = "{\n  \"_type\": \"playlist\",\n  \"id\": \"RDQMx7f2kq9Wcy\",\n  \"playlist_count\": 25,\n  \"entries\": [{ \"id\": \"a8Fq0Lw2xZc\" }, { \"id\": \"Qm3tX1bR7yE\" }],\n  \"title\": \"Mix\",\n  \"uploader\": null,\n  \"webpage_url\": \"https://www.youtube.com/playlist?list=RDQMx7f2kq9Wcy\"\n}";
    let write = s.at("14:26:31").call(
        FABLE,
        "Write",
        json!({ "file_path": "/home/user/code/fc/crates/fc_downloads/tests/fixtures/mix.json", "content": mix }),
    );
    s.result(
        &write,
        json!("File created successfully."),
        json!({ "type": "create", "content": mix }),
    );
    let grep = s.at("14:26:40").call(
        FABLE,
        "Grep",
        json!({ "pattern": "starts_with\\(\"PL\"\\)", "path": "crates", "output_mode": "content" }),
    );
    s.result(
        &grep,
        json!("crates/fc_downloads/src/playlist.rs\n  30          Some((_, id)) if id.starts_with(\"PL\") => id.into_owned(),\ncrates/fc_ui/src/downloads/paste.rs\n  57      let is_list = url.contains(\"list=\") && id.starts_with(\"PL\");\n  91          .filter(|id| id.starts_with(\"PL\"))"),
        json!({ "mode": "content", "numMatches": 3, "numFiles": 2 }),
    );
    let glob = s.at("14:26:44").call(
        FABLE,
        "Glob",
        json!({ "pattern": "crates/**/fixtures/*.json" }),
    );
    s.result(
        &glob,
        json!("crates/fc_downloads/tests/fixtures/channel.json\ncrates/fc_downloads/tests/fixtures/mix.json\ncrates/fc_downloads/tests/fixtures/playlist.json\ncrates/fc_downloads/tests/fixtures/shorts.json\ncrates/fc_tools/tests/fixtures/dump_json.json\ncrates/fc_tools/tests/fixtures/dump_json_429.json"),
        json!({ "numFiles": 6 }),
    );
    let todos = s.at("14:26:50").call(
        FABLE,
        "TodoWrite",
        json!({ "todos": [
            { "content": "Reproduce the Mix failure in a test", "status": "completed", "activeForm": "Reproducing" },
            { "content": "Accept RD… list ids", "status": "completed", "activeForm": "Accepting" },
            { "content": "Show \"25 of 25 (Mix limit)\" on the paste badge", "status": "in_progress", "activeForm": "Showing" },
            { "content": "Update the Downloads docs", "status": "pending", "activeForm": "Updating" },
        ] }),
    );
    s.result(
        &todos,
        json!("Todos have been modified successfully."),
        json!({}),
    );

    s.at("14:51:00").prompt(
        "Check what yt-dlp does for Mixes, and whether someone already reported the 25 cap.",
    );
    s.at("14:51:02").thinking(
        FABLE,
        "A Mix is generated per viewer and has no fixed length, so the 25 is probably the first \
         page of the continuation. If yt-dlp stops after one page by design, the right fix is on \
         our side: say \"Mix limit\" rather than claiming the list is complete.",
        22_000,
    );
    let search = s.at("14:51:24").call(
        FABLE,
        "WebSearch",
        json!({ "query": "yt-dlp youtube mix playlist only 25 entries" }),
    );
    s.at("14:51:26").result(
        &search,
        json!("Web search results for query: \"yt-dlp youtube mix playlist only 25 entries\"\n\nLinks: [{\"title\":\"YouTube Mix playlists stop at 25 videos · Issue · yt-dlp/yt-dlp\",\"url\":\"https://github.com/yt-dlp/yt-dlp/issues\"},{\"title\":\"--flat-playlist and continuation pages\",\"url\":\"https://github.com/yt-dlp/yt-dlp/wiki/FAQ\"}]"),
        json!({}),
    );
    let fetch = s.at("14:51:30").call(
        FABLE,
        "WebFetch",
        json!({ "url": "https://github.com/yt-dlp/yt-dlp/wiki/FAQ", "prompt": "What does it say about Mix playlists and pagination?" }),
    );
    s.at("14:51:32").result(
        &fetch,
        json!("Mixes are endless and personalised; yt-dlp extracts the first continuation only, which YouTube serves as **25 entries**. Use `--playlist-end` to stop earlier; there is no option to follow further pages."),
        json!({}),
    );
    let mcp = s.at("14:51:35").call(
        FABLE,
        "mcp__github__search_issues",
        json!({ "q": "mix playlist 25 repo:yt-dlp/yt-dlp", "state": "all", "per_page": 5 }),
    );
    s.at("14:51:36").result(
        &mcp,
        json!([{ "type": "text", "text": "{\"total_count\": 3, \"items\": [{\"number\": 4412, \"state\": \"closed\", \"title\": \"Mix only returns 25 items\"}]}" }]),
        json!({}),
    );
    let probe = s.at("14:51:40").call(
        FABLE,
        "PlaylistProbe",
        json!({ "url": "https://www.youtube.com/watch?v=a8Fq0Lw2xZc&list=RDQMx7f2kq9Wcy" }),
    );
    s.at("14:51:40").result(
        &probe,
        json!("{\"kind\": \"mix\", \"first_page\": 25, \"has_more\": true}"),
        json!({}),
    );
    s.at("14:51:45").reply(
        FABLE,
        "It's documented: a Mix only gives its first 25. I'll label it instead of claiming the \
         list is complete.",
    );
    s.at("15:02:00").records.compact(
        "The owner asked to fix the Mix URL test. The playlist expansion now accepts RD list ids, \
         a fixture for a Mix was added, and the paste badge is to say \"25 of 25 · Mix limit\".",
        171_000,
        24_000,
    );
    s.at("15:33:00")
        .prompt("Screenshot the count badge so I can see it.");
    let shot = s.at("15:33:04").call(
        FABLE,
        "Bash",
        json!({ "command": "script/shot downloads --out /tmp/badge.png" }),
    );
    s.at("15:33:07")
        .result(&shot, json!("Saved /tmp/badge.png (1280×720)"), json!({}));
    let look = s
        .at("15:33:09")
        .call(FABLE, "Read", json!({ "file_path": "/tmp/badge.png" }));
    s.at("15:33:09").result(
        &look,
        json!([image_block("image/png", &screenshot(1280, 720, 3))]),
        json!({ "type": "image" }),
    );
    session.title("Downloads: paste a whole playlist");
    session.session_key().to_string()
}

/// `E-Reader-Subagent`: the audit session of toolbox-rs, a subagent with many calls. Returns
/// the session's key.
pub fn audit(tree: &mut ProjectsTree) -> String {
    let mut session = tree.session(TOOLBOX, 0);
    let call = {
        let mut s = Script::new(&mut session.transcript, "2026-11-12", "e");
        s.at("11:48:00").prompt(
            "Now check fc the same way. Anything that came from sqlez has to go before we can \
             lift fc's code into an MIT crate.",
        );
        s.at("11:48:05").reply(
            OPUS_55,
            "fc is big, so I'll hand the search to a subagent and keep this thread on toolbox-rs.",
        );
        s.at("11:48:30").call(
            OPUS_55,
            "Agent",
            json!({
                "subagent_type": "general-purpose",
                "description": "Find code in fc copied from Zed",
                "prompt": "List every file under `/home/user/code/fc` that was copied from Zed (sqlez, gpui helpers), with where it came from and how much it changed.",
            }),
        )
    };
    let mut sub = session.subagent_of(&call, "general-purpose", "Find code in fc copied from Zed");
    let agent = sub.agent.clone();
    {
        let mut s = Script::new(&mut sub, "2026-11-12", "f");
        s.at("11:49:00").prompt(
            "List every file under `/home/user/code/fc` that was copied from Zed (sqlez, gpui \
             helpers), with where it came from and how much it changed.",
        );
        let first = s.at("11:49:04").call(
            HAIKU,
            "Grep",
            json!({ "pattern": "sqlez", "path": "/home/user/code/fc" }),
        );
        s.result(
            &first,
            json!("crates/fc_db/Cargo.toml\n   4  # vendored from zed: crates/sqlez @ 2f9a01c\ncrates/fc_db/src/connection.rs\n   1  // Copied from zed/crates/sqlez/src/connection.rs"),
            json!({ "mode": "content", "numMatches": 31, "numFiles": 9 }),
        );
        let glob = s.at("11:49:10").call(
            HAIKU,
            "Glob",
            json!({ "pattern": "crates/fc_db/src/**/*.rs" }),
        );
        s.result(
            &glob,
            json!(numbered(9, |n| format!("crates/fc_db/src/file_{n}.rs"))),
            json!({ "numFiles": 9 }),
        );
        let read = s.at("11:49:14").call(HAIKU, "Read", json!({ "file_path": "/home/user/code/fc/crates/fc_db/src/connection.rs", "offset": 1, "limit": 40 }));
        s.result(
            &read,
            json!(numbered(40, |n| format!("{n:>4}\t// connection {n}"))),
            json!({ "file": { "numLines": 40 } }),
        );
        let log = s.at("11:49:20").call(HAIKU, "Bash", json!({ "command": "git log --follow --format='%h %s' -- crates/fc_db/src/connection.rs | tail -3" }));
        s.at("11:49:20").result(
            &log,
            json!("2f9a01c Vendor sqlez\n7d1e2f3 Tidy\n9a8b7c6 Rename"),
            json!({}),
        );
        for index in 0..19 {
            let call = s
                .at(&format!("11:{:02}:{:02}", 49 + (24 + index * 6) / 60, (24 + index * 6) % 60))
                .call(HAIKU, "Read", json!({ "file_path": format!("/home/user/code/fc/crates/fc_db/src/file_{index}.rs") }));
            s.result(
                &call,
                json!(numbered(12, |n| format!(
                    "{n:>4}\t// file {index} line {n}"
                ))),
                json!({ "file": { "numLines": 12 } }),
            );
        }
        s.at("11:51:41").reply(
            HAIKU,
            "Three files in `fc_db` come from Zed's sqlez; nothing else does.\n\n| fc path | From Zed \
             | Changed |\n|---|---|---|\n| `fc_db/src/connection.rs` | `sqlez/src/connection.rs` \
             | lightly |\n| `fc_db/src/statement.rs` | `sqlez/src/statement.rs` | no |\n| \
             `fc_db/src/migrations.rs` | `sqlez/src/migrations.rs` | yes, +88 lines |",
        );
    }
    let mut s = Script::new(&mut session.transcript, "2026-11-12", "g");
    s.at("11:51:41").result(
        &call,
        json!("Three files in fc_db come from Zed's sqlez."),
        json!({ "agentId": agent, "status": "completed", "resolvedModel": HAIKU, "totalToolUseCount": 23, "totalDurationMs": 161_000 }),
    );
    s.at("11:51:50").reply(
        OPUS_55,
        "fc carries three files from **sqlez**, all in `fc_db`. They're GPL-3.0, so none of fc's \
         database layer can be lifted into toolbox-rs as is. The rest of fc is clean.\n\nThe way \
         out is the same as for slide: move fc onto `toolbox_store` and delete `fc_db`. Want me to \
         start with the migrations?",
    );
    session.title("Audit fc and slide for code taken from Zed");
    session.session_key().to_string()
}

/// `E-Edge-Source-Gone`: a session of advent-2025 that is later deleted at its location.
/// Returns the session.
pub fn garden(tree: &mut ProjectsTree) -> Session {
    let mut session = tree.session(ADVENT, 0);
    let mut s = Script::new(&mut session.transcript, "2026-01-10", "h");
    s.at("20:14:00")
        .prompt("Day 12, part two: the fences now count sides, not perimeter.");
    s.at("20:14:20").reply(
        OPUS_55,
        "Counting corners counts sides: each region has as many sides as corners. I'll walk each \
         cell's four corners and count the convex and concave ones.",
    );
    session.title("Day 12: garden fences, part two");
    session
}

/**
 * Every construct the reader draws, in one session of fc: long Markdown replies (the
 * markdown corpus), a call of each kind with a small and a large output, output saved beside
 * the transcript, an image a tool returned, a failed call and one never answered, a subagent
 * with its own calls that starts another, a compaction, a prompt from another sender, notes,
 * a record of a kind no provider knows and one of a known kind that doesn't fit, and a
 * transcript that ends inside a record. Returns the session's key.
 */
pub fn constructs(tree: &mut ProjectsTree) -> String {
    let mut session = tree.session(FC, 0);
    let (_, saved_note) = session.saved_output(
        "b7kq2ld9x.txt",
        numbered(900, |n| {
            format!("saved line {n}: a long output kept beside the transcript")
        })
        .as_bytes(),
    );
    let outer = {
        let mut s = Script::new(&mut session.transcript, "2026-11-10", "i");
        s.at("10:00:00")
            .prompt("Walk me through every part of the reader, one of each.");
        for (index, section) in markdown::corpus().into_iter().enumerate() {
            s.at(&format!("10:00:{:02}", 2 + index))
                .reply(OPUS_55, &section.markdown);
        }
        s.records.user(
            json!("<system-reminder>The owner prefers short answers.</system-reminder>"),
            json!({}),
        );
        s.records
            .system("informational", "Auto-update installed a newer version.");
        s.records.user(json!("Caveat: the messages below were generated by the user while running local commands."), json!({ "isMeta": true }));
        s.records.user(
            json!("Please also check the release notes before you finish."),
            json!({ "isMeta": true, "origin": { "kind": "peer", "from": "release-notes", "body": "Please also check the release notes before you finish." } }),
        );
        s.at("10:01:00")
            .reply(OPUS_55, "Noted: I'll look at the release notes too.");
        let kinds: [(&str, Value, Value, Value); 10] = [
            (
                "Bash",
                json!({ "command": "ls crates" }),
                json!("fc_core\nfc_db\nfc_downloads\nfc_tools\nfc_ui"),
                json!({}),
            ),
            (
                "Bash",
                json!({ "command": "cargo build --workspace --verbose" }),
                json!(numbered(320, |n| format!(
                    "   Compiling crate_{n} v0.1.{n}"
                ))),
                json!({}),
            ),
            (
                "Read",
                json!({ "file_path": "/home/user/code/fc/README.md" }),
                json!(numbered(12, |n| format!("{n:>4}\t# readme line {n}"))),
                json!({ "file": { "numLines": 12 } }),
            ),
            (
                "Read",
                json!({ "file_path": "/home/user/code/fc/crates/fc_ui/src/app.rs" }),
                json!(numbered(640, |n| format!("{n:>4}\tfn line_{n}() {{}}"))),
                json!({ "file": { "numLines": 640 } }),
            ),
            (
                "Grep",
                json!({ "pattern": "TODO", "path": "crates" }),
                json!(numbered(60, |n| format!(
                    "crates/file_{n}.rs:{n}: // TODO {n}"
                ))),
                json!({ "mode": "content", "numMatches": 60 }),
            ),
            (
                "Glob",
                json!({ "pattern": "**/*.toml" }),
                json!("Cargo.toml\ncrates/fc_core/Cargo.toml"),
                json!({ "numFiles": 2 }),
            ),
            (
                "WebSearch",
                json!({ "query": "gpui list virtualisation" }),
                json!("Web search results: 3 links"),
                json!({}),
            ),
            (
                "WebFetch",
                json!({ "url": "https://example.com/docs/list", "prompt": "Summarise the list API." }),
                json!(markdown::reply(7, 6_000)),
                json!({}),
            ),
            (
                "mcp__docs__lookup",
                json!({ "topic": "lists" }),
                json!([{ "type": "text", "text": "{\"found\": 2}" }]),
                json!({}),
            ),
            (
                "Unheard",
                json!({ "anything": [1, 2, 3] }),
                json!("whatever it said"),
                json!({}),
            ),
        ];
        for (index, (name, input, output, detail)) in kinds.into_iter().enumerate() {
            let call = s
                .at(&format!("10:02:{:02}", index * 3))
                .call(OPUS_55, name, input);
            s.at(&format!("10:02:{:02}", index * 3 + 1))
                .result(&call, output, detail);
        }
        let saved = s.at("10:03:00").call(
            OPUS_55,
            "Bash",
            json!({ "command": "cargo tree --workspace" }),
        );
        s.result(&saved, json!(saved_note), json!({}));
        let failed = s.at("10:03:10").call(
            OPUS_55,
            "Bash",
            json!({ "command": "cargo test -p fc_core" }),
        );
        s.at("10:03:14")
            .failure(&failed, "Exit code 1\nerror: could not compile `fc_core`");
        let refused = s.at("10:03:20").call(OPUS_55, "Edit", json!({ "file_path": "/home/user/code/fc/src/lib.rs", "old_string": "a", "new_string": "b" }));
        s.failure(
            &refused,
            "The owner doesn't want to proceed with this tool use.",
        );
        let look = s.at("10:03:30").call(
            OPUS_55,
            "Read",
            json!({ "file_path": "/home/user/code/fc/docs/shot.png" }),
        );
        s.result(
            &look,
            json!([image_block("image/png", &screenshot(800, 450, 2))]),
            json!({ "type": "image" }),
        );
        s.at("10:04:00").call(
            OPUS_55,
            "Agent",
            json!({ "subagent_type": "general-purpose", "description": "Map the crates", "prompt": "Map fc's crates and what depends on what." }),
        )
    };
    let mut first = session.subagent_of(&outer, "general-purpose", "Map the crates");
    let first_agent = first.agent.clone();
    let inner = {
        let mut s = Script::new(&mut first, "2026-11-10", "j");
        s.at("10:04:01")
            .prompt("Map fc's crates and what depends on what.");
        let ls = s
            .at("10:04:03")
            .call(HAIKU, "Bash", json!({ "command": "ls crates" }));
        s.result(&ls, json!("fc_core\nfc_db\nfc_ui"), json!({}));
        let read = s.at("10:04:05").call(
            HAIKU,
            "Read",
            json!({ "file_path": "/home/user/code/fc/Cargo.toml" }),
        );
        s.result(
            &read,
            json!(numbered(30, |n| format!("{n:>4}\tline {n}"))),
            json!({ "file": { "numLines": 30 } }),
        );
        s.at("10:04:08").call(
            HAIKU,
            "Agent",
            json!({ "subagent_type": "Explore", "description": "Read fc_db's manifest", "prompt": "What does fc_db depend on?" }),
        )
    };
    let mut second = session.subagent_of(&inner, "Explore", "Read fc_db's manifest");
    let second_agent = second.agent.clone();
    {
        let mut s = Script::new(&mut second, "2026-11-10", "k");
        s.at("10:04:09").prompt("What does fc_db depend on?");
        let read = s.at("10:04:10").call(
            HAIKU,
            "Read",
            json!({ "file_path": "/home/user/code/fc/crates/fc_db/Cargo.toml" }),
        );
        s.result(
            &read,
            json!("[dependencies]\nrusqlite = \"0.32\""),
            json!({ "file": { "numLines": 2 } }),
        );
        s.at("10:04:12")
            .reply(HAIKU, "`fc_db` depends on `rusqlite` alone.");
    }
    {
        let mut s = Script::new(&mut first, "2026-11-10", "l");
        s.at("10:04:13").result(
            &inner,
            json!("fc_db depends on rusqlite alone."),
            json!({ "agentId": second_agent, "status": "completed", "resolvedModel": HAIKU, "totalToolUseCount": 1, "totalDurationMs": 4_000 }),
        );
        s.at("10:04:15")
            .reply(HAIKU, "Three crates: `fc_ui` → `fc_core` → `fc_db`.");
    }
    let mut s = Script::new(&mut session.transcript, "2026-11-10", "m");
    s.at("10:04:16").result(
        &outer,
        json!("Three crates."),
        json!({ "agentId": first_agent, "status": "completed", "resolvedModel": HAIKU, "totalToolUseCount": 4, "totalDurationMs": 15_000 }),
    );
    s.at("10:05:00").records.compact(
        "## Summary\n\nThe owner is reviewing the reader. Every kind of call was shown once; a \
         subagent mapped the crates.",
        152_000,
        19_000,
    );
    s.at("10:06:00").prompt("And the last part?");
    s.records
        .record("mystery-record", json!({ "payload": { "kept": true } }));
    s.records.system("weird-subtype", "");
    s.at("10:06:05")
        .reply(OPUS_55, "The last part is how a transcript ends.");
    s.at("10:06:06")
        .call(OPUS_55, "Bash", json!({ "command": "sleep 600" }));
    let key = session.key.clone();
    session.title("Reader fixture: every construct");
    let path = tree.path(&key);
    let mut file = File::options()
        .append(true)
        .open(&path)
        .expect("the transcript was written");
    file.write_all(b"{\"type\":\"assistant\",\"uuid\":\"cut-short\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Still writ")
        .expect("the transcript takes the cut record");
    file.set_modified(SystemTime::now() - Duration::from_secs(1800))
        .expect("the transcript's time can be set");
    session.session_key().to_string()
}

/// Replies built to get markup or a fetch past the reader, or to stall or crash its Markdown
/// view: each must read literally, as plain text where it is too large or too deep.
pub const HOSTILE_REPLIES: &[&str] = &[
    "!![x](https://evil.example/p.png) and !![y][r]\n\n[r]: https://evil.example/q.png",
    "$x`$ <img src=\"https://evil.example/p.png\"> `y and $a <a href=\"file:///etc/passwd\">b</a> c$",
    "fetch('http://evil.example/script');</script> stays text",
    "<div>\n<img src=\"https://evil.example/p.png\">\n</div>",
    "A path that reads backwards: \u{202E}gnp.exe and a hard break  \nhere",
    "Copied as written: <b>bold</b> at https://example.com/x and \u{202E}txt.exe",
];

/// The links of every scheme the link scenario clicks, one a reply, each long enough that its
/// reply's centre is on it.
pub const SCHEME_LINKS: &[&str] = &[
    "http://example.com/plain",
    "https://example.com/secure",
    "file:///etc/hosts",
    "javascript:alert(1)",
    "app-scheme://open/something",
    "\\\\server\\share\\file",
];

/// A PNG of `width` × `height` with its header's size changed to `claimed`, its checksum
/// fixed, so that only the claim is wrong.
pub fn claiming_png(claimed: u32) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let mut png = screenshot(16, 16, 0);
    png[16..20].copy_from_slice(&claimed.to_be_bytes());
    png[20..24].copy_from_slice(&claimed.to_be_bytes());
    let crc = crc32(&png[12..29]);
    png[29..33].copy_from_slice(&crc.to_be_bytes());
    png
}

/// An animated GIF of `frames` frames.
pub fn animated_gif(frames: usize) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
        for n in 0..frames {
            let frame =
                image::RgbaImage::from_pixel(64, 64, image::Rgba([n as u8 * 20, 80, 160, 255]));
            encoder
                .encode_frame(image::Frame::new(frame))
                .expect("a generated frame encodes");
        }
    }
    out
}

/**
 * The reader's hostile content in one session of fc: replies of markup a sanitiser must catch,
 * Markdown nested past what the view's parser survives, a reply of one very long line and one
 * fenced block of very many lines, links of every scheme, a command and a target that hold
 * bidirectional controls and line breaks, and images that are truncated, animated, claim a
 * vast size, or are SVG. `big` sets how large the long line and the long block are, in bytes.
 * Returns the session's key.
 */
pub fn hostile(tree: &mut ProjectsTree, big: usize) -> String {
    let mut session = tree.session(FC, 0);
    let mut s = Script::new(&mut session.transcript, "2026-11-07", "r");
    s.at("08:00:00")
        .prompt("Show me everything that could go wrong.");
    for (index, reply) in HOSTILE_REPLIES.iter().enumerate() {
        s.at(&format!("08:00:{:02}", 1 + index))
            .reply(OPUS_55, reply);
    }
    s.at("08:01:00")
        .reply(OPUS_55, &(">".repeat(300) + " three hundred quotes deep"));
    let lists: String = (0..200)
        .map(|n| format!("{}- level {n}\n", "  ".repeat(n)))
        .collect();
    s.at("08:01:01").reply(OPUS_55, &lists);
    s.at("08:01:02").reply(OPUS_55, &"word ".repeat(big / 5));
    s.at("08:01:03").reply(
        OPUS_55,
        &format!(
            "A long block:\n\n```text\n{}```\n\nAfter it.",
            "line\n".repeat(big / 5)
        ),
    );
    for (index, url) in SCHEME_LINKS.iter().enumerate() {
        s.at(&format!("08:02:{:02}", index)).reply(
            OPUS_55,
            &format!(
                "[Follow this deliberately long link text to the destination {index}](<{url}>)"
            ),
        );
    }
    /* An output much longer than what a body shows of it. */
    let long = s
        .at("08:02:59")
        .call(OPUS_55, "Bash", json!({ "command": "seq 1 999999" }));
    let mut lines = String::new();
    for n in 1.. {
        if lines.len() >= big {
            break;
        }
        lines.push_str(&format!("{n}\n"));
    }
    s.result(&long, json!(lines), json!({}));
    let bidi = s.at("08:03:00").call(
        OPUS_55,
        "Bash",
        json!({ "command": "cat \u{202E}txt.exe\necho second line" }),
    );
    s.result(&bidi, json!("ok"), json!({}));
    let images: [(&str, Vec<u8>); 4] = [
        ("image/png", screenshot(64, 64, 4)[..60].to_vec()),
        ("image/gif", animated_gif(12)),
        ("image/png", claiming_png(60_000)),
        (
            "image/svg+xml",
            b"<svg xmlns='http://www.w3.org/2000/svg'><image href='file:///etc/passwd'/></svg>"
                .to_vec(),
        ),
    ];
    for (index, (media_type, bytes)) in images.iter().enumerate() {
        let look = s.at(&format!("08:04:{:02}", index)).call(
            OPUS_55,
            "Read",
            json!({ "file_path": format!("/home/user/code/fc/image_{index}") }),
        );
        s.result(
            &look,
            json!([image_block(media_type, bytes)]),
            json!({ "type": "image" }),
        );
    }
    session.title("Reader fixture: hostile content");
    session.session_key().to_string()
}

/// A session whose items hold no main transcript, only a subagent's: what is left when a
/// transcript was never archived. Returns its folder's key.
pub fn subagents_only(tree: &mut ProjectsTree) -> String {
    let mut session = tree.session(FC, 0);
    let call = Call {
        id: "toolu_lost".to_string(),
        uuid: "lost".to_string(),
    };
    let mut sub = session.subagent_of(&call, "Explore", "A subagent whose session is gone");
    let mut s = Script::new(&mut sub, "2026-11-11", "n");
    s.at("08:00:00").prompt("Look at the downloads queue.");
    s.at("08:00:04")
        .reply(HAIKU, "The queue holds three items.");
    std::fs::remove_file(tree.path(&session.key)).expect("the transcript was written");
    session.session_key().to_string()
}

/**
 * A session of fc whose subagents claim each other wrongly: two of its own calls both start
 * the same subagent; of those no call of its own started, one starts itself, and two start each
 * other. Returns its key.
 */
pub fn tangled(tree: &mut ProjectsTree) -> String {
    let mut session = tree.session(FC, 0);
    session.title("Reader fixture: subagents that claim each other");
    let mut twice = session.subagent_without_meta();
    let mut itself = session.subagent_without_meta();
    let mut one = session.subagent_without_meta();
    let mut other = session.subagent_without_meta();
    let id = |records: &Records| records.agent.clone().expect("a subagent has an id");
    let (twice_id, itself_id, one_id, other_id) = (id(&twice), id(&itself), id(&one), id(&other));
    {
        let mut s = Script::new(&mut session.transcript, "2026-11-06", "t");
        s.at("08:00:00").prompt("Look twice.");
        for at in ["08:00:01", "08:00:02"] {
            let call = s.at(at).call(
                OPUS_55,
                "Agent",
                json!({ "description": "Look", "prompt": "Look.", "subagent_type": "Explore" }),
            );
            s.result(&call, json!("Looked."), json!({ "agentId": twice_id }));
        }
        s.at("08:00:03").reply(OPUS_55, "Looked twice.");
    }
    for (records, prefix, starts) in [
        (&mut twice, "u", None),
        (&mut itself, "v", Some(&itself_id)),
        (&mut one, "w", Some(&other_id)),
        (&mut other, "x", Some(&one_id)),
    ] {
        let mut s = Script::new(records, "2026-11-06", prefix);
        s.at("08:00:01").prompt("Look.");
        if let Some(starts) = starts {
            let call = s.at("08:00:02").call(
                HAIKU,
                "Agent",
                json!({ "description": "Again", "prompt": "Again.", "subagent_type": "Explore" }),
            );
            s.result(&call, json!("Again."), json!({ "agentId": starts }));
            /* A second result for the call, and a result for no call: neither starts anything,
            whatever it names. */
            s.result(&call, json!("Again?"), json!({ "agentId": twice_id }));
            let orphan = Call {
                id: format!("toolu_orphan_{prefix}"),
                uuid: format!("orphan_{prefix}"),
            };
            s.result(&orphan, json!("Lost."), json!({ "agentId": twice_id }));
        }
        s.at("08:00:03").reply(HAIKU, "Done.");
        if starts.is_some() {
            /* A call with no input, which the parse doesn't take for one, answered; and a
            result in a record that isn't a user's: neither starts anything. */
            let id = format!("toolu_noinput_{prefix}");
            records.assistant(
                900,
                HAIKU,
                json!([{ "type": "tool_use", "id": id, "name": "Agent" }]),
                json!({}),
            );
            records.user(
                json!([{ "type": "tool_result", "tool_use_id": id, "content": "Done." }]),
                json!({ "toolUseResult": { "agentId": twice_id } }),
            );
            let call = Call {
                id: format!("toolu_system_{prefix}"),
                uuid: format!("system_{prefix}"),
            };
            records.record(
                "system",
                json!({ "message": { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": call.id, "content": "Done." }
                ] }, "toolUseResult": { "agentId": twice_id } }),
            );
        }
    }
    session.session_key().to_string()
}

/**
 * A session of fc with two subagents kept in its own transcript, as earlier releases kept
 * them, neither under a call. Returns its key.
 */
pub fn inline(tree: &mut ProjectsTree) -> String {
    let mut session = tree.session(FC, 0);
    session.title("Reader fixture: subagents kept in the session's transcript");
    let t = &mut session.transcript;
    t.set_time("2026-11-05T08:00:00.000Z");
    t.prompt("Look around twice.");
    let after = t.reply_by(OPUS_46, "Looking.");
    for (message, prompt) in [(93, "First look."), (94, "Second look.")] {
        t.record(
            "user",
            json!({ "isSidechain": true, "parentUuid": null,
                    "message": { "role": "user", "content": prompt } }),
        );
        t.assistant(
            message,
            HAIKU,
            json!([{ "type": "text", "text": format!("Done: {prompt}") }]),
            json!({ "isSidechain": true }),
        );
        t.follow(&after);
    }
    t.reply_by(OPUS_46, "Both looked.");
    session.session_key().to_string()
}

/// What [`large`] writes.
#[derive(Clone, Copy, Debug)]
pub struct Large {
    /// Turns of the session's own conversation.
    pub turns: usize,
    /// Its tool calls, spread over the turns.
    pub calls: usize,
    /// Each call's output, in bytes.
    pub output_bytes: usize,
    /// Of the calls, how many start a subagent, and how many bytes each subagent's transcript
    /// holds.
    pub subagents: usize,
    pub subagent_bytes: usize,
}

impl Large {
    /// PLAN §14's: 500 calls in a main transcript of about 100 MB, with subagents bringing the
    /// session to a few hundred.
    pub const BUDGET: Large = Large {
        turns: 600,
        calls: 500,
        output_bytes: 200_000,
        subagents: 10,
        subagent_bytes: 20_000_000,
    };
}

/// A large session of fc for measuring the reader: replies of generated Markdown, `calls`
/// shell calls with long outputs, and subagents with transcripts of their own. Returns its key.
pub fn large(tree: &mut ProjectsTree, spec: Large) -> String {
    let mut session = tree.session(FC, 0);
    let every = spec.calls / spec.subagents.max(1);
    let mut agents = Vec::new();
    {
        let mut s = Script::new(&mut session.transcript, "2026-11-13", "o");
        let mut made = 0;
        for turn in 0..spec.turns {
            s.at(&format!("{:02}:{:02}:00", 8 + turn / 60, turn % 60))
                .prompt(&markdown::reply(turn as u64, 300));
            s.reply(OPUS_55, &markdown::reply(turn as u64 + 1_000, 1_500));
            /* The calls spread evenly over the turns, the last turn's among them. */
            while made < spec.calls * (turn + 1) / spec.turns {
                if spec.subagents > 0 && made % every == every - 1 && agents.len() < spec.subagents
                {
                    let call = s.call(
                        OPUS_55,
                        "Agent",
                        json!({ "subagent_type": "general-purpose", "description": format!("Part {made}"), "prompt": "Look into it." }),
                    );
                    agents.push(call);
                } else {
                    let call = s.call(
                        OPUS_55,
                        "Bash",
                        json!({ "command": format!("cargo test --package part_{made}") }),
                    );
                    s.result(
                        &call,
                        json!(numbered(spec.output_bytes / 60, |n| format!(
                            "test part_{made}::case_{n:05} ... ok   (filler to sixty)"
                        ))),
                        json!({}),
                    );
                }
                made += 1;
            }
            s.reply(OPUS_55, &markdown::reply(turn as u64 + 2_000, 600));
        }
    }
    for (index, call) in agents.iter().enumerate() {
        let mut sub = session.subagent_of(call, "general-purpose", &format!("Part {index}"));
        let agent = sub.agent.clone();
        let mut s = Script::new(&mut sub, "2026-11-13", "p");
        s.at("12:00:00").prompt("Look into it.");
        let mut written = 0;
        let mut calls = 0;
        while written < spec.subagent_bytes {
            let call = s.call(
                HAIKU,
                "Read",
                json!({ "file_path": format!("/home/user/code/fc/part_{calls}.rs") }),
            );
            let output = numbered(2_000, |n| {
                format!("{n:>5}\tfn part_{calls}_line_{n}() -> usize {{ {n} }}")
            });
            written += output.len();
            s.result(
                &call,
                json!(output),
                json!({ "file": { "numLines": 2_000 } }),
            );
            calls += 1;
        }
        s.reply(HAIKU, "Done.");
        let mut main = Script::new(&mut session.transcript, "2026-11-13", "q");
        main.at("12:30:00").result(
            call,
            json!("Done."),
            json!({ "agentId": agent, "status": "completed", "resolvedModel": HAIKU, "totalToolUseCount": calls, "totalDurationMs": 60_000 }),
        );
    }
    session.title("A long session to measure the reader");
    session.session_key().to_string()
}
