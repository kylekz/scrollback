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

### D11. Markdown: the stack's `TextView`, wrapped *(measured; the owner has not yet seen the result)*
Replies are rendered with the `TextView` that ships with the pinned stack (it lives in `gpui-base`; `gpui-component` re-exports a thinner one), behind a wrapper in `sb_ui::markdown` that applies the theme, feeds it token kinds from our tree-sitter highlighter and adds the code block's header. A spike rendered a generated corpus of every construct and, for numbers only, the real replies on one machine:
- **Coverage:** everything replies contain renders correctly: headings, emphasis, links, nested lists, task lists, quotes, tables, fenced code in a dozen languages, malformed input. Real replies are small (nine in ten under a kilobyte, none over ten) and mostly inline code, bold and lists.
- **It already has what an own renderer would have to write:** selection across blocks, copy, and find highlighting. folio's renderer has none of those, nor tables, and its provenance would need clearing first; adapting it was estimated at several times the work.
- **Rules that follow:** keep parsed states only for replies near the viewport (a state held per reply costs about half a megabyte and slows frames); sanitise raw HTML and resolve images ourselves before real transcripts are shown, as dv does.
- **Known gaps, accepted unless the owner objects:** the code header's language label sits at the right beside Copy; no horizontal scroll in code (long lines clip or wrap); nested ordered lists are lettered; a link's underline colour and the find mark's style can't be set; a selection is lost when its row scrolls far out of view. Closing them means patching the stack or getting options accepted upstream.
- **Unmeasured:** Windows, and the large bodies likely in the bigger archive (web-fetch results, compaction summaries, pasted prompts). A body of about 100 KB takes about 100 ms to lay out and several milliseconds a frame while visible.

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
  orphans/                        files that were under files/ without a catalogue record, set aside
  files/<location-id>/<provider>/<ab>/<item-id>/
      source.json                 provider, location, the item's key
      v0001/0000000000000000.zst  bytes [0, a) of version 1
      v0001/0000000000184467.zst  bytes [a, b): an append
      v0002/0000000000000000.zst  the source was rewritten: version 2 starts at 0
