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
- The shell must exist, be a regular file, and be executable. Bare names
  (no `/`) are resolved through `$PATH`, matching `execvp`.
- An empty `$SHELL` is treated as unset and falls back to `/bin/bash`.

### `--no-integrate`

- Disables bash wrapper rcfile injection.
- PTY passthrough still works.
- No shell OSC markers are expected unless the shell or environment emits them independently.

## Error behavior

- Unknown options exit non-zero with a clear message.
- Missing `--shell` value exits non-zero with a clear message.
- Positional arguments are rejected.
- Arguments that are not valid UTF-8 are reported as usage errors, not panics.
- An unusable shell path fails before the fork with a named error instead of a
  PTY that closes immediately (which surfaced either as a silent exit `0` or as
  a raw `EIO`, depending on timing).

## Tests

- Parser success paths.
- Parser failure paths.
- Non-UTF-8 argv handling.
- Shell path validation: empty, missing, directory, non-executable, `$PATH` lookup.
- Runtime no-integration path preserves PTY output passthrough and suppresses synthetic block capture.

## Non-goals

- Export CLI
- Claude feeder toggles
- Config files
