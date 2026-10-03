//! The backend contract: watch the system clipboard, take ownership on demand.

use crate::backend::WatchSet;
use crate::types::{BackendCommand, BackendEvent};

/// One clipboard backend (X11 or Wayland). Runs on its own blocking thread;
/// events go out on `events`, commands come in on `commands`.
pub trait ClipboardBackend: Send {
    /// Human-readable name for logs/status ("x11", "wayland").
    fn name(&self) -> &'static str;

    /// Start watching and serving. Blocks until a `Shutdown` command or a
    /// fatal error. Implementations must:
    /// - emit `BackendEvent::Ready` once the watch is live,
    /// - never panic on malformed clipboard data (owners misbehave),
    /// - suppress self-notifications after `SetClipboard`.
    fn run(
        &mut self,
        watch: WatchSet,
        events: async_channel::Sender<BackendEvent>,
        commands: async_channel::Receiver<BackendCommand>,
    ) -> Result<(), BackendError>;
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("display server not reachable: {0}")]
    Connect(String),
    #[error("required extension/protocol missing: {0}")]
    MissingCapability(String),
    #[error("backend failure: {0}")]
    Other(#[from] anyhow::Error),
}
