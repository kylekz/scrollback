# scrollback: plan

scrollback is a native desktop app (Rust + GPUI) that backs up, archives and reads coding-agent transcripts. It pulls transcripts from the locations the owner defines (this machine, a WSL distro, a host over SSH) into an archive it owns, and reads only from that archive. Claude Code is the first agent supported. Windows is the main platform, with first-class WSL; macOS is supported.

- **Feature set:** [FEATURES.md](FEATURES.md). This plan covers how.
- **Visual spec:** [DESIGN.md](DESIGN.md) (tokens, layout, materials, screens, keyboard map; cited as `DESIGN §N`), written from concept E on the design canvas. The boards' sources are the `E-*.dc.html` files in `design/project/`; where a board and DESIGN.md disagree, DESIGN.md wins.
- **Status:** plan, written 2026-11-15. Nothing here is built. Decisions marked *(owner)* were made by the owner in the planning conversation; the rest are proposals the owner has seen and not objected to. §14 lists the risks and what is still to be checked. Sections are cited from code and commits as `PLAN §N` / `D<n>`.

Comment and doc rule: code comments explain *what something is and why*, in the present tense, citing `PLAN §N` where a decision is involved. History belongs in commit messages, never in comments.

---

## 0. Goals and non-goals

**Goals (v1)**

1. Back up every Claude Code transcript from this machine, from WSL distros and from SSH hosts into one archive, and keep doing so incrementally (§5, §6).
2. Never lose archived bytes to a sync: appends extend a version, any other change starts a new one, deletions at the source are not propagated (§4). Only the owner deletes from the archive, and that never reaches a location (§4.5).
3. File sessions into the owner's folders automatically by project path, across locations (§8).
4. Read a session comfortably: rendered markdown, tool calls as compact rows that expand, subagents nested, long sessions fast (§9).
5. Find a session again: search across the archive, filters on a folder (§10).
6. WSL distros are first-class locations on Windows: discovered, named, synced without a path typed by hand (§6.3).
7. Hermetic automated testing: generated transcript trees, a loopback shell transport, GUI scenarios (§12).
8. Adding another agent is adding one provider, whether the agent keeps files or a database. Nothing else in the app knows an agent (D8, §7).

**Non-goals (v1):** providers for agents other than Claude Code (the seam for them is built and tested, §7.6), continuing or resuming sessions, writing to an agent's directories, a server or cloud sync, merging archives, encryption, a headless CLI. FEATURES.md lists what is deferred to later and what is out of scope.

---

## 1. Decisions

### D1. UI stack: `gpui-pre` + `gpui-component` 0.9, exact pins, as the sibling apps *(owner)*
`gpui = { package = "gpui-pre", version = "=0.3.11" }`, `gpui-component =0.9.0`, declared once in `[workspace.dependencies]` and bumped together. toolbox-rs owns the pin, so a bump starts there. Same stack as dv, fc, slide and folio means toolbox-rs's crates and the siblings' lessons carry over.

### D2. Locations, not a server *(owner)*
One app owns the archive on the machine it runs on and pulls from locations: Local, WSL, SSH. There is no daemon, no protocol and no accounts. The archive and sync logic live in a gpui-free crate (§3), so a headless sync or a server mode can be added later without redoing storage.

### D3. The archive is plain files plus a SQLite catalogue, in one folder *(owner: no setup)*
The archive is created in the app's data directory on first run, without asking, so the app works as it is on any machine; Settings can point it at a different folder. Transcript bytes are stored as write-once, compressed files under the archive folder; the catalogue (which bytes belong to which source file and version, plus the owner's folders and stars) is a SQLite database in the same folder, through `toolbox_store` with its checked backups (§4). One folder to move, copy or back up. Each archived file also carries a small manifest naming its source, so the transcripts can be recovered without the database.

### D4. Versions are append-aware; nothing is overwritten *(owner: versioning; the mechanism is proposed)*
An item's bytes are stored as segments (an item is a file, for an agent that keeps files; §7.1). New bytes appended at the source become a new segment of the current version. Anything else (earlier bytes changed, the file shrank, it was replaced) starts a new version from offset 0. Existing segments are never modified or deleted by a sync (§4.2, §5).

### D5. Raw bytes in, parse on read
The archive stores exactly what the source held: a file's bytes, or for an agent that keeps a database, a lossless export of its records (§7.2). The session index (titles, project paths, counts, search text) is derived by parsing, carries the parser's version, and is rebuilt when the parser changes (§7). Unknown line types and unknown tools are kept and shown generically, never dropped.

### D6. One shell transport for WSL and SSH; nothing installed remotely
A WSL distro and an SSH host are both "a POSIX shell reached through a child process": `wsl.exe -d <distro> --exec sh -s` and `ssh <host> sh -s`. One implementation with two launchers lists and reads files with standard tools (§6.2). dv's precedent is a Linux backend binary installed in the distro (dv D6); scrollback's sync is batch work that doesn't need one. If the shell approach proves fragile across real hosts, a helper binary behind the same `Transport` trait is the fallback.

