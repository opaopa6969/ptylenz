# Changelog

All notable changes to ptylenz are documented here.  
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).  
ptylenz uses [Semantic Versioning](https://semver.org/).

> **Platform note**: ptylenz supports Linux and macOS only.  
> Windows is not supported and is not planned.

---

## [Unreleased]

### Planned
- Shell input completion via LSP (separate process)
- Shell-script debugger via DAP (separate process)
- syslenz panel integration
- Block-to-block diff
- Session persistence (replace tmux use case)

---

## [0.1.0] - 2026-04-19

### Added

**PTY proxy core**
- `fork/exec` bash inside a new PTY with initial winsize baked in (prevents ncurses staircase on programs that read `LINES`/`COLUMNS` before first `SIGWINCH`)
- Full I/O relay: stdin → PTY master, PTY master → stdout, all bytes pass through
- `SIGWINCH` forwarding with `TIOCSWINSZ` propagation to the child
- `SIGHUP` sent to child on `PtyProxy` drop

**OSC 133 parser (5-state machine)**
- States: `Normal → Escape → OscStart → OscBody → OscStSwallow`
- Consumes `\e]133;A/C/D/E\a` and `\e]133;...\e\\` (BEL + ST terminators)
- Passes all non-133 OSC sequences (title, hyperlink, color queries, clipboard) verbatim so downstream terminals keep working
- Interleaved chunk API preserves byte-event ordering within a single `read()` — prevents misattribution of output at command boundaries

**Block engine**
- Segments the PTY byte stream into discrete `Block` values on `OSC 133;C` / `D` / `A` transitions
- `BlockSource::Shell` for PTY-derived blocks; `BlockSource::ClaudeTurn` for JSONL-ingested turns
- `cached_line_count`: O(1) newline counter maintained on every append (replaces O(n) scan that made list rendering slow on large Claude sessions)
- Auto-collapse blocks with > 50 lines
- Cross-block full-text search (`BlockEngine::search`)
- JSON export in [claude-session-replay](https://github.com/opaopa6969/claude-session-replay) common log model format

**vt100 shadow grid**
- Per-block `vt100::Parser` tee'd from the same byte stream
- Snapshot taken on every feed while alt-screen is active; last non-empty frame stored as `rendered_text`
- `rendered_text` used for display/copy in TUI apps (`vim`, `less`, `claude`); raw buffer retained for line-oriented output so scrollback beyond grid height is preserved
- `normalize_vt_snapshot`: right-trims padded rows and drops blank trailing rows to prevent double-wrapping in narrow overlay panels

**ratatui TUI overlay**
- Two modes, one prefix key (`Ctrl+]`): Normal (fully invisible) and Ptylenz (alt-screen overlay)
- List view: block navigation (`j`/`k`/`g`/`G`), fold/expand (`Enter`), pin (`p`), copy (`y`), export (`e`), search (`/`, `n`/`N`)
- Detail view: full-screen single block with vim-style cursor (`h`/`j`/`k`/`l`), linewise (`v`) and blockwise/rectangular (`Ctrl+v`) selection, yank (`y`/`Y`)
- Status bar: persistent help line + transient per-action messages
- `polling` crate event loop (stdin + PTY master fd, level-triggered); no crossterm event loop overhead

**Shell integration (bash)**
- Wrapper rcfile written to `$TMPDIR`, passed via `--rcfile`; sources `~/.bashrc` first — existing prompt, aliases, completions all preserved
- `PROMPT_COMMAND` → `__ptylenz_precmd`: emits `133;D;$?` (exit code) and `133;E;cmd` (command text from `history 1`)
- `PS0` → `133;C` (command start); `PS1` prepend → `133;A` (prompt start)
- `$PTYLENZ=1` set in child environment to prevent recursive launch

**Claude Code session integration**
- Background thread polls `~/.claude/projects/<cwd-slug>/<session-id>.jsonl` (400 ms interval)
- Decodes `user` / `assistant` turns into `ClaudeTurn` blocks interleaved chronologically with shell blocks
- Tool-use blocks rendered as `→ tool_name(input_json)` with 500-byte grapheme-safe truncation
- Polling over inotify: works over SSH-forwarded homedirs where inotify may not propagate

**Clipboard**
- OSC 52 emitted first (works inside tmux with `set-clipboard on`)
- Linux fallback: `xclip -selection clipboard`
- macOS fallback: `pbcopy`

**Wrap-free copy**
- `output_text()` reads the raw PTY byte stream, never the screen grid — long lines copy as the original one-liner regardless of terminal column width

**Distribution**
- Linux x86_64 release binary
- macOS Apple Silicon (aarch64) release binary
- macOS Intel (x86_64) release binary
- `install.sh` for source installs via `cargo install --path . --force`

**Documentation**
- `README.md` / `README-ja.md` — project overview, install, keybindings, architecture summary
- `CHANGELOG.md` — this file
- `docs/architecture.md` / `docs/architecture-ja.md` — component deep-dive
- `docs/getting-started.md` / `docs/getting-started-ja.md` — install + first run
- `docs/shell-integration.md` / `docs/shell-integration-ja.md` — OSC 133 setup, per-shell reference
- `docs/decisions/notify-dead-dep.md` — why `notify` is in `Cargo.toml` but unused
- `docs/decisions/prompt-command-strategy.md` — `PROMPT_COMMAND` overwrite rationale
- `PROJECT.md` / `PROJECT.ja.md` — handoff document: architecture + design decisions
- `DESIGN.md` / `DESIGN.ja.md` — design rationale

---

[Unreleased]: https://github.com/opaopa6969/ptylenz/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/opaopa6969/ptylenz/releases/tag/v0.1.0
