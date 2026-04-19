# ptylenz — Architecture

> English · [日本語](architecture-ja.md)

This document is a component-level deep-dive into ptylenz.  
For the high-level motivation see [README.md](../README.md).  
For the design rationale see [DESIGN.md](../DESIGN.md).

> **Platform**: Linux and macOS only. Windows is not supported.

---

## System overview

```
┌─────────────────────────────────────────────────────────────┐
│ ptylenz process                                             │
│                                                             │
│  Terminal stdin ──► [PTY proxy] ──► PTY master fd           │
│                                          │                  │
│  Terminal stdout ◄── (clean bytes) ◄─────┤                  │
│                                          │                  │
│                                   [Block Engine]            │
│                                          │                  │
│                                   [vt100 shadow]            │
│                                          │                  │
│                             ┌────────────┘                  │
│                             ▼                               │
│                      [ratatui TUI overlay]                  │
│                        (alt-screen, on demand)              │
│                                                             │
│  [Claude feeder thread] ──► Block Engine                    │
│    (polls JSONL log)                                        │
└─────────────────────────────────────────────────────────────┘
                              │
                    PTY slave fd (kernel)
                              │
                    bash (child process)
                              │
                    fork/exec → commands
```

There are four source files:

| File | Responsibility |
|------|---------------|
| `src/main.rs` | Entry point; reads `$SHELL`, creates `App` |
| `src/pty.rs` | PTY proxy: `fork`, relay, resize, SIGWINCH |
| `src/block.rs` | OSC parser, block engine, vt100 shadow, JSON export |
| `src/tui_app.rs` | Event loop, ratatui rendering, keybindings |
| `src/claude_feeder.rs` | Tails Claude Code JSONL log, emits `ClaudeEvent`s |

---

## PTY proxy (`pty.rs`)

### What it does

`PtyProxy::spawn` forks a child bash inside a new PTY:

1. Query the real terminal's current winsize (`TIOCGWINSZ` on stdout).
2. `openpty()` with that winsize — critical: omitting the initial size causes `LINES`/`COLUMNS` to read as 0 or 80×24, and any ncurses program that calls `setupterm()` before the first `SIGWINCH` draws at the wrong width (staircase effect).
3. In the child: `setsid()` → `TIOCSCTTY` → `dup2(slave, 0/1/2)` → `exec(bash, --rcfile, wrapper.sh, -i)`.
4. In the parent: hold the master fd, return `PtyProxy`.

### Relay loop

The relay is driven by `tui_app.rs` via a `polling` crate `Poller` watching both stdin and the PTY master fd (level-triggered).

- **PTY master readable** → `read()` → pass through `BlockEngine::feed_output` → in Normal mode, write clean bytes to stdout.
- **Stdin readable** → `read()` → in Normal mode, write all bytes to PTY master except `Ctrl+]` which triggers mode switch.

### Resize

When `SIGWINCH` is delivered to the ptylenz process, a flag is set by the signal handler. The main loop checks the flag, calls `TIOCGWINSZ` on stdout, forwards the new size to the PTY master (`TIOCSWINSZ`) and sends `SIGWINCH` to the child shell. The vt100 shadow parser is also resized to match.

---

## OSC 133 parser (`block.rs — OscParser`)

### Why OSC 133

Block detection requires knowing exactly where each command's output begins and ends. The OSC 133 protocol (iTerm2 / Warp / VS Code Terminal) provides exactly this via escape sequences emitted by the shell:

| Sequence | Meaning |
|----------|---------|
| `\e]133;A\a` | Prompt start |
| `\e]133;C\a` | Command execution start (output begins here) |
| `\e]133;D;N\a` | Command finished, exit code N |
| `\e]133;E;text\a` | Command text (block title) |

### 5-state machine

```
Normal
  │ \x1b
  ▼
Escape
  │ ']'          │ other → emit(\x1b + byte) → Normal
  ▼
OscStart
  │ (any byte)
  ▼
OscBody ──────────────────────────────────────────────────────►
  │ \x07 (BEL)  →  decode_osc(buf)                            │
  │               OSC 133 → emit Event, → Normal               │
  │               other   → re-emit original bytes, → Normal   │
  │ \x1b (ESC)  →  decode_osc(buf)                            │
                  OSC 133 → emit Event, → OscStSwallow
                  other   → re-emit original bytes, → Normal

OscStSwallow
  │ '\\' → Normal   (consume the ST terminator)
  │ other → emit byte, → Normal
```

### Passthrough guarantee

Only `\e]133;*` sequences are consumed. All other OSC sequences — `\e]0;title\a` (window title), `\e]8;...` (hyperlinks), `\e]52;...` (clipboard), `\e]11;?\e\\` (color queries) — are re-emitted verbatim. This matters because ncurses programs like `mc` query terminal colors during `setupterm()`; silently dropping the response changes how they draw.

