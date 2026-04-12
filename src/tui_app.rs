///! TUI Application — main event loop and renderer.
///!
///! Two visual modes:
///!   - Normal: transparent passthrough. PTY output (with OSC 133 stripped)
///!     is written directly to stdout, and keystrokes flow to bash. ptylenz
///!     is invisible.
///!   - Overlay: on Ctrl+B, we switch into the alternate screen and draw
///!     a ratatui block-list/detail UI. Leaving the overlay restores the
///!     primary screen untouched.
///!
///! The main loop multiplexes the PTY master fd and stdin via `polling`
///! so we never spin-wait on either source.
///!
///! Key bindings (Normal mode, tmux-style prefix):
///!   Ctrl+]    prefix. After it, within 500ms:
///!     b       open block-nav overlay
///!     /       open search overlay
///!     e       export blocks to JSON (current dir)
///!     y       copy most recent block to clipboard
///!     ]       send a literal Ctrl+] through to the child
///!   If no follow-up key arrives in time, the Ctrl+] itself is
///!   delivered to the child so readline bindings etc. still work.
///!
///! Key bindings (overlay):
///!   q / Esc  close overlay (or: in detail view, go back to list)
///!   j / ↓    next block / scroll down
///!   k / ↑    prev block / scroll up
///!   Enter    list → detail view
///!   y        copy focused block to clipboard (OSC 52 + xclip/pbcopy)
///!   /        (block-nav) search across all blocks

use anyhow::{Context, Result};
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use polling::{Event as PollEvent, Events as PollEvents, PollMode, Poller};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as RBlock, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use std::io::{self, Write};
use std::os::fd::BorrowedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Prefix key (Ctrl+]) — initiates a tmux-style command sequence.
const PREFIX_KEY: u8 = 0x1d;
/// Window during which a follow-up key is accepted before we give up
/// and forward the prefix byte to the child verbatim.
const PREFIX_TIMEOUT: Duration = Duration::from_millis(500);

use crate::block::{Block, BlockSource};
use crate::claude_feeder;
use crate::pty::PtyProxy;

const STDIN_KEY: usize = 0;
const PTY_KEY: usize = 1;

static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigwinch(_: libc::c_int) {
    RESIZED.store(true, Ordering::Relaxed);
}

fn install_sigwinch_handler() {
    unsafe {
        libc::signal(libc::SIGWINCH, on_sigwinch as *const () as libc::sighandler_t);
    }
}

#[derive(Debug, Clone, PartialEq)]
enum OverlayView {
    List,
    Detail,
    Search,
}

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    Normal,
    Overlay {
        view: OverlayView,
        selected: usize,
        detail_scroll: u16,
        query: String,
        results: Vec<(usize, usize, String)>,
    },
}

pub struct App {
    shell_path: String,
}

impl App {
    pub fn new(shell_path: &str) -> Result<Self> {
        Ok(App { shell_path: shell_path.to_string() })
    }

