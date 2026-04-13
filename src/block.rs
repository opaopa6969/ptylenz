//! Block Engine — segments a PTY byte stream into discrete blocks.
//!
//! A "block" is one command invocation and its output:
//!   - command text (from OSC 133;E)
//!   - output bytes (between OSC 133;C and OSC 133;D)
//!   - exit code (from OSC 133;D)
//!   - timestamp, line count
//!
//! Detection uses iTerm2/Warp-compatible OSC 133 sequences:
//!   \e]133;A\a  — prompt start
//!   \e]133;C\a  — command execution start
//!   \e]133;D;N\a — command finished with exit code N
//!   \e]133;E;cmd\a — command text
//!
//! Some helpers (preview, summary, toggle_*, pinned) are part of the MVP
//! roadmap and kept intentionally even if not yet wired into the TUI.

#![allow(dead_code)]

use chrono::{DateTime, Local};
use regex::bytes::Regex;

use crate::claude_feeder::{ClaudeEvent, ToolUse};

/// Events detected in the PTY output stream.
#[derive(Debug, Clone)]
pub enum OscEvent {
    PromptStart,
    CommandStart,
    CommandText(String),
    CommandEnd { exit_code: i32 },
}

/// Where a block came from.
///
/// `Shell` is the original lineage: bash emitted an OSC 133 sequence and the
/// engine sliced the byte stream around it.
///
/// `ClaudeTurn` blocks are synthesized from the Claude Code JSONL log. They
/// are NOT produced by the PTY stream, so they never accumulate raw ANSI
/// output — `output` holds a plain-text rendering of the turn instead.
#[derive(Debug, Clone)]
pub enum BlockSource {
    Shell,
    ClaudeTurn {
        session_id: String,
        role: String, // "user" | "assistant"
        turn_index: usize,
        tool_uses: Vec<ToolUse>,
    },
}

/// A single output block: one command + its output.
#[derive(Debug, Clone)]
pub struct Block {
    pub id: usize,
    pub command: Option<String>,
    pub output: Vec<u8>,
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Local>,
    pub ended_at: Option<DateTime<Local>>,
    pub collapsed: bool,
    pub pinned: bool,
    pub source: BlockSource,
    /// Snapshot of the vt100 shadow grid at the moment this block ended.
    /// Populated only when the block exercised the alternate screen (TUI apps
    /// like `claude`, `vim`, `less`), where the raw byte stream is full of
    /// cursor-positioning escapes and cannot be naively stripped. For plain
    /// line-oriented commands we leave this `None` and display falls back to
    /// the raw `output` buffer — that path preserves scrollback beyond the
    /// shadow grid's capacity.
    pub rendered_text: Option<String>,
}

impl Block {
    fn new(id: usize) -> Self {
        Block {
            id,
            command: None,
            output: Vec::new(),
            exit_code: None,
            started_at: Local::now(),
            ended_at: None,
            collapsed: false,
            pinned: false,
            source: BlockSource::Shell,
            rendered_text: None,
        }
    }

    /// True if this block was synthesized from a Claude Code turn rather
    /// than segmented from the PTY stream.
    pub fn is_claude_turn(&self) -> bool {
        matches!(self.source, BlockSource::ClaudeTurn { .. })
    }

    /// Number of lines in the output.
    pub fn line_count(&self) -> usize {
        self.output.iter().filter(|&&b| b == b'\n').count()
    }

    /// Output as lossy UTF-8 string (for display/search).
    ///
    /// Prefers the vt100 shadow-grid snapshot (populated for alt-screen
    /// sessions — see `rendered_text` docs). Otherwise falls back to
    /// stripping ANSI from the raw byte buffer; that path handles plain
    /// line-oriented output where the raw stream already reads naturally.
    pub fn output_text(&self) -> String {
        if let Some(ref rendered) = self.rendered_text {
            return rendered.clone();
        }
        let raw = String::from_utf8_lossy(&self.output);
        strip_ansi(&raw)
    }

    /// First N lines of output (for collapsed preview).
    pub fn preview(&self, max_lines: usize) -> String {
        let text = self.output_text();
        let lines: Vec<&str> = text.lines().take(max_lines).collect();
        lines.join("\n")
    }

