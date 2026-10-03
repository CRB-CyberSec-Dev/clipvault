//! Single-instance IPC over a Unix socket at `$XDG_RUNTIME_DIR/clipvault.sock`.
//!
//! The daemon binds the socket; CLI invocations (`clipvault toggle`, …)
//! connect, send one JSON line, and (for `status`) read one JSON line back.
//! A refused connection means a stale socket → remove and become the daemon.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum IpcCommand {
    Toggle,
    Show,
    Hide,
    Settings,
    Select { id: i64, plain: bool },
    Clear { all: bool },
    TogglePause,
    Status,
    Quit,
}

pub fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    runtime.join("clipvault.sock")
}

/// Try to deliver a command to the running daemon.
/// Returns Ok(false) if no daemon is listening.
pub fn try_send(cmd: &IpcCommand) -> Result<bool> {
    let path = socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused
            || e.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(false)
        }
        Err(e) => return Err(e).with_context(|| format!("connect {path:?}")),
    };
    let mut line = serde_json::to_string(cmd)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    if matches!(cmd, IpcCommand::Status) {
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply)?;
        print!("{reply}");
    }
    Ok(true)
}

/// Bind the socket as the daemon. If the path is taken, probe it first:
/// a connectable socket means a live daemon — refuse to start (single
/// instance). Only a truly stale socket file gets replaced.
pub fn listen() -> Result<UnixListener> {
    let path = socket_path();
    match UnixListener::bind(&path) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(&path).is_ok() {
                anyhow::bail!("another clipvault daemon is already running ({path:?})");
            }
            std::fs::remove_file(&path)?;
            UnixListener::bind(&path).context("bind after stale cleanup")
        }
        Err(e) => Err(e).with_context(|| format!("bind {path:?}")),
    }
}

/// Read one command line from a freshly accepted stream.
pub fn read_command(stream: UnixStream) -> Option<IpcCommand> {
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim()).ok()
}
