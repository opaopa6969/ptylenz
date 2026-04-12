///! TUI Application — main event loop and renderer.
///!
///! Two modes, no chord/prefix soup:
///!   - Normal: full passthrough. Every keystroke except Ctrl+] goes to
///!     bash; PTY output (OSC-stripped) flows straight to stdout. ptylenz
///!     is invisible.
///!   - Ptylenz: alt-screen overlay rendered by ratatui. bash keeps running
///!     in the background but its stdin is paused while we own the terminal.
///!
///! Only one keystroke in Normal mode is ever intercepted: Ctrl+]
///! (0x1d). Pressing it enters Ptylenz mode; pressing it again (or q / Esc)
///! leaves.
///!
///! Ptylenz-mode keys:
///!   j / ↓         next block
///!   k / ↑         previous block
///!   g             jump to first block
///!   G             jump to last block
///!   Enter         toggle expand/collapse of selected block
///!   /             search sub-mode
///!   n             next search result
///!   N             previous search result
///!   y             copy selected block's output to clipboard
///!   e             export all blocks as JSON (current dir)
///!   p             toggle pin on selected block
///!   q / Esc / Ctrl+]   back to Normal

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
    widgets::{Block as RBlock, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};
use std::io::{self, Write};
use std::os::fd::BorrowedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::block::{Block, BlockSource};
use crate::claude_feeder;
use crate::pty::PtyProxy;

const STDIN_KEY: usize = 0;
const PTY_KEY: usize = 1;

/// The one key ptylenz claims out of Normal mode. Ctrl+] (ASCII GS, 0x1d).
/// Chosen because no common shell/editor uses it and tmux uses Ctrl+B by
/// default, so it doesn't collide.
const MODE_SWITCH_KEY: u8 = 0x1d;

/// Max output lines rendered inline when a block is expanded. Anything
/// beyond this is truncated with an ellipsis — the block still exists
/// in full and can be exported via `e`.
const EXPAND_MAX_LINES: usize = 200;

static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigwinch(_: libc::c_int) {
    RESIZED.store(true, Ordering::Relaxed);
}

fn install_sigwinch_handler() {
    unsafe {
        libc::signal(libc::SIGWINCH, on_sigwinch as *const () as libc::sighandler_t);
    }
}

/// Persistent search state inside Ptylenz mode. Survives exiting the
/// search sub-mode so `n`/`N` keep working on the last result set.
#[derive(Debug, Clone)]
struct SearchState {
    query: String,
    results: Vec<(usize, usize, String)>,
    result_index: usize,
}

#[derive(Debug, Clone)]
enum Mode {
    Normal,
    Ptylenz {
        selected: usize,
        /// Some while the user is typing a search query (`/` pressed).
        /// None otherwise — including after Enter, when we fall back to
        /// the block list with `last_search` populated for n/N.
        search_input: Option<String>,
        /// Populated after a search is run. n/N cycle through its results
        /// until a new search is started or the user leaves Ptylenz mode.
        last_search: Option<SearchState>,
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

        // Alt-screen terminal — only exists while Ptylenz mode is active.
        let mut ptylenz_term: Option<Terminal<CrosstermBackend<io::Stdout>>> = None;

        loop {
            if !proxy.child_alive() {
                break;
            }

            if RESIZED.swap(false, Ordering::Relaxed) {
                if let Some((cols, rows)) = terminal_size() {
                    proxy.resize(cols, rows).ok();
                    if let Some(term) = ptylenz_term.as_mut() {
                        term.autoresize().ok();
                    }
                }
            }

            loop {
                match claude_rx.try_recv() {
                    Ok(ev) => proxy.blocks_mut().ingest_claude_event(ev),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }

            events.clear();
            poller.wait(&mut events, Some(Duration::from_millis(80)))?;

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
                            // Ptylenz mode: the block engine still consumes
                            // the bytes (done inside read_output), but we
                            // don't paint them over the alt-screen UI.
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
                                &mut ptylenz_term,
                            )?;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e).context("read stdin"),
                    }
                }
            }

            if matches!(mode, Mode::Ptylenz { .. }) {
                if let Some(term) = ptylenz_term.as_mut() {
                    draw_ptylenz(term, &mode, &proxy)?;
                }
            }
        }

        if let Some(mut term) = ptylenz_term.take() {
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
    ptylenz_term: &mut Option<Terminal<CrosstermBackend<io::Stdout>>>,
) -> Result<()> {
    match mode {
        Mode::Normal => {
            let mut pass: Vec<u8> = Vec::with_capacity(bytes.len());
            let mut iter = bytes.iter().copied();
            while let Some(b) = iter.next() {
                if b == MODE_SWITCH_KEY {
                    if !pass.is_empty() {
                        proxy.write_input(&pass)?;
                        pass.clear();
                    }
                    enter_ptylenz(mode, ptylenz_term, proxy)?;
                    // Any keys that came in the same batch after Ctrl+]
                    // should be interpreted by the Ptylenz mode handler,
                    // not lost or sent to the shell.
                    let rest: Vec<u8> = iter.collect();
                    if !rest.is_empty() {
                        handle_ptylenz_bytes(&rest, mode, proxy, ptylenz_term)?;
                    }
                    return Ok(());
                }
                pass.push(b);
            }
            if !pass.is_empty() {
                proxy.write_input(&pass)?;
            }
        }
        Mode::Ptylenz { .. } => {
            handle_ptylenz_bytes(bytes, mode, proxy, ptylenz_term)?;
        }
    }
    Ok(())
}