### D7. `wsl.exe` and `ssh` are shelled out to
Both run through `toolbox_process` (no console window, a process tree of its own, back-pressure). `ssh` runs with `BatchMode=yes` and a connect timeout: it uses the owner's config, keys and agent, and fails instead of prompting. scrollback stores no credentials.

### D8. Agent-agnostic: everything about an agent is in its provider; Claude Code first *(owner)*
The archive, the sync engine, filing, search and the reader know only items, sessions and the common transcript model. A provider supplies everything specific to an agent: whether it is present at a location, how to enumerate and fetch its data through a transport, which items make a session, and how to parse them (§7). Agents don't all keep flat files as Claude Code does; some keep a database. So fetching belongs to the provider and not to the sync engine, with shared helpers for the two common shapes (§7.2). v1 ships one provider, Claude Code, plus a test-only provider backed by SQLite that keeps the seam honest (§12). Nothing outside `provider/claude_code` knows Claude Code's format, and adding an agent changes nothing outside its provider (§7.6).

### D9. A session's project path is its first working directory
Claude Code records `cwd` on each record. The project path is the first one in the main transcript, which is also what the transcript's directory name encodes (lossily). Sessions move between directories as they run; only the first counts for filing (§8).

### D10. Search through SQLite FTS5; tool output is not indexed *(owner: tool output)*
`toolbox_store` bundles SQLite with FTS5. The index covers titles, user prompts, assistant text, and tool calls' commands and paths. Tool output, most of a transcript's bytes, is left out for now to keep the index small (§10).

### D11. Markdown: `gpui-component`'s `TextView` first, measured against real transcripts
dv renders comment markdown with it. Transcripts need tables, fenced code with highlighting and very long documents. Phase 4 starts by rendering a corpus of real replies with it; if tables, code blocks or performance fall short, the fallback is an own renderer on `pulldown-cmark`, as folio has (its `StoryView`).

### D12. Layout: concept E *(owner)*
A's sidebar (library rows, folder tree, locations at the foot) is always present; the main area is B's folder page or B's reader, without the timeline gutter. A 40 px title bar with global search; no status bar (§11).

### D13. The theme is a module of the app for now
toolbox-rs has no shared tokens or components yet (`gpui_theme_kit` is scaffolding only). scrollback carries its own theme module as the siblings do, with `gpui_appearance` for light/dark and window materials.

### D14. Deleting is the owner's act, and stays in the archive *(owner)*
A session can be deleted from the archive. It goes to Trash first and can be restored; deleting it for good removes its stored bytes and leaves a tombstone, so a later sync doesn't archive it again while it still exists at its location (§4.5). No delete ever reaches a location (D6: the transports have no write operation).

### D15. Licence: MIT; lift from the siblings, except what they took from GPL sources *(owner)*
The sibling apps are the owner's, so their code can be lifted and relicensed here. dv and toolbox-rs are already MIT. fc and slide are GPL-3.0 because they carry copies of Zed's `sqlez`; that code, and anything else in a sibling that came from a GPL project, is not the owner's to relicense and stays out. scrollback never needs `sqlez`: its database layer is `toolbox_store`, the MIT crate on rusqlite that toolbox-rs built to replace it (D3). The limit only matters when lifting other code from those apps, so check where a file came from before lifting it.

---

## 2. Concept model

### 2.1 Entities

| Entity | What it is | Identity |
|---|---|---|
| Archive | The folder scrollback owns: stored bytes, catalogue, backups | An id written at creation (`archive.json`) |
| Location | A place to pull from: Local, WSL distro, SSH host | An id fixed at creation, independent of its display name |
| Provider | The code for one agent (Claude Code) | A fixed id |
| Item | One unit a provider archives at a location: a file, or an export of a database's records (§7.2) | Location + provider + the provider's key for it (a file's path relative to the agent's root) |
| Version | One continuous history of an item's bytes | Item + number, from 1 |
| Segment | One stored byte range of a version | Version + offset |
| Session | A conversation: a main transcript and the items that belong to it | Location + provider + the provider's session id |
| Folder | The owner's grouping of sessions; folders nest | An id |
| Attached path | A project path on a folder, optionally limited to one location | Folder + normalised path + location |

### 2.2 What is derived and what is not

- **Authoritative, written by sync:** locations, items, versions, segments. Losing these loses the mapping from stored bytes to sources; the per-file manifests (§4.1) allow rebuilding them.
- **Authoritative, written by the owner:** folders, attached paths, a session's folder when filed by hand, stars, which version is the reading copy, what is in Trash, and the tombstones of sessions deleted for good. Only the database and its backups hold these.
- **Derived, rebuildable:** sessions' parsed fields (title, project path, times, models, branch, counts, tokens), the search index, rule-filed folder assignments.

Owner state is keyed by session identity, not by a derived row's id, so a reindex can drop and rebuild every derived table.

### 2.3 Invariants

