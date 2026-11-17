# scrollback: feature set

Scope as of 2026-11-15, from the planning conversation and the owner's answers to its open questions ([Settled in planning](#settled-in-planning)). This fixes *what* scrollback does; [PLAN.md](PLAN.md) covers how.

scrollback is a native desktop app (Rust + GPUI) that backs up, archives and reads coding-agent transcripts. The owner has over a year of Claude Code transcripts, most of them inside a WSL distro on a Windows PC and some on a Mac. Claude Code deletes old transcripts on its own, so anything not copied out is eventually lost.

## Principles

1. **scrollback never shows a transcript from an agent's own directory.** It syncs transcripts into its archive and reads only the archive.
2. **A sync only adds.** It never overwrites or deletes anything already archived. Bytes appended at the source extend the current version; any other change starts a new version; a file that disappears at the source stays in the archive. Only the owner removes something from the archive, on purpose.
3. **The archive holds exactly what the source held:** a file's bytes, or for an agent that keeps a database, a lossless export of its records. Parsing happens when a transcript is read or indexed, so a better parser or a new tool renderer applies to everything already archived. The index is derived and can be rebuilt from the archive.
4. **Sources are read-only.** scrollback never writes to an agent's directories and installs nothing on the machines it reads from. Deleting a session from the archive never deletes it at its location.
5. **Locations, not a server.** One app owns the archive on the machine it runs on and pulls from the locations the owner defines: this machine, a WSL distro, a host over SSH.
6. **External tools are shelled out to, not bundled:** `wsl.exe` and the system `ssh`, which brings the owner's SSH config, keys and agent with it.
7. **Agent-agnostic.** Everything specific to a coding agent lives in that agent's provider: where its transcripts are, how they are fetched, how they are parsed. Sync, the archive, filing, search and the reader work the same for any agent, whether it keeps flat files as Claude Code does or a database. Only Claude Code is built now; adding another agent is adding a provider and nothing else.

## Data model

