# scrollback

A native desktop app (Rust + GPUI) that backs up, archives and reads coding-agent transcripts.

scrollback pulls transcripts from the locations you define (this machine, a WSL distro, a host over SSH) into an archive it owns, files them into your folders by project path, and reads only from that archive. Claude Code is the first agent supported.

**Status:** planning. Nothing is built yet.

- [docs/FEATURES.md](docs/FEATURES.md): what it does.
- [docs/PLAN.md](docs/PLAN.md): how, and in what order.
- `design/project/`: the design boards; concept E is the chosen direction.

MIT licensed.
