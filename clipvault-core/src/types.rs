//! Shared data model used by backends, storage and UI.

use serde::{Deserialize, Serialize};

/// What kind of payload a clip carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipKind {
    Text,
    Html,
    Image,
}

impl ClipKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Html => "html",
            Self::Image => "image",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Self::Text),
            "html" => Some(Self::Html),
            "image" => Some(Self::Image),
            _ => None,
        }
    }
}

/// One clipboard-history entry, as stored and as shown in the popup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipItem {
    pub id: i64,
    /// blake3-256 of the canonical bytes (dedup key).
    pub hash: [u8; 32],
    pub kind: ClipKind,
    /// Always populated for text/html clips; None for pure images.
    pub text_content: Option<String>,
    /// Only for `ClipKind::Html`.
    pub html_content: Option<String>,
    /// Only for `ClipKind::Image` — path relative to the images dir.
    pub image_path: Option<String>,
    pub byte_size: u64,
    /// Best-effort origin (WM_CLASS / app-id); None when unknown.
    pub source_app: Option<String>,
    pub pinned: bool,
    pub created_at: i64,
    pub last_used_at: i64,
    pub use_count: u64,
}

/// A fresh clip observed by a backend, before storage assigns an id.
#[derive(Debug, Clone)]
pub struct NewClip {
    pub kind: ClipKind,
    pub text_content: Option<String>,
    pub html_content: Option<String>,
    /// PNG bytes for image clips.
    pub image_png: Option<Vec<u8>>,
    pub byte_size: u64,
    pub source_app: Option<String>,
}

impl NewClip {
    /// Canonical bytes used for hashing/dedup.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        match self.kind {
            ClipKind::Image => self.image_png.clone().unwrap_or_default(),
            ClipKind::Html => self
                .html_content
                .clone()
                .unwrap_or_default()
                .into_bytes(),
            ClipKind::Text => self
                .text_content
                .clone()
                .unwrap_or_default()
                .into_bytes(),
        }
    }

    pub fn hash(&self) -> [u8; 32] {
        *blake3::hash(&self.canonical_bytes()).as_bytes()
    }
}

/// Events flowing backend → daemon/UI.
#[derive(Debug, Clone)]
pub enum BackendEvent {
    NewClip(NewClip),
    /// Backend is up and watching (carries a human-readable backend name).
    Ready(&'static str),
    /// Fatal backend failure; daemon should surface this, not swallow it.
    Error(String),
}

/// Payload handed to a backend when we take clipboard ownership.
/// (The daemon resolves a stored ClipItem into this — backends never
/// touch the database.)
#[derive(Debug, Clone, Default)]
pub struct ClipPayload {
    pub text: Option<String>,
    pub html: Option<String>,
    pub png: Option<Vec<u8>>,
}

/// Commands flowing daemon/UI → backend.
#[derive(Debug, Clone)]
pub enum BackendCommand {
    /// Take clipboard ownership serving this payload (plain-text only when
    /// true — Windows' "paste as text").
    SetClipboard {
        payload: Box<ClipPayload>,
        plain_text_only: bool,
    },
    /// Simulate the configured paste chord into the focused window.
    SimulatePaste { chord: String },
    /// Give input focus back to a window (X11 window id) before pasting.
    RefocusWindow(u32),
    /// Pause/resume capture (settings "clipboard history" toggle).
    SetPaused(bool),
    Shutdown,
}

/// Which X selection / Wayland seat clipboard to watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Selection {
    Clipboard,
    Primary,
}
