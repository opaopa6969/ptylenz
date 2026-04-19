# Decision: PROMPT_COMMAND overwrite strategy

**Status**: Active  
**Date**: 2026-04-19

---

## Context

ptylenz needs to emit four OSC 133 sequences at the right moments in the bash lifecycle:

| Sequence | Moment |
|----------|--------|
| `\e]133;A\a` | Before every prompt |
| `\e]133;C\a` | Before every command runs |
| `\e]133;D;N\a` | After every command, with exit code |
| `\e]133;E;text\a` | After every command, with command text |

bash provides several mechanisms that could serve each role. This document records the choices made and the alternatives rejected.

---

## Chosen approach

```bash
PROMPT_COMMAND='__ptylenz_precmd'   # emits 133;D and 133;E
PS0='\[\e]133;C\a\]'                # emits 133;C (before command)
PS1='\[\e]133;A\a\]'"$PS1"          # emits 133;A (before prompt)
```

`PROMPT_COMMAND` is **assigned, not appended**. This means any previous value of `PROMPT_COMMAND` is replaced.

---

## Why not append to PROMPT_COMMAND?

The natural instinct is to append:

```bash
PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND; }__ptylenz_precmd"
```

This preserves any existing `PROMPT_COMMAND`. However:

1. **Order dependency**: if an existing `PROMPT_COMMAND` reads `$?` (exit code) for its own purposes, it must run first — before `__ptylenz_precmd` captures `$?`. The order is unpredictable when appending.

2. **Idempotency**: appending on every ptylenz launch (even a re-launch in the same shell) would accumulate duplicate `__ptylenz_precmd` entries.

3. **Conflict complexity**: the wrapper rcfile is ephemeral (written to `$TMPDIR`, deleted after session). The PROMPT_COMMAND it sets is also ephemeral — it only exists while ptylenz is running. In a fresh bash spawned by ptylenz, the only source of `PROMPT_COMMAND` before ptylenz's assignment is `~/.bashrc`. The risk of a destructive overwrite is low.

The simple overwrite trades robustness to unusual dotfile arrangements for implementation simplicity and predictable behavior.

---

## The real risk: ~/.bashrc sources another file after ptylenz's wrapper

The wrapper rcfile is structured as:

```bash
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"
# ... (ptylenz integration follows)
PROMPT_COMMAND='__ptylenz_precmd'
```

ptylenz's `PROMPT_COMMAND` assignment happens after `~/.bashrc` is fully sourced. This means any `PROMPT_COMMAND` set inside `~/.bashrc` (or any file sourced by it) is overwritten by ptylenz. So far so good.

The failure mode is when `~/.bashrc` sources a file that is re-evaluated after the wrapper's integration section. In practice this does not happen — shell rc files are sourced linearly — but it could happen with unusual `eval`-based frameworks.

---

## Why not DEBUG trap?

The `DEBUG` trap is a natural fit for "before every command":

```bash
trap '__ptylenz_preexec "$BASH_COMMAND"' DEBUG
```

Rejected because:

1. **Nesting**: the `DEBUG` trap fires before every simple command, including subcommands run inside functions. `__ptylenz_precmd` itself calls `history 1` via a subshell. The `DEBUG` trap would fire on that subshell call and emit a spurious `133;C` event in the middle of `PROMPT_COMMAND`.

2. **Interaction with existing traps**: bash only supports one `DEBUG` trap handler per shell. Setting it overwrites any existing `DEBUG` trap. This is more disruptive than overwriting `PROMPT_COMMAND`.

3. **`extdebug` requirement**: to get the original command text from `$BASH_COMMAND`, the text in the `DEBUG` trap is already expanded. For the block title this is fine, but `set -o extdebug` has side effects (changes `return` behavior in functions).

---

## Why not PS0 alone for command text?

`PS0` is evaluated and printed before the command runs, which makes it appear before the command output. We could embed `133;E;$text` in `PS0`, but at that point bash has not yet recorded the command in history — `history 1` inside `PS0` returns the previous command, not the one about to run.

The `133;E` sequence is emitted from `PROMPT_COMMAND` after the command completes, where `history 1` correctly returns the command that just ran. The block engine handles the out-of-order arrival by patching the last closed block when `133;E` arrives after `133;D`.

---

## Summary

| Mechanism | Used for | Why |
|-----------|----------|-----|
| `PS1` prepend | `133;A` (prompt start) | Fires at every prompt, before the prompt string is displayed |
| `PS0` | `133;C` (command start) | Fires between reading the command and executing it |
| `PROMPT_COMMAND` (overwrite) | `133;D` + `133;E` | Fires after every command; `$?` is preserved; command text available via `history 1` |
| `DEBUG` trap | not used | Nesting problem with subcommands inside the handler |