### Interleaved chunk API

`OscParser::parse` returns `Vec<ParseChunk>` where each chunk is either `Bytes(Vec<u8>)` or `Event(OscEvent)`. Callers iterate these in order. This matters: when a command's last output bytes, the `133;D` end marker, and the next command's `133;C` start marker all land in the same `read()` (common for fast commands), collapsing the result into `(all_bytes, all_events)` would attribute the trailing bytes to the wrong block.

---

## Block engine (`block.rs — BlockEngine`)

### Block lifecycle

```
OSC 133;C  →  open new Block (self.current = Some(block))
raw bytes  →  append to current.output; update cached_line_count;
               tee into vt100 shadow (see below)
OSC 133;E  →  set current.command (or patch last closed block if 133;E
               arrives after 133;D — bash emits it from PROMPT_COMMAND)
OSC 133;D  →  close current: set exit_code, ended_at, finalize rendered_text;
               auto-collapse if line_count > 50; push to self.blocks
OSC 133;A  →  close current if any (handles prompt-without-D edge cases)
```

### cached_line_count

`Block.cached_line_count` is incremented by counting `\n` bytes in every `append_clean` call. This replaces a previous O(total output size) scan in `line_count()` that was called once per block per redraw and made the list UI take seconds once `claude` or `mc` had accumulated megabytes of stream data.

### Search

`BlockEngine::search(query)` does case-insensitive substring search across all completed blocks and the current in-flight block. Returns `(block_id, line_number, line_text)` tuples for use by the `n`/`N` navigation.

### JSON export

