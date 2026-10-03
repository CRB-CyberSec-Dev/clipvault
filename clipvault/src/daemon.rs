//! Daemon: owns the backend thread, storage, IPC listener, and the GTK
//! main loop that hosts the popup and settings windows.

use crate::ipc::{self, IpcCommand};
use crate::ui;
use anyhow::{Context, Result};
use clipvault_core::backend::{ClipboardBackend, WatchSet};
use clipvault_core::config::Config;
use clipvault_core::filter;
use clipvault_core::storage::Storage;
use clipvault_core::types::{BackendCommand, BackendEvent, ClipItem, ClipKind, ClipPayload};
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Shared, lock-free bits the IPC thread can answer with.
struct StatusInfo {
    backend: String,
    started: Instant,
    captured: AtomicU64,
}

pub struct Daemon {
    config: Rc<RefCell<Config>>,
    storage: Rc<RefCell<Storage>>,
    backend_cmd: async_channel::Sender<BackendCommand>,
    status: Arc<StatusInfo>,
    /// Window that had focus before the popup appeared (for paste-back).
    prev_focus: Cell<u32>,
    popup_visible: Cell<bool>,
}

impl Daemon {
    /// Resolve a stored item into backend payload bytes.
    fn payload_for(item: &ClipItem) -> ClipPayload {
        let png = if item.kind == ClipKind::Image {
            item.image_path.as_ref().and_then(|rel| {
                Config::images_dir()
                    .ok()
                    .and_then(|dir| std::fs::read(dir.join(rel)).ok())
            })
        } else {
            None
        };
        ClipPayload {
            text: item.text_content.clone(),
            html: item.html_content.clone(),
            png,
        }
    }

    fn handle_clip(&self, clip: clipvault_core::types::NewClip) {
        let cfg = self.config.borrow().clone();
        if !cfg.history_enabled {
            return;
        }
        match filter::judge(&clip, &[], &cfg) {
            filter::Verdict::Capture => {}
            filter::Verdict::Skip(reason) => {
                tracing::debug!("clip skipped: {reason:?}");
                return;
            }
        }
        let mut st = self.storage.borrow_mut();
        match st.insert(&clip) {
            Ok(outcome) => {
                self.status.captured.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = st.enforce_caps(&cfg) {
                    tracing::warn!("eviction failed: {e}");
                }
                drop(st);
                ui::popup::refresh_if_visible();
                tracing::trace!("stored clip: {outcome:?}");
            }
            Err(e) => tracing::warn!("store failed: {e}"),
        }
    }

    fn handle_ipc(&self, cmd: IpcCommand) {
        match cmd {
            IpcCommand::Toggle => self.toggle_popup(),
            IpcCommand::Show => self.show_popup(),
            IpcCommand::Hide => ui::popup::hide(),
            IpcCommand::Settings => ui::settings::show(self.config.clone()),
            IpcCommand::Select { id, plain } => self.select(id, plain, false),
            IpcCommand::Clear { all } => {
                match self.storage.borrow().clear(all) {
                    Ok(n) => tracing::info!("cleared {n} items (all={all})"),
                    Err(e) => tracing::warn!("clear failed: {e}"),
                }
                ui::popup::refresh_if_visible();
            }
            IpcCommand::Status => {} // answered directly by the IPC thread
            IpcCommand::TogglePause => {
                let new = {
                    let mut cfg = self.config.borrow_mut();
                    cfg.history_enabled = !cfg.history_enabled;
                    let _ = cfg.save();
                    cfg.history_enabled
                };
                let _ = self
                    .backend_cmd
                    .send_blocking(BackendCommand::SetPaused(!new));
                tracing::info!("history capture {}", if new { "resumed" } else { "paused" });
            }
            IpcCommand::Quit => std::process::exit(0),
        }
    }

