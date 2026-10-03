//! Wayland backend: wlr-data-control watch + copy via wl-clipboard-rs.
//!
//! Notes:
//! - No owner identity in the protocol, so self-copies can't be filtered by
//!   origin — we set a suppress flag around our own `copy` calls instead
//!   (storage dedup is the safety net).
//! - `SimulatePaste` shells out to `wtype` (virtual-keyboard protocol,
//!   wlroots compositors); elsewhere we log "press Ctrl+V".
//! - `RefocusWindow` is meaningless on Wayland — focus is the compositor's
//!   business.
//! - Compositors without wlr-data-control: `Watcher::new` fails with a
//!   clear error; the README documents GNOME quirks and the
//!   `wl-paste --watch` fallback plan.

use super::traits::{BackendError, ClipboardBackend};
use super::WatchSet;
use crate::types::{BackendCommand, BackendEvent, ClipKind, ClipPayload, NewClip};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};
use wl_clipboard_rs::paste::Seat;
use wl_clipboard_rs::watch::{ClipboardEvent, ClipboardType, Watcher};

const PASSWORD_HINT_MIME: &str = "x-kde.passwordManagerHint";

pub struct WaylandBackend;

impl WaylandBackend {
    pub fn new() -> Result<Self, BackendError> {
        Ok(Self)
    }

    /// Offered MIME list → (mime to request, kind). Password hint wins.
    fn pick_mime(mimes: &[String]) -> Option<(String, ClipKind)> {
        if mimes.iter().any(|m| m == PASSWORD_HINT_MIME) {
            tracing::info!("skipping clip: password-manager hint present");
            return None;
        }
        for (mime, kind) in [
            ("image/png", ClipKind::Image),
            ("text/html", ClipKind::Html),
            ("text/plain;charset=utf-8", ClipKind::Text),
            ("UTF8_STRING", ClipKind::Text),
            ("text/plain", ClipKind::Text),
            ("STRING", ClipKind::Text),
        ] {
            if mimes.iter().any(|m| m == mime) {
                return Some((mime.to_string(), kind));
            }
        }
        None
    }

    fn copy_payload(payload: &ClipPayload, plain_text_only: bool) -> Result<(), BackendError> {
        let mut sources: Vec<MimeSource> = Vec::new();
        if let Some(text) = &payload.text {
            sources.push(MimeSource {
                source: Source::Bytes(text.clone().into_bytes().into_boxed_slice()),
                mime_type: MimeType::Text,
            });
        }
        if !plain_text_only {
            if let Some(html) = &payload.html {
                sources.push(MimeSource {
                    source: Source::Bytes(html.clone().into_bytes().into_boxed_slice()),
                    mime_type: MimeType::Specific("text/html".into()),
                });
            }
            if let Some(png) = &payload.png {
                sources.push(MimeSource {
                    source: Source::Bytes(png.clone().into_boxed_slice()),
                    mime_type: MimeType::Specific("image/png".into()),
                });
            }
        }
        if sources.is_empty() {
            return Err(BackendError::Other(anyhow::anyhow!("empty payload")));
        }
        Options::new()
            .copy_multi(sources)
            .map_err(|e| BackendError::Other(anyhow::anyhow!("wayland copy: {e}")))
    }

    /// Paste via wtype's virtual keyboard (wlroots). Elsewhere: advise.
    fn simulate_paste(chord: &str) -> Result<(), BackendError> {
        let mut args: Vec<String> = Vec::new();
        let mut mods: Vec<&str> = Vec::new();
        for part in chord.split('+') {
            match part.trim().to_lowercase().as_str() {
                "ctrl" | "control" => mods.push("ctrl"),
                "shift" => mods.push("shift"),
                "alt" => mods.push("alt"),
                "super" | "meta" | "win" => mods.push("logo"),
                key => {
                    for m in &mods {
                        args.push("-M".into());
                        args.push((*m).into());
                    }
                    let wtype_key = match key {
                        "insert" | "ins" => "Insert",
                        "enter" | "return" => "Return",
                        k if k.len() == 1 => {
                            args.push("-k".into());
                            args.push(k.to_uppercase());
                            for m in &mods {
                                args.push("-m".into());
                                args.push((*m).into());
                            }
                            return run_wtype(&args);
                        }
                        other => other,
                    };
                    args.push("-k".into());
                    args.push(wtype_key.to_string());
                    for m in &mods {
                        args.push("-m".into());
                        args.push((*m).into());
                    }
                    return run_wtype(&args);
                }
            }
        }
        Err(BackendError::Other(anyhow::anyhow!(
            "unparseable paste chord: {chord}"
        )))
    }
}

