//! System tray via StatusNotifierItem (ksni). Works with xfce4-panel's
//! Status Tray / Indicator plugin, KDE, and GNOME-with-extension.
//!
//! Menu actions travel into the daemon through the same IpcCommand
//! channel the CLI uses — one dispatch path for everything.

use crate::ipc::IpcCommand;
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Category, ToolTip, Tray};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct CvTray {
    tx: async_channel::Sender<IpcCommand>,
    paused: Arc<AtomicBool>,
}

impl Tray for CvTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "clipvault".into()
    }

    fn title(&self) -> String {
        "ClipVault".into()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn icon_name(&self) -> String {
        // Our own icon (packaged to hicolor) — theme-provided names like
        // "edit-paste-symbolic" are missing from many themes (e.g.
        // Flat-Remix) and render as a blank tray entry.
        "clipvault".into()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "ClipVault".into(),
            description: "Clipboard history — Super+V".into(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let paused = self.paused.load(Ordering::Relaxed);
        vec![
            StandardItem {
                label: "Show Clipboard History".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send_blocking(IpcCommand::Toggle);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: if paused { "Resume history" } else { "Pause history" }.into(),
                activate: Box::new(|t: &mut Self| {
                    // Daemon flips config + backend; we mirror the label state.
                    let _ = t.paused.fetch_not(Ordering::Relaxed);
                    let _ = t.tx.send_blocking(IpcCommand::TogglePause);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Settings…".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send_blocking(IpcCommand::Settings);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send_blocking(IpcCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Spawn the tray on a dedicated thread. ksni's blocking API creates its
/// own runtime internally; we just keep the handle parked forever.
/// Failure is logged, never fatal.
pub fn spawn(tx: async_channel::Sender<IpcCommand>) {
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            let tray = CvTray {
                tx,
                paused: Arc::new(AtomicBool::new(false)),
            };
            match ksni::blocking::TrayMethods::spawn(tray) {
                Ok(handle) => {
                    tracing::info!("tray icon live");
                    std::mem::forget(handle); // daemon lifetime
                }
                Err(e) => tracing::warn!("tray unavailable: {e}"),
            }
        })
        .ok();
}