    /// Summary line for the block header.
    pub fn summary(&self) -> String {
        let cmd = self.command.as_deref().unwrap_or("(unknown)");
        let lines = self.line_count();
        let status = match self.exit_code {
            Some(0) => "ok".to_string(),
            Some(c) => format!("exit {}", c),
            None => "running...".to_string(),
        };
        let time = self.started_at.format("%H:%M:%S");
        format!("#{} {} — {} — {} lines — {}", self.id, cmd, time, lines, status)
    }
}

/// The block engine: accumulates PTY output and segments it.
pub struct BlockEngine {
    blocks: Vec<Block>,
    next_id: usize,
    /// Current partial block being accumulated (between command start and end).
    current: Option<Block>,
    /// OSC parser state machine.
    osc_parser: OscParser,
    /// Fallback prompt pattern for environments without shell integration.
    prompt_pattern: Option<Regex>,
    /// Per-session turn counters for ClaudeTurn blocks.
    claude_turn_counters: std::collections::HashMap<String, usize>,
    /// Shadow vt100 parser for the current block. Bytes are tee'd in so that
    /// alt-screen TUIs (claude, vim, less) render correctly in the overlay
    /// while the raw byte buffer is still kept for scrollback/export.
    vt_parser: Option<vt100::Parser>,
    /// Latest known terminal size — initial parser size and resize target.
    term_rows: u16,
    term_cols: u16,
    /// Last snapshot of the vt100 grid taken while the current block was in
    /// alt-screen mode. We can't wait until CommandEnd to snapshot — TUIs
    /// typically exit alt-screen (?1049l) immediately before finishing, which
    /// restores the primary screen and wipes the view we actually want.
    /// Instead, resample on every feed while alt-screen is active and keep
    /// the most recent frame. Presence of Some(_) also signals "this block
    /// used alt-screen" to the finalizer.
    last_alt_snapshot: Option<String>,
}

/// Scrollback depth for the shadow vt100 parser. Short TUI commands don't
/// need this; it exists to keep non-TUI commands that barely overflow the
/// screen (think `ls -la /usr/bin`) representable via the shadow grid if we
/// ever switch display to always prefer it.
const VT_SCROLLBACK_ROWS: usize = 2000;

impl BlockEngine {
    pub fn new() -> Self {
        BlockEngine {
            blocks: Vec::new(),
            next_id: 1,
            current: None,
            osc_parser: OscParser::new(),
            prompt_pattern: None,
            claude_turn_counters: std::collections::HashMap::new(),
            vt_parser: None,
            term_rows: 24,
            term_cols: 80,
            last_alt_snapshot: None,
        }
    }

