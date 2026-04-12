# ptylenz

> **Wireshark for your PTY** — structured output blocks for the terminal.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## What is this?

ptylenz sits between you and your shell. Every command's output becomes a **block** you can collapse, search, copy, and navigate. No more scroll hell.

```
Before:                              After:
$ claude-code "fix auth"             ┌─ #42 claude-code "fix auth" ─ 14:23 ─┐
(2000 lines scroll by)               │ ▶ Modified 3 files (2847 lines)       │
(you scroll up frantically)          │ ▷ [expand] [copy] [search] [pin]      │
(give up and re-run)                 └───────────────────────────────────────┘
```

## Quick Start

```bash
cargo install --path .
ptylenz
```

That's it. Your bash starts inside ptylenz. Everything works as before, plus blocks.

## Key Bindings

| Key | Action |
|-----|--------|
| `Ctrl+B` | Toggle block navigation |
| `Ctrl+F` | Search across all blocks |
| `Ctrl+E` | Export session as JSON |
| `j/k` | Navigate blocks (in block nav mode) |
| `Enter` | Expand block |
| `c` | Copy block to clipboard |
| `q/Esc` | Exit block nav / search |

Everything else passes through to bash normally.

## How It Works

ptylenz is a **PTY proxy**. It creates a pseudo-terminal, runs bash inside it,
and intercepts all I/O. Shell integration (OSC 133 markers) tells ptylenz
where each command's output begins and ends.

```
You ←→ ptylenz (PTY master) ←→ bash (PTY slave) → fork/exec → commands
              ↓
         Block Engine
         (segment, index, store)
```

## Zero Config

No `.ptylenzrc`. No themes. No plugins. Just run it.

Works on any machine where you can copy a binary. SSH into a server, `./ptylenz`, done.

## Relationship to syslenz

| | syslenz | ptylenz |
|-|---------|---------|
| **What it structures** | `/proc` and `/sys` | PTY output stream |
| **Motivation** | `cat /proc/meminfo` is 1970s UX | scrollback grep is 1978 VT100 UX |
| **Tech** | Rust + ratatui | Rust + ratatui |

Same family, same philosophy: take a raw OS text interface and make it navigable.

## License

MIT
