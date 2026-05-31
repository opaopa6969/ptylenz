///! PTY Proxy — sits between the real terminal and a child shell.
///!
///! Architecture:
///!   Terminal (stdin/stdout) <-> ptylenz (PTY master) <-> bash (PTY slave)
///!
///! All bytes flowing in both directions pass through ptylenz,
///! allowing us to detect block boundaries (via OSC markers)
///! and index the output.

use anyhow::{Context, Result};
use nix::pty::{openpty, OpenptyResult, Winsize};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{self, ForkResult, Pid};
use std::ffi::CString;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use tempfile::NamedTempFile;

use crate::block::{BlockEngine, OscEvent};

/// Shell integration script injected into bash to emit OSC 133 markers.
/// Uses PS0 + PS1 + PROMPT_COMMAND (iTerm2-style) to avoid DEBUG-trap
/// nesting issues when functions call other commands.
///
///   133;A — prompt start (printed via PS1)
///   133;C — command execution start (printed via PS0)
///   133;D;N — command finished with exit code N (via PROMPT_COMMAND)
///   133;E;cmd — command text (via PROMPT_COMMAND, from last history entry)
const BASH_INTEGRATION: &str = r#"
# ptylenz shell integration — do not edit
__ptylenz_precmd() {
    local __ptylenz_ec=$?
    printf '\e]133;D;%d\a' "$__ptylenz_ec"
    local __ptylenz_last
    __ptylenz_last=$(HISTTIMEFORMAT='' history 1 2>/dev/null | sed -E 's/^[[:space:]]*[0-9]+[[:space:]]*//')
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
"#;

/// The PTY proxy: owns the master side of a PTY pair and
/// the child shell process.
pub struct PtyProxy {
    master: OwnedFd,
    child_pid: Pid,
    block_engine: BlockEngine,
}

impl PtyProxy {
    /// Spawn a child shell inside a new PTY, with shell integration.
    pub fn spawn(shell_path: &str) -> Result<Self> {
        // Detect whether the requested shell is bash so we can pass bash-specific
        // flags.  Only the final path component is checked so that paths like
        // /usr/local/bin/bash, /opt/homebrew/bin/bash, etc. all match.
        let is_bash = std::path::Path::new(shell_path)
            .file_name()
            .and_then(|n| n.to_str())
            .map_or(false, |n| n == "bash");

        // Write shell integration to a temp rcfile that also sources user's ~/.bashrc.
        // Using --rcfile with a wrapper preserves the user's bash environment while
        // letting us inject the OSC 133 markers we need for block detection.
        // Only done for bash; other shells don't support --rcfile.
        let rcfile = if is_bash { Some(write_bash_rcfile()?) } else { None };

        // Create PTY pair with the real terminal's size baked in. Without this
        // the kernel hands us 0×0 (or 80×24 on some platforms) and any child
        // that reads LINES/COLUMNS before we SIGWINCH — ncurses apps like mc,
        // which do exactly this during setupterm — draws at the wrong width
        // and the real terminal wraps every line into a diagonal staircase.
        let initial_ws = query_winsize();
        let OpenptyResult { master, slave } = openpty(initial_ws.as_ref(), None)
            .context("Failed to open PTY pair")?;

        let block_engine = BlockEngine::new();

        match unsafe { unistd::fork() }.context("fork failed")? {
            ForkResult::Child => {
                drop(master);
                unistd::setsid().ok();

                unsafe {
                    // libc::TIOCSCTTY is c_ulong on Linux but c_uint on macOS;
                    // cast to the ioctl request type to compile on both.
                    libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY as _, 0);
                }

                unistd::dup2(slave.as_raw_fd(), 0).ok();
                unistd::dup2(slave.as_raw_fd(), 1).ok();
                unistd::dup2(slave.as_raw_fd(), 2).ok();
                drop(slave);

                std::env::set_var("PTYLENZ", "1");
                std::env::set_var("PTYLENZ_VERSION", env!("CARGO_PKG_VERSION"));

                // CString::new returns Err if the input contains a NUL byte.
                // Rather than unwrap() — which would panic on adversarial input
                // or unusual environment values — propagate the error and exit
                // with a diagnostic status code so the parent sees a clean failure
                // instead of an unexpected panic in the child.
                let shell = match CString::new(shell_path) {
                    Ok(s) => s,
                    Err(_) => {
                        eprintln!("ptylenz: shell path contains NUL byte");
                        std::process::exit(126);
                    }
                };

                // Build the argument list depending on the shell.  bash accepts
                // --rcfile to load our integration script; other shells (zsh,
                // fish, sh, …) do not understand that flag and would fail to
                // start.
                if is_bash {
                    if let Some(ref tmp) = rcfile {
                        let rcfile_cstr = match CString::new(tmp.path().as_os_str().as_encoded_bytes()) {
                            Ok(s) => s,
                            Err(_) => {
                                eprintln!("ptylenz: rcfile path contains NUL byte");
                                std::process::exit(126);
                            }
                        };
                        let rcfile_flag = CString::new("--rcfile").expect("static string");
                        let interactive_flag = CString::new("-i").expect("static string");
                        let args = [shell.clone(), rcfile_flag, rcfile_cstr, interactive_flag];
                        // Keep `tmp` alive until after exec so the file exists
                        // when bash opens it.  execvp replaces the process image,
                        // so the NamedTempFile destructor never runs in this
                        // branch — that is intentional: bash will read the file
                        // and the OS reclaims it when all handles are closed.
                        let _ = &tmp;
                        unistd::execvp(&shell, &args).ok();
                    } else {
                        unistd::execvp(&shell, &[shell.clone()]).ok();
                    }
                } else {
                    // For non-bash shells just exec the shell directly without
                    // any bash-specific flags.
                    unistd::execvp(&shell, &[shell.clone()]).ok();
                }
                std::process::exit(127);
            }
            ForkResult::Parent { child } => {
                drop(slave);
                Ok(PtyProxy {
                    master,
                    child_pid: child,
                    block_engine,
                })
            }
        }
    }

    /// Get the master FD for poll/select.
    pub fn master_fd(&self) -> i32 {
        self.master.as_raw_fd()
    }

    /// Read from the PTY master (child's output).
    /// Returns (clean_bytes_without_osc_133, detected_events).
    /// The clean bytes are safe to forward directly to the user's terminal.
    pub fn read_output(&mut self, buf: &mut [u8]) -> Result<(Vec<u8>, Vec<OscEvent>)> {
        let n = unistd::read(self.master.as_raw_fd(), buf)
            .context("read from PTY master")?;
        if n == 0 {
            return Ok((Vec::new(), Vec::new()));
        }

        let (clean, events) = self.block_engine.feed_output(&buf[..n]);
        Ok((clean, events))
    }

    /// Write to the PTY master (user's input → child's stdin).
    pub fn write_input(&self, data: &[u8]) -> Result<usize> {
        let n = unistd::write(&self.master, data)
            .context("write to PTY master")?;
        Ok(n)
    }

    /// Send a signal to the child shell.
    pub fn signal_child(&self, sig: Signal) -> Result<()> {
        signal::kill(self.child_pid, sig)
            .context("signal child")?;
        Ok(())
    }

    /// Check if child is still running.
    pub fn child_alive(&self) -> bool {
        match waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => true,
            _ => false,
        }
    }

    /// Resize the PTY (forward terminal resize to child).
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let winsize = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &winsize);
        }
        // Keep the shadow vt100 parser's grid matched to the real terminal.
        self.block_engine.resize(rows, cols);
        // Notify child of resize
        self.signal_child(Signal::SIGWINCH)?;
        Ok(())
    }

    /// Access the block engine for querying blocks.
    pub fn blocks(&self) -> &BlockEngine {
        &self.block_engine
    }

    pub fn blocks_mut(&mut self) -> &mut BlockEngine {
        &mut self.block_engine
    }
}