`BlockEngine::export_json()` serializes the session in the [claude-session-replay](https://github.com/opaopa6969/claude-session-replay) common log model: each block becomes a `user` message (command text) + `assistant` message (output), with an `exit_code` extension field.

---

## vt100 shadow grid (`block.rs — per-block vt100::Parser`)

### Why

TUI applications (vim, less, claude, mc) use the alternate screen (`\e[?1049h` / `\e[?1049l`) and fill it with cursor-positioning sequences. Naively stripping ANSI from the raw byte buffer leaves a jumbled mess of partial overwrites. The shadow grid captures the final visual state of the screen.

### Mechanism

On `CommandStart`, a new `vt100::Parser` is created at the current terminal size. Every byte appended to `current.output` is also fed to this parser. When the parser's screen reports `alternate_screen() == true`, the current grid contents are snapshotted via `screen().contents()`, normalized, and stored in both `last_alt_snapshot` and `current.rendered_text`.

### Why snapshot before CommandEnd

TUI apps typically exit the alternate screen immediately before the command ends. Waiting until `CommandEnd` to snapshot would see the restored primary screen, losing the frame we want. Instead, each feed while alt-screen is active updates the snapshot, and the last non-empty frame is committed at `finalize_rendered_text`.

### Normalization

`vt100::Screen::contents()` pads every row to the parser's column width with spaces. `normalize_vt_snapshot` right-trims each row and drops trailing blank rows. Without this, a snapshot wider than the overlay panel causes every padded row to wrap to two visual lines, doubling the apparent height and breaking scroll math.

### CJK full-width note

CJK characters (Chinese, Japanese, Korean) are double-width: each occupies two terminal cells. The `unicode-width` crate provides `UnicodeWidthChar::width()` for correct column accounting. However, when the ratatui overlay is narrower than the vt100 grid, a line containing many CJK characters may still overflow or misalign. This is a known cosmetic issue with no workaround at present.

### Fallback for line-oriented output

When a block never enters the alternate screen, `rendered_text` stays `None` and `output_text()` falls back to stripping ANSI from the raw byte buffer. This path preserves scrollback beyond the grid height — important for commands like `cargo test` that produce hundreds of lines.

---

## ratatui TUI overlay (`tui_app.rs`)

### Mode model

```
Mode::Normal
  - every keystroke except Ctrl+] → proxy.write_input(bytes)
  - PTY output → stdout (clean bytes)

Mode::Ptylenz { selected, view, search_input, last_search, status_message }
  - PTY output → block engine only; not painted over the alt-screen UI
  - ratatui renders on every event iteration (≤ 80 ms timeout)
  - Ctrl+] → back to Normal; 'q' / Esc → same
```

### Event loop

```
Poller::wait(80ms)
  ├─ PTY_KEY readable → proxy.read_output() → optionally write to stdout
  ├─ STDIN_KEY readable → handle_input()
  └─ SIGWINCH flag → resize
claude_rx.try_recv() → ingest_claude_event()  (drains before each poll)
if Ptylenz mode → draw_ptylenz()
```

The 80 ms timeout keeps the UI responsive even when no I/O arrives (e.g., the user is viewing a block and the shell is idle).

### List view

Renders all completed blocks plus the current in-flight block as a ratatui `List`. Each item shows:
- Fold indicator (`▸`/`▾`), pin (`📌`), block ID, timestamp, line count, exit status, command text
- If expanded: up to 200 lines of output body (truncated with "N more lines — press e to export")

### Detail view

Full-screen view of one block with cursor navigation and two selection modes:

- **Linewise** (`v`): selects every character on every line in `[anchor_row, cursor_row]`
- **Blockwise** (`Ctrl+v`): selects the rectangle bounded by anchor cell and cursor cell — the vim `Ctrl-v` model

Both modes highlight selected cells in the ratatui render pass and yank via `y`.

### Status bar

One-line bar at the bottom: either a contextual help string (key hints) or a transient action message (e.g., "copied block #7 (4316 chars)"). Messages are cleared at the start of each keypress iteration, so they are visible for exactly one draw cycle.

---

## Claude Code session integration (`claude_feeder.rs`)

### File layout

Claude Code writes session logs to:

```
~/.claude/projects/<cwd-slug>/<session-id>.jsonl
```

where `<cwd-slug>` is the absolute working directory with `/` replaced by `-`.  
Example: `/home/opa/work/ptylenz` → `-home-opa-work-ptylenz`.

### Watch loop

`spawn_watcher(cwd)` starts a background thread that:

1. Polls the project directory every 400 ms.
2. Finds the newest `.jsonl` by mtime.
3. On file switch: emits `ClaudeEvent::SessionStarted`, seeks to EOF (history is not replayed on startup).
4. On same file: seeks from the last known offset, reads new complete lines.
5. Decodes each line with `decode_line`, emits `ClaudeEvent::Turn` for `user`/`assistant` entries, ignores all other record types.

Polling is used instead of `inotify`/`kqueue` because ptylenz often runs over SSH-forwarded homedirs where filesystem notifications may not propagate across the network.

### notify crate

`notify = "6"` appears in `Cargo.toml` but is not used in the current implementation. See [docs/decisions/notify-dead-dep.md](decisions/notify-dead-dep.md).

### ClaudeEvent ingestion

The main loop drains `claude_rx.try_recv()` before each `Poller::wait`. `BlockEngine::ingest_claude_event` synthesizes a `Block` with `source = BlockSource::ClaudeTurn`. These blocks appear in the same chronological list as shell blocks.

---

## Shell integration detail

See [docs/shell-integration.md](shell-integration.md) for per-shell setup.

The bash integration is injected via a wrapper rcfile:

```bash
# Auto-generated by ptylenz, safe to delete
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"

__ptylenz_precmd() {
    local __ptylenz_ec=$?
    printf '\e]133;D;%d\a' "$__ptylenz_ec"          # command end + exit code
    local __ptylenz_last
    __ptylenz_last=$(HISTTIMEFORMAT='' history 1 2>/dev/null \
        | sed -E 's/^[[:space:]]*[0-9]+[[:space:]]*//')
    if [ -n "$__ptylenz_last" ]; then
        printf '\e]133;E;%s\a' "$__ptylenz_last"    # command text
    fi
}
PROMPT_COMMAND='__ptylenz_precmd'
PS0='\[\e]133;C\a\]'
case "$PS1" in
  *'133;A'*) ;;
  *) PS1='\[\e]133;A\a\]'"$PS1" ;;
esac
```

`PROMPT_COMMAND` is chosen over the `DEBUG` trap because the `DEBUG` trap nests inside every function that calls subcommands, generating spurious 133;C markers.

The `PROMPT_COMMAND` assignment overwrites any existing value. See [docs/decisions/prompt-command-strategy.md](decisions/prompt-command-strategy.md).

---

## Dependency notes

| Crate | Role |
|-------|------|
| `ratatui 0.29` | TUI rendering |
| `crossterm 0.28` | alt-screen enter/leave, cursor hide/show |
| `nix 0.29` | `openpty`, `fork`, `execvp`, `signal`, `waitpid` |
| `libc 0.2` | `ioctl`, `TIOCGWINSZ`, `TIOCSWINSZ`, `tcgetattr`, raw mode |
| `polling 3` | level-triggered fd polling (stdin + PTY master) |
| `vt100 0.15` | shadow grid for alt-screen TUI capture |
| `regex 1` | byte-level regex for future prompt-pattern fallback |
| `unicode-width 0.2` | column width of CJK and other wide characters |
| `unicode-segmentation 1` | grapheme-cluster-safe truncation in tool-use rendering |
| `serde + serde_json 1` | JSONL decoding (Claude feeder) and JSON export |
| `anyhow 1` | error propagation |
| `chrono 0.4` | block timestamps |
| `notify 6` | **unused** — kept for future inotify/kqueue path; see decisions/ |
