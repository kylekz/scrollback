# scrollback

Native desktop app (Rust + GPUI) that backs up, archives and reads coding-agent transcripts. It pulls transcripts from locations (this machine, a WSL distro, a host over SSH) into an archive it owns and reads only from that archive. Claude Code is the first agent supported. Windows is the main platform, with first-class WSL; macOS is supported.

- Spec: `docs/FEATURES.md` (what it does) and `docs/PLAN.md` (decisions D1–D15, the archive, sync, transports, phases; cited as `PLAN §N` / `D<n>`). Read the relevant sections before changing anything; when code and spec disagree, fix one of them in the same change.
- Design: `docs/DESIGN.md` (tokens, layout, materials, screens, keyboard map; cited as `DESIGN §N`), written from concept E on the design canvas. The boards' sources are in `design/project/` (the `E-*.dc.html` files; A–D are the concepts E was chosen from). Where a board and DESIGN.md disagree, DESIGN.md wins.
- Status: phases 0, 1, 2 and 5 of PLAN §13 are built. `sb_core` archives, syncs, parses and indexes from this machine, a WSL distro or an SSH host: the archive and catalogue, the local and shell transports, the provider seam with the first provider's scan, fetch and parser, the transcript model, the session index, folders with attached paths and filing rules. Dev examples drive it (`sync`, `check`); the app still opens the empty concept E shell (the library UI, phase 3, is in progress on a branch).
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

## Commands

Rust 1.98.1 (pinned in `rust-toolchain.toml`). Run `script/check` with bash; on Windows that is Git Bash.

```bash
script/check                 # the gates: fmt, clippy x2, gpui boundary, agent names, tests, e2e
script/check --fast          # the same without the e2e scenarios
script/check --e2e-only      # the e2e scenarios alone
cargo deny check             # licences, advisories and sources (deny.toml); needs cargo-deny

cargo build                  # the scrollback binary (default member)
cargo run -- --data-dir target/tmp/data --window-size 1440x900 --theme dark
                             # the app, one process, in the foreground; --theme is system, light or dark
cargo test --workspace
cargo run --release -p sb_core --example sync -- <archive folder> [--wsl <distro> | --ssh <host>]
                             # a dev trigger: syncs and indexes this machine, a WSL distro or an SSH host into an archive
cargo run --release -p sb_core --example sync -- <archive folder> verify | set-aside | index | reindex
cargo run --release -p sb_core --example sync -- test --wsl <distro> | --ssh <host>
                             # the connection test; `sync -- distros` lists WSL distros
cargo run --release -p sb_core --example check [-- <home folder>]
                             # parses every session under a home folder and prints aggregates only
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p scrollback --no-default-features --all-targets -- -D warnings   # the shipped app

cargo build -p scrollback && cargo run -p sb_e2e --bin e2e -- tests/e2e
                             # the e2e scenarios; screenshots and logs in target/e2e/<scenario>/
cargo run -p sb_e2e --bin e2e -- --foreground tests/e2e/shell.jsonl
                             # one scenario, its window focused and in front
```

- The e2e runner never builds the app: build it with a plain `cargo build -p scrollback` right before a run. Its windows open on the real desktop, in the background unless `--foreground` is given. toolbox-rs's `docs/automation.md` documents the runner, the scenario format and the protocol; `crates/scrollback/src/automation.rs` documents scrollback's `state` and commands.
- Never point a run at the real data folder: pass `--data-dir` (or set `SCROLLBACK_DATA_DIR`) under `target/`. The runner gives each scenario its own.
