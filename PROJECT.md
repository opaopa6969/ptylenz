# ptylenz — PROJECT.md

> English · [日本語](PROJECT.ja.md)

> Handoff document: architecture, design decisions, and implementation notes.

## One-liner

**As `syslenz` changes the experience of `/proc`, `ptylenz` changes the experience of the PTY.**

A PTY proxy plus a TUI. Command output becomes structured blocks you can search, fold, and copy. A terminal that runs inside your terminal.

## The problem

### Pain ranking

1. **Output scrolls past and disappears.** When Claude Code prints 2,000 lines, the top is gone.
2. **Selecting and copying is awful.** tmux / mouse-select bake `\n` into the wrap column.
3. **You can't find "that thing earlier".** Eyeball-grepping scrollback is 1978 VT100 UX.

### Root cause

All three traceto the same fact: **the PTY stream has no structure**.

Bytes flow, bytes vanish. Command boundaries, output length, success/failure — none of it is visible at the PTY layer.

## Architecture

### Why a PTY proxy and not a shell

After the shell `fork/exec`s a child, **the shell is no longer in the data path**:

```
child process (claude-code)
  stdout → PTY slave → PTY master → terminal emulator

bash is just sitting in wait(); it never sees the output.
```

So to give output structure, we have to sit on the master side of the PTY. That's the same layer as `tmux` (a PTY multiplexer), but with a different purpose:

- tmux: panes + session persistence
- ptylenz: output blocking + search + copy

### Data flow

```
┌──────────────────────────────────────────────┐
│ ptylenz process                              │
│                                              │
│  Terminal ←→ [PTY proxy] ←→ [PTY] ←→ bash    │
│                  ↓                     ↓     │
│            [Block Engine]         fork/exec  │
│                  ↓                     ↓     │
│            [TUI Renderer]         commands   │
│                                              │
│        [Claude JSONL feeder] ────────┐       │
│              ↓                       ↓       │
│        [Block Engine] ←──── claude turns     │
└──────────────────────────────────────────────┘
```

The PTY proxy relays all I/O. Bash emits OSC markers around each command, and ptylenz uses those to slice the byte stream into blocks. In parallel a feeder thread tails Claude Code's session JSONL log and ingests user/assistant turns as sibling blocks.

### Block detection: OSC 133

iTerm2 / Warp / VS Code Terminal–compatible OSC 133 sequences:

| Sequence | Meaning |
|----------|---------|
| `\e]133;A\a` | prompt start |
| `\e]133;C\a` | command execution start |
| `\e]133;D;N\a` | command finished, exit code = N |
| `\e]133;E;cmd\a` | command text (block title) |

The bash integration emits these via `PROMPT_COMMAND` + `PS0` + `PS1`. We deliberately avoid the `DEBUG` trap because it nests inside any function that calls subcommands.

### Two modes, one prefix

The whole UI follows a single rule: **Normal mode is invisible**. `Ctrl+]` is the one and only boundary key.

```
Normal mode: every keystroke goes straight to bash
                ↓ Ctrl+]
Ptylenz mode: ratatui overlay
              ├─ List view: blocks
              └─ Detail view: one block, full screen, cursor + vim-style visual
```

### Wrap-free copy

`block.output` is the raw PTY byte stream. `output_text()` strips ANSI but never consults the screen width — so a long `curl` invocation wrapped onto three visual lines in an 80-column terminal still copies as the original one-liner. tmux / mouse-select copy from the screen grid, so they bake the wrap in; ptylenz copies from the source.

### Alt-screen support (vt100 shadow)

For TUI applications that take over the screen (`vim`, `less`, `claude`), we tee the same bytes into a parallel vt100 parser and snapshot the grid at command end into `rendered_text`. The block stays readable even after the TUI exits the alternate screen and wipes the live view.

### Claude Code session integration

A background thread polls `~/.claude/projects/<cwd-slug>/<session-id>.jsonl`, decodes user/assistant turns, and ingests them into the block engine as `BlockSource::ClaudeTurn` blocks. Shell commands and Claude turns interleave chronologically in the same list.

## Design decisions

### D1: keep bash inside

ptylenz doesn't replace bash. It runs bash in a PTY and relays the I/O.

- existing `~/.bashrc`, completions, aliases all work
- learning cost is zero
- single binary; copy it, run it, anywhere

### D2: zero config, 80% out of the box

Same philosophy as syslenz's "21 seconds to first insight". No config file at all.

### D3: ratatui (shared with syslenz)

We use the same TUI substrate so the two projects can share widgets later.

### D4: claude-session-replay-compatible JSON