1. A sync writes new files and new rows. It never modifies or deletes a stored segment.
2. A segment is recorded in the catalogue only after it is durable on disk.
3. An item missing at the source is marked gone, with the time it was first seen missing. Its versions stay.
4. Reading never touches a location. Every view is served from the archive.
5. Nothing is ever written to a location, and nothing is ever deleted there.
6. Stored bytes are removed only when the owner deletes a session for good (§4.5).

---

## 3. Workspace layout

| Crate | Kind | Role | gpui? |
|---|---|---|---|
| `sb_core` | lib | The archive (layout, segments, versions, verify), the catalogue, locations and transports, the sync engine, providers, the transcript model and parser, path normalisation and filing, the search index | **never** |
| `sb_ui` | lib | Views and services (§11), the theme, markdown, tool-call renderers, syntax highlighting | yes |
| `scrollback` | bin | Entry: the app; the automation host behind an `automation` feature | yes |
| `sb_e2e` | bin `e2e` | The scenario runner: toolbox-rs's `gpui_automation_runner` with scrollback's profile | no |
| `sb_test_support` | lib (dev) | The fixture transcript-tree builder, a location root in a temp dir, the loopback launcher (§12) | no |

Direction: `scrollback` → `sb_ui` → {`sb_core`, `gpui-component`}. From toolbox-rs, pinned by tag: `toolbox_store` and `toolbox_process` (`sb_core`), `gpui_appearance` (`sb_ui`), `gpui_automation` (`sb_ui`, `scrollback`), `gpui_automation_runner` (`sb_e2e`). A gpui import in `sb_core` is a build failure, checked by `script/check` with `cargo tree`, as dv does.

`sb_core` starts as one crate with modules (`archive`, `catalogue`, `location`, `transport`, `sync`, `provider`, `transcript`, `filing`, `search`). It is split only when a second binary needs part of it.

Rules, as in dv: errors are `anyhow` in app code and `thiserror` enums in libraries; no `unwrap()`/`expect()` outside tests unless the message states the invariant; files stay under about 1,500 lines.

---

## 4. The archive

### 4.1 On disk

```
<archive>/
  archive.json                    format version, archive id, created
  scrollback.db                   catalogue and index (toolbox_store)
  backups/                        toolbox_store's checked backups of the database
  lock                            held while an app has the archive open
  tmp/                            in-flight writes; emptied on start
  files/<location-id>/<provider>/<ab>/<item-id>/
      source.json                 provider, location, the item's key
      v0001/0000000000000000.zst  bytes [0, a) of version 1
      v0001/0000000000184467.zst  bytes [a, b): an append
      v0002/0000000000000000.zst  the source was rewritten: version 2 starts at 0
```

- **`<item-id>`** is the first 32 hex digits of the SHA-256 of the item's key; `<ab>` is its first two digits. Keys, which for files are source paths, are not mirrored on disk: Claude Code's project directory names alone run past 170 characters, which with the archive's own prefix exceeds Windows' 260-character limit, and Linux paths can hold characters NTFS rejects. `source.json` keeps the original path, so the tree is still recoverable without the database.
- **A segment** is the zstd-compressed bytes of one range, named by its 16-digit offset. A version's content is its segments decompressed in order.
- **Writing a segment:** write to `tmp/`, flush to disk, rename into place, then record it in the catalogue in one transaction. A crash before the transaction leaves an uncatalogued file, which the next start removes; the next sync fetches the range again.
- **Hashes:** the catalogue stores the SHA-256 of each segment's uncompressed bytes and of each version's content up to its current length. SHA-256, because a remote can compute it with standard tools (§6.2).

### 4.2 Versions

Given the archived length `L` and hash `H` of the current version, and the source's size `S`:

| Source | Meaning | Action |
|---|---|---|
| Size and modification time unchanged | Unchanged | Skip |
| `S > L`, and the hash of the source's first `L` bytes is `H` | Appended | Fetch `[L, S)` as a new segment of the current version |
| `S == L`, time changed, whole-file hash is `H` | Touched | Record the new time; store nothing |
| Anything else (`S < L`, or the prefix hash differs) | Rewritten, truncated or replaced | Fetch the whole file as version n+1 |
| Not present | Deleted at the source | Mark gone; keep everything |

- **The table is the same for an item that isn't a file.** Its provider gives it a length, a stamp and a way to read a range (§7.1), so records appended to a database-backed session extend its version just as appended lines extend a file's (§7.2).
- **Line-oriented items (JSONL) are cut at the last newline.** A transcript being written may end mid-record; the tail after the last `\n` is left for the next sync, so a stored version always ends on a complete line. Other items are taken whole.
- **Versions belong to items, not sessions.** The version number the reader shows for a session is its main transcript's. The session's other items (subagent transcripts, saved tool results) version on their own.
- **Held back.** When version n+1 is shorter than version n, or its transcript no longer parses where n did, n stays the session's reading copy and n+1 is flagged for the owner (FEATURES: Versions). Otherwise the newest version is the reading copy.
- **Each version records why it started:** first sync, rewritten, shrank, replaced.

### 4.3 Catalogue

Tables, in outline (the schema is `toolbox_store` migrations, append-only):

