//! The Win+V-style history popup.
//!
//! One reused `gtk4::ApplicationWindow` initialized as a layer-shell
//! surface: on Wayland it anchors bottom-center (Wayland forbids cursor
//! tracking by design); on X11 gtk4-layer-shell emulates the protocol and
//! we anchor at the mouse cursor (clamped inside the monitor workarea).

use clipvault_core::config::{Config, PopupPosition};
use clipvault_core::storage::Storage;
use clipvault_core::types::{ClipItem, ClipKind};
use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const MAX_ROWS: u32 = 200;
const THUMB_SIZE: i32 = 96;

pub struct Popup {
    window: gtk4::ApplicationWindow,
    search: gtk4::SearchEntry,
    list: gtk4::ListBox,
    status: gtk4::Label,
    /// Row index → clip id, rebuilt together with the list.
    ids: Vec<i64>,
    storage: Rc<RefCell<Storage>>,
    config: Rc<RefCell<Config>>,
    textures: HashMap<String, gdk::Texture>,
    /// The one row currently showing its action buttons
    /// (pin_btn, del_btn, was_pinned) — reset when hovering elsewhere or
    /// when the pointer leaves the window, so icons never get stuck.
    hover_slot: Rc<RefCell<Option<(gtk4::Button, gtk4::Button, bool)>>>,
}

thread_local! {
    static POPUP: RefCell<Option<Rc<RefCell<Popup>>>> = const { RefCell::new(None) };
}

/// Best-effort current focused X11 window id (for paste-back refocus).
pub fn focused_window_x11() -> Option<u32> {
    clipvault_core::backend::x11::focused_window()
}

/// Borrow the singleton popup. GTK signal handlers can't unwind, so a
/// RefCell panic here would abort the whole daemon — instead we
/// try_borrow, log the caller, and skip. (A logged skip is a bug report;
/// an abort is a dead daemon.)
#[track_caller]
fn with_popup<R>(f: impl FnOnce(&mut Popup) -> R) -> Option<R> {
    let caller = std::panic::Location::caller();
    POPUP.with(|p| {
        let Ok(outer) = p.try_borrow_mut() else {
            tracing::error!("popup re-entry (outer) from {caller}");
            return None;
        };
        // Clone the Rc out so no thread-local borrow stays alive.
        let Some(rc) = outer.as_ref().cloned() else { return None };
        drop(outer);
        let result = match rc.try_borrow_mut() {
            Ok(mut inner) => Some(f(&mut inner)),
            Err(_) => {
                tracing::error!("popup re-entry (inner) from {caller}");
                None
            }
        };
        result
    })
}

pub fn is_visible() -> bool {
    with_popup(|p| p.window.is_visible()).unwrap_or(false)
}

pub fn hide() {
    with_popup(|p| {
        p.window.set_visible(false);
        p.textures.clear(); // bound memory: drop decoded images
    });
}

/// Rebuild the list if the popup is currently open.
pub fn refresh_if_visible() {
    if is_visible() {
        with_popup(|p| p.rebuild());
    }
}

pub fn show(storage: Rc<RefCell<Storage>>, config: Rc<RefCell<Config>>) {
    POPUP.with(|p| {
        let mut slot = p.borrow_mut();
        if slot.is_none() {
            *slot = Some(Popup::build(storage.clone(), config.clone()));
        }
    });
    with_popup(|p| {
        // Clear search first — set_text fires search_changed, whose handler
        // try_borrows and skips while we hold the borrow; our explicit
        // rebuild below covers it.
        p.search.set_text("");
        p.rebuild();
        p.position_and_present();
        p.search.grab_focus();
    });
}

