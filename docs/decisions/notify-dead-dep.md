# Decision: notify crate — kept but unused

**Status**: Active  
**Date**: 2026-04-19

---

## Context

`Cargo.toml` lists `notify = "6"` as a dependency. `notify` is a cross-platform filesystem notification library that wraps `inotify` (Linux), `kqueue` (macOS/BSDs), and `ReadDirectoryChangesW` (Windows).

However, the Claude Code JSONL feeder (`src/claude_feeder.rs`) does not use `notify`. It polls the project directory with `thread::sleep(Duration::from_millis(400))`.

---

## Why notify is unused

The feeder was written with polling by design, for one reason: **SSH-forwarded homedirs**.

ptylenz is frequently used over SSH — `ssh server`, then `./ptylenz`. In that scenario the `~/.claude/projects/` directory lives on the remote filesystem. Kernel filesystem notifications (`inotify`, `kqueue`) are local-only: they do not fire for events on network-mounted filesystems (NFS, SSHFS, SMB). A feeder that depended on `inotify` would silently see no events and produce no Claude blocks whenever ptylenz ran over SSH.

Polling at 400 ms is cheap (one `readdir` + one `stat` per tick) and universally reliable, regardless of filesystem type.

---

## Why notify is still in Cargo.toml

`notify` was added during initial project scaffolding as the likely implementation path, before the SSH constraint was recognized. Removing it from `Cargo.toml` now would be the right thing to do, but:

1. The crate compiles fine; the extra build time is small.
2. The 6.x API is already explored and the migration path to an event-driven feeder is clear — if a future platform (e.g. a local-only mode that never runs over SSH) warrants it.
3. No code outside `Cargo.toml` and `Cargo.lock` references `notify`, so it imposes no maintenance burden.

---

## Decision

Leave `notify` in `Cargo.toml` as a marker of future intent, but document it here as unused.

**If you want to remove it**: `cargo remove notify`, then `cargo build` to verify.

**If you want to use it**: the feeder's `run_watch_loop` can be rewritten to use `notify::recommended_watcher` with a `RecursiveMode::NonRecursive` watcher on the project directory. Gate the inotify path behind a `--local` flag or a `PTYLENZ_NO_POLL` environment variable so SSH users retain polling.

---

## References

- `src/claude_feeder.rs`, `run_watch_loop` function
- `Cargo.toml`, `[dependencies]` section