    pub fn run(self) -> Result<()> {
        let mut proxy = PtyProxy::spawn(&self.shell_path)?;

        if let Some((cols, rows)) = terminal_size() {
            proxy.resize(cols, rows).ok();
        }

        install_sigwinch_handler();

        // Start watching Claude Code's JSONL for the current cwd so that
        // `claude` sessions launched from the inner shell become per-turn
        // blocks instead of one opaque alt-screen dump. Silent-fail if the
        // env gives us no HOME or the user never runs claude.
        let claude_rx = match std::env::current_dir() {
            Ok(cwd) => claude_feeder::spawn_watcher(&cwd),
            Err(_) => std::sync::mpsc::channel().1,
        };

        let saved_termios = set_raw_mode()?;
        let _term_guard = TermiosGuard(saved_termios);

        let stdin_fd = libc::STDIN_FILENO;
        let master_fd = proxy.master_fd();

        set_nonblocking(stdin_fd)?;
        set_nonblocking(master_fd)?;

        let poller = Poller::new().context("create poller")?;
        let stdin_borrowed = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
        let master_borrowed = unsafe { BorrowedFd::borrow_raw(master_fd) };
        unsafe {
            poller.add_with_mode(&stdin_borrowed, PollEvent::readable(STDIN_KEY), PollMode::Level)?;
            poller.add_with_mode(&master_borrowed, PollEvent::readable(PTY_KEY), PollMode::Level)?;
        }

        let mut events = PollEvents::new();
        let mut buf = [0u8; 8192];
        let mut stdout = io::stdout();
        let mut mode = Mode::Normal;
        // Some(instant) while we're waiting for the second key of a prefix
        // command. None otherwise.
        let mut prefix_since: Option<Instant> = None;

        // Terminal used only while an overlay is active.
        let mut overlay_term: Option<Terminal<CrosstermBackend<io::Stdout>>> = None;

        loop {
            if !proxy.child_alive() {
                break;
            }

            if RESIZED.swap(false, Ordering::Relaxed) {
                if let Some((cols, rows)) = terminal_size() {
                    proxy.resize(cols, rows).ok();
                    if let Some(term) = overlay_term.as_mut() {
                        term.autoresize().ok();
                    }
                }
            }

            // Drain any Claude turn events that arrived since the last tick.
            // try_recv is non-blocking; Disconnected means the watcher thread
            // exited (e.g. bad HOME) and we just stop listening.
            loop {
                match claude_rx.try_recv() {
                    Ok(ev) => proxy.blocks_mut().ingest_claude_event(ev),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }

            // Prefix timeout: the user pressed Ctrl+] but never followed up.
            // Deliver the Ctrl+] to the child so things like readline
            // (Ctrl+] is the emacs-mode char-search binding) still work.
            if let Some(t) = prefix_since {
                if t.elapsed() >= PREFIX_TIMEOUT {
                    proxy.write_input(&[PREFIX_KEY])?;
                    prefix_since = None;
                }
            }

            // Shorten the poll so the timeout above fires close to 500ms
            // instead of up to 580ms. Default 80ms tick otherwise.
            let poll_timeout = match prefix_since {
                Some(t) => {
                    let remaining = PREFIX_TIMEOUT.saturating_sub(t.elapsed());
                    remaining.min(Duration::from_millis(80))
                }
                None => Duration::from_millis(80),
            };

            events.clear();
            poller.wait(&mut events, Some(poll_timeout))?;

            for ev in events.iter() {
                if ev.key == PTY_KEY {
                    match proxy.read_output(&mut buf) {
                        Ok((clean, _)) if clean.is_empty() => {
                            if !proxy.child_alive() {
                                // child exited
                            }
                        }
                        Ok((clean, _)) => {
                            if matches!(mode, Mode::Normal) {
                                stdout.write_all(&clean)?;
                                stdout.flush()?;
                            }
                            // In overlay mode we keep reading (so the block
                            // engine stays fresh) but don't paint output over
                            // the alt screen.
                        }
                        Err(e) => {
                            let msg = format!("{e}");
                            if !msg.contains("EAGAIN")
                                && !msg.contains("Resource temporarily unavailable")
                            {
                                return Err(e);
                            }
                        }
                    }
                } else if ev.key == STDIN_KEY {
                    let mut sbuf = [0u8; 512];
                    match read_stdin(stdin_fd, &mut sbuf) {
                        Ok(0) => return Ok(()),
                        Ok(n) => {
                            handle_input(
                                &sbuf[..n],
                                &mut mode,
                                &mut proxy,
                                &mut overlay_term,
                                &mut prefix_since,
                            )?;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e).context("read stdin"),
                    }
                }
            }

            // Redraw overlay on every loop tick (covers resize + block updates).
            if matches!(mode, Mode::Overlay { .. }) {
                if let Some(term) = overlay_term.as_mut() {
                    draw_overlay(term, &mode, &proxy)?;
                }
            }
        }

        // Make sure we're not stuck in the alt screen on exit.
        if let Some(mut term) = overlay_term.take() {
            let _ = term.show_cursor();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }

        println!(
            "\r\n[ptylenz] Session ended. {} blocks captured.\r",
            proxy.blocks().block_count()
        );
        Ok(())
    }
}

fn handle_input(
    bytes: &[u8],
    mode: &mut Mode,
    proxy: &mut PtyProxy,
    overlay_term: &mut Option<Terminal<CrosstermBackend<io::Stdout>>>,
    prefix_since: &mut Option<Instant>,
) -> Result<()> {
    match mode {
        Mode::Normal => {
            let mut passthrough: Vec<u8> = Vec::with_capacity(bytes.len());
            let mut iter = bytes.iter().copied();
            while let Some(b) = iter.next() {
                // Follow-up key after the prefix.
                if prefix_since.is_some() {
                    *prefix_since = None;
                    if !passthrough.is_empty() {
                        proxy.write_input(&passthrough)?;
                        passthrough.clear();
                    }
                    match dispatch_prefix(b, mode, proxy, overlay_term)? {
                        PrefixOutcome::Handled => {}
                        PrefixOutcome::Passthrough(bytes_to_send) => {
                            proxy.write_input(&bytes_to_send)?;
                        }
                        PrefixOutcome::EnteredOverlay => {
                            // Any remaining bytes in this batch belong to the
                            // overlay now — feed them through the overlay path.
                            let rest: Vec<u8> = iter.collect();
                            if !rest.is_empty() {
                                handle_overlay_bytes(&rest, mode, proxy, overlay_term)?;
                            }
                            return Ok(());
                        }
                    }
                } else if b == PREFIX_KEY {
                    if !passthrough.is_empty() {
                        proxy.write_input(&passthrough)?;
                        passthrough.clear();
                    }
                    *prefix_since = Some(Instant::now());
                } else {
                    passthrough.push(b);
                }
            }
            if !passthrough.is_empty() {
                proxy.write_input(&passthrough)?;
            }
        }
        Mode::Overlay { .. } => {
            // Re-interpret the bytes via crossterm's key parser for convenience.
            // Bytes were already delivered to stdin; we poll crossterm here
            // non-blockingly (it may or may not have the events depending on
            // timing, so we ALSO parse raw bytes directly for reliability).
            handle_overlay_bytes(bytes, mode, proxy, overlay_term)?;
        }
    }
    Ok(())
}

/// Result of consuming a post-prefix key.
enum PrefixOutcome {
    /// Command handled fully inside Normal mode (no overlay, no passthrough).
    Handled,
    /// The command is a passthrough to the child — these bytes should be
    /// written to the PTY master.
    Passthrough(Vec<u8>),
    /// Switched into overlay mode. Remaining batch bytes, if any, should be
    /// forwarded to the overlay handler rather than the shell.
    EnteredOverlay,
}

fn dispatch_prefix(
    key: u8,
    mode: &mut Mode,
    proxy: &mut PtyProxy,
    overlay_term: &mut Option<Terminal<CrosstermBackend<io::Stdout>>>,
) -> Result<PrefixOutcome> {
    match key {
        b'b' => {
            enter_overlay(mode, overlay_term, proxy, OverlayView::List)?;
            Ok(PrefixOutcome::EnteredOverlay)
        }
        b'/' => {
            enter_overlay(mode, overlay_term, proxy, OverlayView::Search)?;
            Ok(PrefixOutcome::EnteredOverlay)
        }
        b'e' => {
            let json = proxy.blocks().export_json();
            let path = format!(
                "ptylenz-{}.json",
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            );
            std::fs::write(&path, json)?;
            eprint!(
                "\r\n[ptylenz] exported {} blocks → {}\r\n",
                proxy.blocks().block_count(),
                path
            );
            Ok(PrefixOutcome::Handled)
        }
        b'y' => {
            let count = proxy.blocks().block_count();
            if count > 0 {
                if let Some(block) = proxy.blocks().get_block_by_index(count - 1) {
                    let id = block.id;
                    let text = block.output_text();
                    copy_to_clipboard(&text);
                    eprint!("\r\n[ptylenz] copied block #{}\r\n", id);
                }
            }
            Ok(PrefixOutcome::Handled)
        }
        PREFIX_KEY => {
            // prefix + ] — explicit escape hatch to send a literal Ctrl+].
            Ok(PrefixOutcome::Passthrough(vec![PREFIX_KEY]))
        }
        // Unknown key: drop the prefix silently and forward the key as-is.
        // tmux-ish behavior; avoids injecting surprise control bytes.
        other => Ok(PrefixOutcome::Passthrough(vec![other])),
    }
}

fn enter_overlay(
    mode: &mut Mode,
    overlay_term: &mut Option<Terminal<CrosstermBackend<io::Stdout>>>,
    proxy: &PtyProxy,
    view: OverlayView,
) -> Result<()> {
    let count = proxy.blocks().block_count();
    let selected = count.saturating_sub(1);

    execute!(io::stdout(), EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend).context("create terminal")?;
    term.hide_cursor().ok();

    *mode = Mode::Overlay {
        view,
        selected,
        detail_scroll: 0,
        query: String::new(),
        results: Vec::new(),
    };

    draw_overlay(&mut term, mode, proxy)?;
    *overlay_term = Some(term);
    Ok(())
}

fn leave_overlay(
    mode: &mut Mode,
    overlay_term: &mut Option<Terminal<CrosstermBackend<io::Stdout>>>,
) -> Result<()> {
    if let Some(mut term) = overlay_term.take() {
        let _ = term.show_cursor();
    }
    execute!(io::stdout(), LeaveAlternateScreen).context("leave alt screen")?;
    *mode = Mode::Normal;
    Ok(())
}

fn handle_overlay_bytes(
    bytes: &[u8],
    mode: &mut Mode,
    proxy: &mut PtyProxy,
    overlay_term: &mut Option<Terminal<CrosstermBackend<io::Stdout>>>,
) -> Result<()> {
    // Decode the byte stream into (key_code, modifiers) pairs. We handle
    // the common cases: plain chars, ESC, arrow keys (CSI A/B/C/D),
    // backspace, enter. Anything unrecognised is ignored.
    let keys = decode_keys(bytes);

    for key in keys {
        let (code, ctrl) = key;
        let is_escape = code == Key::Esc;

        let Mode::Overlay {
            view,
            selected,
            detail_scroll,
            query,
            results,
        } = mode
        else {
            return Ok(());
        };

        match view {
            OverlayView::List => match code {
                Key::Char('q') | Key::Esc => {
                    return leave_overlay(mode, overlay_term);
                }
                Key::Char('j') | Key::Down => {
                    let max = proxy.blocks().block_count().saturating_sub(1);
                    if *selected < max {
                        *selected += 1;
                    }
                }
                Key::Char('k') | Key::Up => {
                    if *selected > 0 {
                        *selected -= 1;
                    }
                }
                Key::Enter => {
                    *view = OverlayView::Detail;
                    *detail_scroll = 0;
                }
                Key::Char('y') => {
                    if let Some(block) = proxy.blocks().get_block_by_index(*selected) {
                        copy_to_clipboard(&block.output_text());
                    }
                }
                Key::Char('/') => {
                    *view = OverlayView::Search;
                    query.clear();
                    results.clear();
                }
                _ => {}
            },
            OverlayView::Detail => match code {
                Key::Char('q') | Key::Esc => {
                    *view = OverlayView::List;
                }
                Key::Char('j') | Key::Down => {
                    *detail_scroll = detail_scroll.saturating_add(1);
                }
                Key::Char('k') | Key::Up => {
                    *detail_scroll = detail_scroll.saturating_sub(1);
                }
                Key::Char('y') => {
                    if let Some(block) = proxy.blocks().get_block_by_index(*selected) {
                        copy_to_clipboard(&block.output_text());
                    }
                }
                _ => {}
            },
            OverlayView::Search => match code {
                Key::Esc => {
                    *view = OverlayView::List;
                }
                Key::Enter => {
                    *results = proxy.blocks().search(query);
                    // Jump selection to first hit, if any. The search result
                    // carries a block *id*, so translate it to the index the
                    // list view is navigating by.
                    if let Some(first) = results.first() {
                        if let Some(idx) = proxy.blocks().index_of_block_id(first.0) {
                            *selected = idx;
                        }
                        *view = OverlayView::List;
                    }
                }
                Key::Backspace => {
                    query.pop();
                }
                Key::Char(c) if !ctrl => {
                    query.push(c);
                }
                _ => {}
            },
        }

        if is_escape {
            // Extra guard: handle_input loop may have already closed overlay.
            if matches!(mode, Mode::Normal) {
                return Ok(());
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Backspace,
    Esc,
    Tab,
    Unknown,
}

/// Minimal key decoder for overlay mode. Ctrl-modifier flag returned alongside.
fn decode_keys(bytes: &[u8]) -> Vec<(Key, bool)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1b => {
                // ESC — check for CSI sequence
                if i + 2 < bytes.len() && bytes[i + 1] == b'[' {
                    let c = bytes[i + 2];
                    let key = match c {
                        b'A' => Key::Up,
                        b'B' => Key::Down,
                        b'C' => Key::Right,
                        b'D' => Key::Left,
                        _ => Key::Unknown,
                    };
                    out.push((key, false));
                    i += 3;
                    continue;
                }
                out.push((Key::Esc, false));
                i += 1;
            }
            b'\r' | b'\n' => {
                out.push((Key::Enter, false));
                i += 1;
            }
            0x7f | 0x08 => {
                out.push((Key::Backspace, false));
                i += 1;
            }
            b'\t' => {
                out.push((Key::Tab, false));
                i += 1;
            }
            0x01..=0x1a => {
                // Ctrl+A..Ctrl+Z
                let c = (b - 1 + b'a') as char;
                out.push((Key::Char(c), true));
                i += 1;
            }
            0x20..=0x7e => {
                out.push((Key::Char(b as char), false));
                i += 1;
            }
            _ => {
                // Treat other bytes as utf-8 chars (best-effort single byte).
                out.push((Key::Unknown, false));
                i += 1;
            }
        }
    }
    out
}

fn draw_overlay(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mode: &Mode,
    proxy: &PtyProxy,
) -> Result<()> {
    let Mode::Overlay {
        view,
        selected,
        detail_scroll,
        query,
        results,
    } = mode
    else {
        return Ok(());
    };

    term.draw(|f| {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);

        match view {
            OverlayView::List => draw_list(f, chunks[0], proxy, *selected),
            OverlayView::Detail => draw_detail(f, chunks[0], proxy, *selected, *detail_scroll),
            OverlayView::Search => draw_search(f, chunks[0], proxy, query, results),
        }

        let help = match view {
            OverlayView::List => "j/k move  Enter detail  y copy  / search  q back",
            OverlayView::Detail => "j/k scroll  y copy  q back",
            OverlayView::Search => "type query  Enter run  Esc cancel",
        };
        let status = Paragraph::new(Span::styled(
            format!(" [ptylenz] {}   blocks: {}", help, proxy.blocks().block_count()),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ));
        f.render_widget(status, chunks[1]);
    })?;
    Ok(())
}