| Table | Holds |
|---|---|
| `location` | id, kind, name, connection details (distro, or SSH host), last sync attempt and success, last error; per provider found there, the retention last read |
| `item` | location, provider, key, item id, kind (transcript, subagent, tool result, sidecar), the session it belongs to, last seen length and stamp, gone-since, damaged |
| `version` | item, number, reason, started, length, hash, held-back, the provider's export version when the item is an export |
| `segment` | version, offset, length, hash, stored-at |
| `folder` | id, parent, name, position |
| `folder_path` | folder, normalised path, the path as typed, location (nullable) |
| `session_state` | session identity → folder, filed-by (rule or hand), starred, chosen reading version, trashed-at |
| `tombstone` | session identity of a session deleted for good, and when |
| `session` *(derived)* | session identity, main item, title, project path, first and last timestamp, models, branch, entrypoint, counts, tokens, parser version |
| `session_text` *(derived)* | the FTS5 index (§10) |
| `kv` | settings that belong to the archive |

### 4.4 Opening, locking, verify

- The archive defaults to a folder in the app's data directory, created on first run (D3). Its path is a setting, kept outside the archive. If the folder can't be reached (an unplugged drive, a changed letter), the app says so and offers to locate it; it never creates a fresh archive silently in its place.
- `lock` stops a second app from opening the same archive for writing.
- **Pointing the app at a different folder** opens the archive there, or creates one if the folder has none. It moves nothing; moving an archive is moving its folder and then pointing the app at it.
- **Verify** re-hashes every segment against the catalogue and every version's running hash, and reports mismatches and missing files. It repairs nothing by itself. For each damaged item it offers **Fetch again from its location** when the item still exists there, which stores what the source has now as a new version; otherwise the item is marked damaged and stays readable as far as it decodes.

### 4.5 Deleting

- **Delete** moves a session to Trash: `session_state.trashed-at` is set. Its bytes stay, sync keeps archiving its files, and it disappears from every view except Trash. **Restore** clears the mark and returns the session to its folder; if that folder was deleted meanwhile, the session goes to Unsorted and the filing rules run on it (§8).
- **Delete for good** (from Trash, after a confirmation that names the session) removes the session's items under `files/`, their catalogue rows and its derived rows, and writes a tombstone in the same transaction as the rows. The files are removed after the commit; a crash in between leaves uncatalogued files, which the next start removes (§4.1).
- **A tombstoned session's items are skipped by sync** (§5), so deleting isn't undone by the next pass while the session still exists at its location.
- **Removing a tombstone** (Settings lists them) lets the next sync archive the session again, if its location still has it.
- Trash is never emptied automatically.

---

## 5. Sync

One **pass** syncs one location with one provider. The engine drives it and knows nothing about the agent; the provider does the two steps that touch the location, through the location's transport (D8):

