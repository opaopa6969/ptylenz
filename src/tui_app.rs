///! TUI Application — the main event loop and renderer.
///!
///! Modes:
///!   Normal — passthrough to bash, blocks rendered above input
///!   Search — full-text search across all blocks
///!   BlockNav — navigate between blocks with j/k
///!
///! Key bindings (in Normal mode):
///!   Ctrl+B      — toggle block navigation mode
///!   Ctrl+F      — search all blocks
///!   Ctrl+↑/↓    — jump to prev/next block
///!   Ctrl+Y      — copy current block to clipboard
///!   Ctrl+P      — pin/unpin current block
///!   Ctrl+E      — export blocks to JSON

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io;
use std::os::fd::AsRawFd;
use std::time::Duration;

use crate::block::BlockEngine;
use crate::pty::PtyProxy;

/// Application mode.
#[derive(Debug, Clone, PartialEq)]
enum Mode {
    /// Normal terminal passthrough.
    Normal,
    /// Block navigation (j/k to move, Enter to expand, q to exit).
    BlockNav { selected: usize },
    /// Full-text search.
    Search { query: String, results: Vec<(usize, usize, String)> },
}

/// The TUI application.
pub struct App {
    shell_path: String,
}

impl App {
    pub fn new(shell_path: &str) -> Result<Self> {
        Ok(App {
            shell_path: shell_path.to_string(),
        })
    }