impl Drop for PtyProxy {
    fn drop(&mut self) {
        self.signal_child(Signal::SIGHUP).ok();
    }
}

/// Read the current terminal's winsize from STDOUT so the PTY can be opened
/// at the correct dimensions before fork. Returns None if STDOUT isn't a tty
/// or the ioctl fails — caller falls back to openpty's kernel default.
fn query_winsize() -> Option<Winsize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ as _, &mut ws) };
    if rc == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        Some(ws)
    } else {
        None
    }
}

/// Write a wrapper rcfile for bash that sources the user's ~/.bashrc then
/// installs our OSC 133 emitters.
///
/// Returns a `NamedTempFile` so that:
/// - the kernel assigns a unique, unpredictable path (no PID-based guessing),
/// - the file is created with mode 0600 (no world-readable leak of the script),
/// - the caller can keep the `NamedTempFile` alive as long as needed; when it
///   is dropped the file is deleted automatically.
fn write_bash_rcfile() -> Result<NamedTempFile> {
    use std::os::unix::fs::PermissionsExt;

    let mut tmp = tempfile::Builder::new()
        .prefix("ptylenz-rc-")
        .suffix(".sh")
        .tempfile()
        .context("create temp rcfile")?;

    // Tighten permissions to 0600 — the file holds no secrets but is
    // world-readable by default on some systems, and there is no reason
    // for other users to inspect the generated integration script.
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))
        .context("set rcfile permissions")?;

    let contents = format!(
        "# ptylenz wrapper rcfile — auto-generated, safe to delete\n\
         [ -f \"$HOME/.bashrc\" ] && . \"$HOME/.bashrc\"\n\
         {}",
        BASH_INTEGRATION
    );

    tmp.write_all(contents.as_bytes())
        .context("write rcfile contents")?;
    Ok(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

    /// End-to-end: spawn bash under the proxy, drive a few commands, confirm
    /// the block engine picks them up with exit codes and command text.
    #[test]
    fn spawn_bash_and_detect_blocks() {
        let mut proxy = match PtyProxy::spawn("/bin/bash") {
            Ok(p) => p,
            Err(_) => return,
        };

        // Drain output in a loop until we see the first prompt marker,
        // then send commands and drain again.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 8192];
        unsafe {
            let flags = libc::fcntl(proxy.master_fd(), libc::F_GETFL);
            libc::fcntl(proxy.master_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        // Give bash a moment to print its first prompt
        thread::sleep(Duration::from_millis(400));
        let _ = proxy.read_output(&mut buf);

        proxy.write_input(b"echo hello-ptylenz\n").unwrap();
        proxy.write_input(b"false\n").unwrap();
        proxy.write_input(b"exit\n").unwrap();

        while Instant::now() < deadline {
            match proxy.read_output(&mut buf) {
                Ok((clean, _)) if clean.is_empty() && !proxy.child_alive() => break,
                Ok(_) => {}
                Err(_) => {}
            }
            if !proxy.child_alive() {
                let _ = proxy.read_output(&mut buf);
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        let blocks = proxy.blocks().completed_blocks();
        assert!(
            !blocks.is_empty(),
            "expected at least one block after driving bash"
        );
        // We should have seen `echo hello-ptylenz` somewhere
        let any_echo = blocks
            .iter()
            .any(|b| b.command.as_deref().map_or(false, |c| c.contains("echo hello-ptylenz")));
        assert!(any_echo, "expected to capture the echo command; blocks={:?}", blocks.iter().map(|b| &b.command).collect::<Vec<_>>());
    }

    /// End-to-end: run a command that enters the alternate screen (the mc /
    /// claude lineage), writes content, and exits. Verify that a block is
    /// produced and that rendered_text carries the vt100 snapshot of the
    /// alt-screen frame — the path that list/detail rendering depends on for
    /// TUI apps.
    #[test]
    fn alt_screen_command_produces_rendered_text() {
        let mut proxy = match PtyProxy::spawn("/bin/bash") {
            Ok(p) => p,
            Err(_) => return,
        };

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 8192];
        unsafe {
            let flags = libc::fcntl(proxy.master_fd(), libc::F_GETFL);
            libc::fcntl(proxy.master_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        thread::sleep(Duration::from_millis(400));
        let _ = proxy.read_output(&mut buf);

        // Start an alt-screen session, idle in it, then leave. The sleep
        // guarantees that the post-entry output arrives in a separate read
        // from the alt-screen-leave, so vt100's shadow grid sees
        // alternate_screen() == true at least once during feed_output() and
        // last_alt_snapshot actually captures a frame. This matches the real
        // mc/claude lineage where the TUI lives in alt-screen across many
        // reads.
        proxy
            .write_input(
                b"printf '\\e[?1049h\\e[2J\\e[1;1HTUI-MARKER-XYZZY hello'; sleep 0.4; printf '\\e[?1049l'; echo done\n",
            )
            .unwrap();

        // Drain across the sleep so feed_output() sees alt-screen bytes,
        // pauses (no data), then later sees the exit-alt bytes.
        let mid_deadline = Instant::now() + Duration::from_millis(900);
        while Instant::now() < mid_deadline {
            let _ = proxy.read_output(&mut buf);
            thread::sleep(Duration::from_millis(50));
        }

        proxy.write_input(b"exit\n").unwrap();

        while Instant::now() < deadline {
            match proxy.read_output(&mut buf) {
                Ok((clean, _)) if clean.is_empty() && !proxy.child_alive() => break,
                Ok(_) => {}
                Err(_) => {}
            }
            if !proxy.child_alive() {
                let _ = proxy.read_output(&mut buf);
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        let blocks = proxy.blocks().completed_blocks();
        let alt_block = blocks.iter().find(|b| {
            b.command
                .as_deref()
                .map_or(false, |c| c.contains("TUI-MARKER"))
        });
        assert!(
            alt_block.is_some(),
            "expected an alt-screen block; blocks={:?}",
            blocks.iter().map(|b| &b.command).collect::<Vec<_>>()
        );
        let rendered = alt_block.unwrap().rendered_text.as_deref();
        assert!(
            rendered.map_or(false, |s| s.contains("TUI-MARKER-XYZZY")),
            "expected rendered_text to contain the alt-screen token; rendered={:?}",
            rendered
        );
    }

    /// Regression: a plain `ls`-style command (no alt-screen) must produce
    /// a block whose output_text contains the visible output — i.e. the
    /// non-alt-screen path hasn't been broken by recent changes to the vt100
    /// mirroring / line_count caching.
    #[test]
    fn plain_command_captures_visible_output() {
        let mut proxy = match PtyProxy::spawn("/bin/bash") {
            Ok(p) => p,
            Err(_) => return,
        };

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 8192];
        unsafe {
            let flags = libc::fcntl(proxy.master_fd(), libc::F_GETFL);
            libc::fcntl(proxy.master_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        thread::sleep(Duration::from_millis(400));
        let _ = proxy.read_output(&mut buf);

        proxy
            .write_input(b"echo LINE-ONE; echo LINE-TWO; echo LINE-THREE\n")
            .unwrap();
        proxy.write_input(b"exit\n").unwrap();

        while Instant::now() < deadline {
            match proxy.read_output(&mut buf) {
                Ok((clean, _)) if clean.is_empty() && !proxy.child_alive() => break,
                Ok(_) => {}
                Err(_) => {}
            }
            if !proxy.child_alive() {
                let _ = proxy.read_output(&mut buf);
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        let blocks = proxy.blocks().completed_blocks();
        let echo_block = blocks
            .iter()
            .find(|b| b.command.as_deref().map_or(false, |c| c.contains("LINE-ONE")));
        assert!(
            echo_block.is_some(),
            "expected a non-alt-screen block for the echo command; blocks={:?}",
            blocks.iter().map(|b| &b.command).collect::<Vec<_>>()
        );
        let b = echo_block.unwrap();
        let text = b.output_text();
        assert!(text.contains("LINE-ONE"), "missing LINE-ONE; text={:?}", text);
        assert!(text.contains("LINE-TWO"), "missing LINE-TWO; text={:?}", text);
        assert!(text.contains("LINE-THREE"), "missing LINE-THREE; text={:?}", text);
        // cached_line_count should match the number of newlines we actually saw.
        let actual_newlines = b.output.iter().filter(|&&c| c == b'\n').count();
        assert_eq!(
            b.cached_line_count, actual_newlines,
            "cached_line_count drifted from actual newline count"
        );
    }
}
