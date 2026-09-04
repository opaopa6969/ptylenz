# DGE Session: CLI Launch Controls

**Date**: 2026-09-05  
**Issue**: #25  
**Branch**: `feat/issue-25-cli-launch-controls`

## Product framing

ptylenz already has a differentiated core: structured PTY output plus Claude Code timeline integration. The current launch contract, however, is implicit and underpowered. That reduces trust at the exact moment a user decides whether to adopt the tool.

## Value gaps considered

### 1. Onboarding / discoverability

- There is no `--help` or `--version`.
- Startup behavior is only discoverable by reading repository docs or source.
- Result: lower trial-to-success rate for advanced users evaluating quickly.

### 2. Operability / one-shot control

- Shell selection is effectively environment-driven (`$SHELL`) rather than invocation-driven.
- Users cannot test ptylenz against another shell path without mutating broader session state.
- Result: avoidable friction for debugging and reproducibility.

### 3. Compatibility / existing shell integration

- bash OSC integration is always injected when bash is launched.
- Users who already emit OSC 133 externally cannot opt out cleanly.
- Result: duplication/confusion risk in advanced shell setups.

### 4. Trust / privacy

- Claude Code JSONL tailing is automatic when project context exists.
- Some users may want a launch-time opt-out.
- Result: real value, but changing default story too early risks diluting the repo's current market thesis.

### 5. Scale / memory

- Session blocks are unbounded in-memory.
- Long sessions can grow substantially.
- Result: important, but higher design surface and not the smallest reversible slice.

## Chosen gap

Implement minimal CLI launch controls:

- `--shell <PATH>`
- `--no-integrate`
- `--version`
- `-h, --help`

## Why this one

- Highest user-visible value per line changed.
- No migration or persistence impact.
- Safe and reversible.
- Already anticipated by `spec/SPEC.md`, so implementation risk is low.

## Non-goals

- `--export`
- Claude feeder opt-out
- persistent config
- multi-shell integration redesign
- memory retention policy

## Tramli / tramli-appspec fit check

- `human-in-loop`: not required. This is local launch-time behavior, deterministic, and fully testable.
- `long-term state`: none added. No database, cache, or config persistence.
- `compensation`: not needed. Behavior is process-local and ends with process exit.
- `external event`: none beyond normal terminal/PTY I/O already handled by the app.
- `requires`: local argv, environment, shell executable.
- `produces`: help/version output, modified process startup behavior.

Decision: full tramli/tramli-appspec machinery is unnecessary for this slice.
