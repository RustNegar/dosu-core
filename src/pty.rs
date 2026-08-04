//! Spawns the child shell (or command) inside a pseudo-terminal.
//!
//! Uses `portable-pty`, which abstracts the macOS/Linux/BSD PTY ioctl
//! differences, so no platform-specific PTY code lives here.

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

/// A cloneable handle for writing to the child's stdin from contexts
/// other than the main input-forwarding loop -- e.g. terminal-query
/// replies (DSR cursor-position) that originate on the read side, from
/// `grid::feed`. Wrapped in a `Mutex` since the underlying `Write` isn't
/// safely shareable across concurrent writers on its own.
pub type PtyWriterHandle = Arc<Mutex<Box<dyn Write + Send>>>;

pub struct PtySession {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: PtyWriterHandle,
    pub child: Box<dyn Child + Send + Sync>,
}

impl PtySession {
    /// Spawn `command` (or the user's `$SHELL` if `None`) inside a new PTY
    /// sized `cols` x `rows`.
    pub fn spawn(command: Option<&str>, cols: u16, rows: u16) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open pty")?;

        let shell = command
            .map(|s| s.to_string())
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/zsh".to_string());

        let mut cmd = CommandBuilder::new(shell);
        // Lets rc-files (e.g. .zshrc) detect we're already inside Dosu
        // and guard against re-exec'ing into it.
        cmd.env("DOSU", "1");

        // `CommandBuilder` inherits the whole parent environment,
        // including terminal-identity vars like TERM/KITTY_WINDOW_ID/
        // TERM_PROGRAM/WEZTERM_PANE/ITERM_SESSION_ID. Left as-is, the
        // child sees the real outer terminal's identity even though
        // Grid/altscreen.rs only implement a pragmatic ECMA-48/xterm
        // subset (no kitty keyboard/graphics protocols, no capability
        // handshake). This was the root cause of the fzf ctrl-j/ctrl-k
        // bug: seeing KITTY_WINDOW_ID, fzf enabled the kitty keyboard
        // protocol without a real handshake and started expecting
        // `CSI ... u`-encoded keys, but the real terminal (never told to
        // switch) kept sending legacy ctrl-j/ctrl-k bytes, so they got
        // dropped. Arrow keys were unaffected since they're
        // escape-sequence-encoded under both protocols.
        //
        // Fix: advertise a plain terminal identity that matches what
        // Grid actually supports, so children use legacy key encoding.
        cmd.env("TERM", "xterm-256color");
        for key in [
            "KITTY_WINDOW_ID",
            "TERM_PROGRAM",
            "TERM_PROGRAM_VERSION",
            "WEZTERM_PANE",
            "WEZTERM_EXECUTABLE",
            "ITERM_SESSION_ID",
            "VTE_VERSION",
            "KONSOLE_VERSION",
        ] {
            cmd.env_remove(key);
        }

        // Continue from the user's existing shell cwd rather than
        // resetting to $HOME -- cwd is process state and doesn't
        // propagate via the inherited environment on its own.
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }

        // Not a login shell (argv[0] has no leading `-`), like a normal
        // nested/exec'd interactive shell -- avoids re-running
        // login-only setup (.zprofile/.zlogin) a second time, which
        // could duplicate PATH entries or double-run oh-my-zsh hooks.

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn child shell in pty")?;

        let writer = pair
            .master
            .take_writer()
            .context("failed to take pty writer")?;

        Ok(PtySession {
            master: Arc::new(Mutex::new(pair.master)),
            writer: Arc::new(Mutex::new(writer)),
            child,
        })
    }

    pub fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
        self.master
            .lock()
            .unwrap()
            .try_clone_reader()
            .context("failed to clone pty reader")
    }

    pub fn write_input(&self, bytes: &[u8]) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    /// A cloneable handle to the same writer used by `write_input`, for
    /// callers writing to the child from a different task/thread (see
    /// `PtyWriterHandle`).
    pub fn writer_handle(&self) -> PtyWriterHandle {
        self.writer.clone()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to resize pty")
    }
}
