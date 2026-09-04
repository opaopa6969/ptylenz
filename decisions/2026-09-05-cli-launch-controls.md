# Decision: Add minimal CLI launch controls

**Status**: Active  
**Date**: 2026-09-05  
**Issue**: #25

## Context

`ptylenz` currently launches with no CLI surface. It reads `$SHELL`, falls back to `/bin/bash`, and always injects bash OSC 133 shell integration when launching bash. That behavior is workable but opaque.

The repo's market and spec documents already imply a more explicit startup contract. The missing piece is implementation, not a new product bet.

## Decision

Add a minimal hand-rolled CLI parser supporting:

- `--shell <PATH>`
- `--no-integrate`
- `--version`
- `-h, --help`

## Why hand-rolled

- The option surface is small and stable.
- Adding a parser dependency would increase compile surface for little benefit.
- Reverting remains trivial.

## Alternatives rejected

### Add `clap`

Rejected because the dependency weight is not justified for four flags.

### Add `--no-claude` in the same PR

Rejected because it broadens the launch contract and changes the AI-pairing story, which `docs/market/2026-08-23.md` identifies as the repo's strongest differentiator.

### Implement the full planned CLI including `--export`

Rejected because `--export` implies a wider design discussion around export inputs and session lifecycle. It is not needed to recover immediate value.

## Revert

Delete `src/cli.rs`, restore env-only startup in `src/main.rs`, and revert the spawn/config wiring changes.