impl Popup {
    fn build(storage: Rc<RefCell<Storage>>, config: Rc<RefCell<Config>>) -> Rc<RefCell<Self>> {
        let app = gtk4::gio::Application::default()
            .and_downcast::<libadwaita::Application>()
            .expect("daemon must run an AdwApplication");

        let window = gtk4::ApplicationWindow::builder()
            .application(&app)
            .title("ClipVault")
            .icon_name("clipvault")
            .decorated(false)
            .default_width(420)
            .default_height(480)
            .build();
        window.add_css_class("clipvault-popup");

        if gtk4_layer_shell::is_supported() {
            window.init_layer_shell();
            window.set_layer(Layer::Top);
            window.set_keyboard_mode(KeyboardMode::Exclusive);
            window.set_namespace(Some("clipvault-popup"));
            // Anchors get set per-show (cursor on X11, bottom on Wayland).
        }

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        vbox.set_margin_top(8);
        vbox.set_margin_bottom(8);
        vbox.set_margin_start(8);
        vbox.set_margin_end(8);

        let search = gtk4::SearchEntry::new();
        search.set_placeholder_text(Some("Type to search…"));
        vbox.append(&search);

        let scrolled = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .propagate_natural_height(true)
            .build();
        let list = gtk4::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk4::SelectionMode::Single);
        list.set_placeholder(Some(&gtk4::Label::new(Some(
            "No clipboard history yet",
        ))));
        scrolled.set_child(Some(&list));
        vbox.append(&scrolled);

        let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let hints = gtk4::Label::new(Some("⏎ paste · ⇧⏎ plain · ⌃P pin · Del remove · Esc close"));
        hints.add_css_class("dim-label");
        hints.set_hexpand(true);
        hints.set_xalign(0.0);
        let status = gtk4::Label::new(None);
        status.add_css_class("dim-label");
        let clear_btn = gtk4::Button::with_label("Clear");
        clear_btn.add_css_class("destructive-action");
        footer.append(&hints);
        footer.append(&status);
        footer.append(&clear_btn);
        vbox.append(&footer);

        window.set_child(Some(&vbox));

        let popup = Rc::new(RefCell::new(Self {
            window: window.clone(),
            search: search.clone(),
            list: list.clone(),
            status,
            ids: Vec::new(),
            storage,
            config,
            textures: HashMap::new(),
            hover_slot: Rc::new(RefCell::new(None)),
        }));

        // Pointer leaving the window → reset any revealed row buttons.
        {
            let hs = popup.borrow().hover_slot.clone();
            let win_motion = gtk4::EventControllerMotion::new();
            win_motion.connect_leave(move |_| {
                if let Some((pin, del, was_pinned)) = hs.borrow_mut().take() {
                    pin.set_visible(was_pinned);
                    del.set_visible(false);
                }
            });
            window.add_controller(win_motion);
        }

        // --- behavior wiring ---
        {
            let p = popup.clone();
            search.connect_search_changed(move |_| {
                if let Ok(mut p) = p.try_borrow_mut() {
                    p.apply_filter();
                }
            });
        }
        {
            list.connect_row_activated(move |_, row| {
                if let Some(id) = row_id(row) {
                    hide();
                    crate::daemon::select_from_popup(id, false);
                }
            });
        }
        {
            clear_btn.connect_clicked(move |_| {
                crate::daemon::clear_history(false);
                refresh_if_visible();
            });
        }