fn run_wtype(args: &[String]) -> Result<(), BackendError> {
    match std::process::Command::new("wtype").args(args).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(BackendError::Other(anyhow::anyhow!(
            "wtype exited {s} — press Ctrl+V manually"
        ))),
        Err(_) => Err(BackendError::Other(anyhow::anyhow!(
            "wtype not installed — press Ctrl+V manually"
        ))),
    }
}

impl ClipboardBackend for WaylandBackend {
    fn name(&self) -> &'static str {
        "wayland"
    }

    fn run(
        &mut self,
        watch: WatchSet,
        events: async_channel::Sender<BackendEvent>,
        commands: async_channel::Receiver<BackendCommand>,
    ) -> Result<(), BackendError> {
        let clipboard_type = if watch.primary {
            ClipboardType::Both
        } else {
            ClipboardType::Regular
        };
        let mut watcher = Watcher::new(clipboard_type, Seat::Unspecified)
            .map_err(|e| BackendError::MissingCapability(format!("wlr-data-control: {e}")))?;

        let suppress = Arc::new(AtomicBool::new(false));

        // Command thread: copies + shutdown (the watcher blocks this thread).
        {
            let suppress = suppress.clone();
            let cancel = watcher.cancel_handle();
            std::thread::Builder::new()
                .name("wayland-cmds".into())
                .spawn(move || {
                    while let Ok(cmd) = commands.recv_blocking() {
                        match cmd {
                            BackendCommand::Shutdown => {
                                cancel.cancel();
                                return;
                            }
                            BackendCommand::SetClipboard {
                                payload,
                                plain_text_only,
                            } => {
                                suppress.store(true, Ordering::SeqCst);
                                if let Err(e) = Self::copy_payload(&payload, plain_text_only) {
                                    tracing::error!("wayland copy: {e}");
                                    suppress.store(false, Ordering::SeqCst);
                                }
                            }
                            BackendCommand::SimulatePaste { chord } => {
                                if let Err(e) = Self::simulate_paste(&chord) {
                                    tracing::warn!("{e}");
                                }
                            }
                            BackendCommand::RefocusWindow(_) => {} // N/A on Wayland
                            BackendCommand::SetPaused(_) => {}     // handled below via flag
                        }
                    }
                })
                .map_err(|e| BackendError::Other(e.into()))?;
        }

        let _ = events.send_blocking(BackendEvent::Ready("wayland"));

        loop {
            match watcher.next_event() {
                Ok(Some(ClipboardEvent::Changed {
                    mime_types, mut offer, ..
                })) => {
                    if suppress.swap(false, Ordering::SeqCst) {
                        continue; // our own copy
                    }
                    let Some((mime, kind)) = Self::pick_mime(&mime_types) else {
                        continue;
                    };
                    let mut buf = Vec::new();
                    match offer.receive(&mime) {
                        Ok(mut pipe) => {
                            if let Err(e) = pipe.read_to_end(&mut buf) {
                                tracing::debug!("read failed: {e}");
                                continue;
                            }
                        }
                        Err(e) => {
                            tracing::debug!("receive failed: {e}");
                            continue;
                        }
                    }
                    let clip = match kind {
                        ClipKind::Image => NewClip {
                            kind,
                            text_content: None,
                            html_content: None,
                            byte_size: buf.len() as u64,
                            image_png: Some(buf),
                            source_app: None,
                        },
                        ClipKind::Html => {
                            let html = String::from_utf8_lossy(&buf).into_owned();
                            NewClip {
                                kind,
                                text_content: Some(html.clone()),
                                html_content: Some(html),
                                image_png: None,
                                byte_size: buf.len() as u64,
                                source_app: None,
                            }
                        }
                        ClipKind::Text => {
                            let text = String::from_utf8_lossy(&buf).into_owned();
                            NewClip {
                                kind,
                                text_content: Some(text),
                                html_content: None,
                                image_png: None,
                                byte_size: buf.len() as u64,
                                source_app: None,
                            }
                        }
                    };
                    let _ = events.send_blocking(BackendEvent::NewClip(clip));
                }
                Ok(Some(ClipboardEvent::Cleared { .. })) | Ok(None) => {}
                Err(e) => {
                    // Cancelled (Shutdown) or fatal protocol error.
                    let _ = events.send_blocking(BackendEvent::Error(e.to_string()));
                    return Ok(());
                }
            }
        }
    }
}
