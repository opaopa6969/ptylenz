///! PTY Proxy — sits between the real terminal and a child shell.
///!
///! Architecture:
///!   Terminal (stdin/stdout) <-> ptylenz (PTY master) <-> bash (PTY slave)
///!
///! All bytes flowing in both directions pass through ptylenz,
///! allowing us to detect block boundaries (via OSC markers)
///! and index the output.

use anyhow::{Context, Result};
use nix::pty::{openpty, OpenptyResult};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{self, ForkResult, Pid};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::block::{BlockEngine, OscEvent};

/// Shell integration script injected into bash to emit OSC markers.
/// These markers let ptylenz know where each command starts/ends.
const BASH_INTEGRATION: &str = r#"
# ptylenz shell integration — do not edit
__ptylenz_preexec() {
    # OSC 133;C = command execution start
    printf '\e]133;C\a'
    # OSC 133;E = command text (for block title)
    printf '\e]133;E;%s\a' "$1"
}
__ptylenz_precmd() {
    local exit_code=$?
    # OSC 133;D = command finished, with exit code
    printf '\e]133;D;%d\a' "$exit_code"
    # OSC 133;A = new prompt starting
    printf '\e]133;A\a'
}
trap '__ptylenz_preexec "$BASH_COMMAND"' DEBUG
PROMPT_COMMAND='__ptylenz_precmd'
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
        // Create PTY pair
        let OpenptyResult { master, slave } = openpty(None, None)
            .context("Failed to open PTY pair")?;

        let block_engine = BlockEngine::new();

        match unsafe { unistd::fork() }.context("fork failed")? {
            ForkResult::Child => {
                // Child: become session leader, attach to slave PTY
                drop(master);
                unistd::setsid().ok();

                // Make slave PTY the controlling terminal
                unsafe {
                    libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY, 0);
                }

                // Redirect stdin/stdout/stderr to slave PTY
                unistd::dup2(slave.as_raw_fd(), 0).ok();
                unistd::dup2(slave.as_raw_fd(), 1).ok();
                unistd::dup2(slave.as_raw_fd(), 2).ok();
                drop(slave);

                // Set environment hint
                std::env::set_var("PTYLENZ", "1");
                std::env::set_var("PTYLENZ_VERSION", env!("CARGO_PKG_VERSION"));

                // Exec the shell with integration
                // We use --rcfile to inject our integration alongside user's bashrc
                let shell = CString::new(shell_path).unwrap();
                let init_cmd = format!(
                    "source ~/.bashrc 2>/dev/null; {}",
                    BASH_INTEGRATION
                );
                let args = [
                    shell.clone(),
                    CString::new("--rcfile").unwrap(),
                    CString::new("/dev/null").unwrap(),
                    CString::new("-i").unwrap(),
                ];

                // Alternative: pass integration via BASH_ENV or --init-file
                std::env::set_var("PTYLENZ_INIT", BASH_INTEGRATION);

                unistd::execvp(&shell, &args).ok();
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
    /// Returns the raw bytes and any detected OSC events.
    pub fn read_output(&mut self, buf: &mut [u8]) -> Result<(usize, Vec<OscEvent>)> {
        let n = unistd::read(self.master.as_raw_fd(), buf)
            .context("read from PTY master")?;
        if n == 0 {
            return Ok((0, vec![]));
        }

        let events = self.block_engine.feed_output(&buf[..n]);
        Ok((n, events))
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
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let winsize = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &winsize);
        }
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
        // Try graceful shutdown
        self.signal_child(Signal::SIGHUP).ok();
    }
}