        let keyctl = gtk4::EventControllerKey::new();
        // CAPTURE phase: the window sees keys before the focused search
        // entry, so Enter/arrows/Escape can't be eaten by the entry.
        keyctl.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let p = popup.clone();
            keyctl.connect_key_pressed(move |_, key, _code, mods| {
                match p.try_borrow_mut() {
                    Ok(mut p) => p.on_key(key, mods),
                    Err(_) => glib::Propagation::Proceed,
                }
            });
        }
        window.add_controller(keyctl);

        // Enter inside the search entry = activate the selected row.
        {
            let p = popup.clone();
            search.connect_activate(move |_| {
                let id = {
                    let p = p.borrow();
                    p.list
                        .selected_row()
                        .and_then(|r| p.ids.get(r.index() as usize).copied())
                };
                if let Some(id) = id {
                    hide();
                    crate::daemon::select_from_popup(id, false);
                }
            });
        }

        // Hide on focus loss (window deactivated).
        {
            let w = window.clone();
            window.connect_is_active_notify(move |win| {
                if !win.is_active() && w.is_visible() {
                    // Small delay so clicks on our own buttons don't hide.
                    let w2 = w.clone();
                    gtk4::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(150),
                        move || {
                            if !w2.is_active() {
                                hide();
                            }
                        },
                    );
                }
            });
        }

        popup
    }

    /// Position per config/backend and show.
    fn position_and_present(&mut self) {
        if gtk4_layer_shell::is_supported() {
            // Wayland path (layer-shell).
            let pos = self.config.borrow().popup_position;
            let on_x11 = std::env::var_os("WAYLAND_DISPLAY").is_none();
            match pos {
                PopupPosition::Cursor if on_x11 => {
                    let (x, y) = clipvault_core::backend::x11::pointer_position()
                        .unwrap_or((100, 100));
                    self.window.set_anchor(Edge::Left, true);
                    self.window.set_anchor(Edge::Top, true);
                    self.window.set_anchor(Edge::Bottom, false);
                    self.window.set_margin(Edge::Left, (x as i32).max(0));
                    self.window.set_margin(Edge::Top, (y as i32).max(0));
                }
                PopupPosition::Center => {
                    self.window.set_anchor(Edge::Left, false);
                    self.window.set_anchor(Edge::Top, false);
                    self.window.set_anchor(Edge::Bottom, false);
                }
                _ => {
                    // Bottom-center: Windows-like, and the only sane
                    // option on Wayland.
                    self.window.set_anchor(Edge::Left, false);
                    self.window.set_anchor(Edge::Top, false);
                    self.window.set_anchor(Edge::Bottom, true);
                    self.window.set_margin(Edge::Bottom, 24);
                }
            }
            self.window.present();
        } else {
            // X11 path: gtk4-layer-shell has no X11 support in Debian's
            // build, so present a plain window and position it ourselves
            // via x11rb. xfwm4's initial smart-placement overwrites early
            // moves, so retry at increasing delays until the window's
            // root-relative origin matches (or attempts run out). Opacity
            // hides the placement jump; the final attempt always restores
            // it. (Driven from here, not connect_map — re-showing a
            // hidden window does not reliably re-emit `map`.)
            let pos = self.config.borrow().popup_position;
            self.window.set_opacity(0.0);
            self.window.present();
            if let Some((tx, ty)) = compute_target(&self.window, pos) {
                let delays = [80u64, 200, 400, 800];
                for (i, d) in delays.iter().enumerate() {
                    let w = self.window.clone();
                    let last = i == delays.len() - 1;
                    gtk4::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(*d),
                        move || {
                            if !w.is_visible() {
                                return; // user closed meanwhile
                            }
                            let done = x11_move_attempt(&w, tx, ty);
                            if done || last {
                                w.set_opacity(1.0);
                            }
                        },
                    );
                }
            } else {
                self.window.set_opacity(1.0);
            }
        }
    }

    fn rebuild(&mut self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.ids.clear();

        let items = {
            let st = self.storage.borrow();
            let q = self.search.text();
            if q.is_empty() {
                st.list(MAX_ROWS).unwrap_or_default()
            } else {
                st.search(&q, MAX_ROWS).unwrap_or_default()
            }
        };

        let conceal = self.config.borrow().conceal_previews;
        for item in &items {
            let row = self.build_row(item, conceal);
            self.list.append(&row);
            self.ids.push(item.id);
        }
        if let Some(first) = self.list.row_at_index(0) {
            self.list.select_row(Some(&first));
        }
        self.status.set_text(&format!("{} items", items.len()));
    }

    fn apply_filter(&mut self) {
        // Simplest correct approach: rebuild from storage with the query.
        self.rebuild();
    }

    fn build_row(&mut self, item: &ClipItem, conceal: bool) -> gtk4::ListBoxRow {
        let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        hbox.set_margin_top(6);
        hbox.set_margin_bottom(6);
        hbox.set_margin_start(8);
        hbox.set_margin_end(8);

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        vbox.set_hexpand(true);

        match item.kind {
            ClipKind::Image => {
                if let Some(rel) = &item.image_path {
                    let path = Config::images_dir()
                        .map(|d| d.join(rel))
                        .unwrap_or_default();
                    let picture = if let Some(tex) = self.textures.get(rel) {
                        gtk4::Picture::for_paintable(tex)
                    } else {
                        let file = gtk4::gio::File::for_path(&path);
                        let tex = gdk::Texture::from_file(&file).ok();
                        if let Some(t) = &tex {
                            self.textures.insert(rel.clone(), t.clone());
                        }
                        tex.map(|t| gtk4::Picture::for_paintable(&t))
                            .unwrap_or_default()
                    };
                    picture.set_size_request(320, THUMB_SIZE);
                    picture.set_content_fit(gtk4::ContentFit::Contain);
                    picture.set_halign(gtk4::Align::Start);
                    vbox.append(&picture);
                    let meta = gtk4::Label::new(Some(&format!(
                        "image · {} · {}",
                        human_bytes(item.byte_size),
                        rel_time(item.last_used_at)
                    )));
                    meta.add_css_class("dim-label");
                    meta.set_xalign(0.0);
                    vbox.append(&meta);
                }
            }
            _ => {
                let text = item.text_content.clone().unwrap_or_default();
                // Title: first line only. Preview: the rest collapsed to a
                // single line. Neither wraps — wrapping multi-line previews
                // caused lines to overdraw each other (Pango lines+wrap
                // overflow), so every row stays a predictable height.
                let mut lines = text.lines();
                let first_line = lines.next().unwrap_or("");
                let rest = lines.collect::<Vec<_>>().join(" · ");
                let rest = rest.trim().to_string();

                let title_text = if conceal {
                    "••••••••".to_string()
                } else {
                    first_line.to_string()
                };
                let title_lbl = gtk4::Label::new(None);
                if item.kind == ClipKind::Html {
                    title_lbl.set_markup(&format!(
                        "{} <span size='small' alpha='60%'>[rich]</span>",
                        glib_markup_escape(&title_text)
                    ));
                } else {
                    title_lbl.set_text(&title_text);
                }
                title_lbl.set_xalign(0.0);
                title_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                title_lbl.set_single_line_mode(true);
                vbox.append(&title_lbl);

                if !conceal && !rest.is_empty() {
                    let prev = gtk4::Label::new(Some(&rest));
                    prev.set_xalign(0.0);
                    prev.set_single_line_mode(true);
                    prev.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                    prev.add_css_class("dim-label");
                    vbox.append(&prev);
                }

                let mut meta = format!(
                    "{} · {}",
                    human_bytes(item.byte_size),
                    rel_time(item.last_used_at)
                );
                if let Some(app) = &item.source_app {
                    meta.push_str(&format!(" · {app}"));
                }
                let meta_lbl = gtk4::Label::new(Some(&meta));
                meta_lbl.add_css_class("dim-label");
                meta_lbl.add_css_class("caption");
                meta_lbl.set_xalign(0.0);
                vbox.append(&meta_lbl);
            }
        }

        hbox.append(&vbox);

        // Actions: pin + delete, revealed on hover only. Pinned rows keep
        // the (accent-colored) pin visible as the pinned indicator — no
        // emoji in the text anymore.
        let pin_btn = gtk4::Button::from_icon_name("view-pin-symbolic");
        pin_btn.add_css_class("flat");
        pin_btn.set_tooltip_text(Some(if item.pinned { "Unpin" } else { "Pin" }));
        let del_btn = gtk4::Button::from_icon_name("user-trash-symbolic");
        del_btn.add_css_class("flat");
        del_btn.set_tooltip_text(Some("Remove"));

        let pinned = item.pinned;
        if pinned {
            pin_btn.add_css_class("accent");
        }
        pin_btn.set_visible(pinned);
        del_btn.set_visible(false);

        let id = item.id;
        {
            pin_btn.connect_clicked(move |_| {
                let now = crate::daemon::is_pinned(id);
                crate::daemon::set_pinned(id, !now);
                refresh_if_visible();
            });
        }
        del_btn.connect_clicked(move |_| {
            crate::daemon::delete_item(id);
            refresh_if_visible();
        });

        let actions = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        actions.set_valign(gtk4::Align::Center);
        actions.append(&pin_btn);
        actions.append(&del_btn);
        hbox.append(&actions);

        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&hbox));

        // Hover reveal: entering a row shows its buttons (and resets any
        // previously revealed row); leaving resets. A window-level sweep
        // covers fast pointer exits that skip the row's leave event.
        let motion = gtk4::EventControllerMotion::new();
        {
            let slot = self.hover_slot.clone();
            let (pin_b, del_b) = (pin_btn.clone(), del_btn.clone());
            motion.connect_enter(move |_, _, _| {
                if let Some((p, d, was_pinned)) = slot.borrow_mut().take() {
                    p.set_visible(was_pinned);
                    d.set_visible(false);
                }
                pin_b.set_visible(true);
                del_b.set_visible(true);
                *slot.borrow_mut() = Some((pin_b.clone(), del_b.clone(), pinned));
            });
        }
        {
            let slot = self.hover_slot.clone();
            motion.connect_leave(move |_| {
                if let Some((p, d, was_pinned)) = slot.borrow_mut().take() {
                    p.set_visible(was_pinned);
                    d.set_visible(false);
                }
            });
        }
        row.add_controller(motion);

        row
    }

    fn on_key(&mut self, key: gdk::Key, mods: gdk::ModifierType) -> glib::Propagation {
        match key {
            gdk::Key::Escape => {
                // Defer the hide until the RefCell borrow is released.
                glib::idle_add_local_once(|| hide());
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                let sel = self
                    .list
                    .selected_row()
                    .and_then(|r| self.ids.get(r.index() as usize).copied());
                if let Some(id) = sel {
                    let plain = mods.contains(gdk::ModifierType::SHIFT_MASK);
                    glib::idle_add_local_once(move || {
                        hide();
                        crate::daemon::select_from_popup(id, plain);
                    });
                }
                glib::Propagation::Stop
            }
            gdk::Key::Down => {
                self.move_selection(1);
                glib::Propagation::Stop
            }
            gdk::Key::Up => {
                self.move_selection(-1);
                glib::Propagation::Stop
            }
            gdk::Key::Delete | gdk::Key::KP_Delete => {
                let sel = self
                    .list
                    .selected_row()
                    .and_then(|r| self.ids.get(r.index() as usize).copied());
                if let Some(id) = sel {
                    crate::daemon::delete_item(id);
                    self.rebuild();
                }
                glib::Propagation::Stop
            }
            gdk::Key::p if mods.contains(gdk::ModifierType::CONTROL_MASK) => {
                let sel = self
                    .list
                    .selected_row()
                    .and_then(|r| self.ids.get(r.index() as usize).copied());
                if let Some(id) = sel {
                    let pinned = self
                        .storage
                        .borrow()
                        .get(id)
                        .ok()
                        .flatten()
                        .map(|i| i.pinned)
                        .unwrap_or(false);
                    crate::daemon::set_pinned(id, !pinned);
                    self.rebuild();
                }
                glib::Propagation::Stop
            }
            _ => {
                // Launcher UX: typing while a row (not the entry) has focus
                // forwards the character into the search field.
                if !self.search.has_focus() {
                    if let Some(ch) = key.to_unicode().filter(|c| !c.is_control()) {
                        let mut t = self.search.text().to_string();
                        t.push(ch);
                        self.search.set_text(&t);
                        self.search.grab_focus();
                        self.search.set_position(-1);
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            }
        }
    }

    fn move_selection(&mut self, dir: i32) {
        let current = self.list.selected_row().map(|r| r.index()).unwrap_or(-1);
        let next = current + dir;
        if let Some(row) = self.list.row_at_index(next) {
            self.list.select_row(Some(&row));
            row.grab_focus();
        }
    }
}

/// Map a ListBoxRow to its clip id via insertion order (rows are only
/// ever appended in order during rebuild).
fn row_id(row: &gtk4::ListBoxRow) -> Option<i64> {
    with_popup(|p| p.ids.get(row.index() as usize).copied()).flatten()
}

fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.0} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