    /// Put a history item on the clipboard. `with_paste` = popup flow:
    /// also refocus the previous window and inject the paste chord.
    pub fn select(&self, id: i64, plain: bool, with_paste: bool) {
        let item = match self.storage.borrow().get(id) {
            Ok(Some(it)) => it,
            Ok(None) => {
                tracing::warn!("select: no item {id}");
                return;
            }
            Err(e) => {
                tracing::warn!("select: {e}");
                return;
            }
        };
        let payload = Self::payload_for(&item);
        let _ = self.backend_cmd.send_blocking(BackendCommand::SetClipboard {
            payload: Box::new(payload),
            plain_text_only: plain,
        });
        // Bump recency so the used item floats to the top.
        let _ = self.storage.borrow_mut().touch(id);

        if with_paste {
            let cfg = self.config.borrow().clone();
            if cfg.auto_paste {
                let prev = self.prev_focus.get();
                let chord = cfg.paste_chord.clone();
                let delay = cfg.paste_delay_ms;
                let cmd = self.backend_cmd.clone();
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(delay),
                    move || {
                        if prev != 0 {
                            let _ = cmd.send_blocking(BackendCommand::RefocusWindow(prev));
                        }
                        let _ = cmd.send_blocking(BackendCommand::SimulatePaste { chord });
                    },
                );
            }
        }
    }

    fn show_popup(&self) {
        self.prev_focus.set(ui::popup::focused_window_x11().unwrap_or(0));
        ui::popup::show(self.storage.clone(), self.config.clone());
        self.popup_visible.set(true);
    }

    fn toggle_popup(&self) {
        if self.popup_visible.get() && ui::popup::is_visible() {
            ui::popup::hide();
            self.popup_visible.set(false);
        } else {
            self.show_popup();
        }
    }
}

// The daemon is a singleton living on the glib main thread; UI callbacks
// (popup buttons, settings) reach it through this thread-local handle.
thread_local! {
    static GLOBAL: RefCell<Option<Rc<Daemon>>> = const { RefCell::new(None) };
}

/// Current daemon handle for UI modules (popup buttons etc.).
pub fn current() -> Option<Rc<Daemon>> {
    GLOBAL.with(|g| g.borrow().clone())
}

/// Called by the popup when the user picks an item.
pub fn select_from_popup(id: i64, plain: bool) {
    if let Some(d) = current() {
        d.select(id, plain, true);
    }
}

/// Called by the popup to pin/unpin an item.
pub fn set_pinned(id: i64, pinned: bool) {
    if let Some(d) = current() {
        let _ = d.storage.borrow_mut().set_pinned(id, pinned);
    }
}

/// Called by the popup to delete an item.
pub fn delete_item(id: i64) {
    if let Some(d) = current() {
        let _ = d.storage.borrow_mut().delete(id);
    }
}

/// Called by the popup's Clear button: wipes unpinned history.
pub fn clear_history(all: bool) {
    if let Some(d) = current() {
        match d.storage.borrow_mut().clear(all) {
            Ok(n) => tracing::info!("cleared {n} items (all={all})"),
            Err(e) => tracing::warn!("clear failed: {e}"),
        }
    }
}

/// Live pause/resume of capture (settings "clipboard history" switch).
pub fn pause_backend(paused: bool) {
    if let Some(d) = current() {
        let _ = d.backend_cmd.send_blocking(BackendCommand::SetPaused(paused));
    }
}