fn enter_ptylenz(
    mode: &mut Mode,
    ptylenz_term: &mut Option<Terminal<CrosstermBackend<io::Stdout>>>,
    proxy: &PtyProxy,
) -> Result<()> {
    let count = proxy.blocks().block_count();
    let selected = count.saturating_sub(1);

    execute!(io::stdout(), EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend).context("create terminal")?;
    term.hide_cursor().ok();

    *mode = Mode::Ptylenz {
        selected,
        search_input: None,
        last_search: None,
    };

    draw_ptylenz(&mut term, mode, proxy)?;
    *ptylenz_term = Some(term);
    Ok(())
}

fn leave_ptylenz(
    mode: &mut Mode,
    ptylenz_term: &mut Option<Terminal<CrosstermBackend<io::Stdout>>>,
) -> Result<()> {
    if let Some(mut term) = ptylenz_term.take() {
        let _ = term.show_cursor();
    }
    execute!(io::stdout(), LeaveAlternateScreen).context("leave alt screen")?;
    *mode = Mode::Normal;
    Ok(())
}

fn handle_ptylenz_bytes(
    bytes: &[u8],
    mode: &mut Mode,
    proxy: &mut PtyProxy,
    ptylenz_term: &mut Option<Terminal<CrosstermBackend<io::Stdout>>>,
) -> Result<()> {
    let keys = decode_keys(bytes);
    for (code, ctrl) in keys {
        let Mode::Ptylenz {
            selected,
            search_input,
            last_search,
        } = mode
        else {
            return Ok(());
        };

        // Ctrl+] always leaves Ptylenz mode, regardless of sub-state.
        if ctrl && code == Key::Char(']') {
            return leave_ptylenz(mode, ptylenz_term);
        }

        if let Some(query) = search_input.as_mut() {
            match handle_search_input(code, ctrl, query, selected, last_search, proxy) {
                SearchInputOutcome::Continue => {}
                SearchInputOutcome::Submitted | SearchInputOutcome::Cancelled => {
                    *search_input = None;
                }
            }
            continue;
        }

        match code {
            Key::Char('q') | Key::Esc => {
                return leave_ptylenz(mode, ptylenz_term);
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
            Key::Char('g') => {
                *selected = 0;
            }
            Key::Char('G') => {
                *selected = proxy.blocks().block_count().saturating_sub(1);
            }
            Key::Enter => {
                if let Some(block) = proxy.blocks().get_block_by_index(*selected) {
                    let id = block.id;
                    proxy.blocks_mut().toggle_collapse(id);
                }
            }
            Key::Char('/') => {
                *search_input = Some(String::new());
            }
            Key::Char('n') => {
                jump_search(last_search, selected, proxy, 1);
            }
            Key::Char('N') => {
                jump_search(last_search, selected, proxy, -1);
            }
            Key::Char('y') => {
                if let Some(block) = proxy.blocks().get_block_by_index(*selected) {
                    let id = block.id;
                    let text = block.output_text();
                    copy_to_clipboard(&text);
                    eprint!("\r\n[ptylenz] copied block #{}\r\n", id);
                }
            }
            Key::Char('e') => {
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
            }
            Key::Char('p') => {
                if let Some(block) = proxy.blocks().get_block_by_index(*selected) {
                    let id = block.id;
                    proxy.blocks_mut().toggle_pin(id);
                }
            }
            _ => {}
        }
    }

    Ok(())
}

enum SearchInputOutcome {
    Continue,
    Submitted,
    Cancelled,
}

fn handle_search_input(
    code: Key,
    ctrl: bool,
    query: &mut String,
    selected: &mut usize,
    last_search: &mut Option<SearchState>,
    proxy: &PtyProxy,
) -> SearchInputOutcome {
    match code {
        Key::Esc => {
            // Cancel input; keep any prior last_search intact so the user
            // can still n/N through an earlier query.
            SearchInputOutcome::Cancelled
        }
        Key::Enter => {
            let results = proxy.blocks().search(query);
            if let Some(first) = results.first() {
                if let Some(idx) = proxy.blocks().index_of_block_id(first.0) {
                    *selected = idx;
                }
            }
            *last_search = Some(SearchState {
                query: std::mem::take(query),
                results,
                result_index: 0,
            });
            SearchInputOutcome::Submitted
        }
        Key::Backspace => {
            query.pop();
            SearchInputOutcome::Continue
        }
        Key::Char(c) if !ctrl => {
            query.push(c);
            SearchInputOutcome::Continue
        }
        _ => SearchInputOutcome::Continue,
    }
}

/// Advance (or reverse, when `dir` is -1) the pointer into the last search
/// result set and move the list selection to that match's block. No-ops if
/// there is no active search or it had zero hits.
fn jump_search(
    last_search: &mut Option<SearchState>,
    selected: &mut usize,
    proxy: &PtyProxy,
    dir: isize,
) {
    let Some(state) = last_search.as_mut() else {
        return;
    };
    if state.results.is_empty() {
        return;
    }
    let len = state.results.len() as isize;
    let new_idx = ((state.result_index as isize + dir).rem_euclid(len)) as usize;
    state.result_index = new_idx;
    let target_block_id = state.results[new_idx].0;
    if let Some(idx) = proxy.blocks().index_of_block_id(target_block_id) {
        *selected = idx;
    }
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

/// Minimal key decoder. Ctrl-modifier flag returned alongside.
fn decode_keys(bytes: &[u8]) -> Vec<(Key, bool)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1b => {
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
            0x1d => {
                // Ctrl+] — exposed as Char(']') with ctrl=true so the
                // handler can recognize the "exit Ptylenz" key uniformly.
                out.push((Key::Char(']'), true));
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
                out.push((Key::Unknown, false));
                i += 1;
            }
        }
    }
    out
}