`e` exports the session in the [claude-session-replay](https://github.com/opaopa6969/claude-session-replay) common log model, so the same JSON is consumable by that project's HTML / terminal / MP4 renderers.

### D5: LSP / DAP as external processes (future)

When we add shell completion (LSP) or script debugging (DAP), they should run as separate processes. ptylenz's TUI becomes a JSON-RPC client. Crashes don't propagate, and VS Code / Neovim can use the same servers.

## MVP scope

### Done

- [x] PTY proxy (fork/exec bash, I/O relay, SIGWINCH forwarding)
- [x] OSC 133 parser (BEL and ESC-\\ terminators)
- [x] Block engine (segment, search, JSON export)
- [x] ratatui overlay (List view / Detail view)
- [x] Block navigation (j/k, g/G, n/N, `/` search)
- [x] Cross-block full-text search
- [x] Clipboard via OSC 52 + xclip / pbcopy
- [x] claude-session-replay-compatible JSON export
- [x] Auto-collapse for long output (>50 lines)
- [x] vt100 shadow grid for alt-screen TUI capture
- [x] Claude Code JSONL session integration
- [x] Detail view with linewise + blockwise (rectangular) selection
- [x] Wrap-free copy (raw PTY byte stream)
- [x] Linux x86_64 / macOS arm64 / macOS x86_64 release binaries

### Future

- Shell input completion via LSP (separate process)
- Script debugger via DAP (separate process)
- syslenz panel integration
- Block-to-block diff
- Session persistence (replace tmux for that purpose)

## Crate layout

```
ptylenz/
├── Cargo.toml
├── src/
│   ├── main.rs            # entry point
│   ├── pty.rs             # PTY proxy: fork, relay, resize
│   ├── block.rs           # block engine: OSC parse, segment, search, vt100 shadow
│   ├── tui_app.rs         # TUI: event loop, ratatui rendering, keybindings
│   └── claude_feeder.rs   # tail JSONL under ~/.claude/projects/
├── .github/workflows/
│   └── release.yml        # build release binaries on tag push
├── PROJECT.md / PROJECT.ja.md
├── DESIGN.md / DESIGN.ja.md
└── README.md / README.ja.md
```

## Implementation notes

### PTY proxy essentials

- `nix::pty::openpty()` with the initial winsize baked in (critical: 0×0 makes ncurses apps draw at width 0 and the screen turns into a staircase)
- child: `setsid()` → `TIOCSCTTY` → `dup2()` for stdin/stdout/stderr → slave
- parent: hold the master fd and relay I/O
- resize: `TIOCSWINSZ` plus `SIGWINCH` to the child

### Event loop multiplexing

Use the `polling` crate to watch stdin and the PTY master fd, level-triggered. We don't go through crossterm's event loop — raw bytes are decoded inline (`decode_keys`). Switching to and from the alternate screen is `crossterm::execute!`.

### Bash integration injection

A wrapper rcfile is written to `$TMPDIR` and passed via `--rcfile`:

```bash
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"
PS0='\[\e]133;C\a\]'
PS1='\[\e]133;A\a\]'"$PS1"
PROMPT_COMMAND='__ptylenz_precmd'
```

`PROMPT_COMMAND` emits 133;D (exit code) and 133;E (the prior command, recovered from `history 1`).

### OSC parser state machine

```
Normal → ESC(\x1b) → Escape
Escape → ']' → OscStart → OscBody
OscBody → BEL(\x07) → [decode] → Normal
OscBody → ESC \\ (ST) → [decode] → Normal
Escape → other → emit(ESC + byte) → Normal
```

Non-OSC escapes (colors, cursor moves) pass through untouched.

### Clipboard

OSC 52 first (works inside tmux when `set-clipboard` is enabled). Fall back to xclip on Linux, pbcopy on macOS.

## Testing strategy

1. **Unit tests**: OSC parser, block engine (boundaries, search, export), selection math.
2. **Integration test**: spawn a real bash under the proxy, drive a few commands, assert the captured blocks.
3. **Regression**: a 400-character line into a 40-column engine must come back as a single line — proves the wrap-free copy contract.

## Related projects

| Project | Relationship |
|---------|--------------|
| [syslenz](https://github.com/opaopa6969/syslenz) | sibling — shared ratatui base; `/proc` → structure transposed onto the PTY |
| [claude-session-replay](https://github.com/opaopa6969/claude-session-replay) | the export target — shared common log model |
| tmux | same layer (PTY multiplexer); different purpose (panes vs. structure) |
| Warp | same idea (block-based output) in a GUI — we do it in the TUI |
| bash | wrapped, never replaced |

## Build & run

```bash
cargo build
cargo test
cargo run

# install
cargo install --path .
ptylenz
```

Pre-built binaries are on the [Releases page](https://github.com/opaopa6969/ptylenz/releases).