    /// Main run loop.
    ///
    /// This is the core event loop that:
    /// 1. Polls for user input (keyboard)
    /// 2. Polls for child output (PTY master)
    /// 3. Dispatches to the appropriate handler
    /// 4. Renders the TUI
    ///
    /// For the MVP, this runs in a simplified mode:
    /// - In Normal mode, keystrokes pass through to bash
    /// - PTY output is intercepted and block-segmented
    /// - Ctrl+B toggles block navigation overlay
    ///
    /// TODO: Full ratatui rendering. The initial spike can use
    /// a simpler raw-terminal approach to validate the PTY proxy
    /// works correctly before adding the full TUI layer.
    pub fn run(self) -> Result<()> {
        // Spawn the child shell
        let mut proxy = PtyProxy::spawn(&self.shell_path)?;

        // Put our terminal into raw mode
        terminal::enable_raw_mode()?;

        let mut mode = Mode::Normal;
        let mut buf = [0u8; 4096];
        let mut stdout = io::stdout();

        // Main event loop using poll
        loop {
            // Check if child is still alive
            if !proxy.child_alive() {
                break;
            }

            // Poll for PTY output (child → us)
            // Using a simple non-blocking read with select/poll
            let master_fd = proxy.master_fd();

            // Use crossterm's poll for keyboard events
            if event::poll(Duration::from_millis(10))? {
                if let Event::Key(key) = event::read()? {
                    match &mode {
                        Mode::Normal => {
                            match (key.modifiers, key.code) {
                                // Ctrl+B: toggle block nav
                                (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
                                    let count = proxy.blocks().block_count();
                                    if count > 0 {
                                        mode = Mode::BlockNav { selected: count - 1 };
                                        // TODO: render block nav overlay
                                        eprintln!("\r\n[ptylenz] Block nav: {} blocks. j/k navigate, q quit\r", count);
                                    }
                                }
                                // Ctrl+F: search
                                (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
                                    mode = Mode::Search {
                                        query: String::new(),
                                        results: vec![],
                                    };
                                    eprintln!("\r\n[ptylenz] Search: \r");
                                }
                                // Ctrl+E: export
                                (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                                    let json = proxy.blocks().export_json();
                                    let path = format!("ptylenz-{}.json", chrono::Local::now().format("%Y%m%d-%H%M%S"));
                                    std::fs::write(&path, &json)?;
                                    eprintln!("\r\n[ptylenz] Exported {} blocks to {}\r", proxy.blocks().block_count(), path);
                                }
                                // Everything else: pass through to shell
                                _ => {
                                    let bytes = key_to_bytes(&key);
                                    proxy.write_input(&bytes)?;
                                }
                            }
                        }
                        Mode::BlockNav { selected } => {
                            let selected = *selected;
                            match key.code {
                                KeyCode::Char('q') | KeyCode::Esc => {
                                    mode = Mode::Normal;
                                }
                                KeyCode::Char('k') | KeyCode::Up => {
                                    if selected > 0 {
                                        mode = Mode::BlockNav { selected: selected - 1 };
                                    }
                                    // TODO: render
                                }
                                KeyCode::Char('j') | KeyCode::Down => {
                                    let max = proxy.blocks().block_count().saturating_sub(1);
                                    if selected < max {
                                        mode = Mode::BlockNav { selected: selected + 1 };
                                    }
                                    // TODO: render
                                }
                                KeyCode::Enter => {
                                    // Show block detail
                                    if let Some(block) = proxy.blocks().get_block(selected + 1) {
                                        eprintln!("\r\n{}\r", block.summary());
                                        eprintln!("{}\r", block.preview(20));
                                    }
                                }
                                KeyCode::Char('c') => {
                                    // Copy block to clipboard (via xclip/pbcopy)
                                    if let Some(block) = proxy.blocks().get_block(selected + 1) {
                                        copy_to_clipboard(&block.output_text());
                                        eprintln!("\r\n[ptylenz] Copied block #{}\r", block.id);
                                    }
                                }
                                _ => {}
                            }
                        }
                        Mode::Search { query, .. } => {
                            match key.code {
                                KeyCode::Esc => {
                                    mode = Mode::Normal;
                                }
                                KeyCode::Enter => {
                                    let results = proxy.blocks().search(query);
                                    eprintln!("\r\n[ptylenz] {} matches:\r", results.len());
                                    for (block_id, line, text) in &results {
                                        eprintln!("  Block #{} L{}: {}\r", block_id, line, text);
                                    }
                                    mode = Mode::Normal;
                                }
                                KeyCode::Char(c) => {
                                    if let Mode::Search { ref mut query, .. } = mode {
                                        query.push(c);
                                        eprint!("{}", c);
                                    }
                                }
                                KeyCode::Backspace => {
                                    if let Mode::Search { ref mut query, .. } = mode {
                                        query.pop();
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                // Handle terminal resize
                if let Event::Resize(cols, rows) = event::read().unwrap_or(Event::FocusLost) {
                    proxy.resize(cols, rows).ok();
                }
            }

            // Read from PTY (non-blocking via poll timeout above)
            match proxy.read_output(&mut buf) {
                Ok((0, _)) => break, // EOF
                Ok((n, events)) => {
                    // In Normal mode, forward output to our stdout
                    if mode == Mode::Normal {
                        use io::Write;
                        stdout.write_all(&buf[..n])?;
                        stdout.flush()?;
                    }

                    // Log block events (for debugging; remove later)
                    for event in &events {
                        match event {
                            crate::block::OscEvent::CommandEnd { exit_code } => {
                                // Could show a subtle block boundary indicator
                            }
                            _ => {}
                        }
                    }
                }
                Err(_) => break,
            }
        }

        // Cleanup
        terminal::disable_raw_mode()?;
        println!("\r\n[ptylenz] Session ended. {} blocks captured.", proxy.blocks().block_count());

        Ok(())
    }
}

/// Convert a crossterm KeyEvent to raw bytes for the PTY.
fn key_to_bytes(key: &KeyEvent) -> Vec<u8> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+A = 0x01, Ctrl+B = 0x02, etc.
                let ctrl = (c as u8).wrapping_sub(b'a').wrapping_add(1);
                vec![ctrl]
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                s.as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        _ => vec![],
    }
}

/// Copy text to system clipboard (best-effort).
fn copy_to_clipboard(text: &str) {
    // Try OSC 52 (works in many terminals including tmux)
    let encoded = base64_encode(text.as_bytes());
    eprint!("\x1b]52;c;{}\x07", encoded);

    // Fallback: try xclip or pbcopy
    #[cfg(target_os = "linux")]
    {
        use std::process::{Command, Stdio};
        if let Ok(mut child) = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(stdin) = child.stdin.as_mut() {
                use io::Write;
                stdin.write_all(text.as_bytes()).ok();
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        if let Ok(mut child) = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(stdin) = child.stdin.as_mut() {
                use io::Write;
                stdin.write_all(text.as_bytes()).ok();
            }
        }
    }
}

/// Simple base64 encode (for OSC 52 clipboard).
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((n >> 18) & 63) as usize] as char);
        result.push(CHARS[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((n >> 6) & 63) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(n & 63) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}