fn draw_list(
    f: &mut ratatui::Frame,
    area: Rect,
    proxy: &PtyProxy,
    selected: usize,
) {
    let all: Vec<&Block> = proxy
        .blocks()
        .completed_blocks()
        .iter()
        .chain(proxy.blocks().current_block().into_iter())
        .collect();

    let items: Vec<ListItem> = all
        .iter()
        .map(|b| {
            let (tag, tag_style, row_style) = match &b.source {
                BlockSource::Shell => {
                    let s = match b.exit_code {
                        Some(0) => Style::default().fg(Color::Green),
                        Some(_) => Style::default().fg(Color::Red),
                        None => Style::default().fg(Color::Yellow),
                    };
                    ("S", Style::default().fg(Color::DarkGray), s)
                }
                BlockSource::ClaudeTurn { role, .. } => {
                    let role_style = if role == "user" {
                        Style::default().fg(Color::Magenta)
                    } else {
                        Style::default().fg(Color::Cyan)
                    };
                    ("C", role_style, role_style)
                }
            };
            let cmd = b.command.clone().unwrap_or_else(|| "(unknown)".to_string());
            let status = match &b.source {
                BlockSource::Shell => match b.exit_code {
                    Some(0) => "ok".to_string(),
                    Some(c) => format!("exit {c}"),
                    None => "…".to_string(),
                },
                BlockSource::ClaudeTurn { role, .. } => role.clone(),
            };
            let text = format!(
                " {:<3} {}  ·  {} lines  ·  {}  ·  {}",
                format!("#{}", b.id),
                b.started_at.format("%H:%M:%S"),
                b.line_count(),
                status,
                cmd,
            );
            ListItem::new(Line::from(vec![
                Span::styled(format!("[{}] ", tag), tag_style),
                Span::styled(text, row_style),
            ]))
        })
        .collect();

    let block = RBlock::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " ptylenz · blocks ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let mut state = ListState::default();
    if !all.is_empty() {
        state.select(Some(selected.min(all.len().saturating_sub(1))));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(
    f: &mut ratatui::Frame,
    area: Rect,
    proxy: &PtyProxy,
    selected: usize,
    scroll: u16,
) {
    let block_ref = proxy.blocks().get_block_by_index(selected);
    // For the currently-running block we prefer the live vt100 snapshot over
    // `output_text()`: the raw buffer keeps every frame a TUI redraws, and
    // strip_ansi only removes CSI — CR and cursor-overwrite bytes survive and
    // render as stacked/overlapping lines in the Paragraph.
    let is_current = block_ref
        .map(|b| Some(b.id) == proxy.blocks().current_block().map(|c| c.id))
        .unwrap_or(false);
    let text = match block_ref {
        Some(b) => {
            if is_current {
                if let Some(snap) = proxy.blocks().current_alt_snapshot() {
                    snap.to_string()
                } else {
                    strip_bare_cr(&b.output_text())
                }
            } else {
                b.output_text()
            }
        }
        None => "(no block)".to_string(),
    };
    let title = match block_ref {
        Some(b) => match &b.source {
            BlockSource::Shell => format!(
                " #{} · {} · {} ",
                b.id,
                b.command.as_deref().unwrap_or("(unknown)"),
                match b.exit_code {
                    Some(c) => format!("exit {c}"),
                    None => "running".into(),
                }
            ),
            BlockSource::ClaudeTurn {
                role,
                turn_index,
                session_id,
                ..
            } => format!(
                " #{} · [C] {} turn #{} · session {} ",
                b.id,
                role,
                turn_index,
                short_session(session_id),
            ),
        },
        None => " detail ".to_string(),
    };

    let rblock = RBlock::default().borders(Borders::ALL).title(title);
    let para = Paragraph::new(text)
        .block(rblock)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(para, area);
}

fn draw_search(
    f: &mut ratatui::Frame,
    area: Rect,
    _proxy: &PtyProxy,
    query: &str,
    results: &[(usize, usize, String)],
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);

    let input = Paragraph::new(query).block(
        RBlock::default()
            .borders(Borders::ALL)
            .title(" search (Enter to run, Esc to cancel) "),
    );
    f.render_widget(input, chunks[0]);

    let items: Vec<ListItem> = results
        .iter()
        .map(|(id, line, text)| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("#{id} L{line}  "), Style::default().fg(Color::Cyan)),
                Span::raw(trim_line(text, 200)),
            ]))
        })
        .collect();
    let list = List::new(items).block(
        RBlock::default()
            .borders(Borders::ALL)
            .title(format!(" {} matches ", results.len())),
    );
    f.render_widget(list, chunks[1]);
}

