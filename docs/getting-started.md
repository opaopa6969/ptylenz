# Getting Started with ptylenz

> English · [日本語](getting-started-ja.md)

> **Platform**: Linux and macOS only. Windows is not supported.

---

## Prerequisites

- Linux x86_64 or macOS (Apple Silicon or Intel)
- bash 4+ (the default shell on most Linux distros; macOS ships bash 3.2 — upgrade via Homebrew is optional but recommended)
- For clipboard support on Linux: `xclip` (optional)

To build from source you also need:

- Rust 1.76 or later (`rustup.rs`)

---

## Install: pre-built binary (recommended)

Download the binary for your platform from the [Releases page](https://github.com/opaopa6969/ptylenz/releases/latest):

```bash
# Linux x86_64
curl -L https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-linux-x86_64 \
     -o ptylenz
chmod +x ptylenz
sudo mv ptylenz /usr/local/bin/

# macOS Apple Silicon
curl -L https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-macos-aarch64 \
     -o ptylenz
chmod +x ptylenz
sudo mv ptylenz /usr/local/bin/

# macOS Intel
curl -L https://github.com/opaopa6969/ptylenz/releases/latest/download/ptylenz-macos-x86_64 \
     -o ptylenz
chmod +x ptylenz
sudo mv ptylenz /usr/local/bin/
```

Verify:

```bash
ptylenz --version
```

---

## Install: from source

```bash
git clone https://github.com/opaopa6969/ptylenz.git
cd ptylenz
./install.sh
```

`install.sh` runs `cargo install --path . --force` which puts the binary in `~/.cargo/bin/ptylenz`. If that directory is not on your `PATH`, add it:

```bash
# add to ~/.bashrc or ~/.zshrc
export PATH="$HOME/.cargo/bin:$PATH"
```

To update later:

```bash
cd ptylenz && git pull && ./install.sh
```

---

## First run

```bash
ptylenz
```

Your shell starts inside ptylenz. The terminal looks exactly the same as before — no banner, no prompt change, no lag. ptylenz is invisible in Normal mode.

Run a few commands as usual:

```bash
ls -la
echo "hello ptylenz"
cargo test 2>&1 | head -20
```

Now press **`Ctrl+]`** to enter Ptylenz mode.

You should see a block list overlaid on the screen. Each command you ran is a block showing its timestamp, line count, exit status, and command text.

Navigate with `j`/`k` (or arrow keys). Press `Enter` to expand/collapse a block. Press `q` or `Esc` or `Ctrl+]` again to return to Normal mode.

---

## Basic workflow

### Navigate blocks

| Key | Action |
|-----|--------|
| `j` / `↓` | next block |
| `k` / `↑` | previous block |
| `g` | jump to first block |
| `G` | jump to last block |
| `Enter` | expand / collapse selected block |

### Search

Press `/` to open the search bar. Type your query and press `Enter`. Use `n`/`N` to jump to the next/previous match.

### Copy

Press `y` to copy the selected block's output to the clipboard.

For fine-grained selection, press `v` to open Detail view, then:
- `v` for linewise selection
- `Ctrl+v` for blockwise (rectangular) selection
- `y` to copy the selection

### Export

Press `e` to export all blocks to a JSON file in the current directory. The JSON follows the [claude-session-replay](https://github.com/opaopa6969/claude-session-replay) common log model.

---

## Shell integration

ptylenz injects OSC 133 markers into bash automatically via a wrapper rcfile. You do not need to configure anything for bash.

For other shells (zsh, fish) see [docs/shell-integration.md](shell-integration.md).

---

## Optional: auto-enter on shell startup

Add this at the end of `~/.bashrc` to automatically launch ptylenz whenever you open a new terminal:

```bash
# Skip in non-interactive shells (scp / rsync / ssh host cmd, etc.)
case $- in *i*) ;; *) return ;; esac
[ -z "$PTYLENZ" ] && command -v ptylenz >/dev/null && exec ptylenz
```

**Caution**: if ptylenz crashes, every new shell will also fail to start. Only enable this after you have used ptylenz long enough to trust its stability. Recovery: `bash --norc`.

The `$PTYLENZ=1` environment variable set by ptylenz inside the child bash prevents recursive launches.

---

## Clipboard support

**macOS**: `pbcopy` is used automatically — no setup needed.

**Linux**: ptylenz tries OSC 52 first (works inside tmux when `set-clipboard` is enabled), then falls back to `xclip`. Install xclip if you do not use tmux:

```bash
# Debian / Ubuntu
sudo apt install xclip

# Fedora
sudo dnf install xclip

# Arch
sudo pacman -S xclip
```

---

## Updating

### Binary install

Download the new binary from the Releases page and replace the old one using the same steps as the initial install.

### Source install

```bash
cd ptylenz
git pull
./install.sh
```

---

## Uninstalling

```bash
# if installed to /usr/local/bin
sudo rm /usr/local/bin/ptylenz

# if installed via cargo install
cargo uninstall ptylenz
```

Remove the optional auto-enter snippet from `~/.bashrc` if you added it.

---

## Troubleshooting

### ptylenz exits immediately

Your shell path may not be bash. ptylenz reads `$SHELL` to pick the shell to spawn. If `$SHELL` points to zsh or fish, ptylenz will start but shell integration will not inject (OSC 133 markers will not be emitted) and no blocks will be detected.

Workaround until per-shell integration is complete:

```bash
SHELL=/bin/bash ptylenz
```

### The block list is empty after running commands

Shell integration may not have loaded. Check that no prior `PROMPT_COMMAND` assignment in your `~/.bashrc` is running after ptylenz's wrapper rcfile sources it and silently overriding ptylenz's `PROMPT_COMMAND`. See [docs/shell-integration.md](shell-integration.md) for details.

### Clipboard does not work on Linux

Install `xclip` (see above). If you are inside a tmux session, run `tmux set-option -g set-clipboard on` to enable OSC 52 passthrough.

### CJK characters appear misaligned in Detail view

This is a known cosmetic issue. CJK (Chinese, Japanese, Korean) characters are double-width (two terminal cells each). When the ratatui overlay panel is narrower than the captured terminal, lines with many CJK characters may overflow or shift. There is no workaround at present.

### Recovery if auto-enter loop breaks the shell

```bash
bash --norc
```

Then remove the auto-enter snippet from `~/.bashrc`.