```

- **`<item-id>`** is the first 32 hex digits of the SHA-256 of the item's key; `<ab>` is its first two digits. Keys, which for files are source paths, are not mirrored on disk: Claude Code's project directory names alone run past 170 characters, which with the archive's own prefix exceeds Windows' 260-character limit, and Linux paths can hold characters NTFS rejects. `source.json` keeps the original path, so the tree is still recoverable without the database.
- **A segment** is the zstd-compressed bytes of one range, named by its 16-digit offset. A version's content is its segments decompressed in order. A segment's uncompressed size is capped, so a large range is stored as several and a read can start at any offset by opening the segment that holds it.
- **Writing a segment:** write to `tmp/`, flush to disk, rename into place, then record it in the catalogue in one transaction. A crash before the transaction leaves an uncatalogued file, and nothing goes looking for it at a start (a start only empties `tmp/`). When a later pass places a segment at that path, a file in the way that the catalogue doesn't record is kept if it is byte-identical to the new segment, and moved to `orphans/` under the same path if it differs; so a killed pass leaves nothing behind once it is retried, and after a catalogue is restored from an older backup the segments stored since are adopted again wherever the source still has the same bytes. A file the catalogue records is never touched or replaced. Verify reports what is left unrecorded (§4.4), and the owner can have it set aside.
- **The transport hashes in request order.** The snapshot proof (§7.2) depends on it, so every transport must keep it.
- **Hashes:** the catalogue stores the SHA-256 of each segment's uncompressed bytes and of each version's content up to its current length. SHA-256, because a remote can compute it with standard tools (§6.2).

### 4.2 Versions

Given the archived length `L` and hash `H` of the current version, and the source's size `S`:

| Source | Plan | The proof decides |
|---|---|---|
| Size and modification time unchanged | Unchanged: skip | |
| `S > L` | Fetch `[L, S)` | The source's hash of `[0, S)` equals the archived prefix plus the fetched bytes: **appended**, a new segment of the current version. Otherwise fetch the whole file as version n+1: **rewritten** |
| `S == L`, time changed | Fetch nothing | The source's hash of `[0, L)` is `H`: **touched**, record the new time. Otherwise fetch the whole file as version n+1: **rewritten** |
| `S < L` | Fetch the whole file as version n+1 | **Shrank** |
| Not present | | Deleted at the source: mark gone; keep everything |

- **Planning reads nothing at the source.** It compares the scan with the catalogue. Every range that is fetched is then proven: after it is read, the source is asked for the hash of everything up to the range's end, and the range is stored only if that equals the hash of what the version would then hold. Size and modification time decide what to look at; only the hash decides what is stored.
- **A file modified within two seconds of the scan** is given a stamp marked fresh, which never equals the stamp a later scan gives it once it has aged, so it is looked at once more rather than trusted as unchanged.
- **The table is the same for an item that isn't a file.** Its provider gives it a length, a stamp and a way to read a range (§7.1), so records appended to a database-backed session extend its version just as appended lines extend a file's (§7.2).
- **A line has a bound.** A line-oriented item whose unfinished last line passes 32 MiB is stored as far as it goes and proven like any other item, so no file can make a pass hold gigabytes in memory.
- **Line-oriented items (JSONL) are cut at the last newline while they are changing.** A transcript being written may end mid-record; the tail after the last `\n` is left for the next sync. When a later pass finds the item with the size and stamp it had before, the tail has settled and is stored too, so a file that never gets its final newline is still archived whole. A stored version can therefore end mid-line, and parsers tolerate it (§7.5). Other items are taken whole.
- **A version only ever holds bytes the source held at one instant.** That is what the proof above enforces. An item whose proof fails because it was changing under the pass is deferred to the next one, and the catalogue records which items are waiting and since when.
- **Versions belong to items, not sessions.** The version number the reader shows for a session is its main transcript's. The session's other items (subagent transcripts, saved tool results) version on their own.
- **Held back.** Only transcripts (a session's own and its subagents') can be held back; for any other item the newest version is the one read. A new version of a transcript is held back when it is shorter than the reading copy (the newest version that is not itself held back), or parses worse than it: it has fewer recognised blocks over all branches. An append can therefore never be held, and damage in the archive is not a parsing matter (verify and the refetch deal with it, §4.4). The reading copy stays, and the newer version is flagged for the owner (FEATURES: Versions). A hold for parsing is recorded apart from a hold for length. Storing never lifts a hold: an append to a held version leaves it held, and the index lifts it once the version is no shorter than the copy before it and parses no worse. A read that failed, or damage in the archive, never decides a hold.
- **The owner's choice** is per item. Choosing a held-back version accepts it: its hold is cleared for good and later versions follow the rule as usual (if the rule would still read another version, the acceptance also acts as a pin). Choosing an older version pins it until a version arrives that is newer than any that existed when the choice was made, or until the pinned version is found damaged; then the pin lapses. A damaged version can't be chosen.
- **Each version records why it started:** first sync, rewritten, shrank, replaced (the item was gone and came back with other content, or its export format changed), refetched (the version before it was found damaged, §4.4).

### 4.3 Catalogue

Tables, in outline (the schema is `toolbox_store` migrations, append-only):

| Table | Holds |
|---|---|
| `location` | id, kind, name, connection details (distro, or SSH host), its drive mount when it has its own, last sync attempt and success, last error, unreachable since and why |
| `location_provider` | per provider found at a location: the retention last read |
| `item` | location, provider, key, item id, kind (transcript, subagent, tool result, sidecar), the session it belongs to, last seen length and stamp, gone-since |
| `version` | item, number, reason, started, length, hash, held-back, held for parsing, accepted by the owner, damaged, the provider's export version when the item is an export |
| `segment` | version, offset, length, hash, stored-at |
| `folder` | id, parent, name, position |
| `folder_path` | folder, normalised path, the path as typed, location (nullable) |
| `session_state` | session identity → folder, filed-by (rule or hand), starred, trashed-at (phase 6). Keyed by identity, never by the derived row, so it survives a rebuild of the index |
| `reading_choice` | item → the version the owner chose to read; removed when a newer version of the item starts (§4.2) |
| `tombstone` | session identity of a session deleted for good, and when |
| `session` *(derived)* | session identity, main item, title, project path, first and last timestamp, models, branch, entrypoint, counts, tokens, parser version |
| `session_text` *(derived)* | the FTS5 index (§10) |
| `kv` | settings that belong to the archive |

### 4.4 Opening, locking, verify

- The archive defaults to a folder in the app's data directory, created on first run (D3). Its path is a setting, kept outside the archive. If the folder can't be reached (an unplugged drive, a changed letter), the app says so and offers to locate it; it never creates a fresh archive silently in its place.
- `lock` stops a second app from opening the same archive for writing.
- **An archive folder without its database is not opened as empty.** Opening fails and names the newest backup in `backups/`; a missing `archive.json` means the folder is not an archive.
- **Pointing the app at a different folder** opens the archive there, or creates one if the folder has none. It moves nothing; moving an archive is moving its folder and then pointing the app at it.
- **Verify** re-hashes every segment against the catalogue and every version's running hash, and reports mismatches, missing files and files under `files/` that the catalogue doesn't record, which the owner can then have moved to `orphans/`. It repairs nothing by itself: it marks each damaged version (a missing segment, one that doesn't decode, or a hash mismatch; a read error alone marks nothing). The next sync fetches an item whose newest version is damaged whole, as a new version, when the item still exists at its location; **Fetch again from its location** runs that for one item at once. Otherwise the version stays marked and readable as far as it decodes. Reading a version checks each segment against the catalogue as it goes and fails rather than return wrong bytes.

### 4.5 Deleting

- **Delete** moves a session to Trash: `session_state.trashed-at` is set. Its bytes stay, sync keeps archiving its files, and it disappears from every view except Trash. **Restore** clears the mark and returns the session to its folder; if that folder was deleted meanwhile, the session goes to Unsorted and the filing rules run on it (§8).
- **Delete for good** (from Trash, after a confirmation that names the session) removes the session's items under `files/`, their catalogue rows and its derived rows, and writes a tombstone in the same transaction as the rows. The files are removed after the commit. Nothing removes unrecorded files by itself (§4.1), so the removal is recorded as pending in the same transaction and finished on the next start if a crash interrupts it.
- **A tombstoned session's items are skipped by sync** (§5), so deleting isn't undone by the next pass while the session still exists at its location.
- **Removing a tombstone** (Settings lists them) lets the next sync archive the session again, if its location still has it.
- Trash is never emptied automatically.

---

## 5. Sync

One **pass** syncs one location with one provider. The engine drives it and knows nothing about the agent; the provider does the two steps that touch the location, through the location's transport (D8):

1. **Scan** (provider): every item the agent has at the location: its key, kind, session, length, and a stamp that changes when the item does. For a file the stamp is its size and modification time.
2. **Plan** (engine): drop the items of tombstoned sessions (§4.5), compare the rest with `item` and decide per item what to fetch (§4.2). Planning touches nothing at the location.
3. **Fetch and prove** (provider), a batch at a time: the planned byte ranges, in one stream, then one request for the source's hash up to the end of each.
4. **Store** (engine): each proven range becomes one or more segments (§4.1); the batch is made durable and its rows are committed in one transaction. A range whose proof fails is not stored: the item is fetched whole as a new version later in the pass, or **deferred** to the next pass if it is still changing.
5. **Index** (engine, with the provider's parser): sessions whose items changed are parsed and indexed (§7.5), and filed (§8). Which sessions are stale is decided from the catalogue, so an index that was cancelled or killed is picked up by the next pass or the next start. It runs whenever the store step ended for any reason but a cancel, after the pass's outcome is recorded. One session that fails to index is recorded and doesn't stop the others, and isn't a failure of the sync.

- **Passes for different locations run in parallel.** One location runs one pass at a time; when it has several agents, their passes follow one another.
- **One item's failure is one item's.** A file that can't be read, a folder that can't be listed and a name that isn't Unicode are reported with the pass; everything else is archived, and nothing under an unlisted folder is marked gone. One provider's failure doesn't stop the others at the location. A pass counts as a success for the location only when nothing failed and nothing has been waiting, deferred pass after pass, for more than a day; otherwise the location keeps its last success and records how many items could not be archived and why.
- **A pass can stop anywhere.** It works in batches (a few hundred items or tens of megabytes), so a cancel, a crash or a dropped connection loses at most the batch in flight. Everything committed stays; the next pass continues from the catalogue. Cancelling is the same as a dropped connection.
- **Triggers:** app start, the owner (one location or all), and an interval while the app is open. Any of them starts a stopped WSL distro to sync it *(owner)*; only listing distros never starts one (§6.3).
- **States** a location shows: synced, syncing (files and bytes done of planned), unreachable (the launcher failed to connect: since when, last success and how long ago), failed (anything else, with the tool's message). Unreachable is not an error and isn't surfaced as one; it is recorded with the location and cleared by the next sync that reaches it. A failure to connect is the location's, recorded once, and what earlier batches stored is kept and indexed.
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

- **Each operation is one child process** running `sh -s`. Its stdin carries a fixed script (one function per operation) and then the requests, one quoted function call per line, inside a block that takes its own input from `/dev/null`. The shell has parsed everything before it runs anything, no tool can read the script as input, and nothing but a fixed argument list crosses a command line, so no quoting rule of `wsl.exe`, `ssh` or the remote shell applies. No here-document or temporary file is used; nothing is ever written on the far side.
- **The answer is framed.** It begins at a marker line chosen per call, so anything a login script prints first is skipped, and ends with a terminator; an answer without its end fails the call. Names are NUL-terminated and blocks are counted, so no file's contents or name can be mistaken for the protocol.
- **Listing** uses `find` with batches of `stat`, the far side's own clock, and a second pass that reports folders the user may not read or search. GNU, BSD and busybox `stat` differ; the script recognises which it has from the shape of its output, in one place. A file is taken for missing only when the tool says so in so many words; any other failure makes it unlisted, never gone (§5).
- **Paths that start with `-`** are normal here (Claude Code's project directories all do), so every relative path is prefixed with `./` and every tool gets `--`.
- **Reading ranges:** a block is always exactly the length asked for plus one, the file's bytes followed by a mark and zero padding, so a file that shrinks or vanishes mid-read ends as short or missing and the stream stays in frame. A range inside a large file seeks with `dd` and trims with `tail`. Runs of small whole files are read through one `cat` between two `stat`s; a run that doesn't hold together is asked again file by file.
- **Hashing** uses `sha256sum`, else `shasum -a 256`, over `head -c <n>`, one request after another in order (§4.1). Runs of whole files are hashed by one invocation, each output line checked against its path and each size checked afterwards.
- **The environment** a provider asks for (where an agent keeps its data) is read through the far side's login shell, once per pass, because `wsl.exe --exec` and `ssh host command` don't run profiles. A shell that can't be asked this way (csh, a failing profile) leaves the bare environment. A login shell still starting after five seconds is stopped and the pass fails, rather than looking for the agent in the wrong place. A variable set only for interactive shells is still not seen; the connection test shows which environment answered and the root that was resolved.
- **Timeouts and cancel:** stopping the pass kills the process tree, and nothing is left running on the far side once the pipe closes. A call that receives nothing for ten minutes is ended as a failure. The script is written from its own thread, so a launcher that is slow to start can still be cancelled.
- **What the remote needs:** `sh`, `find`, `stat`, `head`, `tail`, `dd`, `cat`, `date`, `/dev/zero`, and `sha256sum` or `shasum`. A missing tool is reported by name when the location is added (the connection test) and as the location's failure after that.
- **Cost:** a pass with nothing changed is four processes. A first sync costs under a millisecond per small file in the transport on a local shell; through `wsl.exe` it is to be measured again. One long-lived shell per pass, which would spare SSH a connection per call, is listed for later.

### 6.3 WSL

- **Discovery:** the installed distros and whether each is running, from `wsl.exe --list --verbose` (its output is UTF-16). dv already does this and never starts a stopped distro to list it; its code is the reference.
- **A WSL location is a distro and the distro's default user.** Each provider finds its agent's data inside it (for Claude Code, `$HOME/.claude`), not through a `\\wsl.localhost` path.
- **Adding a stopped distro** doesn't run the connection test, which would start it. It is added as it is, and its first sync starts it.
- **Launch:** `wsl.exe -d <distro> --cd / --exec sh -s`, with `WSL_UTF8=1` so its own messages are readable. Distro names are matched without case, as `wsl.exe` does, and stored as the listing spells them. Docker's and Rancher's utility distros are marked so the UI can leave them out.
- **Path equivalence** for filing (§8): a WSL path under the distro's Windows mount (`/mnt/<letter>/…` by default) is the same project as `<LETTER>:\…`. The mount root comes from the distro's `/etc/wsl.conf` when it sets one; it is read at every sync of a WSL location, and when it changes, the location's project paths and attached paths are read again under the new root and its sessions refiled.

### 6.4 SSH

- **A location is a host string** exactly as the owner would give it to `ssh`: an alias from `~/.ssh/config` or `user@host`.
- **Launch:** `ssh -T -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -o ClearAllForwardings=yes -o ControlMaster=no -o LogLevel=INFO -- <host> sh -s`. No password or passphrase prompt can appear; a host that needs one fails with `ssh`'s message, which the location shows. The keep-alives stop a host that goes to sleep mid-pass from hanging the sync.
- **How a failure is read:** `ssh` exiting with its own error is unreachable when it couldn't resolve, reach or stay connected to the host, or gave up without a word (not an error, §5), refused when sign-in was refused, and a host-key failure when the key changed, which is shown plainly and never bypassed. Any other failure carries the tool's last line as its reason.
- **Adding** runs the connection test: connect, find the tools, then ask each provider what it finds there (which agents are present, and how many sessions). A host that fails the test can still be added, so a machine that is asleep can be set up now and synced when it wakes.
- Windows ships an OpenSSH client; if `ssh` isn't on `PATH`, adding an SSH location says so.

---

## 7. Providers and the transcript model

### 7.1 The `Provider` trait

Everything specific to an agent is behind this trait (D8). The archive, the sync engine, filing, search and the reader are written against items, sessions and the transcript model (§7.4), and never name an agent.

| On | Method | Purpose |
|---|---|---|
| `Provider` | `id()` | The agent's id: lower-case, stable, used in the archive's paths |
| `Provider` | `detect(transport, scratch)` | Whether the agent is present at a location; if so, a `Source` for this pass. `scratch` is a folder in the archive's `tmp/` for anything the source needs to keep during the pass |
| `Source` | `scan()` | The agent's items there: key, kind, session, length and stamp (§5) |
| `Source` | `hash_prefixes(requests)` | SHA-256 of the first *n* bytes of each requested item |
| `Source` | `fetch(requests, sink)` | The requested byte ranges of items, streamed |
| `Source` | `retention_days()` | How long the agent keeps transcripts there, if it says |
| `Provider` | `parse(session items)` | The transcript model (§7.4) of a session's own conversation, from its archived items |
| `Provider` | `subagent(session items, item)` | One subagent's conversation, read when asked for |
| `Provider` | `classify(key)` | The kind of an item and the session it belongs to, from its key alone |
| `Provider` | `parser_version()` | Bumped when parsing or classifying changes; drives re-indexing (§7.5) |

- **A `Provider` is what is known about an agent; a `Source` is that agent at one location for one pass.** The source holds what `detect` found (the root) and any scratch state (a database snapshot, §7.2), and is dropped when the pass ends. Only a source touches a location. `parse` and `subagent` read from the archive, and never fail: what can't be read is recorded in the transcript. The index (§7.5) and the tool kinds (§7.4) are derived from the parsed transcript, not asked of the provider.
- **An item** is whatever unit the provider archives: a byte stream with a key the provider chooses, stable across passes. The engine versions it (§4.2) without knowing what it is.
- **Providers are registered in one list** in `sb_core::provider`. Every location is asked by every provider; each pair where `detect` finds the agent syncs.

### 7.2 Two shapes of source

Agents store transcripts differently, so fetching is the provider's job, with a shared helper for each common shape.

- **A file tree** (Claude Code; any agent that writes files per session). The `FileTree` helper implements `scan`, `hash_prefixes` and `fetch` from a root and a function that classifies a relative path. Items are files, keys are relative paths, and the stamp is size and modification time. A provider for such an agent is three things: where the root is, how paths map to sessions, and a parser.
- **A database** (an agent that keeps its sessions in SQLite). The `DbSnapshot` helper reads the database file with its write-ahead log and rollback journal through the transport into a scratch copy in the archive's `tmp/`, has the location hash the journals, the database and the journals again in one request, and keeps the copy only if every hash and length matches and nothing grew (copying again otherwise, and giving up after a few attempts on a database that never stops committing), and opens the copy with writes refused (SQLite itself may still roll a hot journal back in the copy). SQLite never opens the database at the location. The provider then defines its items as **exports**: per session, a lossless, deterministic byte stream of that session's rows, one record per line in the order they were inserted.
  - Because records come out in insertion order, a session that gained messages is an append and extends its version; a row edited or deleted changes earlier bytes and starts a new version. §4.2 applies unchanged.
  - The scratch copy is deleted when the pass ends. Only the exports are archived.
  - The export format has a version, recorded with each archived version. Changing it re-exports into new versions; formats are never mixed within one.
  - Nothing is written at the location, and the agent's own tools are not run there (FEATURES principle 4).
- **Neither.** A provider can implement the four methods itself.

Which shape another agent has (opencode is believed to use SQLite; pi is unchecked) is established when its provider is written. Nothing here assumes it.

### 7.3 Claude Code

Observed with Claude Code 2.1.325 on macOS, 2026-11-15, from the structure of real transcripts (file names, record types and key names; no content). Transcript files run from a couple of megabytes to tens of megabytes; the largest seen run past 100 MB. The layout inside WSL was checked in phase 1 and has the same shape.

- **Shape:** a file tree (§7.2).
- **Root:** `$CLAUDE_CONFIG_DIR` if set, else `~/.claude`; transcripts are under `projects/`.
- **Layout under `projects/<project>/`**, where `<project>` is the starting directory with its punctuation replaced by `-` (seen for `/`, `.`, `_` and spaces), so the name can't be turned back into the path:

| Path | What it is |
|---|---|
| `<session>.jsonl` | A session's main transcript |
| `<session>/subagents/agent-<id>.jsonl` | A subagent's transcript |
| `<session>/subagents/agent-<id>.meta.json` | The subagent's type, description, and the `toolUseId` of the call that started it |
| `<session>/subagents/agent-<id>.prefix.json` | Bookkeeping beside a subagent (also beside workflow agents) |
| `<session>/subagents/workflows/<wf>/agent-<id>.jsonl` (+ `.meta.json`, `journal.jsonl`) | Subagents run by a workflow |
| `<session>/tool-results/<name>.{txt,json,png,jpg,html}` | Tool output saved outside the transcript; the folder is flat |
| `<session>/custom-title.json` | The session's title, when one was set |
| `<session>/workflows/…` | Workflow definitions and scripts |
| `memory/<name>.md` | The project's memory files (archived; not shown in v1) |
| `<session>.<name>.json`, `sessions-index.json`, other files | Sidecars beside a transcript, and project-level files that belong to no session |

- **The tree is irregular, and all of it is archived.** Seen on macOS and inside WSL across many releases: a session folder with no main transcript (most still hold subagent transcripts), a transcript with no folder, stray `.txt` files in a session folder, empty files and empty `memory/` folders. A file the provider doesn't recognise is still an item. A project-level file belongs to a session only when its name starts with a session id, which is UUID-shaped; otherwise it is a project item with no session.
- **Names are tame:** project folder names start with `-`, use letters, digits and `-`; no links or special files were seen, and no two keys that differ only in case.
- **A transcript is JSON Lines.** Each record has a `type`. Over twenty types are seen; the conversation is in `user`, `assistant`, `system` and `attachment`, and the rest is bookkeeping (`api-request` and its relatives, which are about a quarter of a transcript's bytes, `queue-operation`, `last-prompt`, `custom-title`, `file-history-snapshot`, and others). The set changes between releases. The parser lists the types and the `attachment` and `system` kinds it knows carry no conversation; anything not listed is kept and shown as unrecognised, never dropped silently (D5).
- **Conversation records** carry `uuid`, `parentUuid`, `timestamp`, `sessionId`, `cwd`, `gitBranch`, `version`, `entrypoint` and `isSidechain`. `assistant` records add the model and usage. Content is a list of blocks: `text`, `thinking`, `tool_use` (assistant) and `tool_result` (user), paired by the tool-use id.
- **A reply is split across records:** mostly one `assistant` record per content block, all sharing `message.id` and repeating the same `usage`, so tokens are counted once per message id. Records are also written again under a `uuid` already used.
- **Records form a tree** through `parentUuid`, and it forks without any rewind: calls made at once chain their `tool_use` records and each result hangs off its own call, so walking up from the latest record would drop results. Parents are resolved after the whole file is read. Everything is shown except a side branch whose first words are the user's prompt, which is what a rewound conversation leaves; those are kept and not shown in v1. No rewound conversation has been observed yet, so that shape is assumed.
- **Much text in the user's role is not the user's.** Markers are `isMeta`, `origin.kind` and wrapper elements (reminders, command output, task notifications). Leading wrappers are peeled off as notes and the rest is judged on its own. A message from another session is a prompt with its sender; a prompt delivered mid-turn exists only as an `attachment` of kind `queued_command`; an interruption is a note. Many sessions have no prompt typed by a person at all.
- **Compaction:** a `system` record of subtype `compact_boundary` with no parent, a `logicalParentUuid` and the token counts before and after; its summary is a `user` record marked `isCompactSummary`.
- **Saved output:** a result too large for the transcript is a note in the transcript carrying the path of a file under `tool-results/`; the file's name is not the tool-use id. Images are inline base64, and image files under `tool-results/` duplicate them.
- **Subagents** are linked to the call that started them by the `agentId` (or, for a workflow, the `runId`) in the call's result, with the `toolUseId` in `.meta.json` as the fallback. A subagent can start subagents. A few link to nothing and are kept as unattached.
- **A session's key is its project folder and its id.** The same id can appear under two project folders; those are two sessions.
- **Older releases wrote subagents inline:** records marked `isSidechain` inside the main transcript, with their own parent chain. They are gathered into a subagent conversation, attached to the call whose prompt matches when exactly one does, and never shown as the owner's turns. This shape is from memory of older releases and is unverified until the check runs over an older archive.
- **Shell mode:** a command the owner ran with `!` is a prompt whose text is the command; its output is a note.
- **Project path** (D9): the first `cwd` in the main transcript. It matched the directory name's encoding in every session sampled; a minority touched more than one directory later.
- **Title:** the last `custom-title` record, else `custom-title.json`; else the start of the first prompt.
- **Retention:** `cleanupPeriodDays` in `settings.json`, 30 days when absent and at least 1 (confirmed against Claude Code's documentation). The same cutoff removes a session's `subagents/` and `tool-results/`. Sessions started from the desktop app have their own setting, `desktopSessionCleanupPeriodDays`. The provider reads the user-level `settings.json` at the agent's root only; a project or managed override is not seen. When the sweep runs, and whether age is by modification time, is not documented.

### 7.4 The transcript model

Provider-neutral, what the reader renders:

- **Transcript:** session facts (the agent, title, project path, branch, entrypoint), the session's own conversation, and any subagents no call is known to have started. A session whose main transcript is missing still has a transcript: no conversation, and its subagents unattached.
- **Conversation:** blocks in recorded order with their parent links, and the turns that are shown. A turn is a prompt and everything up to the next one; a conversation's first turn may have no prompt. A conversation says how it ended: whole, inside a record (a version can end mid-line, §4.2), or unreadable from some point (§4.4), keeping what was read.
- **Block:** a prompt (text, images, and its sender when it isn't the owner), a reply, thinking (text and duration), a tool call, a compaction boundary, a note (text the agent's harness wrote, by kind), or an unrecognised record kept raw.
- **Tool call:** the agent's tool name, its kind, a typed input where the input fits the kind and the raw input always, the result, and its subagents. A result has a status, a time, its output parts (text or image) and a **brief**: what a collapsed row shows without loading output (exit status, lines, lines changed, matches, a subagent's run), captured at parse time where the source says.
- **Large bodies are references, not text.** Tool input and output, file bodies, subagent prompts, summaries and images are an item, an offset and a length, loaded when a row is expanded. Prompts, replies, thinking and notes are in memory.
- **Subagents are read on demand.** Parsing a session reads its main transcript only (tens of milliseconds for the largest seen); a subagent's conversation is read when its call is expanded, and the index reads them all.
- **Conversations are made through one builder** that holds the model's rules (turn splitting, parent before child), so every provider's output obeys them.
- **Tool kinds are common to all agents:** shell, read, edit, write, content search, file search, todo list, subagent, web search, web fetch, MCP, other. Each provider maps its agent's tool names onto them (Claude Code: Bash, Read, Edit, Write, Grep, Glob, TodoWrite, Agent, WebSearch, WebFetch, and `mcp__…` tools; the three in the middle are absent from current releases). The kind comes from the tool's name and is on the call. The reader has one renderer and one icon per kind (DESIGN §4.2.4) and shows the tool under the agent's own name; a tool a provider doesn't map renders as `other`, with its name, input and output.

### 7.5 Indexing and large files

- Parsing streams a transcript line by line and never holds the file in memory; the model holds a few percent of the bytes parsed.
- **The index is one agent-neutral function over a parsed transcript:** title, first prompt, project path, branch, entrypoint, start and end, models, the session's own totals and the totals with its subagents, and the search spans (prompts, replies, tool commands and paths, each with the block it opens at; a subagent's text opens at the call that started it).
- **Staleness is a fingerprint** over each of the session's items (its kind, the version read and the newest version, with their lengths, and whether the version read is damaged), stored with the row beside the parser's and the index's versions. A pass indexes sessions whose fingerprint moved; a version bump is worked off in the background, never-indexed sessions first, then newest first. A read error from the operating system is not recorded as the session's state: the row keeps what it had and is tried again.
- **A provider can reclassify archived items** (`classify(key)`): when its parser version changes, the engine re-applies it to every archived item, including those gone at the source, so a fix to how files map to sessions reaches the whole archive. The owner's state follows the session's own transcript: when one session becomes two it goes with the first own transcript; when two become one the first to arrive keeps its state; state left under an id that no item has is removed. A pass and the background index change an item's session through the same code, so neither can strand state.
- A session is always parsed from its start. That is fast enough that the reader needs no per-turn byte offsets; search results name a turn and a block.
- Parser output is cached in memory per open session only; nothing parsed is written to the archive except the derived tables.
- The index rows carry the parser's version and the index's. A new version of either re-indexes in the background, newest sessions first.

### 7.6 Adding an agent

1. A module under `provider/` that implements the trait, on `FileTree` or `DbSnapshot` where one fits.
2. One line in the provider list.
3. Its tool names mapped to the common kinds.
4. A display name, a mark and the mark's colour for the UI (the theme holds no agent's colour).
5. A fixture builder for its storage in `sb_test_support`, and golden parser tests.

Nothing else changes: no migration, and no change to the archive, sync, filing, search or the reader. If adding an agent needs one, the seam is wrong and is fixed first.

---

## 8. Filing

- **Normalising a path** for comparison: split into components; on Windows-style paths (`X:\…`) fold case and accept either separator; map a WSL mount path to its drive form (§6.3); drop a trailing separator. A share path (`\\server\share\…`) is its own root, and one that names a WSL distro keeps its case. A path with no components (empty, `.`, `/`) can't be attached. The path as typed is kept for display.
- **A session's candidates** are the attached paths whose components are a prefix of the session's project path, and whose location is unset or is the session's.
- **The longest candidate wins,** and of two equal ones the path attached for the session's location beats the same path attached for all. Its folder gets the session, marked as filed by rule.
- **The same path can't be attached to two folders** for the same location scope; the second attach is refused and names the first folder. Spellings that normalise alike are the same path, and a WSL mount path is its drive form everywhere (`/mnt/d/Projects/app` is `D:\Projects\app`); a WSL location whose distro mounts drives elsewhere supplies its own mount (§6.3). A typed path is trimmed and must be absolute.
- **When rules run:** whenever a session that isn't filed by hand is indexed; and when a folder's paths change or a folder is deleted, over every session not filed by hand. So such a session is always where the rules put it. A session filed by hand is never moved by a rule. A session whose parse failed stays where it is.
- **File automatically** hands a hand-filed session back to the rules: it clears the mark and runs filing on that session.
- **A folder contains its subfolders' sessions.** Its page lists them and its count includes them.
- **Deleting a folder** drops its paths, moves its subfolders up to its parent, and hands its sessions back to the rules, hand-filed ones included: each goes to the folder another attached path claims, or to Unsorted.
- **Unsorted** is every session without a folder, grouped by project path and location: the same path on two machines is two places. Groups merge across locations only where the path has a drive form (a WSL mount path and its Windows path).

---

## 9. Reader

- **A virtualised list of blocks.** Tool calls render as fixed-height activity rows until expanded, so a session with hundreds of calls has a known height and scrolls without measuring everything.
- **Parsing is off the UI thread.** The session's own conversation is parsed whole (§7.5) and shown; subagents are read when their calls are expanded.
- **Tool output is loaded when its row is expanded,** including output saved in `tool-results/`. A collapsed row's short result comes from the call's brief (§7.4). Segments are capped in size (§4.1), so loading a body near the end of a large transcript decompresses one segment, not the whole version.
- **Markdown** per D11. Code blocks and diffs are highlighted with tree-sitter, producing token kinds that the theme colours, as dv's `syntax` does.
- **Diffs** for Edit and Write come from the tool call's own input and result. dv's diff rows and highlighting are lifted (D15).
- **Subagents** render through the same block list, nested and collapsed by default, under the call the parser attached them to (§7.3).
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
- **The shell transport, hermetically:** the `Command` launcher runs the same scripts through a local `sh` against a fixture tree, and the whole sync test matrix is run again through it. `SB_TEST_SHELL` names another shell to run it under, `SB_TEST_SHELL_VIA` a container's userland, and `SB_TEST_WSL` a real distro. On Windows these tests run inside WSL, as dv's Linux tests do. GNU and BSD tool variants are both covered (Linux and macOS runs).
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
| 4 | Reader (§9): markdown per D11, activity rows, each tool renderer, subagents, facts margin, find in session | The owner reads real sessions of each size class; a session of 100 MB or more meets the budget (§14) |
| 5 | Shell transport (§6.2), WSL locations (§6.3), then SSH (§6.4); location states and staleness | Loopback tests pass on Linux and macOS; the owner's live WSL and SSH checks pass |
| 6 | Search (§10), filters, versions UI, Trash and deleting for good (§4.5) | A `Ctrl K` query opens a session at the matching turn; a held-back version can be chosen; a session deleted for good stays gone after the next sync |
| 7 | Settings, polish, performance pass, packaging and updates as dv does (its D9), CI | Design review of every screen; budgets met; an installed build syncs the owner's real WSL archive |

Phase 5 is late only because Local exercises the whole pipeline on the development Mac first. If the owner wants the WSL backup running early, phases 1 and 5 can be done back to back before any UI, with a temporary command-line trigger.

---

## 14. Risks and open questions

- **Budgets,** to be measured, not yet met by anything: a first sync limited by the transport, not by scrollback (compression and hashing faster than a WSL read); a pass with nothing changed over 20,000 files in under 5 s for Local and under 15 s for WSL; opening a 100 MB session to first paint in under 500 ms; scrolling a 500-call session at the display's refresh rate.
- **Claude Code's format changes between releases.** The provider must tolerate it (D5), and a fixture per observed release guards the parser. The layout in §7.3 is from one machine and one release.
- **The provider seam is proven by a test provider only** until a second real agent exists (D8, §12). The first real one may still show something the trait lacks; §7.6's rule is to fix the seam then, not to work around it.
- **Reading a live database** (§7.2). `DbSnapshot` copies a database that its agent may be writing, without locking it. Re-reading when the files changed and opening the copy read-only should give a consistent snapshot; this is checked against a real agent's database when the first such provider is written, along with how large those databases get.
- **The shell transport across real hosts** (D6): BSD and GNU tool differences, unusual shells, slow links. The connection test is where problems should surface. The fallback is a helper binary behind `Transport`.
- **Provenance when lifting** (D15). Each file lifted from a sibling is checked for where it came from; GPL-derived code is rewritten or replaced, not copied.
- **Index size.** FTS5 with `trigram` over a year of prompts and replies; measured in phase 6 before the tokenizer is fixed (D10).
- **Deleting for good is the one irreversible act** in the app (§4.5). It needs its own tests: nothing outside the session's files is removed, and a location is never touched.
- **A writer that hides its changes.** Change detection is by size and modification time (§4.2). A program that rewrites a file in place and puts its time back, or writes through a memory map, stays invisible: the item reads as unchanged and the archive keeps the older content. Every range that is fetched is proven against the source before it is stored, so this can leave the archive behind but not wrong. A periodic deep check that re-hashes unchanged items is listed for later.
- **`orphans/` is never emptied.** It gains a file only when an unrecorded file is in the way of a different segment, or when the owner sets strays aside after a verify. Rebuilding a catalogue from the per-item `source.json` files is listed for later.
- **A database that never stops committing** can't be snapshotted by `DbSnapshot` as built (§7.2): the pass reports it and tries again next time. Tolerating growth soundly costs more round trips and waits for the first provider that needs it.
- **Many small segments.** A session synced while it is active gains a segment per pass. Harmless for correctness; compaction is listed for later.
- **The same conversation in two sessions.** Resuming or forking can copy earlier history into a new transcript file, so text is archived twice and search shows both. Linking them is listed for later.
- **Found in the shared stack while building,** to fix in toolbox-rs or gpui rather than here: on Windows, closing the window logs a few `Invalid window handle` errors to stderr (dv does the same; the exit code is 0); the automation host treats a line of invalid UTF-8 on stdin as quit, and a `resize` to an absurd size aborts the app; on macOS gpui writes one preference file for the app at launch, outside the data folder.
- **Two apps on one archive** are prevented by the lock (§4.4). A read-only second opener is not planned.
- **Secrets.** Transcripts contain whatever the agent saw. The archive is not encrypted (FEATURES: out of scope); it is as private as its folder.
- **One archive per machine.** Each machine the app is opened on has its own archive; they are independent until merging exists (FEATURES: Later).
- **WSL details to check on the PC:** the distro's default user and `$HOME`, a custom mount root in `/etc/wsl.conf`, `wsl.exe`'s behaviour when the distro is stopped or WSL is updating.
