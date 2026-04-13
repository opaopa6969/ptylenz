# ptylenz

> **Wireshark for your PTY** — structured output blocks for the terminal.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## What is this?

ptylenz sits between you and your shell. Every command's output becomes a **block** you can navigate, fold, search, and copy without leaving the terminal. No more scroll hell, no more "where did that error scroll off to."

```
Before:                              After:
$ claude-code "fix auth"             ┌─ #42 claude-code "fix auth" ─ 14:23 ─┐
(2000 lines scroll by)               │ ▶ Modified 3 files (2847 lines)       │
(you scroll up frantically)          │ ▷ [expand] [copy] [search] [pin]      │
(give up and re-run)                 └───────────────────────────────────────┘
```

## Why ptylenz vs. tmux / mouse-select

Most terminal tools cope with scrollback by reading the **screen grid** — the 80×24 (or whatever) cell array your terminal emulator paints. ptylenz reads the **PTY byte stream** — what the child process actually wrote. That distinction is small in description and large in practice:

| | tmux / mouse-select | ptylenz |
|-|---------------------|---------|
| **Wrapped long line** | inserts `\n` at the wrap column | copies as the original one-liner |
| **ANSI escapes** | leaks color codes into clipboard | stripped on copy |
| **Block boundaries** | none — you eyeball where the prompt was | OSC 133 markers, exact |
| **Long-output search** | manual scrolling | full-text across every block |
| **Selection** | screen-cell rectangle | linewise + blockwise on the raw output |

Concrete: paste a 200-character `curl -X POST … -H "Authorization: …" -d '{…}'` into a 80-column terminal. The terminal wraps it for display. With tmux, copy gives you a broken-in-three-pieces command. With ptylenz, copy gives you the original one-liner — because ptylenz never looked at the screen, it looked at the bytes.

## Install

### Pre-built binaries

Grab the latest release from [Releases](https://github.com/opaopa6969/ptylenz/releases):

```bash
# Linux x86_64
curl -L https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-linux-x86_64 -o ptylenz
chmod +x ptylenz && sudo mv ptylenz /usr/local/bin/

# macOS (Apple Silicon)
curl -L https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-macos-aarch64 -o ptylenz
chmod +x ptylenz && sudo mv ptylenz /usr/local/bin/

# macOS (Intel)
curl -L https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-macos-x86_64 -o ptylenz
chmod +x ptylenz && sudo mv ptylenz /usr/local/bin/
```

### From source

```bash
cargo install --path .
```

Then just run:

```bash
ptylenz
```

Your bash starts inside ptylenz. Everything works exactly as before — except the moment you want to look back at output, it's structured.

## Two modes, one prefix

The whole UI follows a single rule: **Normal mode is invisible**. Every keystroke (except one) flows straight to bash, every byte of output flows straight to your screen. ptylenz is doing nothing visible. The one exception is `Ctrl+]` — that switches into Ptylenz mode, where ratatui takes over the screen.

### Normal mode

| Key | Action |
|-----|--------|
| (everything) | passes through to bash |
| `Ctrl+]` | enter Ptylenz mode |

### Ptylenz mode — block list

| Key | Action |
|-----|--------|
| `j` / `k` / `↑` / `↓` | next / previous block |
| `g` / `G` | jump to first / last block |
| `Enter` | expand / collapse selected block |
| `v` | open Detail view of selected block |
| `/` | search across all blocks |
| `n` / `N` | next / previous search hit |
| `y` | copy selected block to clipboard |
| `e` | export session as JSON |
| `p` | pin / unpin selected block |
| `q` / `Esc` / `Ctrl+]` | back to Normal |

### Ptylenz mode — Detail view

A full-screen view of one block with a movable cursor and vim-style selection.

| Key | Action |
|-----|--------|
| `h` / `j` / `k` / `l` | move cursor |
| `g` / `G` / `0` / `$` | top / bottom / line start / end |
| `Ctrl+u` / `Ctrl+d` | page up / down |
| `v` | start / end **linewise** selection |
| `Ctrl+v` | start / end **blockwise** (rectangular) selection |
| `y` | yank selection (or whole block if none) |
| `Y` | yank whole block always |
| `Esc` | clear selection (or back to list if none) |
| `q` | back to list |

Blockwise selection is the move when you want a single column out of `ls -l`, or the body of a script without the leading marker. Vim users will feel at home.

## How it works

ptylenz is a **PTY proxy**. It creates a pseudo-terminal, runs bash inside it,
and intercepts all I/O. Shell integration (OSC 133 markers, the same protocol used by iTerm2 / Warp / VS Code Terminal) tells ptylenz where each command's output begins and ends.

```
You ←→ ptylenz (PTY master) ←→ bash (PTY slave) → fork/exec → commands
              ↓
         Block Engine
         (segment, index, store)
```

The integration is injected via a wrapper rcfile that `source`s your existing `~/.bashrc` first, so your prompt, aliases, and completions all keep working untouched.

For TUI apps that take over the screen (`vim`, `less`, `claude`), ptylenz keeps a shadow vt100 grid in parallel so the captured "block" still reads sensibly after the program exits — without polluting the live display.

## Zero config

No `.ptylenzrc`. No themes. No plugins. Just run it.

Works on any machine where you can copy a binary. SSH into a server, `./ptylenz`, done.

## Claude Code session integration

When ptylenz launches in a project directory that Claude Code has touched, it tails the active session JSONL log and surfaces each turn (user / assistant) as its own block in the same list as your shell commands. Shell blocks and AI turns interleave chronologically — useful when you're pairing with Claude and want a single timeline.

The `e` export uses the [claude-session-replay](https://github.com/opaopa6969/claude-session-replay) common log model, so the exported JSON is consumable by that project's HTML / terminal / MP4 renderers.

## Relationship to syslenz

| | [syslenz](https://github.com/opaopa6969/syslenz) | ptylenz |
|-|---------|---------|
| **What it structures** | `/proc` and `/sys` | PTY output stream |
| **Motivation** | `cat /proc/meminfo` is 1970s UX | scrollback grep is 1978 VT100 UX |
| **Tech** | Rust + ratatui | Rust + ratatui |

Same family, same philosophy: take a raw OS text interface and make it navigable.

## License

MIT