pub fn run() -> Result<()> {
    let config = Config::load().unwrap_or_else(|e| {
        tracing::warn!("config load failed ({e}); using defaults");
        Config::default()
    });
    // Persist defaults on first run so users see a documented file.
    if let Ok(p) = Config::path() {
        if !p.exists() {
            let _ = config.save();
        }
    }
    std::fs::create_dir_all(Config::images_dir()?)?;
    let storage = Storage::open(&Config::db_path()?, &Config::images_dir()?)?;

    let resolved = config.backend.resolve();
    tracing::info!("backend: {}", resolved.name());

    let (ev_tx, ev_rx) = async_channel::unbounded::<BackendEvent>();
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<BackendCommand>();
    let (ipc_tx, ipc_rx) = async_channel::unbounded::<IpcCommand>();

    let status = Arc::new(StatusInfo {
        backend: resolved.name().to_string(),
        started: Instant::now(),
        captured: AtomicU64::new(0),
    });

    // --- backend thread ---
    {
        let watch = WatchSet {
            clipboard: true,
            primary: config.monitor_primary,
        };
        match resolved {
            clipvault_core::backend::Resolved::X11 => {
                std::thread::Builder::new()
                    .name("backend-x11".into())
                    .spawn(move || {
                        match clipvault_core::backend::x11::X11Backend::new() {
                            Ok(mut b) => {
                                if let Err(e) = b.run(watch, ev_tx, cmd_rx) {
                                    tracing::error!("x11 backend died: {e}");
                                }
                            }
                            Err(e) => tracing::error!("x11 backend init: {e}"),
                        }
                    })
                    .context("spawn backend thread")?;
            }
            clipvault_core::backend::Resolved::Wayland => {
                std::thread::Builder::new()
                    .name("backend-wayland".into())
                    .spawn(move || {
                        match clipvault_core::backend::wayland::WaylandBackend::new() {
                            Ok(mut b) => {
                                if let Err(e) = b.run(watch, ev_tx, cmd_rx) {
                                    tracing::error!("wayland backend died: {e}");
                                }
                            }
                            Err(e) => tracing::error!("wayland backend init: {e}"),
                        }
                    })
                    .context("spawn backend thread")?;
            }
            clipvault_core::backend::Resolved::Unsupported => {
                tracing::error!("no supported display server found (DISPLAY/WAYLAND_DISPLAY)");
            }
        }
    }

    // --- global hotkey (X11; Wayland uses compositor bindings → IPC) ---
    let hk_rx = if resolved == clipvault_core::backend::Resolved::X11
        && !config.use_de_keybinding
    {
        let (hk_tx, hk_rx) = async_channel::unbounded::<()>();
        match crate::hotkey::spawn(&config.hotkey, hk_tx) {
            Ok(()) => tracing::info!("hotkey {} armed", config.hotkey),
            Err(e) => tracing::error!(
                "hotkey grab failed: {e:#} — bind your DE shortcut to `clipvault toggle` instead"
            ),
        }
        Some(hk_rx)
    } else {
        None
    };

    // --- system tray ---
    if config.show_tray {
        crate::tray::spawn(ipc_tx.clone());
    }

    // --- IPC listener thread ---
    {
        let listener = ipc::listen().context("IPC listen")?;
        let status = status.clone();
        std::thread::Builder::new()
            .name("ipc".into())
            .spawn(move || loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let cmd = ipc::read_command(stream.try_clone().unwrap());
                        if let Some(cmd) = cmd {
                            if matches!(cmd, IpcCommand::Status) {
                                let line = format!(
                                    "{{\"ok\":true,\"backend\":\"{}\",\"uptime_secs\":{},\"captured\":{}}}\n",
                                    status.backend,
                                    status.started.elapsed().as_secs(),
                                    status.captured.load(Ordering::Relaxed),
                                );
                                use std::io::Write;
                                let _ = stream
                                    .try_clone()
                                    .and_then(|mut s| s.write_all(line.as_bytes()));
                            }
                            let _ = ipc_tx.send_blocking(cmd);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("ipc accept: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            })
            .context("spawn ipc thread")?;
    }

    // --- GTK app hosting popup + settings ---
    let app = adw::Application::builder()
        .application_id("io.clipvault.Clipvault")
        // Single-instance is enforced by our IPC socket, not D-Bus app
        // registration — NON_UNIQUE lets isolated test daemons (Xvfb)
        // coexist with a real session daemon.
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|app| {
        let _ = adw::init();
        // Daemon: no persistent window. The guard must live forever,
        // otherwise the app quits as soon as the last window closes.
        std::mem::forget(app.hold());
    });
    app.connect_activate(|_| {}); // headless daemon — nothing to activate

    let daemon = Rc::new(Daemon {
        config: Rc::new(RefCell::new(config)),
        storage: Rc::new(RefCell::new(storage)),
        backend_cmd: cmd_tx,
        status: status.clone(),
        prev_focus: Cell::new(0),
        popup_visible: Cell::new(false),
    });
    GLOBAL.with(|g| *g.borrow_mut() = Some(daemon.clone()));

    // Backend events → storage (+ UI refresh) on the main loop.
    {
        let d = daemon.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(ev) = ev_rx.recv().await {
                match ev {
                    BackendEvent::NewClip(clip) => d.handle_clip(clip),
                    BackendEvent::Ready(name) => tracing::info!("backend ready: {name}"),
                    BackendEvent::Error(e) => tracing::error!("backend: {e}"),
                }
            }
        });
    }
    // IPC commands on the main loop.
    {
        let d = daemon.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(cmd) = ipc_rx.recv().await {
                d.handle_ipc(cmd);
            }
        });
    }
    // Hotkey presses on the main loop.
    if let Some(hk_rx) = hk_rx {
        let d = daemon.clone();
        glib::MainContext::default().spawn_local(async move {
            while hk_rx.recv().await.is_ok() {
                d.toggle_popup();
            }
        });
    }

    app.run_with_args(&[] as &[&str]);
    Ok(())
}
