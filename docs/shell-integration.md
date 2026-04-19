# Shell Integration

> English · [日本語](shell-integration-ja.md)

ptylenz uses **OSC 133** — the same shell integration protocol supported by iTerm2, Warp, and VS Code Terminal — to detect block boundaries precisely.

> **Platform**: Linux and macOS only. Windows is not supported.

---

## How it works

When ptylenz spawns bash, it writes a wrapper rcfile to `$TMPDIR` and passes it via `bash --rcfile <wrapper>`. The wrapper:

1. Sources `~/.bashrc` first — your existing prompt, aliases, and completions are preserved exactly.
2. Installs the `__ptylenz_precmd` function and wires it into `PROMPT_COMMAND`, `PS0`, and `PS1`.

The four OSC 133 sequences emitted:

| Sequence | When emitted | Source |
|----------|-------------|--------|
| `\e]133;A\a` | Before every prompt is displayed | `PS1` prepend |
| `\e]133;C\a` | Immediately before the command runs | `PS0` |
| `\e]133;D;N\a` | After the command finishes (exit code N) | `PROMPT_COMMAND` |
| `\e]133;E;text\a` | Command text (from `history 1`) | `PROMPT_COMMAND` |

This is automatic for bash. No configuration is required.

---

## bash

### Automatic injection

Everything is handled by ptylenz. You do not need to add anything to `~/.bashrc`.

### What ptylenz injects

```bash
__ptylenz_precmd() {
    local __ptylenz_ec=$?
    printf '\e]133;D;%d\a' "$__ptylenz_ec"
    local __ptylenz_last
    __ptylenz_last=$(HISTTIMEFORMAT='' history 1 2>/dev/null \
        | sed -E 's/^[[:space:]]*[0-9]+[[:space:]]*//')
    if [ -n "$__ptylenz_last" ]; then
        printf '\e]133;E;%s\a' "$__ptylenz_last"
    fi
}
PROMPT_COMMAND='__ptylenz_precmd'
PS0='\[\e]133;C\a\]'
case "$PS1" in
  *'133;A'*) ;;
  *) PS1='\[\e]133;A\a\]'"$PS1" ;;
esac
```

### PROMPT_COMMAND overwrite

`PROMPT_COMMAND` is assigned directly, not appended. This means any `PROMPT_COMMAND` set **after** ptylenz sources `~/.bashrc` inside the wrapper will overwrite ptylenz's value and break block detection.

Common problem: a `~/.bashrc` that contains `PROMPT_COMMAND="something"` near the end. ptylenz sources `~/.bashrc` first, then sets `PROMPT_COMMAND='__ptylenz_precmd'`, so in that order it works correctly. However, if your `~/.bashrc` sources another file at the end (e.g. a company dotfile) that also sets `PROMPT_COMMAND`, ptylenz's assignment will be overwritten.

Fix: either chain the other function instead of overwriting:

```bash
# in ~/.bashrc, if you need to keep an existing PROMPT_COMMAND
# and also support ptylenz:
# (ptylenz sets PROMPT_COMMAND='__ptylenz_precmd' after sourcing ~/.bashrc,
#  so this should be fine in most cases — only an issue if another file is
#  sourced after the wrapper sets it)
```

See [docs/decisions/prompt-command-strategy.md](decisions/prompt-command-strategy.md) for the full rationale.

### DEBUG trap vs PROMPT_COMMAND

ptylenz deliberately avoids the `DEBUG` trap. The `DEBUG` trap fires before every simple command inside functions, including subshell calls and command substitutions. A shell function that runs `history 1` as part of `__ptylenz_precmd` would trigger the `DEBUG` trap on that subcommand too, generating spurious `133;C` events mid-block. `PROMPT_COMMAND` fires only between prompts, so no such nesting occurs.

---

## zsh

ptylenz does not currently auto-inject into zsh. Manual setup:

Add the following to `~/.zshrc`:

```zsh
__ptylenz_precmd() {
    printf '\e]133;D;%d\a' "$?"
    local last_cmd
    last_cmd=$(fc -ln -1 2>/dev/null | sed 's/^[[:space:]]*//')
    [ -n "$last_cmd" ] && printf '\e]133;E;%s\a' "$last_cmd"
}
__ptylenz_preexec() {
    printf '\e]133;C\a'
}
precmd_functions+=(__ptylenz_precmd)
preexec_functions+=(__ptylenz_preexec)
# Prompt start — add to your existing PROMPT or PS1:
PROMPT=$'\e]133;A\a'"$PROMPT"
```

Notes:
- zsh's `precmd` and `preexec` hooks are the correct mechanism; no `PROMPT_COMMAND` equivalent exists in zsh.
- `preexec` fires with the command string as `$1` — you can emit `133;E;$1` directly instead of reading from history.
- Test with `printf '\e]133;C\a'; echo hello; printf '\e]133;D;0\a'` to verify sequences reach ptylenz.

---

## fish

ptylenz does not currently auto-inject into fish. Manual setup:

Add the following to `~/.config/fish/config.fish`:

```fish
function __ptylenz_prompt_start --on-event fish_prompt
    printf '\e]133;A\a'
end

function __ptylenz_preexec --on-event fish_preexec
    printf '\e]133;C\a'
    printf '\e]133;E;%s\a' "$argv"
end

function __ptylenz_postexec --on-event fish_postexec
    printf '\e]133;D;%d\a' "$status"
end
```

Notes:
- fish events `fish_prompt`, `fish_preexec`, and `fish_postexec` are the correct hooks.
- `$argv` in `fish_preexec` contains the command string — no history lookup needed.
- Test: run `echo test` and check that ptylenz shows the block.

---

## OSC 133 reference

| Code | Payload | Meaning |
|------|---------|---------|
| `A` | (none) | Prompt about to be displayed |
| `B` | (none) | Prompt finished being displayed (end of prompt) — not used by ptylenz |
| `C` | (none) | Command execution about to start |
| `D` | `;N` (exit code) | Command finished |
| `E` | `;text` (command) | Command text for the block title |

Both BEL (`\a`, `\x07`) and ST (`\e\\`) are accepted as terminators.

### Sequence ordering

In the iTerm2 model, `133;E` arrives at prompt display time (same time as `133;A`). In ptylenz's bash integration, `133;E` arrives from `PROMPT_COMMAND` which fires after `133;D`. The block engine handles both orderings: if a `133;E` arrives when `current` is open, the command text is attached to the open block; if `current` is `None`, it patches the last closed block.

---

## Verifying integration is active

From inside a ptylenz session, run:

```bash
printf '\e]133;C\a'; echo "test output"; printf '\e]133;D;0\a'; printf '\e]133;E;test\a'
```

Then press `Ctrl+]` to open the block list. If integration is working you should see a block labelled `test` containing "test output".

If no block appears, OSC 133 sequences are not reaching ptylenz — check that your shell integration is loaded and that `PROMPT_COMMAND` has not been overwritten.

---

## tmux and multiplexer compatibility

When ptylenz runs inside a tmux session:

- OSC 52 clipboard requires `set-clipboard on` in `~/.tmux.conf`
- OSC 133 sequences pass through tmux transparently in tmux 3.3+

When tmux runs inside a ptylenz session (ptylenz is the outer layer):

- OSC 133 sequences from the shell inside tmux are intercepted by tmux and do not reach ptylenz
- Clipboard via `xclip`/`pbcopy` still works

The recommended setup is ptylenz on the outside, tmux on the inside only if you need session persistence or split panes.
