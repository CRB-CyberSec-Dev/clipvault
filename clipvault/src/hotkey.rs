//! X11 global hotkey: grab Super+V on the root window from a dedicated
//! thread/connection and forward presses to the daemon.
//!
//! Lock-modifier gotcha: grabbing only Mod4+V silently stops working when
//! NumLock/CapsLock is on. We grab all four variants (±MOD2, ±LOCK).
//!
//! Wayland has no global-hotkey API by design — there the compositor binds
//! a shortcut to `clipvault toggle`, which arrives via IPC instead.

use anyhow::{Context, Result};
use clipvault_core::backend::x11::keycode_for_keysym;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, GrabMode, ModMask};
use x11rb::protocol::Event;

/// Parse "Super+V" → (modifier mask, keysym).
pub fn parse_hotkey(spec: &str) -> Option<(u16, u32)> {
    let mut mask = ModMask::from(0u16);
    let mut key: Option<u32> = None;
    for part in spec.split('+') {
        match part.trim().to_lowercase().as_str() {
            "ctrl" | "control" => mask |= ModMask::CONTROL,
            "shift" => mask |= ModMask::SHIFT,
            "alt" | "mod1" => mask |= ModMask::M1,
            "super" | "win" | "meta" | "mod4" => mask |= ModMask::M4,
            s if s.len() == 1 => key = Some(s.chars().next()? as u32),
            _ => return None,
        }
    }
    key.map(|k| (u16::from(mask), k))
}

/// Spawn the grab thread. Sends `()` on `tx` each time the hotkey fires.
/// Returns Err (with a human-readable message) if the grab is refused
/// (BadAccess = something else owns the binding).
pub fn spawn(spec: &str, tx: async_channel::Sender<()>) -> Result<()> {
    let (mods, keysym) = parse_hotkey(spec)
        .with_context(|| format!("cannot parse hotkey {spec:?}"))?;

    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            if let Err(e) = grab_loop(mods, keysym, tx) {
                tracing::error!("hotkey thread: {e}");
            }
        })?;
    Ok(())
}

fn grab_loop(mods: u16, keysym: u32, tx: async_channel::Sender<()>) -> Result<()> {
    let (conn, screen_num) = x11rb::connect(None).context("connect X11")?;
    let root = conn.setup().roots[screen_num].root;
    let keycode =
        keycode_for_keysym(&conn, keysym).context("no keycode for hotkey keysym")?;

    let base = ModMask::from(mods);
    let variants = [
        base,
        base | ModMask::M2,                 // NumLock
        base | ModMask::LOCK,               // CapsLock
        base | ModMask::M2 | ModMask::LOCK, // both
    ];
    for m in variants {
        let cookie = conn
            .grab_key(false, root, m, keycode, GrabMode::ASYNC, GrabMode::ASYNC)
            .context("grab_key request failed")?;
        cookie
            .check()
            .context("hotkey grab refused (already bound elsewhere?)")?;
    }
    conn.flush()?;
    tracing::info!("hotkey grabbed (mods={mods:#x}, keycode={keycode})");

    loop {
        match conn.wait_for_event() {
            Ok(Event::KeyPress(ev)) if ev.detail == keycode => {
                let _ = tx.send_blocking(());
            }
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
    }
}