1. **Scan** (provider): every item the agent has at the location: its key, kind, session, length, and a stamp that changes when the item does. For a file the stamp is its size and modification time.
2. **Plan** (engine): drop the items of tombstoned sessions (§4.5), compare the rest with `item` and decide per item (§4.2). Items that grew or changed need a prefix hash from the provider before the decision; those are asked for in one batch.
3. **Fetch** (provider): the planned byte ranges of the planned items, in one stream.
4. **Store** (engine): each range becomes a segment (§4.1), then its rows are committed.
5. **Index** (engine, with the provider's parser): sessions whose items changed are re-parsed and re-indexed (§7); new sessions are filed (§8).

- **Passes for different locations run in parallel.** One location runs one pass at a time; when it has several agents, their passes follow one another.
- **A pass can stop anywhere.** Everything committed stays; the next pass continues from the catalogue. Cancelling is the same as a dropped connection.
- **Triggers:** app start, the owner (one location or all), and an interval while the app is open. Any of them starts a stopped WSL distro to sync it *(owner)*; only listing distros never starts one (§6.3).
- **States** a location shows: synced, syncing (files and bytes done of planned), unreachable (the launcher failed to connect: last success and how long ago), failed (anything else, with the tool's message). Unreachable is not an error and isn't surfaced as one.
- **Staleness:** a location with no successful pass for longer than the threshold is flagged. A provider can report how long its agent keeps transcripts at a location (Claude Code: `cleanupPeriodDays`, §7.3), and the flag says how close that is. The value is stored with the location, so an unreachable location uses the one last read.
- **Progress** is reported per pass (files and bytes done, of the plan) to the sidebar and the locations sheet.

---

## 6. Locations and transports

### 6.1 The `Transport` trait

A read-only view of a location's file system, and nothing more. It knows nothing about agents; providers are its only callers (§7).

| Operation | Returns |
|---|---|
| `env(names)` | The location's home directory and the named environment variables |
| `stat(paths)` | Whether each path exists, its size and modification time |
| `list(root)` | Each file under a directory: relative path, size and modification time |
| `hash_prefixes(requests)` | SHA-256 of the first *n* bytes of each requested file |
| `read_ranges(requests, sink)` | The bytes of each requested range, streamed |
| `read_small(path)` | One small file whole (a settings file) |

No operation writes, and none runs an agent's own tools. That is enough for an agent that keeps files, and for one that keeps a SQLite database, whose provider reads the database file through the same operations (§7.2).

- **`LocalTransport`** implements it with `std::fs` on this machine.
- **`ShellTransport`** implements it over a child process (§6.2), with a `Launcher`: `Wsl { distro }`, `Ssh { host }`, and `Command` (a local `sh`, for tests).

### 6.2 The shell transport

- **Each operation is one child process** running `sh -s`, with a script on stdin. The request list (paths, offsets, lengths) is embedded in the script as a here-document, so nothing but a fixed argument list ever crosses a command line and no quoting rules of `wsl.exe`, `ssh` or the remote shell apply.
- **Listing** uses `find` and `stat`. GNU and BSD `stat` take different flags, so the script tries the GNU form and falls back to the BSD one. Records are NUL-separated; paths are never parsed out of whitespace-separated columns.
- **Paths that start with `-`** are normal here (Claude Code's project directories all do), so every path the script handles is absolute or prefixed with `./`.
- **Hashing** uses `sha256sum`, else `shasum -a 256`, over `head -c <n>`.
- **Reading ranges** streams, per request, a header line (path index, offset, length) followed by exactly that many bytes (`tail -c +<offset+1> | head -c <length>`). The reader checks each length; a short stream fails that file only.
- **Timeouts and cancel** are `toolbox_process`'s: stopping the pass kills the process tree.
- **What the remote needs:** `sh`, `find`, `stat`, `head`, `tail`, and `sha256sum` or `shasum`. A missing tool is reported by name when the location is added (the connection test) and as the location's failure after that.

### 6.3 WSL

- **Discovery:** the installed distros and whether each is running, from `wsl.exe --list --verbose` (its output is UTF-16). dv already does this and never starts a stopped distro to list it; its code is the reference.
- **A WSL location is a distro and the distro's default user.** Each provider finds its agent's data inside it (for Claude Code, `$HOME/.claude`), not through a `\\wsl.localhost` path.
- **Adding a stopped distro** doesn't run the connection test, which would start it. It is added as it is, and its first sync starts it.
- **Launch:** `wsl.exe -d <distro> --exec sh -s`.
- **Path equivalence** for filing (§8): a WSL path under the distro's Windows mount (`/mnt/<letter>/…` by default) is the same project as `<LETTER>:\…`. The mount root comes from the distro's `/etc/wsl.conf` when it sets one.

### 6.4 SSH

- **A location is a host string** exactly as the owner would give it to `ssh`: an alias from `~/.ssh/config` or `user@host`.
- **Launch:** `ssh -o BatchMode=yes -o ConnectTimeout=<s> <host> sh -s`. No password or passphrase prompt can appear; a host that needs one fails with `ssh`'s message, which the location shows.
- **Adding** runs the connection test: connect, find the tools, then ask each provider what it finds there (which agents are present, and how many sessions). A host that fails the test can still be added, so a machine that is asleep can be set up now and synced when it wakes.
- Windows ships an OpenSSH client; if `ssh` isn't on `PATH`, adding an SSH location says so.

---

## 7. Providers and the transcript model

### 7.1 The `Provider` trait

Everything specific to an agent is behind this trait (D8). The archive, the sync engine, filing, search and the reader are written against items, sessions and the transcript model (§7.4), and never name an agent.

| Method | Purpose |
|---|---|
| `detect(transport)` | Whether the agent is present at a location, and where its data is |
| `scan(transport)` | The agent's items there: key, kind, session, length and stamp (§5) |
| `hash_prefixes(transport, requests)` | SHA-256 of the first *n* bytes of each requested item |
| `fetch(transport, requests, sink)` | The requested byte ranges of items, streamed |
| `retention(transport)` | How long the agent keeps transcripts there, if it says |
| `index(session items)` | The session's derived fields and search text |
| `parse(session items)` | The transcript model for the reader |
| `tools()` | Which common tool kind each of the agent's tool names is (§7.4) |

- `detect`, `scan`, `hash_prefixes`, `fetch` and `retention` are the only methods that touch a location. `index` and `parse` read from the archive.
- **An item** is whatever unit the provider archives: a byte stream with a key the provider chooses, stable across passes. The engine versions it (§4.2) without knowing what it is.
- **Providers are registered in one list** in `sb_core::provider`. Every location is asked by every provider; each pair where `detect` finds the agent syncs.

### 7.2 Two shapes of source

Agents store transcripts differently, so fetching is the provider's job, with a shared helper for each common shape.

- **A file tree** (Claude Code; any agent that writes files per session). The `FileTree` helper implements `scan`, `hash_prefixes` and `fetch` from a root and a function that classifies a relative path. Items are files, keys are relative paths, and the stamp is size and modification time. A provider for such an agent is three things: where the root is, how paths map to sessions, and a parser.
- **A database** (an agent that keeps its sessions in SQLite). The `DbSnapshot` helper reads the database file and its write-ahead log through the transport into a scratch copy on this machine, reads again if either changed meanwhile, and opens the copy read-only. The provider then defines its items as **exports**: per session, a lossless, deterministic byte stream of that session's rows, one record per line in the order they were inserted.
  - Because records come out in insertion order, a session that gained messages is an append and extends its version; a row edited or deleted changes earlier bytes and starts a new version. §4.2 applies unchanged.
  - The scratch copy is deleted when the pass ends. Only the exports are archived.
  - The export format has a version, recorded with each archived version. Changing it re-exports into new versions; formats are never mixed within one.
  - Nothing is written at the location, and the agent's own tools are not run there (FEATURES principle 4).
- **Neither.** A provider can implement the four methods itself.

Which shape another agent has (opencode is believed to use SQLite; pi is unchecked) is established when its provider is written. Nothing here assumes it.

### 7.3 Claude Code

Observed with Claude Code 2.1.325 on macOS, 2026-11-15, from the structure of real transcripts (file names, record types and key names; no content). Transcript files run from a couple of megabytes to tens of megabytes; the largest seen was about 80 MB. The Windows and WSL layouts are expected to match and are checked in phase 1.

- **Shape:** a file tree (§7.2).
- **Root:** `$CLAUDE_CONFIG_DIR` if set, else `~/.claude`; transcripts are under `projects/`.
- **Layout under `projects/<project>/`**, where `<project>` is the starting directory with its punctuation replaced by `-` (seen for `/`, `.`, `_` and spaces), so the name can't be turned back into the path:

| Path | What it is |
|---|---|
| `<session>.jsonl` | A session's main transcript |
| `<session>/subagents/agent-<id>.jsonl` | A subagent's transcript |
| `<session>/subagents/agent-<id>.meta.json` | The subagent's type, description, and the `toolUseId` of the call that started it |
| `<session>/subagents/workflows/<wf>/agent-<id>.jsonl` (+ `.meta.json`, `journal.jsonl`) | Subagents run by a workflow |
| `<session>/tool-results/<name>.{txt,png,jpg}` | Tool output saved outside the transcript |
| `<session>/custom-title.json` | The session's title, when one was set |
| `<session>/workflows/…` | Workflow definitions and scripts |
| `memory/<name>.md` | The project's memory files (archived; not shown in v1) |

- **A transcript is JSON Lines.** Each record has a `type`. 21 types were seen; the conversation is in `user`, `assistant`, `system` and `attachment`, and the rest is bookkeeping (`api-request`, `queue-operation`, `last-prompt`, `custom-title`, `file-history-snapshot`, and others). The set changes between releases, so the parser keeps what it doesn't know and ignores it (D5).
- **Conversation records** carry `uuid`, `parentUuid`, `timestamp`, `sessionId`, `cwd`, `gitBranch`, `version`, `entrypoint` and `isSidechain`. `assistant` records add the model and usage. Content is a list of blocks: `text`, `thinking`, `tool_use` (assistant) and `tool_result` (user), paired by the tool-use id.
- **Records form a tree** through `parentUuid`: a rewound conversation leaves side branches. The reader shows the branch that ends at the latest record; the others are kept and not shown in v1.
- **`system` records** mark, among other things, a context compaction (`compact_boundary`).
- **Project path** (D9): the first `cwd` in the main transcript. It matched the directory name's encoding in every session sampled; about half of them touched more than one directory later.
- **Title:** the last `custom-title` record or `custom-title.json`; else the start of the first user prompt.
- **Retention:** `cleanupPeriodDays` in the location's `settings.json`. The default is believed to be 30 days; to be confirmed against Claude Code's documentation in phase 1.

### 7.4 The transcript model

Provider-neutral, what the reader renders:

- **Transcript:** session facts (the agent among them), then turns.
- **Turn:** a user prompt and everything up to the next one.
- **Block:** user text (with images), assistant markdown, thinking (text and duration), tool call, compaction boundary, system note.
- **Tool call:** the agent's tool name, its kind, a typed input where the kind is known and raw JSON where it isn't, the paired result, status, duration, and for a subagent call, its transcript.
- **Tool kinds are common to all agents:** shell, read, edit, write, content search, file search, todo list, subagent, web search, web fetch, MCP, other. Each provider maps its agent's tool names onto them (Claude Code: Bash, Read, Edit, Write, Grep, Glob, TodoWrite, Agent, WebSearch, WebFetch, and `mcp__…` tools). The reader has one renderer and one icon per kind (DESIGN §4.2.4) and shows the tool under the agent's own name; a tool a provider doesn't map renders as `other`, with its name, input and output.

### 7.5 Indexing and large files

- Indexing streams a transcript line by line and never holds the file in memory.
- The index stores, per session, the byte offset of each turn, so the reader can open at a turn without parsing what precedes it.
- Parser output is cached in memory per open session only; nothing parsed is written to the archive except the derived tables.
- The index rows carry the parser version. A new version re-indexes in the background, newest sessions first.

### 7.6 Adding an agent

1. A module under `provider/` that implements the trait, on `FileTree` or `DbSnapshot` where one fits.
2. One line in the provider list.
3. Its tool names mapped to the common kinds.
4. A display name and a mark for the UI.
5. A fixture builder for its storage in `sb_test_support`, and golden parser tests.

Nothing else changes: no migration, and no change to the archive, sync, filing, search or the reader. If adding an agent needs one, the seam is wrong and is fixed first.

---

## 8. Filing

- **Normalising a path** for comparison: split into components; on Windows-style paths (`X:\…`) fold case and accept either separator; map a WSL mount path to its drive form (§6.3); drop a trailing separator. The path as typed is kept for display.
- **A session's candidates** are the attached paths whose components are a prefix of the session's project path, and whose location is unset or is the session's.
- **The longest candidate wins.** Its folder gets the session, marked as filed by rule.
- **The same path can't be attached to two folders** for the same location scope; the second attach is refused and names the first folder.
- **When rules run:** on a new session; and when a folder's paths change, over sessions in Unsorted and sessions filed by rule. A session filed by hand is never moved by a rule.
- **File automatically** hands a hand-filed session back to the rules: it clears the mark and runs filing on that session.
- **A folder contains its subfolders' sessions.** Its page lists them and its count includes them.
- **Deleting a folder** sends its sessions to Unsorted and drops its paths; its subfolders move up to its parent.
- **Unsorted** is every session without a folder, grouped by project path.

---

## 9. Reader

- **A virtualised list of blocks.** Tool calls render as fixed-height activity rows until expanded, so a session with hundreds of calls has a known height and scrolls without measuring everything.
- **Parsing is off the UI thread.** The reader opens on the first turns from the index's offsets (§7.5) and fills in as parsing proceeds.
- **Tool output is loaded when its row is expanded,** including output saved in `tool-results/`.
- **Markdown** per D11. Code blocks and diffs are highlighted with tree-sitter, producing token kinds that the theme colours, as dv's `syntax` does.
- **Diffs** for Edit and Write come from the tool call's own input and result. dv's diff rows and highlighting are lifted (D15).
- **Subagents** render through the same block list, nested and collapsed by default, located by the `toolUseId` in their metadata.
- **Find in session** searches the parsed text of the open session, not the archive's index.

---

## 10. Search

- **One FTS5 table,** a row per indexed span: session, turn, kind (title, prompt, reply, tool call), text. The tokenizer is chosen in phase 6 against real queries; `trigram` allows substring matches (paths, identifiers) at the cost of index size.
- **Indexed:** titles, user prompts, assistant text, tool calls' commands and paths. **Not indexed in v1:** tool output and thinking (D10).
- **Subagent transcripts are indexed** under their parent session. A match in one opens the parent at the call that started the subagent, expanded.
- **Filters** (folder, location, agent, model, branch, date) are ordinary columns joined to the match.
- **A result** carries the session and the turn, so it opens the reader at that turn.

---

## 11. UI architecture (`sb_ui`)

- **Services,** as gpui globals or entities, each the only writer of its state: `Archive` (open archive, catalogue reads), `Sync` (passes, location states, progress), `Library` (folders, filing, selection), `Settings`.
- **Views:** `shell` (title bar, sidebar, main area), `sidebar` (library rows, folder tree with drag and drop, locations), `folder_page` (header, path chips, filters, cards), `reader` (header, block list, facts margin), `locations_sheet`, `first_run`, `search` (the `Ctrl K` palette), `versions` (the history popover), `settings`.
- **Sync runs on background tasks** and reports through the `Sync` service; views observe it. No view starts a process.
- **Drag and drop:** sessions onto folders, folders into folders, a path chip onto a folder. folio's sidebar tree is the precedent, including a pre-rendered drag ghost (GPUI can't rotate a div).
- **Views never name an agent.** A session's agent is shown from the provider's display name and mark (§7.6), beside its model, and agent is a filter wherever model is.
- **Keyboard first,** as the siblings: every action has a key and appears in the palette. The key map is DESIGN's.
- **Automation:** views register targets with `gpui_automation` for the scenarios (§12).

---

## 12. Testing

- **Fixture trees, never real transcripts.** `sb_test_support` builds a Claude Code `projects/` tree in a temp dir: sessions with chosen sizes, tools, subagents, branches and bookkeeping records. The owner's transcripts are private and never enter the repository or a test.
- **Sync, as unit tests over fixtures:** first sync; append; rewrite; truncate; touch without change; deletion at the source; a file that ends mid-line; a pass cancelled at each step and resumed; a crash between rename and commit.
- **The shell transport, hermetically:** the `Command` launcher runs the same scripts through a local `sh` against a fixture tree. On Windows these tests run inside WSL, as dv's Linux tests do. GNU and BSD tool variants are both covered (Linux and macOS runs).
- **The provider seam, with a second provider.** A test-only provider in `sb_test_support` keeps its sessions in a SQLite database and uses `DbSnapshot` (§7.2). The same sync tests run against it: first sync, rows appended, a row edited, a session deleted at the source. Search, filing and the reader's model are exercised on its sessions too. It exists so the first real second agent is not the first test of the seam.
- **No agent's name outside its provider:** `script/check` fails if `claude` appears in `sb_core` outside `provider/claude_code`, or anywhere in `sb_ui` outside the provider's display name and mark.
- **Filing:** table tests of path normalisation and longest-prefix matching, including the WSL mount mapping and Windows case.
- **Parser:** golden tests from fixture transcripts; a record of an unknown type and a tool of an unknown name must survive.
- **GUI scenarios** with `gpui_automation_runner`: first run → add a local location → sync → a session appears in Unsorted → create a folder from its path → open the session → expand a tool call.
- **The owner's live checks,** not automated: a real WSL distro on the PC; SSH from the PC to the Mac; a first sync of the real archive, timed.

---

## 13. Phases

Each phase ends with its tests green and, from phase 3 on, a screenshot review against the design.

| Phase | Scope | Done when |
|---|---|---|
| 0 | Workspace on the pinned stack, crates per §3, `script/check`, toolbox-rs dependencies, an empty window in the E shell, the automation host | The shell screenshots in light and dark on Windows and macOS |
| 1 | `sb_core`: archive layout, catalogue, `LocalTransport`, the `Provider` trait with `FileTree` and `DbSnapshot`, the Claude Code provider's `detect`, `scan` and `fetch`, the test-only database provider, the sync engine (§4, §5, §7.1, §7.2) | The sync unit tests pass for both providers; the Mac's own `~/.claude` syncs into a scratch archive and verifies; Claude Code's retention setting is confirmed |
| 2 | Parsing and indexing (§7), filing (§8) | Fixture sessions index with titles, paths and counts; filing tests pass; a parser-version bump re-indexes |
| 3 | Library UI: sidebar, folders with drag and drop, folder page, Unsorted, first run with a local location, locations in the sidebar and the sheet | The first GUI scenario passes up to opening a folder |
| 4 | Reader (§9): markdown per D11, activity rows, each tool renderer, subagents, facts margin, find in session | The owner reads real sessions of each size class; an 80 MB session meets the budget (§14) |
| 5 | Shell transport (§6.2), WSL locations (§6.3), then SSH (§6.4); location states and staleness | Loopback tests pass on Linux and macOS; the owner's live WSL and SSH checks pass |
| 6 | Search (§10), filters, versions UI, Trash and deleting for good (§4.5) | A `Ctrl K` query opens a session at the matching turn; a held-back version can be chosen; a session deleted for good stays gone after the next sync |
| 7 | Settings, polish, performance pass, packaging and updates as dv does (its D9), CI | Design review of every screen; budgets met; an installed build syncs the owner's real WSL archive |

Phase 5 is late only because Local exercises the whole pipeline on the development Mac first. If the owner wants the WSL backup running early, phases 1 and 5 can be done back to back before any UI, with a temporary command-line trigger.

---

## 14. Risks and open questions

- **Budgets,** to be measured, not yet met by anything: a first sync limited by the transport, not by scrollback (compression and hashing faster than a WSL read); a pass with nothing changed over 20,000 files in under 5 s for Local and under 15 s for WSL; opening an 80 MB session to first paint in under 500 ms; scrolling a 500-call session at the display's refresh rate.
- **Claude Code's format changes between releases.** The provider must tolerate it (D5), and a fixture per observed release guards the parser. The layout in §7.3 is from one machine and one release.
- **The provider seam is proven by a test provider only** until a second real agent exists (D8, §12). The first real one may still show something the trait lacks; §7.6's rule is to fix the seam then, not to work around it.
- **Reading a live database** (§7.2). `DbSnapshot` copies a database that its agent may be writing, without locking it. Re-reading when the files changed and opening the copy read-only should give a consistent snapshot; this is checked against a real agent's database when the first such provider is written, along with how large those databases get.
- **The shell transport across real hosts** (D6): BSD and GNU tool differences, unusual shells, slow links. The connection test is where problems should surface. The fallback is a helper binary behind `Transport`.
- **Provenance when lifting** (D15). Each file lifted from a sibling is checked for where it came from; GPL-derived code is rewritten or replaced, not copied.
- **Index size.** FTS5 with `trigram` over a year of prompts and replies; measured in phase 6 before the tokenizer is fixed (D10).
- **Deleting for good is the one irreversible act** in the app (§4.5). It needs its own tests: nothing outside the session's files is removed, and a location is never touched.
- **Many small segments.** A session synced while it is active gains a segment per pass. Harmless for correctness; compaction is listed for later.
- **The same conversation in two sessions.** Resuming or forking can copy earlier history into a new transcript file, so text is archived twice and search shows both. Linking them is listed for later.
- **Two apps on one archive** are prevented by the lock (§4.4). A read-only second opener is not planned.
- **Secrets.** Transcripts contain whatever the agent saw. The archive is not encrypted (FEATURES: out of scope); it is as private as its folder.
- **One archive per machine.** Each machine the app is opened on has its own archive; they are independent until merging exists (FEATURES: Later).
- **WSL details to check on the PC:** the distro's default user and `$HOME`, a custom mount root in `/etc/wsl.conf`, `wsl.exe`'s behaviour when the distro is stopped or WSL is updating.
