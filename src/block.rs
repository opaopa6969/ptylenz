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

/// Events detected in the PTY output stream.
#[derive(Debug, Clone)]
pub enum OscEvent {
    PromptStart,
    CommandStart,
    CommandText(String),
    CommandEnd { exit_code: i32 },
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
        }
    }

    /// Number of lines in the output.
    pub fn line_count(&self) -> usize {
        self.output.iter().filter(|&&b| b == b'\n').count()
    }

    /// Output as lossy UTF-8 string (for display/search).
    pub fn output_text(&self) -> String {
        // Strip ANSI escape sequences for plain text
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
}

impl BlockEngine {
    pub fn new() -> Self {
        BlockEngine {
            blocks: Vec::new(),
            next_id: 1,
            current: None,
            osc_parser: OscParser::new(),
            prompt_pattern: None,
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
                    // Start a new block
                    let block = Block::new(self.next_id);
                    self.next_id += 1;
                    self.current = Some(block);
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
                        // Auto-collapse long output
                        if block.line_count() > 50 {
                            block.collapsed = true;
                        }
                        self.blocks.push(block);
                    }
                }
                OscEvent::PromptStart => {
                    // If there's an open block without a proper end, close it
                    if let Some(mut block) = self.current.take() {
                        block.ended_at = Some(Local::now());
                        if block.line_count() > 50 {
                            block.collapsed = true;
                        }
                        self.blocks.push(block);
                    }
                }
            }
        }

        if let Some(ref mut block) = self.current {
            block.output.extend_from_slice(&clean_data);
        }

        (clean_data, osc_events)
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
                        // BEL = end of OSC
                        if let Some(event) = self.decode_osc(&self.buf.clone()) {
                            events.push(event);
                        }
                        self.buf.clear();
                        self.state = ParseState::Normal;
                    } else if byte == 0x1b {
                        // Might be \e\\ (ST) — simplified: treat as end
                        if let Some(event) = self.decode_osc(&self.buf.clone()) {
                            events.push(event);
                        }
                        self.buf.clear();
                        self.state = ParseState::Normal;
                    } else {
                        self.buf.push(byte);
                    }
                }
            }
        }

        (clean, events)
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
}