fn rel_time(unix: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let dt = (now - unix).max(0);
    if dt < 60 {
        "just now".into()
    } else if dt < 3600 {
        format!("{} min ago", dt / 60)
    } else if dt < 86_400 {
        format!("{} h ago", dt / 3600)
    } else {
        format!("{} d ago", dt / 86_400)
    }
}

fn glib_markup_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Compute the target root-coordinate position for the popup (cursor /
/// center / bottom), clamped inside the pointer's monitor.
fn compute_target(win: &gtk4::ApplicationWindow, pos: PopupPosition) -> Option<(i32, i32)> {
    let (px, py) = clipvault_core::backend::x11::pointer_position()?;
    let display = gtk4::prelude::WidgetExt::display(win);
    let monitors = display.monitors();
    let mut chosen = monitors.item(0).and_downcast::<gtk4::gdk::Monitor>();
    for i in 0..monitors.n_items() {
        if let Some(m) = monitors.item(i).and_downcast::<gtk4::gdk::Monitor>() {
            let g = m.geometry();
            let s = m.scale_factor();
            let (mx, my, mw, mh) = (g.x() * s, g.y() * s, g.width() * s, g.height() * s);
            if (px as i32) >= mx && (px as i32) < mx + mw && (py as i32) >= my && (py as i32) < my + mh
            {
                chosen = Some(m);
                break;
            }
        }
    }
    let mon = chosen?;
    let g = mon.geometry();
    let s = mon.scale_factor();
    let (mx, my, mw, mh) = (g.x() * s, g.y() * s, g.width() * s, g.height() * s);
    let (ww, wh) = (
        win.default_width().max(200) * s,
        win.default_height().max(200) * s,
    );

    let (x, y) = match pos {
        PopupPosition::Cursor => (px as i32 + 8, py as i32 + 8),
        PopupPosition::Center => (mx + (mw - ww) / 2, my + (mh - wh) / 2),
        PopupPosition::Bottom => (mx + (mw - ww) / 2, my + mh - wh - 24),
    };
    Some((
        x.clamp(mx, mx + (mw - ww).max(0)),
        y.clamp(my, my + (mh - wh).max(0)),
    ))
}

/// One move attempt: set EWMH props (idempotent), configure position,
/// then verify the root-relative origin via translate_coordinates
/// (reparenting-safe). Returns true when the target is reached.
fn x11_move_attempt(win: &gtk4::ApplicationWindow, tx: i32, ty: i32) -> bool {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, EventMask, PropMode,
        StackMode, CLIENT_MESSAGE_EVENT,
    };
    use x11rb::wrapper::ConnectionExt as _; // change_property32

    let Some(surface) = win.surface() else { return false };
    let Ok(x11s) = surface.downcast::<gdk4_x11::X11Surface>() else {
        return true; // not X11 — nothing to do, stop retrying
    };
    let xid = x11s.xid() as u32;
    let Ok((conn, screen_num)) = x11rb::connect(None) else {
        return false;
    };
    let root = conn.setup().roots[screen_num].root;

    let intern = |name: &str| -> Option<u32> {
        conn.intern_atom(false, name.as_bytes())
            .ok()?
            .reply()
            .ok()
            .map(|r| r.atom)
    };
    if let (Some(net_state), Some(above), Some(skip_tb), Some(wm_type), Some(notif)) = (
        intern("_NET_WM_STATE"),
        intern("_NET_WM_STATE_ABOVE"),
        intern("_NET_WM_STATE_SKIP_TASKBAR"),
        intern("_NET_WM_WINDOW_TYPE"),
        intern("_NET_WM_WINDOW_TYPE_NOTIFICATION"),
    ) {
        let _ = conn.change_property32(PropMode::REPLACE, xid, wm_type, AtomEnum::ATOM, &[notif]);
        let msg = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: xid,
            type_: net_state,
            data: [1u32, above, skip_tb, 0, 0].into(), // 1 = _NET_WM_STATE_ADD
        };
        let _ = conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            msg,
        );
    }

    let _ = conn.configure_window(
        xid,
        &ConfigureWindowAux::new().x(tx).y(ty).stack_mode(StackMode::ABOVE),
    );

    // Raising our client window only reorders it inside the WM's frame —
    // the frame keeps its stack slot and the popup stays occluded. Walk
    // up to the frame (root's direct child) and raise THAT.
    let mut topmost = xid;
    let mut cur = xid;
    for _ in 0..8 {
        match conn.query_tree(cur).ok().and_then(|c| c.reply().ok()) {
            Some(tree) if tree.parent != root && tree.parent != 0 => cur = tree.parent,
            Some(_) => {
                topmost = cur;
                break;
            }
            None => break,
        }
    }
    let _ = conn.configure_window(
        topmost,
        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
    );
    let _ = conn.flush();

    // Verify: root-relative origin must match (WM may have adjusted).
    let placed = conn
        .translate_coordinates(xid, root, 0, 0)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| (r.dst_x as i32, r.dst_y as i32));
    match placed {
        Some((cx, cy)) => {
            let done = (cx - tx).abs() <= 2 && (cy - ty).abs() <= 2;
            tracing::debug!("x11-position: target ({tx},{ty}), now ({cx},{cy}), done={done}");
            done
        }
        None => false,
    }
}
