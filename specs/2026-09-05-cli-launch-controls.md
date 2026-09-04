# Spec Addendum: CLI Launch Controls

**Date**: 2026-09-05  
**Issue**: #25

## Scope

Expose a minimal supported startup CLI.

## User-facing behavior

### `--help`

- Prints usage text.
- Exits with status `0`.
- Does not spawn the PTY proxy or TUI.

### `--version`

- Prints `ptylenz <version>`.
- Exits with status `0`.
- Does not spawn the PTY proxy or TUI.

### `--shell <PATH>`

- Overrides `$SHELL` for this invocation only.
- Empty values are rejected.

### `--no-integrate`

- Disables bash wrapper rcfile injection.
- PTY passthrough still works.
- No shell OSC markers are expected unless the shell or environment emits them independently.

## Error behavior

- Unknown options exit non-zero with a clear message.
- Missing `--shell` value exits non-zero with a clear message.
- Positional arguments are rejected.

## Tests

- Parser success paths.
- Parser failure paths.
- Runtime no-integration path preserves PTY output passthrough and suppresses synthetic block capture.

## Non-goals

- Export CLI
- Claude feeder toggles
- Config files
