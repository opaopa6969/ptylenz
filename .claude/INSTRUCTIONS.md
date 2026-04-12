# ptylenz — Claude Code Instructions

## Project Context

ptylenz is a PTY proxy + TUI that structures terminal output into blocks.
Think "Warp's block-based output, but as a TUI that wraps bash."

The developer (Opa) also maintains syslenz (Wireshark for /proc) which shares
the same Rust + ratatui tech stack and zero-config philosophy.

## Key Files

- `PROJECT.md` — Full architecture, decisions, and roadmap
- `DESIGN.md` — Design rationale from the original brainstorming session
- `src/pty.rs` — PTY proxy core (fork, exec, relay)
- `src/block.rs` — Block engine (OSC parsing, segmentation, search)
- `src/tui_app.rs` — TUI event loop and rendering (currently minimal)

## Current State

The code compiles and has the core architecture in place:
- PTY proxy spawns bash in a new PTY
- OSC 133 parser detects block boundaries
- Block engine segments and indexes output
- Basic TUI event loop with mode switching

**What needs work (priority order):**

1. **Event loop fix**: The current poll loop has issues with concurrent
   PTY read + keyboard input. Needs proper `poll(2)` or `mio` for
   multiplexing the PTY master fd and stdin.

2. **Shell integration injection**: The BASH_INTEGRATION script needs
   to be reliably injected. Current approach via `--rcfile` breaks
   user's `.bashrc`. Better: write a temp file that sources both
   `.bashrc` and the integration, then use `--rcfile` pointing to that.

3. **TUI rendering with ratatui**: The current TUI is eprintln-based
   (for spike purposes). Needs proper ratatui rendering with:
   - Block list panel (collapsed view with summaries)
   - Expanded block view (full output with ANSI colors)
   - Search overlay
   - Status bar

4. **Terminal raw mode interaction**: When in Normal mode, the terminal
   needs to be in raw mode for key capture, but the child bash also
   expects certain terminal settings. Need careful termios management.

## Design Constraints

- **bash stays bash** — Don't try to replace or modify bash behavior.
  ptylenz is transparent; the user shouldn't notice it's there
  until they press Ctrl+B.

- **Zero config** — No config file required. Sensible defaults only.

- **Single binary** — No runtime dependencies beyond libc.

- **OSC 133 is the standard** — Don't invent a custom protocol.
  Use the same markers as iTerm2/Warp/VS Code Terminal.

## Code Style

- Follow syslenz conventions (see github.com/opaopa6969/syslenz)
- Modules are separated by concern (pty, block, tui)
- Tests in the same file (mod tests)
- Error handling: anyhow for application errors, proper Result types
- Comments in English, user-facing strings can be bilingual (EN/JA)

## Testing

```bash
cargo test                 # Unit tests (OSC parser, block engine)
cargo run                  # Manual smoke test
cargo run -- --shell /bin/sh  # Test with different shell
```

## Don't

- Don't add a config file system
- Don't add plugin support
- Don't try to parse command output semantically
- Don't add session persistence (tmux handles that)
- Don't add a web UI (syslenz has that covered separately)
