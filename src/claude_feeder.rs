//! Claude Code JSONL feeder.
//!
//! Claude Code writes a newline-delimited JSON log of every session to
//!   `~/.claude/projects/<cwd-slug>/<session-id>.jsonl`
//! where `<cwd-slug>` is the working directory with `/` replaced by `-`.
//!
//! This module:
//!   - finds the project directory for a given cwd,
//!   - watches it for new/updated `.jsonl` files,
//!   - tails the active session file,
//!   - decodes each line into a `ClaudeEvent` (user turn, assistant turn),
//!   - and forwards them on an mpsc channel.
//!
//! The module is PTY-agnostic: it takes a cwd, it emits `ClaudeEvent`s.
//! Integration with the block engine lives in `block.rs`.
//!
//! Lines that are not turns (permission-mode, file-history-snapshot, …)
//! are ignored.
//!
//! We only tail files as they grow; we do not replay history on startup.
//! That keeps the block list scoped to what happens DURING a ptylenz
//! session — re-reading a huge historical jsonl on every ptylenz launch
//! is neither useful nor cheap.

#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

/// One decoded event from a Claude Code session log.
#[derive(Debug, Clone)]
pub enum ClaudeEvent {
    /// A turn (user or assistant) appeared in the active session.
    Turn {
        session_id: String,
        role: String, // "user" | "assistant"
        text: String,
        /// Any tool_use blocks the assistant emitted (name + json input).
        tool_uses: Vec<ToolUse>,
        timestamp: Option<String>,
    },
    /// The feeder switched to a new session file (claude restarted, /resume).
    SessionStarted {
        session_id: String,
        path: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct ToolUse {
    pub name: String,
    pub input_json: String,
}

/// Convert a cwd like `/home/opa/work/ptylenz` into the directory-slug
/// Claude Code uses, e.g. `-home-opa-work-ptylenz`.
pub fn cwd_slug(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('/', "-")
}

/// Return the projects directory Claude writes to for `cwd`.
pub fn project_dir_for(cwd: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".claude");
    p.push("projects");
    p.push(cwd_slug(cwd));
    Some(p)
}

/// Start a background thread that watches the Claude project dir for `cwd`
/// and emits `ClaudeEvent`s on the returned channel.
///
/// The thread exits silently if the projects dir does not exist or cannot
/// be watched — ptylenz still works without it.
pub fn spawn_watcher(cwd: &Path) -> Receiver<ClaudeEvent> {
    let (tx, rx) = mpsc::channel();
    let Some(dir) = project_dir_for(cwd) else {
        return rx;
    };
    thread::spawn(move || {
        let _ = run_watch_loop(&dir, tx);
    });
    rx
}

/// The watch loop. Polls the directory for the newest `.jsonl` file and
/// tails it, switching when a newer file appears.
///
/// We use polling rather than inotify here because:
///   - ptylenz must work over SSH-forwarded homedirs where inotify may not
///     propagate,
///   - polling every 500ms is cheap enough for our workload,
///   - it keeps the dependency graph smaller (we pulled `notify` in the
///     Cargo.toml but never rely on its async guts — the poll path below
///     is the one we ship today; `notify` is kept available for a future
///     revision).
fn run_watch_loop(dir: &Path, tx: Sender<ClaudeEvent>) -> Result<()> {
    let mut active: Option<(PathBuf, u64)> = None; // (path, file offset)
    loop {
        // Wait until the directory exists. Claude creates it on first use.
        if !dir.exists() {
            thread::sleep(Duration::from_millis(500));
            continue;
        }

        let newest = newest_jsonl(dir)?;
        let same_as_active = match (&active, &newest) {
            (Some((path, _)), Some(new_path)) => path == new_path,
            _ => false,
        };

        if same_as_active {
            if let Some((path, offset)) = active.as_mut() {
                tail_once(&path.clone(), offset, &tx).ok();
            }
        } else if let Some(new_path) = newest {
            let session_id = session_id_from_path(&new_path);
            let len = std::fs::metadata(&new_path).map(|m| m.len()).unwrap_or(0);
            let _ = tx.send(ClaudeEvent::SessionStarted {
                session_id,
                path: new_path.clone(),
            });
            active = Some((new_path, len));
        }

        thread::sleep(Duration::from_millis(400));
    }
}

fn newest_jsonl(dir: &Path) -> Result<Option<PathBuf>> {
    let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {:?}", dir))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let meta = entry.metadata()?;
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        match &newest {
            Some((_, best)) if *best >= mtime => {}
            _ => newest = Some((path, mtime)),
        }
    }
    Ok(newest.map(|(p, _)| p))
}

fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Read from `*offset` to EOF, parse each complete line, advance offset.
fn tail_once(path: &Path, offset: &mut u64, tx: &Sender<ClaudeEvent>) -> Result<()> {
    let mut f = std::fs::File::open(path).with_context(|| format!("open {:?}", path))?;
    let len = f.metadata()?.len();
    if len < *offset {
        // File was truncated / rotated — reset.
        *offset = 0;
    }
    if len == *offset {
        return Ok(());
    }
    f.seek(SeekFrom::Start(*offset))?;
    let mut reader = BufReader::new(f);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        // Partial final line: stop here, pick up next tick.
        if !line.ends_with('\n') {
            break;
        }
        *offset += n as u64;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(ev) = decode_line(trimmed) {
            let _ = tx.send(ev);
        }
    }
    Ok(())
}

/// Decode one JSONL line into a `ClaudeEvent`.
/// Returns `None` for lines we don't care about (metadata, snapshots, ...).
pub fn decode_line(line: &str) -> Option<ClaudeEvent> {
    let raw: RawEntry = serde_json::from_str(line).ok()?;

    // Only user / assistant turns produce events.
    match raw.type_.as_deref() {
        Some("user") | Some("assistant") => {}
        _ => return None,
    }

    let role = raw.type_.clone().unwrap_or_default();
    let session_id = raw.session_id.clone().unwrap_or_default();
    let message = raw.message?;

    let (text, tool_uses) = extract_text_and_tools(&message);

    Some(ClaudeEvent::Turn {
        session_id,
        role,
        text,
        tool_uses,
        timestamp: raw.timestamp,
    })
}

/// Walk `message.content` — which can be either a plain string or an array
/// of typed blocks — and pull out plain text + tool_use payloads.
fn extract_text_and_tools(message: &RawMessage) -> (String, Vec<ToolUse>) {
    let mut text = String::new();
    let mut tools = Vec::new();

    match &message.content {
        Some(serde_json::Value::String(s)) => {
            text.push_str(s);
        }
        Some(serde_json::Value::Array(arr)) => {
            for block in arr {
                if let Some(obj) = block.as_object() {
                    match obj.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                        }
                        Some("tool_use") => {
                            let name = obj
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let input_json = obj
                                .get("input")
                                .map(|v| v.to_string())
                                .unwrap_or_default();
                            tools.push(ToolUse { name, input_json });
                        }
                        // tool_result, thinking, etc. — skip for now, we
                        // surface them in detail view in a later phase.
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    (text, tools)
}

#[derive(Debug, Deserialize)]
struct RawEntry {
    #[serde(rename = "type")]
    type_: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    timestamp: Option<String>,
    message: Option<RawMessage>,
    // Tolerate any other fields.
    #[serde(flatten)]
    _other: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    #[serde(default)]
    content: Option<serde_json::Value>,
    // role, id, model, … — unused.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn slug_replaces_slashes_with_dashes() {
        let s = cwd_slug(Path::new("/home/opa/work/ptylenz"));
        assert_eq!(s, "-home-opa-work-ptylenz");
    }

    #[test]
    fn decode_user_turn_with_string_content() {
        let line = r#"{"parentUuid":null,"type":"user","message":{"role":"user","content":"hello"},"uuid":"u1","timestamp":"2026-04-12T13:40:46.377Z","sessionId":"sess1"}"#;
        let ev = decode_line(line).expect("must decode");
        match ev {
            ClaudeEvent::Turn {
                role,
                text,
                session_id,
                tool_uses,
                ..
            } => {
                assert_eq!(role, "user");
                assert_eq!(text, "hello");
                assert_eq!(session_id, "sess1");
                assert!(tool_uses.is_empty());
            }
            _ => panic!("expected Turn"),
        }
    }

    #[test]
    fn decode_assistant_turn_with_text_and_tool_use() {
        let line = r#"{"type":"assistant","sessionId":"s2","timestamp":"2026-04-12T13:41:00Z","message":{"role":"assistant","content":[{"type":"text","text":"looking…"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#;
        let ev = decode_line(line).expect("must decode");
        match ev {
            ClaudeEvent::Turn {
                role,
                text,
                tool_uses,
                ..
            } => {
                assert_eq!(role, "assistant");
                assert_eq!(text, "looking…");
                assert_eq!(tool_uses.len(), 1);
                assert_eq!(tool_uses[0].name, "Bash");
                assert!(tool_uses[0].input_json.contains("\"command\":\"ls\""));
            }
            _ => panic!("expected Turn"),
        }
    }

    #[test]
    fn decode_skips_non_turn_lines() {
        let line = r#"{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"s3"}"#;
        assert!(decode_line(line).is_none());

        let line2 = r#"{"type":"file-history-snapshot","messageId":"m"}"#;
        assert!(decode_line(line2).is_none());
    }

    #[test]
    fn decode_malformed_is_none() {
        assert!(decode_line("not json").is_none());
        assert!(decode_line("").is_none());
    }
}