- **Location:** a place transcripts are pulled from. Local (this machine's user), WSL (one distro) or SSH (one host). Has a name, a kind, and a sync state.
- **Agent:** a coding agent whose transcripts are archived, through its provider. Claude Code is the first. A location can hold more than one.
- **Item:** one unit archived from an agent at a location. For Claude Code an item is a file, and every file is archived, not only the transcripts: subagent transcripts, saved tool results, images, titles. For an agent that keeps a database, an item is an export of one session's records.
- **Version:** one continuous history of an item's bytes. Appends extend it. A rewrite, a truncation or a replacement starts the next version.
- **Session:** what the owner reads: one conversation, made of a main transcript and the items that belong to it. Has a title, an agent, a project path, a time span, the models used, a git branch, token and tool-call counts.
- **Project path:** the working directory a session started in, for example `/home/user/code/fc` or `D:\Projects\fc`.
- **Folder:** the owner's own grouping of sessions. Folders nest. A session is in one folder or in none (Unsorted).
- **Attached path:** a project path attached to a folder, optionally limited to one location. Sessions started under an attached path file themselves into that folder.

## Locations

- **A location is a machine or environment, not an agent's folder.** Every supported agent found at a location is synced; the location shows which agents it has.
- **Local:** this machine's user. For Claude Code that means `~/.claude`, or `CLAUDE_CONFIG_DIR` when set.
- **WSL** (Windows only): installed distros are found automatically and offered by name (`Ubuntu-24.04`); nothing is typed by hand. A stopped distro is listed without being started, and can be added without starting it; syncing it starts it.
- **SSH:** a host as `ssh` knows it (a `~/.ssh/config` alias or `user@host`). Adding one runs a connection test and shows what it found: the agents present and how many sessions. A host that fails the test can still be added, so a machine that is asleep can be set up now. The remote needs a POSIX shell: Linux and macOS hosts.
- **Add, rename, remove.** Removing a location stops syncing it and keeps everything already archived from it.
- **State per location:** synced (when), syncing (progress in files and bytes), unreachable (since when, last successful sync), failed (the reason, in the tool's own words).
- **An unreachable location is a normal state**, not an error: a laptop that is asleep, a distro that is stopped. The app shows how stale it is.
- **Staleness warning:** a location that hasn't synced for a set number of days is flagged, because the agent's own cleanup may delete transcripts before the next sync. It says how close that cleanup is, from the retention period last read at the location.
- **First run:** "Add your first location", with this machine and any WSL distros offered at once. The archive is created in the app's data directory without asking; it works the same on any machine the app is opened on, each with its own archive.

## Sync

- **When:** on launch, on demand (one location or all), and on an interval while the app is open. Any of them starts a stopped WSL distro to sync it.
- **What:** everything each agent's provider finds at the location. For Claude Code that is every file under `projects/`: session transcripts, subagent transcripts and their metadata, saved tool results, images, custom titles.
- **Incremental:** unchanged files are skipped by size and modification time. A file that grew is checked against what the archive holds, and only the new bytes are fetched.
- **Versions:** a file whose earlier bytes changed, or that shrank, becomes a new version. The earlier version stays readable.
- **Held back:** when a new version is a loss (the source shrank, or no longer parses), the earlier version stays the reading copy until the owner chooses otherwise.
- **Source gone:** a file deleted at the source is marked as gone there, with the date. Nothing is removed from the archive.
- **Sessions in progress:** a transcript being written during a sync is archived up to its last complete line; the rest arrives with the next sync.
- **Verify:** re-hash the archive against its catalogue on demand and report anything that doesn't match.
- **Sync never blocks reading.** The library and reader stay usable throughout.

## Filing

- A new session is filed by its project path: the folder with the longest attached path that contains it wins. A subfolder's more specific path claims a session before its parent's.
- **Paths are compared as paths, not text:** separators and Windows case are normalised, and a WSL view of a Windows drive matches the drive (`/mnt/d/Projects/fc` is `D:\Projects\fc`).
- **Under a path counts:** a session started in `…/fc/crates/fc_ui` files into the folder that has `…/fc`.
- **Manual filing sticks.** A session the owner dragged into a folder is never moved by a rule. **File automatically** hands such a session back to the rules.
- **Changing a folder's paths re-files** the sessions in Unsorted and the ones a rule filed, never the ones filed by hand.
- **Unsorted** holds sessions that match no folder, grouped by project path, with "Create folder from this path" and dragging a path onto a folder to attach it.

## Library

The layout is concept E on the design canvas (`design/project/E-Reader.dc.html`, `E-Folder.dc.html`): a sidebar that is always present, and one main area that shows either a folder page or the reader.

- **Sidebar:** All sessions, Starred, Unsorted (with a count), Trash, the folder tree, and the locations with their sync state at the foot.
- **Folders:** create, rename, delete (its sessions go to Unsorted), nest, reorder. Drag sessions onto a folder; drag a folder into another.
- **Folder page:** the folder's name and totals (sessions, tool calls, time span, locations), its attached paths as chips edited in place, and its sessions as cards grouped by month. A folder's page and count include its subfolders' sessions.
- **Session card:** title, the start of the first prompt, agent and model, location, date, number of tool calls.
- **Filters and sort** on a folder page: text filter, agent, model, branch, location, date, starred; newest or oldest first; cards or a list.
- **Session title:** the custom title when the agent stored one, otherwise the start of the first prompt.
- **Star** a session.
- **Delete** a session from the archive. It moves to Trash, where it can be restored or deleted for good. A restored session returns to its folder, or to Unsorted and the filing rules if that folder is gone. A session deleted for good is not archived again by later syncs, even while it still exists at its location. Nothing is ever deleted at a location.

## Reader

- **Markdown, fully rendered:** headings, lists, task lists, tables, block quotes, links, inline code, fenced code with syntax highlighting.
- **Tool calls as one-line activity rows** (kind, target, a short result) that expand in place. A switch shows every call expanded.
  - **Bash:** the command, its output, exit status and duration.
  - **Read:** the file and range; the content collapsed.
  - **Edit and Write:** a diff with syntax highlighting.
  - **Search tools (Grep, Glob):** the query and the number of matches; the matches collapsed.
  - **Todo and task lists:** the list with its states.
  - **Subagents:** the subagent's own transcript nested under the call that started it, with its model, call count and duration.
  - **Web fetch and search, MCP tools, anything unknown:** the tool's name with its input and output, formatted.
  - These are kinds of tool, not Claude Code's tools: each agent's tool names are mapped onto the same kinds, so one renderer serves every agent.
- **Thinking blocks**, collapsed to one line with their duration.
- **Images** the user attached or a tool returned.
- **Context compaction** shown as a boundary in the transcript.
- **Session facts** beside the transcript: location, project path, branch, models, when and how long, tokens in and out, the version being read.
- **Turns:** the user's prompts as an outline that jumps to each turn.
- **Calls:** counts by kind.
- **Find in session** (`Ctrl F`), previous and next session in the folder, Back to the folder.
- **Copy:** a message, a code block, a tool call's command or output.
- **Long sessions stay fast:** sessions with hundreds of calls and tens of megabytes open without a wait and scroll smoothly.

## Search

- **Across the archive** (`Ctrl K`): session titles, prompts, replies, and the commands and file paths of tool calls.
- **Subagent transcripts are searched too.** A match in one opens the parent session at the call that started the subagent.
- **Narrowed by** folder, location, agent, model, branch and date.
- A result opens the session at the match.

## Versions

- A session's **history:** each version with its date, size and why it started (first sync, source rewritten, source shrank).
- **Choose the reading copy** when a version is held back, or to look at an earlier one.
- The history shown is the main transcript's. A session's subagent transcripts and saved tool results keep versions of their own.

## Settings

- Settings open inside the main window, not a separate window.
- **Archive:** where it is and its size. Two ways to put it elsewhere: **Move…** copies the archive into an empty folder, checks every file, and switches to the copy, with progress and Stop; the old folder is left as it was, for the owner to delete by hand once satisfied. **Use a different folder** opens the archive there or creates one; it moves nothing. **Verify**, and for anything it finds damaged, fetching it again from its location when it still exists there. The sessions deleted for good, each with a way to let it be archived again.
- **Sync:** the interval, the staleness warning's threshold.
- **Appearance:** System, Light or Dark, and the window material, as the sibling apps do.
- **Shortcuts:** a read-only table in v1.

## Platform

- **Windows is the owner's main platform**, where most transcripts are (in WSL). **macOS is supported**, and is where development happens.
- **GPUI stack follows the sibling apps:** `gpui-pre 0.3.11` (used as `gpui`) with `gpui-component 0.9.0`, the pins toolbox-rs owns.
- **From toolbox-rs:** `toolbox_store` (the SQLite catalogue and its checked backups), `toolbox_process` (running `wsl.exe` and `ssh`), `gpui_appearance`, and the automation crates for end-to-end scenarios.
- **Licence: MIT.** Code is lifted from the owner's sibling apps where it fits (dv's diff rows, highlighting and WSL discovery), except anything those apps took from GPL sources ([PLAN D15](PLAN.md#1-decisions)).

## Later

- **Other agents** (opencode, pi, Codex CLI and others), each as a provider beside Claude Code. The seam they plug into is part of the first version and is tested with a stand-in agent that keeps a database ([PLAN §7](PLAN.md#7-providers-and-the-transcript-model)).
- **Headless sync** (`scrollback sync`) for a scheduled backup without the window, and a tray or background mode.
- **Merging archives:** an export and import bundle, so an archive on the Mac and one on the PC can be combined.
- **Usage over time:** tokens, models and sessions per folder.
- **Tags and notes** on sessions.
- **Export** a session to Markdown or HTML; copy the command that resumes it.
- **Branches:** the alternatives left behind when a conversation was rewound; sessions that continue or fork another, linked.
- **Version compare:** what differs between two versions of a session.
- **Reader options from the other concepts:** an inspector pane for split diffs and long output (C), hiding calls by kind (D).
- **More of the agent's data:** memory files, file-history snapshots.
- **Compacting** a quiet session's many small appends into one file.

## Out of scope

- Editing, continuing or resuming a session from scrollback.
- Writing anything to an agent's directories.
- A server, accounts or cloud sync.
- SSH hosts without a POSIX shell (native Windows).
- Encrypting the archive. Transcripts can contain secrets; the archive is as private as the folder it is in.

## Settled in planning

- **One archive per machine.** Opening the app on another machine simply creates an archive there. The two are independent until merging exists (Later).
- **The archive needs no setup.** It is created in the app's data directory on first run, catalogue included ([PLAN D3](PLAN.md#1-decisions)).
- **Search leaves tool output out** of the index for now ([PLAN D10](PLAN.md#1-decisions)).
- **Licence: MIT** ([PLAN D15](PLAN.md#1-decisions)).
- **Any sync starts a stopped WSL distro,** scheduled ones included.
- **Sessions can be deleted from the archive,** never from their location ([PLAN D14](PLAN.md#1-decisions)).
- **Fetching is agent-agnostic.** Only Claude Code is built, but nothing outside its provider knows it, and agents that keep a database instead of files fit the same seam ([PLAN D8](PLAN.md#1-decisions)).
