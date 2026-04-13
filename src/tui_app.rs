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
///! Ptylenz · List view (default):
///!   j / ↓         next block
///!   k / ↑         previous block
///!   g / G         jump to first / last block
///!   Enter         toggle expand/collapse of selected block
///!   v             open Detail view of selected block
///!   /             search sub-mode
///!   n / N         next / previous search result
///!   y             copy whole selected block to clipboard
///!   e             export all blocks as JSON (current dir)
///!   p             toggle pin on selected block
///!   q / Esc / Ctrl+]   back to Normal
///!
///! Ptylenz · Detail view (one block, full-screen):
///!   j/k/h/l       move cursor
///!   g / G         top / bottom
///!   0 / $         line start / end
///!   Ctrl+u/d      page up / down
///!   v             start/end linewise (line-range) selection
///!   Ctrl+v        start/end blockwise (rectangular) selection
///!   y             yank selection (or whole block if no selection)
///!   Y             yank whole block (always)
///!   Esc           clear selection, or back to list if none
///!   q             back to list

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

/// Selection style inside the Detail view.
///
/// `Linewise` selects every character on every line in `[anchor_row, cursor_row]`.
/// `Blockwise` selects the rectangle bounded by anchor and cursor — the vim
/// `Ctrl-v` model — handy for grabbing a single column out of `ls -l` or
/// trimming the leading whitespace off a script.
#[derive(Debug, Clone)]
enum Selection {
    None,
    Linewise { anchor_row: usize },
    Blockwise { anchor_row: usize, anchor_col: usize },
}

/// State for Detail view: full-screen view of one block with a movable
/// cursor and optional selection.
#[derive(Debug, Clone)]
struct DetailState {
    block_id: usize,
    cursor_row: usize,
    cursor_col: usize,
    selection: Selection,
}

#[derive(Debug, Clone)]
enum PtylenzView {
    List,
    Detail(DetailState),
}