fn draw_ptylenz(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mode: &Mode,
    proxy: &PtyProxy,
) -> Result<()> {
    let Mode::Ptylenz {
        selected,
        search_input,
        last_search,
    } = mode
    else {
        return Ok(());
    };

    term.draw(|f| {
        let area = f.area();

        // Optionally reserve a 3-row strip at the top for the search input.
        let (list_area, search_bar) = if search_input.is_some() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(1), Constraint::Length(1)])
                .split(area);
            (chunks[1], Some((chunks[0], chunks[2])))
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(area);
            (chunks[0], None)
        };

        if let Some((bar_area, _)) = search_bar {
            let q = search_input.as_deref().unwrap_or("");
            let input = Paragraph::new(q).block(
                RBlock::default()
                    .borders(Borders::ALL)
                    .title(" / search — Enter run, Esc cancel "),
            );
            f.render_widget(input, bar_area);
        }

        draw_blocks(f, list_area, proxy, *selected);

        let status_area = match search_bar {
            Some((_, s)) => s,
            None => Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(area)[1],
        };

        let help = if search_input.is_some() {
            "type query · Enter run · Esc cancel".to_string()
        } else if let Some(s) = last_search {
            format!(
                "/{} · n/N ({}/{}) · j/k move · Enter expand · y copy · e export · p pin · g/G · q back",
                s.query,
                if s.results.is_empty() { 0 } else { s.result_index + 1 },
                s.results.len()
            )
        } else {
            "j/k move · Enter expand · / search · y copy · e export · p pin · g/G top/bot · q back".to_string()
        };
        let status = Paragraph::new(Span::styled(
            format!(" [ptylenz] {}   blocks: {}", help, proxy.blocks().block_count()),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ));
        f.render_widget(status, status_area);
    })?;
    Ok(())
}

fn draw_blocks(
    f: &mut ratatui::Frame,
    area: Rect,
    proxy: &PtyProxy,
    selected: usize,
) {
    let all: Vec<&Block> = proxy
        .blocks()
        .completed_blocks()
        .iter()
        .chain(proxy.blocks().current_block())
        .collect();

    let items: Vec<ListItem> = all
        .iter()
        .map(|b| build_list_item(b))
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

fn build_list_item(b: &Block) -> ListItem<'static> {
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
    let pin = if b.pinned { "📌" } else { "  " };
    let fold = if b.collapsed { "▸" } else { "▾" };
    let header = format!(
        " {} {}{:<5} {}  ·  {} lines  ·  {}  ·  {}",
        fold,
        pin,
        format!("#{}", b.id),
        b.started_at.format("%H:%M:%S"),
        b.line_count(),
        status,
        cmd,
    );

    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(format!("[{}] ", tag), tag_style),
        Span::styled(header, row_style),
    ])];

    if !b.collapsed {
        let text = b.output_text();
        let body_style = Style::default().fg(Color::Gray);
        for (i, raw) in text.lines().enumerate() {
            if i >= EXPAND_MAX_LINES {
                let extra = text.lines().count().saturating_sub(EXPAND_MAX_LINES);
                lines.push(Line::from(Span::styled(
                    format!("      … ({} more lines — press e to export)", extra),
                    Style::default().fg(Color::DarkGray),
                )));
                break;
            }
            lines.push(Line::from(Span::styled(
                format!("      {}", trim_line(raw, 200)),
                body_style,
            )));
        }
    }

    ListItem::new(lines)
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
