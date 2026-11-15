# scrollback

Native desktop app (Rust + GPUI) that backs up, archives and reads coding-agent transcripts. It pulls transcripts from locations (this machine, a WSL distro, a host over SSH) into an archive it owns and reads only from that archive. Claude Code is the first agent supported. Windows is the main platform, with first-class WSL; macOS is supported.

- Spec: `docs/FEATURES.md` (what it does) and `docs/PLAN.md` (decisions D1–D15, the archive, sync, transports, phases; cited as `PLAN §N` / `D<n>`). Read the relevant sections before changing anything; when code and spec disagree, fix one of them in the same change.
- Design: concept E on the design canvas. Its source boards are in `design/project/` (`E-Reader.dc.html`, `E-Folder.dc.html`; rows A–D are the concepts it was chosen from).
- Status: planning. There is no code yet.
- Agent-agnostic (PLAN D8, §7): everything specific to a coding agent lives in its provider. Sync, the archive, filing, search and the reader never name an agent. Adding an agent must not need a change outside its provider; if it does, fix the seam first.
- Sibling apps on the same stack, all the owner's: dv (`kylekz/diff`), fc, slide, folio, and the shared crates in `kylekz/toolbox-rs`. Lift from them per PLAN D15.
- Machine-specific notes live in the git-ignored `CLAUDE.local.md` beside this file, never in the repository:
  @CLAUDE.local.md

## Nothing machine-specific in the repository

The repository must not reveal anything about the machines it is developed or used on. This covers every file, commit message, branch name and pull request.

- No real usernames, home directories, hostnames, IP addresses, SSH destinations, drive layouts or WSL distro names.
- No real transcripts, excerpts of them, session ids, or project names and paths taken from them. Tests use generated fixtures (PLAN §12).
- No counts or sizes measured on a particular machine's data. General ranges are fine ("tens of megabytes").
- Examples use placeholders: `user`, `/home/user/code/…`, `/Users/user/code/…`, `C:\Users\user\…`, `D:\Projects\…`, `user@macbook`, `Ubuntu-24.04`.
- Anything that has to name a real path or host goes in `CLAUDE.local.md`.

Before committing, search the staged changes for the local username, home directory and hostnames, and read the diff for anything else that came from a real machine.