#[derive(Debug, Clone)]
enum Mode {
    Normal,
    Ptylenz {
        selected: usize,
        view: PtylenzView,
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
        view: PtylenzView::List,
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
        // Ctrl+] always leaves Ptylenz mode, regardless of sub-state.
        if ctrl && code == Key::Char(']') {
            return leave_ptylenz(mode, ptylenz_term);
        }

        let Mode::Ptylenz {
            selected,
            view,
            search_input,
            last_search,
        } = mode
        else {
            return Ok(());
        };

        // Detail view captures everything; never falls back to list bindings.
        if let PtylenzView::Detail(detail) = view {
            match handle_detail_key(code, ctrl, detail, *selected, proxy) {
                DetailOutcome::StayInDetail => {}
                DetailOutcome::BackToList => {
                    *view = PtylenzView::List;
                }
            }
            continue;
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
            Key::Char('v') => {
                if let Some(block) = proxy.blocks().get_block_by_index(*selected) {
                    *view = PtylenzView::Detail(DetailState {
                        block_id: block.id,
                        cursor_row: 0,
                        cursor_col: 0,
                        selection: Selection::None,
                    });
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

enum DetailOutcome {
    StayInDetail,
    BackToList,
}

fn handle_detail_key(
    code: Key,
    ctrl: bool,
    detail: &mut DetailState,
    selected_index: usize,
    proxy: &PtyProxy,
) -> DetailOutcome {
    // Resolve the block & line buffer once per key — cheap, and avoids holding
    // a borrow across the cursor-mutation logic below.
    let lines: Vec<String> = match proxy.blocks().get_block_by_index(selected_index) {
        Some(b) => b.output_text().lines().map(|s| s.to_string()).collect(),
        None => return DetailOutcome::BackToList,
    };
    let row_count = lines.len().max(1);
    let line_len = |row: usize| -> usize {
        lines.get(row).map(|l| l.chars().count()).unwrap_or(0)
    };

    match (code, ctrl) {
        (Key::Char('q'), false) => return DetailOutcome::BackToList,
        (Key::Esc, false) => {
            if !matches!(detail.selection, Selection::None) {
                detail.selection = Selection::None;
            } else {
                return DetailOutcome::BackToList;
            }
        }
        (Key::Char('j'), false) | (Key::Down, false) => {
            if detail.cursor_row + 1 < row_count {
                detail.cursor_row += 1;
                detail.cursor_col = detail.cursor_col.min(line_len(detail.cursor_row));
            }
        }
        (Key::Char('k'), false) | (Key::Up, false) => {
            if detail.cursor_row > 0 {
                detail.cursor_row -= 1;
                detail.cursor_col = detail.cursor_col.min(line_len(detail.cursor_row));
            }
        }
        (Key::Char('h'), false) | (Key::Left, false) => {
            if detail.cursor_col > 0 {
                detail.cursor_col -= 1;
            }
        }
        (Key::Char('l'), false) | (Key::Right, false) => {
            let max = line_len(detail.cursor_row);
            if detail.cursor_col < max {
                detail.cursor_col += 1;
            }
        }
        (Key::Char('g'), false) => {
            detail.cursor_row = 0;
            detail.cursor_col = 0;
        }
        (Key::Char('G'), false) => {
            detail.cursor_row = row_count - 1;
            detail.cursor_col = detail.cursor_col.min(line_len(detail.cursor_row));
        }
        (Key::Char('0'), false) => {
            detail.cursor_col = 0;
        }
        (Key::Char('$'), false) => {
            detail.cursor_col = line_len(detail.cursor_row);
        }
        // Ctrl+u / Ctrl+d → page up / down. decode_keys turns 0x15/0x04 into
        // ('u', true) / ('d', true).
        (Key::Char('d'), true) => {
            detail.cursor_row = (detail.cursor_row + 10).min(row_count - 1);
            detail.cursor_col = detail.cursor_col.min(line_len(detail.cursor_row));
        }
        (Key::Char('u'), true) => {
            detail.cursor_row = detail.cursor_row.saturating_sub(10);
            detail.cursor_col = detail.cursor_col.min(line_len(detail.cursor_row));
        }
        // Visual-line mode: `v` toggles linewise selection anchored at the
        // current cursor row.
        (Key::Char('v'), false) => {
            detail.selection = match detail.selection {
                Selection::Linewise { .. } => Selection::None,
                _ => Selection::Linewise { anchor_row: detail.cursor_row },
            };
        }
        // Visual-block mode: Ctrl+v toggles blockwise selection anchored at
        // the current cursor cell.
        (Key::Char('v'), true) => {
            detail.selection = match detail.selection {
                Selection::Blockwise { .. } => Selection::None,
                _ => Selection::Blockwise {
                    anchor_row: detail.cursor_row,
                    anchor_col: detail.cursor_col,
                },
            };
        }
        // y: yank selection (or whole block if no selection).
        (Key::Char('y'), false) => {
            let text = match &detail.selection {
                Selection::None => lines.join("\n"),
                Selection::Linewise { anchor_row } => {
                    let (lo, hi) = sort_pair(*anchor_row, detail.cursor_row);
                    let hi = hi.min(row_count - 1);
                    lines[lo..=hi].join("\n")
                }
                Selection::Blockwise { anchor_row, anchor_col } => {
                    let (rlo, rhi) = sort_pair(*anchor_row, detail.cursor_row);
                    let (clo, chi) = sort_pair(*anchor_col, detail.cursor_col);
                    let rhi = rhi.min(row_count - 1);
                    let mut out = String::new();
                    for r in rlo..=rhi {
                        let line = lines.get(r).map(String::as_str).unwrap_or("");
                        let chars: Vec<char> = line.chars().collect();
                        let segment_lo = clo.min(chars.len());
                        let segment_hi = (chi + 1).min(chars.len());
                        let segment: String = chars[segment_lo..segment_hi].iter().collect();
                        if r > rlo {
                            out.push('\n');
                        }
                        out.push_str(&segment);
                    }
                    out
                }
            };
            copy_to_clipboard(&text);
            eprint!(
                "\r\n[ptylenz] copied {} chars from block #{}\r\n",
                text.chars().count(),
                detail.block_id
            );
            detail.selection = Selection::None;
        }
        // Y: always yank the whole block (vim's Y semantic, adapted).
        (Key::Char('Y'), false) => {
            let text = lines.join("\n");
            copy_to_clipboard(&text);
            eprint!("\r\n[ptylenz] copied block #{}\r\n", detail.block_id);
        }
        _ => {}
    }

    DetailOutcome::StayInDetail
}

fn sort_pair(a: usize, b: usize) -> (usize, usize) {
    if a <= b { (a, b) } else { (b, a) }
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
        view,
        search_input,
        last_search,
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
            PtylenzView::List => {
                // List view may also show a search input bar on top of the list.
                let (list_area, search_bar_area) = if search_input.is_some() {
                    let inner = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(3), Constraint::Min(1)])
                        .split(chunks[0]);
                    (inner[1], Some(inner[0]))
                } else {
                    (chunks[0], None)
                };

                if let Some(bar) = search_bar_area {
                    let q = search_input.as_deref().unwrap_or("");
                    let input = Paragraph::new(q).block(
                        RBlock::default()
                            .borders(Borders::ALL)
                            .title(" / search — Enter run, Esc cancel "),
                    );
                    f.render_widget(input, bar);
                }

                draw_blocks(f, list_area, proxy, *selected);
            }
            PtylenzView::Detail(detail) => {
                draw_detail(f, chunks[0], proxy, detail);
            }
        }

        let help = match view {
            PtylenzView::Detail(d) => {
                let sel = match &d.selection {
                    Selection::None => "no selection".to_string(),
                    Selection::Linewise { .. } => "VISUAL LINE".to_string(),
                    Selection::Blockwise { .. } => "VISUAL BLOCK".to_string(),
                };
                format!(
                    "{} · h/j/k/l move · g/G · v line · ^v block · y yank · Y all · q back · row {}/col {}",
                    sel,
                    d.cursor_row + 1,
                    d.cursor_col + 1,
                )
            }
            PtylenzView::List => {
                if search_input.is_some() {
                    "type query · Enter run · Esc cancel".to_string()
                } else if let Some(s) = last_search {
                    format!(
                        "/{} · n/N ({}/{}) · j/k · Enter fold · v detail · y copy · e export · p pin · q back",
                        s.query,
                        if s.results.is_empty() { 0 } else { s.result_index + 1 },
                        s.results.len()
                    )
                } else {
                    "j/k move · Enter fold · v detail · / search · y copy · e export · p pin · g/G · q back".to_string()
                }
            }
        };
        let status = Paragraph::new(Span::styled(
            format!(" [ptylenz] {}   blocks: {}", help, proxy.blocks().block_count()),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ));
        f.render_widget(status, chunks[1]);
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

/// Render the Detail view: one block, full body, with cursor and optional
/// linewise / blockwise highlight. Auto-scrolls so the cursor stays in view.
fn draw_detail(
    f: &mut ratatui::Frame,
    area: Rect,
    proxy: &PtyProxy,
    detail: &DetailState,
) {
    let block_ref = proxy.blocks().get_block(detail.block_id);
    let (title, lines): (String, Vec<String>) = match block_ref {
        Some(b) => (
            format!(
                " #{} · {} · {} · {} lines ",
                b.id,
                b.command.as_deref().unwrap_or("(unknown)"),
                match b.exit_code {
                    Some(c) => format!("exit {c}"),
                    None => "running".into(),
                },
                b.line_count(),
            ),
            b.output_text().lines().map(|s| s.to_string()).collect(),
        ),
        None => (" detail ".into(), vec!["(block not found)".into()]),
    };

    let rblock = RBlock::default().borders(Borders::ALL).title(title);
    let inner = rblock.inner(area);

    // Compute viewport scroll so the cursor row is visible.
    let viewport_h = inner.height as usize;
    let scroll_top = if viewport_h == 0 {
        0usize
    } else if detail.cursor_row < viewport_h {
        0
    } else {
        detail.cursor_row + 1 - viewport_h
    };
    let scroll_top = scroll_top.min(lines.len().saturating_sub(viewport_h.max(1)));

    let cursor_style = Style::default().bg(Color::White).fg(Color::Black);
    let select_style = Style::default()
        .bg(Color::Blue)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);

    let mut rendered: Vec<Line> = Vec::with_capacity(viewport_h);
    let visible_end = (scroll_top + viewport_h).min(lines.len());

    for row in scroll_top..visible_end {
        let line_str = &lines[row];
        let chars: Vec<char> = line_str.chars().collect();

        let (sel_lo, sel_hi) = selection_range_for_row(&detail.selection, detail, row, chars.len());

        let mut spans: Vec<Span<'static>> = Vec::new();
        let cursor_in_this_row = row == detail.cursor_row;

        // Walk every column position [0..max(chars.len, cursor_col+1)] so we
        // can paint the cursor even when it sits past end-of-line.
        let max_col = chars.len().max(if cursor_in_this_row { detail.cursor_col + 1 } else { 0 });
        let mut col = 0;
        while col < max_col {
            let ch = chars.get(col).copied().unwrap_or(' ');

            let in_selection = sel_lo.map_or(false, |lo| col >= lo && col < sel_hi.unwrap_or(0));
            let is_cursor = cursor_in_this_row && col == detail.cursor_col;

            let style = if is_cursor {
                cursor_style
            } else if in_selection {
                select_style
            } else {
                Style::default().fg(Color::Gray)
            };
            spans.push(Span::styled(ch.to_string(), style));
            col += 1;
        }
        if spans.is_empty() {
            spans.push(Span::raw(""));
        }
        rendered.push(Line::from(spans));
    }

    let para = Paragraph::new(rendered).block(rblock);
    f.render_widget(para, area);
}

/// Compute (start, end_exclusive) selection columns for `row`. Returns
/// (None, None) when the row is outside the selection.
fn selection_range_for_row(
    sel: &Selection,
    detail: &DetailState,
    row: usize,
    line_chars: usize,
) -> (Option<usize>, Option<usize>) {
    match sel {
        Selection::None => (None, None),
        Selection::Linewise { anchor_row } => {
            let (lo, hi) = sort_pair(*anchor_row, detail.cursor_row);
            if row >= lo && row <= hi {
                (Some(0), Some(line_chars))
            } else {
                (None, None)
            }
        }
        Selection::Blockwise { anchor_row, anchor_col } => {
            let (rlo, rhi) = sort_pair(*anchor_row, detail.cursor_row);
            if row < rlo || row > rhi {
                return (None, None);
            }
            let (clo, chi) = sort_pair(*anchor_col, detail.cursor_col);
            (Some(clo), Some((chi + 1).min(line_chars)))
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn detail_at(row: usize, col: usize, sel: Selection) -> DetailState {
        DetailState {
            block_id: 1,
            cursor_row: row,
            cursor_col: col,
            selection: sel,
        }
    }

    #[test]
    fn linewise_range_covers_whole_lines_in_anchor_to_cursor_span() {
        let d = detail_at(3, 5, Selection::Linewise { anchor_row: 1 });
        // Row inside the span — full line selected.
        assert_eq!(
            selection_range_for_row(&d.selection, &d, 2, 10),
            (Some(0), Some(10))
        );
        // Row outside — no selection.
        assert_eq!(
            selection_range_for_row(&d.selection, &d, 4, 10),
            (None, None)
        );
        // Anchor row itself — included.
        assert_eq!(
            selection_range_for_row(&d.selection, &d, 1, 7),
            (Some(0), Some(7))
        );
    }

    #[test]
    fn blockwise_range_clamps_to_line_length() {
        let d = detail_at(2, 8, Selection::Blockwise { anchor_row: 0, anchor_col: 3 });
        // Long line: full block columns visible.
        assert_eq!(
            selection_range_for_row(&d.selection, &d, 1, 20),
            (Some(3), Some(9))
        );
        // Short line: clamped to its end.
        assert_eq!(
            selection_range_for_row(&d.selection, &d, 1, 5),
            (Some(3), Some(5))
        );
        // Outside row range — nothing.
        assert_eq!(
            selection_range_for_row(&d.selection, &d, 3, 20),
            (None, None)
        );
    }

    #[test]
    fn blockwise_range_handles_reversed_anchor() {
        // Cursor sits above-and-left of the anchor — the range is sorted.
        let d = detail_at(0, 2, Selection::Blockwise { anchor_row: 4, anchor_col: 7 });
        assert_eq!(
            selection_range_for_row(&d.selection, &d, 2, 20),
            (Some(2), Some(8))
        );
    }
}
