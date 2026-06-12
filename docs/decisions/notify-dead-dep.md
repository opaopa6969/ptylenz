# Decision: notify crate — scaffolded, then removed

**Status**: Active (notify removed from `Cargo.toml`)  
**Date**: 2026-04-19

---

## Context

`notify` is a cross-platform filesystem notification library that wraps `inotify` (Linux), `kqueue` (macOS/BSDs), and `ReadDirectoryChangesW` (Windows). It was added to `Cargo.toml` during initial scaffolding as the expected way to watch the Claude Code session log.

The Claude Code JSONL feeder (`src/claude_feeder.rs`) never ended up using it. It polls the project directory with `thread::sleep(Duration::from_millis(400))`, and `notify` has since been dropped from `Cargo.toml` and `Cargo.lock`.

---

## Why notify is unused

The feeder was written with polling by design, for one reason: **SSH-forwarded homedirs**.

ptylenz is frequently used over SSH — `ssh server`, then `./ptylenz`. In that scenario the `~/.claude/projects/` directory lives on the remote filesystem. Kernel filesystem notifications (`inotify`, `kqueue`) are local-only: they do not fire for events on network-mounted filesystems (NFS, SSHFS, SMB). A feeder that depended on `inotify` would silently see no events and produce no Claude blocks whenever ptylenz ran over SSH.

Polling at 400 ms is cheap (one `readdir` + one `stat` per tick) and universally reliable, regardless of filesystem type.

---

## Why it was removed

`notify` was added during initial scaffolding as the likely implementation path, before the SSH constraint was recognized. Once the feeder settled on polling, the crate was pure dead weight: nothing outside `Cargo.toml` / `Cargo.lock` referenced it, so it only added build time. It has been dropped.

---

## Decision

Remove `notify` from `Cargo.toml`. Keep this note as the record of why an event-driven watcher was considered and rejected.

**If you want to reintroduce it**: `cargo add notify`, then rewrite the feeder's `run_watch_loop` to use `notify::recommended_watcher` with a `RecursiveMode::NonRecursive` watcher on the project directory. Gate the inotify path behind a `--local` flag or a `PTYLENZ_NO_POLL` environment variable so SSH users retain polling.

---

## References

- `src/claude_feeder.rs`, `run_watch_loop` function
- `Cargo.toml`, `[dependencies]` section
