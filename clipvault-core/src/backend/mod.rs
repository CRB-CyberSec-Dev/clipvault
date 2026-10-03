//! Clipboard backend abstraction and per-display-server implementations.

pub mod traits;
#[cfg(feature = "wayland")]
pub mod wayland;
#[cfg(feature = "x11")]
pub mod x11;

pub use traits::ClipboardBackend;

use crate::types::Selection;

/// Which backend implementation to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Auto,
    X11,
    Wayland,
}

impl BackendKind {
    /// Resolve `Auto` against the environment.
    pub fn resolve(self) -> Resolved {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
            && std::env::var_os("XDG_SESSION_TYPE").is_none_or(|v| v != "x11");
        let x11 = std::env::var_os("DISPLAY").is_some();
        match self {
            Self::Wayland if wayland => Resolved::Wayland,
            Self::X11 if x11 => Resolved::X11,
            Self::Auto if wayland => Resolved::Wayland,
            Self::Auto | Self::X11 | Self::Wayland if x11 => Resolved::X11,
            _ => Resolved::Unsupported,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    X11,
    Wayland,
    Unsupported,
}

impl Resolved {
    pub fn name(self) -> &'static str {
        match self {
            Self::X11 => "x11",
            Self::Wayland => "wayland",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Selections a backend should watch, derived from config.
#[derive(Debug, Clone, Copy)]
pub struct WatchSet {
    pub clipboard: bool,
    pub primary: bool,
}

impl WatchSet {
    pub fn selections(self) -> impl Iterator<Item = Selection> {
        [
            self.clipboard.then_some(Selection::Clipboard),
            self.primary.then_some(Selection::Primary),
        ]
        .into_iter()
        .flatten()
    }
}