/// Truncate a session uuid to its first 8 chars for the detail-view title.
fn short_session(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Drop standalone CR (not part of a CRLF) so the Paragraph widget doesn't
/// treat them as line continuations that overwrite earlier text — a common
/// effect with progress bars / spinners captured in the raw buffer when no
/// vt100 snapshot is available.
fn strip_bare_cr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                out.push('\r');
                out.push('\n');
                chars.next();
            }
            // else: bare CR — drop
        } else {
            out.push(c);
        }
    }
    out
}

fn trim_line(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn read_stdin(fd: i32, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn set_nonblocking(fd: i32) -> Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error()).context("fcntl F_GETFL");
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error()).context("fcntl F_SETFL");
        }
    }
    Ok(())
}

fn set_raw_mode() -> Result<libc::termios> {
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    unsafe {
        if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
            return Err(io::Error::last_os_error()).context("tcgetattr");
        }
    }
    let mut raw = saved;
    unsafe {
        libc::cfmakeraw(&mut raw);
        if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
            return Err(io::Error::last_os_error()).context("tcsetattr raw");
        }
    }
    Ok(saved)
}

struct TermiosGuard(libc::termios);
impl Drop for TermiosGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
        }
    }
}

fn terminal_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe {
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 {
            return Some((ws.ws_col, ws.ws_row));
        }
    }
    None
}

fn copy_to_clipboard(text: &str) {
    let encoded = base64_encode(text.as_bytes());
    // OSC 52: many terminals (incl. tmux with set-clipboard on) will accept.
    eprint!("\x1b]52;c;{}\x07", encoded);

    #[cfg(target_os = "linux")]
    {
        use std::process::{Command, Stdio};
        if let Ok(mut child) = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
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