    /// Update the terminal size used when starting new blocks and propagate
    /// to any in-flight vt100 parser.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.term_rows = rows.max(1);
        self.term_cols = cols.max(1);
        if let Some(parser) = self.vt_parser.as_mut() {
            parser.set_size(self.term_rows, self.term_cols);
        }
    }

    /// Convert a `ClaudeEvent` into a block.
    ///
    /// Turns always land as COMPLETED sibling blocks — they are atomic facts
    /// from the JSONL log, not partial outputs. `SessionStarted` resets the
    /// per-session turn counter but produces no block of its own; keeping
    /// the block list noise-free matters more than marking boundaries.
    pub fn ingest_claude_event(&mut self, ev: ClaudeEvent) {
        match ev {
            ClaudeEvent::SessionStarted { session_id, .. } => {
                self.claude_turn_counters.entry(session_id).or_insert(0);
            }
            ClaudeEvent::Turn {
                session_id,
                role,
                text,
                tool_uses,
                timestamp: _,
            } => {
                let counter = self.claude_turn_counters.entry(session_id.clone()).or_insert(0);
                *counter += 1;
                let turn_index = *counter;

                let id = self.next_id;
                self.next_id += 1;

                let rendered = render_turn(&role, &text, &tool_uses);
                let now = Local::now();

                let mut block = Block {
                    id,
                    command: Some(format!("claude {} #{}", role, turn_index)),
                    output: rendered.into_bytes(),
                    exit_code: Some(0),
                    started_at: now,
                    ended_at: Some(now),
                    collapsed: false,
                    pinned: false,
                    source: BlockSource::ClaudeTurn {
                        session_id,
                        role,
                        turn_index,
                        tool_uses,
                    },
                    rendered_text: None,
                };
                if block.line_count() > 50 {
                    block.collapsed = true;
                }
                self.blocks.push(block);
            }
        }
    }

    /// Set a fallback prompt regex for non-integrated shells.
    pub fn set_prompt_pattern(&mut self, pattern: &str) {
        self.prompt_pattern = Regex::new(pattern).ok();
    }

    /// Feed raw PTY output bytes into the engine.
    /// Returns the clean stream (OSC 133 markers stripped) plus any events
    /// detected, so callers can forward the clean bytes to stdout.
    pub fn feed_output(&mut self, data: &[u8]) -> (Vec<u8>, Vec<OscEvent>) {
        let (clean_data, osc_events) = self.osc_parser.parse(data);

        for event in &osc_events {
            match event {
                OscEvent::CommandStart => {
                    // Start a new block with a fresh vt100 shadow parser.
                    let block = Block::new(self.next_id);
                    self.next_id += 1;
                    self.current = Some(block);
                    self.vt_parser = Some(vt100::Parser::new(
                        self.term_rows,
                        self.term_cols,
                        VT_SCROLLBACK_ROWS,
                    ));
                    self.last_alt_snapshot = None;
                }
                OscEvent::CommandText(cmd) => {
                    // The iTerm2 protocol emits 133;E at prompt time; our bash
                    // integration emits it from PROMPT_COMMAND (after CommandEnd),
                    // so attach to the current block if open, else to the last
                    // closed block.
                    if let Some(ref mut block) = self.current {
                        block.command = Some(cmd.clone());
                    } else if let Some(last) = self.blocks.last_mut() {
                        if last.command.is_none() {
                            last.command = Some(cmd.clone());
                        }
                    }
                }
                OscEvent::CommandEnd { exit_code } => {
                    if let Some(mut block) = self.current.take() {
                        block.exit_code = Some(*exit_code);
                        block.ended_at = Some(Local::now());
                        self.finalize_rendered_text(&mut block);
                        if block.line_count() > 50 {
                            block.collapsed = true;
                        }
                        self.blocks.push(block);
                    }
                    self.vt_parser = None;
                    self.last_alt_snapshot = None;
                }
                OscEvent::PromptStart => {
                    // If there's an open block without a proper end, close it
                    if let Some(mut block) = self.current.take() {
                        block.ended_at = Some(Local::now());
                        self.finalize_rendered_text(&mut block);
                        if block.line_count() > 50 {
                            block.collapsed = true;
                        }
                        self.blocks.push(block);
                    }
                    self.vt_parser = None;
                    self.last_alt_snapshot = None;
                }
            }
        }

        if !clean_data.is_empty() {
            if let Some(ref mut block) = self.current {
                block.output.extend_from_slice(&clean_data);
            }
            // Tee into the shadow parser so a live TUI's grid is tracked in
            // parallel with the raw buffer.
            if let Some(parser) = self.vt_parser.as_mut() {
                parser.process(&clean_data);
                // Resample while alt-screen is live so we capture the final
                // frame before the TUI restores the primary screen on exit.
                if parser.screen().alternate_screen() {
                    self.last_alt_snapshot =
                        Some(normalize_vt_snapshot(&parser.screen().contents()));
                }
            }
        }

        (clean_data, osc_events)
    }

    /// Commit the last alt-screen frame captured for this block into
    /// `rendered_text`. For purely line-oriented output `last_alt_snapshot`
    /// stays `None` and we leave the raw buffer as the sole record.
    fn finalize_rendered_text(&mut self, block: &mut Block) {
        if let Some(snap) = self.last_alt_snapshot.take() {
            if !snap.is_empty() {
                block.rendered_text = Some(snap);
            }
        }
    }

    /// Live snapshot of the in-flight alt-screen frame, if the currently
    /// accumulating block is a TUI. Used by the overlay detail view so that
    /// viewing a still-running `claude`/`vim`/`less` shows the current frame
    /// instead of the raw byte buffer (which otherwise piles up every frame
    /// and produces visually-stacked overwrites).
    pub fn current_alt_snapshot(&self) -> Option<&str> {
        self.last_alt_snapshot.as_deref()
    }

    /// Get all completed blocks.
    pub fn completed_blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Get the current (in-progress) block, if any.
    pub fn current_block(&self) -> Option<&Block> {
        self.current.as_ref()
    }

    /// Search all blocks for a text pattern. Returns (block_id, line_number, line_text).
    pub fn search(&self, query: &str) -> Vec<(usize, usize, String)> {
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        for block in &self.blocks {
            let text = block.output_text();
            for (line_num, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(&query_lower) {
                    results.push((block.id, line_num + 1, line.to_string()));
                }
            }
        }

        // Also search current block
        if let Some(ref block) = self.current {
            let text = block.output_text();
            for (line_num, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(&query_lower) {
                    results.push((block.id, line_num + 1, line.to_string()));
                }
            }
        }

        results
    }

    /// Get a block by ID.
    pub fn get_block(&self, id: usize) -> Option<&Block> {
        self.blocks.iter().find(|b| b.id == id)
            .or_else(|| {
                self.current.as_ref().filter(|b| b.id == id)
            })
    }

    /// Get the Nth block in chronological order (0-based).
    /// Completed blocks come first, then the currently-running block (if any).
    /// IDs may have gaps; UI code should navigate by index, not by ID.
    pub fn get_block_by_index(&self, index: usize) -> Option<&Block> {
        let completed = self.blocks.len();
        if index < completed {
            self.blocks.get(index)
        } else if index == completed {
            self.current.as_ref()
        } else {
            None
        }
    }

    /// Find the chronological index of the block with the given ID, if any.
    pub fn index_of_block_id(&self, id: usize) -> Option<usize> {
        if let Some(pos) = self.blocks.iter().position(|b| b.id == id) {
            return Some(pos);
        }
        if let Some(ref cur) = self.current {
            if cur.id == id {
                return Some(self.blocks.len());
            }
        }
        None
    }

    /// Toggle collapsed state of a block.
    pub fn toggle_collapse(&mut self, id: usize) {
        if let Some(block) = self.blocks.iter_mut().find(|b| b.id == id) {
            block.collapsed = !block.collapsed;
        }
    }

    /// Toggle pinned state of a block.
    pub fn toggle_pin(&mut self, id: usize) {
        if let Some(block) = self.blocks.iter_mut().find(|b| b.id == id) {
            block.pinned = !block.pinned;
        }
    }

    /// Total number of blocks (completed + current).
    pub fn block_count(&self) -> usize {
        self.blocks.len() + if self.current.is_some() { 1 } else { 0 }
    }

    /// Export the session in the common log model format used by
    /// claude-session-replay. Each block becomes a pair of messages
    /// (user = command, assistant = output). An extra `exit_code` field
    /// is attached to assistant messages as a backwards-compatible extension.
    ///
    /// See `github.com/opaopa6969/claude-session-replay/docs/data-model.md`.
    pub fn export_json(&self) -> String {
        let source = format!(
            "ptylenz-{}.json",
            Local::now().format("%Y%m%d-%H%M%S")
        );

        let mut messages = String::new();
        let mut first = true;

        let all_blocks = self.blocks.iter().chain(self.current.iter());
        for block in all_blocks {
            let ts = block.started_at.to_rfc3339();
            let cmd = block.command.clone().unwrap_or_default();
            let out = block.output_text();

            if !cmd.is_empty() {
                if !first {
                    messages.push_str(",\n");
                }
                first = false;
                messages.push_str(&format_message("user", &cmd, &ts, None));
            }
            if !out.is_empty() || block.exit_code.is_some() {
                if !first {
                    messages.push_str(",\n");
                }
                first = false;
                let ts_end = block
                    .ended_at
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| ts.clone());
                messages.push_str(&format_message(
                    "assistant",
                    &out,
                    &ts_end,
                    block.exit_code,
                ));
            }
        }

        format!(
            "{{\n  \"source\": \"{}\",\n  \"agent\": \"ptylenz\",\n  \"messages\": [\n{}\n  ]\n}}",
            json_escape(&source),
            messages
        )
    }
}

fn format_message(role: &str, text: &str, ts: &str, exit_code: Option<i32>) -> String {
    let mut extra = String::new();
    if let Some(code) = exit_code {
        extra.push_str(&format!(",\n      \"exit_code\": {code}"));
    }
    format!(
        "    {{\n      \"role\": \"{}\",\n      \"text\": \"{}\",\n      \"tool_uses\": [],\n      \"tool_results\": [],\n      \"thinking\": [],\n      \"timestamp\": \"{}\"{extra}\n    }}",
        role,
        json_escape(text),
        json_escape(ts)
    )
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// OSC escape sequence parser.
/// Detects \e]133;X;...\a patterns in a byte stream.
struct OscParser {
    state: ParseState,
    buf: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
enum ParseState {
    Normal,
    Escape,      // saw \e
    OscStart,    // saw \e]
    OscBody,     // accumulating until \a or \e\\
    /// Just consumed an OSC 133 terminated by ESC; the next byte is expected
    /// to be `\` (completing the ST sequence) and should be swallowed so it
    /// doesn't reach the terminal as a stray char.
    OscStSwallow,
}

impl OscParser {
    fn new() -> Self {
        OscParser {
            state: ParseState::Normal,
            buf: Vec::new(),
        }
    }

    /// Parse a chunk of bytes.
    /// Returns (clean_data_without_osc, detected_events).
    ///
    /// OSC 133 sequences are consumed (they are ptylenz's block-boundary
    /// markers and must not reach the user's terminal). Every other OSC —
    /// title-setting (OSC 0/1/2), clipboard (OSC 52), color queries (OSC
    /// 10/11/12), hyperlinks (OSC 8), etc. — is re-emitted verbatim so
    /// downstream terminals keep working. ncurses apps like `mc` also rely
    /// on color-query OSCs returning, so silently dropping them is not
    /// neutral: it can change how the child draws.
    fn parse(&mut self, data: &[u8]) -> (Vec<u8>, Vec<OscEvent>) {
        let mut clean = Vec::with_capacity(data.len());
        let mut events = Vec::new();

        for &byte in data {
            match self.state {
                ParseState::Normal => {
                    if byte == 0x1b {
                        self.state = ParseState::Escape;
                    } else {
                        clean.push(byte);
                    }
                }
                ParseState::Escape => {
                    if byte == b']' {
                        self.state = ParseState::OscStart;
                        self.buf.clear();
                    } else {
                        // Not an OSC, emit the escape + this byte
                        clean.push(0x1b);
                        clean.push(byte);
                        self.state = ParseState::Normal;
                    }
                }
                ParseState::OscStart => {
                    self.state = ParseState::OscBody;
                    self.buf.push(byte);
                }
                ParseState::OscBody => {
                    if byte == 0x07 {
                        self.finish_osc(&mut clean, &mut events, byte);
                    } else if byte == 0x1b {
                        // ESC starts ST (ESC \) — treat as terminator.
                        self.finish_osc(&mut clean, &mut events, byte);
                    } else {
                        self.buf.push(byte);
                    }
                }
                ParseState::OscStSwallow => {
                    // Expected to swallow a trailing `\` completing ESC ST.
                    // Anything else — step back into Normal handling.
                    if byte != b'\\' {
                        if byte == 0x1b {
                            self.state = ParseState::Escape;
                        } else {
                            clean.push(byte);
                            self.state = ParseState::Normal;
                        }
                        continue;
                    }
                    self.state = ParseState::Normal;
                }
            }
        }

        (clean, events)
    }

    /// Finalize the current OSC body: either decode it as an OSC 133 event
    /// (consumed) or re-emit the original bytes verbatim so the terminal
    /// still receives them.
    fn finish_osc(&mut self, clean: &mut Vec<u8>, events: &mut Vec<OscEvent>, terminator: u8) {
        let payload = std::mem::take(&mut self.buf);
        match self.decode_osc(&payload) {
            Some(event) => {
                events.push(event);
                // If the OSC was ST-terminated (ESC \), swallow the trailing
                // `\` so it doesn't leak to the terminal as a stray byte.
                self.state = if terminator == 0x1b {
                    ParseState::OscStSwallow
                } else {
                    ParseState::Normal
                };
            }
            None => {
                clean.push(0x1b);
                clean.push(b']');
                clean.extend_from_slice(&payload);
                clean.push(terminator);
                self.state = ParseState::Normal;
            }
        }
    }

    /// Decode an OSC 133 payload.
    fn decode_osc(&self, payload: &[u8]) -> Option<OscEvent> {
        let s = std::str::from_utf8(payload).ok()?;

        if !s.starts_with("133;") {
            return None;
        }

        let rest = &s[4..];
        if rest.starts_with('A') {
            Some(OscEvent::PromptStart)
        } else if rest.starts_with('C') {
            Some(OscEvent::CommandStart)
        } else if rest.starts_with("D;") {
            let code = rest[2..].parse::<i32>().unwrap_or(-1);
            Some(OscEvent::CommandEnd { exit_code: code })
        } else if rest.starts_with("E;") {
            let cmd = rest[2..].to_string();
            Some(OscEvent::CommandText(cmd))
        } else {
            None
        }
    }
}

/// Render a Claude turn as human-readable plain text for the block's output
/// buffer. Tool uses are summarized as "→ tool_name(input_json)" on their own
/// lines below the main text.
fn render_turn(role: &str, text: &str, tool_uses: &[ToolUse]) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let label = if role == "user" { "▶ user" } else { "▶ assistant" };
    out.push_str(label);
    out.push('\n');
    if !text.is_empty() {
        out.push_str(text);
        out.push('\n');
    }
    for t in tool_uses {
        // Marker without leading spaces so a plain copy of the block doesn't
        // bake whitespace into the start of (often script-shaped) tool input.
        out.push_str("→ ");
        out.push_str(&t.name);
        out.push('(');
        // Truncate very large inputs so the block summary stays useful.
        let max = 500;
        if t.input_json.len() > max {
            out.push_str(&t.input_json[..max]);
            out.push_str("…");
        } else {
            out.push_str(&t.input_json);
        }
        out.push(')');
        out.push('\n');
    }
    out
}

/// Normalize a vt100 screen snapshot for display in a narrower overlay area.
///
/// `vt100::Screen::contents()` pads every row to the parser's column width.
/// Passed straight to a `Paragraph` with wrap enabled, each padded row wraps
/// to two visual lines whenever the overlay is narrower than the captured
/// terminal, throwing off scroll math and visually doubling the frame.
/// Right-trim per row fixes that; final blank rows are also dropped so the
/// TUI's unused bottom area doesn't show as a wall of whitespace.
fn normalize_vt_snapshot(s: &str) -> String {
    let mut lines: Vec<&str> = s.split('\n').map(|l| l.trim_end()).collect();
    while lines.last().map_or(false, |l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Strip ANSI escape sequences from a string (for plain-text search/display).
fn strip_ansi(s: &str) -> String {
    // Simple regex-free approach: skip \e[...m and similar CSI sequences
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip CSI sequence
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                // Skip until we hit a letter (the final byte of CSI)
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            // Skip other escape sequences (\e] already handled by OscParser)
        } else {
            result.push(c);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_osc_parser_detects_command_start() {
        let mut parser = OscParser::new();
        let input = b"hello\x1b]133;C\x07world";
        let (clean, events) = parser.parse(input);

        assert_eq!(clean, b"helloworld");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], OscEvent::CommandStart));
    }

    #[test]
    fn test_osc_parser_detects_command_end() {
        let mut parser = OscParser::new();
        let input = b"\x1b]133;D;0\x07";
        let (_, events) = parser.parse(input);

        assert_eq!(events.len(), 1);
        if let OscEvent::CommandEnd { exit_code } = &events[0] {
            assert_eq!(*exit_code, 0);
        } else {
            panic!("Expected CommandEnd");
        }
    }

    #[test]
    fn test_osc_parser_detects_command_text() {
        let mut parser = OscParser::new();
        let input = b"\x1b]133;E;ls -la\x07";
        let (_, events) = parser.parse(input);

        assert_eq!(events.len(), 1);
        if let OscEvent::CommandText(cmd) = &events[0] {
            assert_eq!(cmd, "ls -la");
        } else {
            panic!("Expected CommandText");
        }
    }

    #[test]
    fn test_osc_parser_passthroughs_non_133() {
        // Title-setting (OSC 0), hyperlink (OSC 8), color query (OSC 11),
        // clipboard (OSC 52): all must be re-emitted verbatim because the
        // user's terminal (or an ncurses child like mc) depends on them.
        // Silent drop was the previous behavior and broke alt-screen-ish
        // apps that query colors during setup.
        let mut parser = OscParser::new();
        let title = b"\x1b]0;hello\x07";
        let link = b"\x1b]8;;https://example.com\x07click\x1b]8;;\x07";
        let color_query_st = b"\x1b]11;?\x1b\\";

        let (clean_title, ev_title) = parser.parse(title);
        assert!(ev_title.is_empty());
        assert_eq!(&clean_title[..], &title[..]);

        let (clean_link, ev_link) = parser.parse(link);
        assert!(ev_link.is_empty());
        assert_eq!(&clean_link[..], &link[..]);

        let (clean_q, ev_q) = parser.parse(color_query_st);
        assert!(ev_q.is_empty());
        assert_eq!(&clean_q[..], &color_query_st[..]);
    }

    #[test]
    fn test_osc_parser_consumes_133_with_st_terminator() {
        // ESC \ (ST) variant of OSC 133 — must still be fully consumed,
        // leaving no stray backslash in the clean stream.
        let mut parser = OscParser::new();
        let input = b"before\x1b]133;C\x1b\\after";
        let (clean, events) = parser.parse(input);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], OscEvent::CommandStart));
        assert_eq!(&clean[..], b"beforeafter");
    }

    #[test]
    fn test_block_engine_lifecycle() {
        let mut engine = BlockEngine::new();

        // Simulate: prompt → command start → command text → output → command end
        engine.feed_output(b"\x1b]133;A\x07");
        engine.feed_output(b"\x1b]133;C\x07");
        engine.feed_output(b"\x1b]133;E;ls\x07");
        engine.feed_output(b"file1.txt\nfile2.txt\n");
        engine.feed_output(b"\x1b]133;D;0\x07");

        assert_eq!(engine.completed_blocks().len(), 1);
        let block = &engine.completed_blocks()[0];
        assert_eq!(block.command.as_deref(), Some("ls"));
        assert_eq!(block.exit_code, Some(0));
        assert_eq!(block.line_count(), 2);
    }

    #[test]
    fn test_search() {
        let mut engine = BlockEngine::new();

        engine.feed_output(b"\x1b]133;C\x07\x1b]133;E;ls\x07");
        engine.feed_output(b"hello.txt\nworld.rs\n");
        engine.feed_output(b"\x1b]133;D;0\x07");

        let results = engine.search("world");
        assert_eq!(results.len(), 1);
        assert!(results[0].2.contains("world"));
    }

    #[test]
    fn test_export_common_model_json() {
        let mut engine = BlockEngine::new();

        engine.feed_output(b"\x1b]133;C\x07");
        engine.feed_output(b"file1.txt\nfile2.txt\n");
        engine.feed_output(b"\x1b]133;D;0\x07");
        engine.feed_output(b"\x1b]133;E;ls\x07");

        let json = engine.export_json();
        // Basic structural checks — schema is defined by claude-session-replay.
        assert!(json.contains("\"agent\": \"ptylenz\""));
        assert!(json.contains("\"messages\":"));
        assert!(json.contains("\"role\": \"user\""));
        assert!(json.contains("\"text\": \"ls\""));
        assert!(json.contains("\"role\": \"assistant\""));
        assert!(json.contains("file1.txt"));
        assert!(json.contains("\"exit_code\": 0"));
        // Invariant: no trailing comma before closing bracket.
        assert!(!json.contains(",\n  ]"));
    }

    #[test]
    fn test_json_escape_handles_quotes_and_newlines() {
        let s = "line1\n\"quoted\"\tpath\\with\\slash";
        let e = json_escape(s);
        assert_eq!(e, "line1\\n\\\"quoted\\\"\\tpath\\\\with\\\\slash");
    }

    #[test]
    fn test_strip_ansi() {
        let input = "\x1b[32mgreen\x1b[0m plain \x1b[1;31mred\x1b[0m";
        let stripped = strip_ansi(input);
        assert_eq!(stripped, "green plain red");
    }

    #[test]
    fn test_ingest_claude_turn_creates_sibling_block() {
        let mut engine = BlockEngine::new();
        engine.ingest_claude_event(ClaudeEvent::SessionStarted {
            session_id: "s1".into(),
            path: std::path::PathBuf::from("/tmp/s1.jsonl"),
        });
        engine.ingest_claude_event(ClaudeEvent::Turn {
            session_id: "s1".into(),
            role: "user".into(),
            text: "hello claude".into(),
            tool_uses: vec![],
            timestamp: None,
        });
        engine.ingest_claude_event(ClaudeEvent::Turn {
            session_id: "s1".into(),
            role: "assistant".into(),
            text: "hi".into(),
            tool_uses: vec![ToolUse {
                name: "Bash".into(),
                input_json: r#"{"command":"ls"}"#.into(),
            }],
            timestamp: None,
        });

        let blocks = engine.completed_blocks();
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].is_claude_turn());
        assert_eq!(blocks[0].command.as_deref(), Some("claude user #1"));
        assert!(blocks[1].is_claude_turn());
        assert_eq!(blocks[1].command.as_deref(), Some("claude assistant #2"));
        // Assistant turn rendering should include the tool use line.
        let text = blocks[1].output_text();
        assert!(text.contains("→ Bash"));
        assert!(text.contains("ls"));
    }

    #[test]
    fn test_alt_screen_block_uses_vt_snapshot() {
        // A TUI session: bytes between 133;C and 133;D include ?1049h (enter
        // alt-screen), some cursor moves that would junk a naive strip_ansi
        // rendering, and ?1049l (leave). The block's output_text should come
        // from the vt100 shadow grid, not from raw bytes.
        let mut engine = BlockEngine::new();
        engine.resize(24, 80);

        engine.feed_output(b"\x1b]133;C\x07");
        // Enter alt-screen, clear, write "HELLO TUI", then jump around.
        engine.feed_output(b"\x1b[?1049h\x1b[2J\x1b[H");
        engine.feed_output(b"HELLO TUI\n");
        engine.feed_output(b"\x1b[10;10HGARBAGE");
        engine.feed_output(b"\x1b[H");
        engine.feed_output(b"HELLO TUI"); // overwrite in place
        engine.feed_output(b"\x1b[?1049l");
        engine.feed_output(b"\x1b]133;D;0\x07");
        engine.feed_output(b"\x1b]133;E;claude\x07");

        assert_eq!(engine.completed_blocks().len(), 1);
        let block = &engine.completed_blocks()[0];
        assert!(block.rendered_text.is_some(), "expected vt100 snapshot");
        let text = block.output_text();
        // The grid snapshot reflects the final screen — "HELLO TUI" on row 1,
        // "GARBAGE" on row 10. Raw-byte strip_ansi would concatenate them
        // without the cursor positioning we exercised.
        assert!(text.contains("HELLO TUI"));
        assert!(text.contains("GARBAGE"));
    }

    #[test]
    fn test_plain_command_skips_vt_snapshot() {
        // Line-oriented output should stay on the raw-buffer path so that
        // scrollback longer than the grid height is preserved.
        let mut engine = BlockEngine::new();
        engine.feed_output(b"\x1b]133;C\x07");
        engine.feed_output(b"file1\nfile2\n");
        engine.feed_output(b"\x1b]133;D;0\x07");
        engine.feed_output(b"\x1b]133;E;ls\x07");

        let block = &engine.completed_blocks()[0];
        assert!(block.rendered_text.is_none());
        assert_eq!(block.output_text().trim(), "file1\nfile2");
    }

    #[test]
    fn test_shell_and_claude_blocks_coexist() {
        let mut engine = BlockEngine::new();

        // One shell block.
        engine.feed_output(b"\x1b]133;C\x07");
        engine.feed_output(b"file\n");
        engine.feed_output(b"\x1b]133;D;0\x07");
        engine.feed_output(b"\x1b]133;E;ls\x07");

        // Then a claude turn lands.
        engine.ingest_claude_event(ClaudeEvent::Turn {
            session_id: "s".into(),
            role: "user".into(),
            text: "t".into(),
            tool_uses: vec![],
            timestamp: None,
        });

        let blocks = engine.completed_blocks();
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[0].source, BlockSource::Shell));
        assert!(blocks[1].is_claude_turn());
    }
}
