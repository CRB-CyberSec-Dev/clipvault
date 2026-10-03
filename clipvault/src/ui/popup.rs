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
}

thread_local! {
    static POPUP: RefCell<Option<Rc<RefCell<Popup>>>> = const { RefCell::new(None) };
}

/// Best-effort current focused X11 window id (for paste-back refocus).
pub fn focused_window_x11() -> Option<u32> {
    clipvault_core::backend::x11::focused_window()
}

fn with_popup<R>(f: impl FnOnce(&mut Popup) -> R) -> Option<R> {
    POPUP.with(|p| p.borrow_mut().as_mut().map(|rc| f(&mut rc.borrow_mut())))
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
    let popup = POPUP.with(|p| {
        let mut slot = p.borrow_mut();
        if slot.is_none() {
            *slot = Some(Popup::build(storage.clone(), config.clone()));
        }
        slot.clone().unwrap()
    });
    {
        let mut p = popup.borrow_mut();
        p.rebuild();
        p.position_and_present();
        p.search.set_text("");
        p.search.grab_focus();
    }
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
        }));

        // --- behavior wiring ---
        {
            let p = popup.clone();
            search.connect_search_changed(move |_| {
                p.borrow_mut().apply_filter();
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
        {
            let p = popup.clone();
            keyctl.connect_key_pressed(move |_, key, _code, mods| {
                p.borrow_mut().on_key(key, mods)
            });
        }
        window.add_controller(keyctl);

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
        }
        self.window.present();
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
                    picture.set_size_request(-1, THUMB_SIZE);
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
                let first_line = text.lines().next().unwrap_or("").to_string();
                let title = if item.pinned {
                    format!("📌 {first_line}")
                } else {
                    first_line
                };
                let title = if conceal { "••••••••".to_string() } else { title };
                let title_lbl = gtk4::Label::new(Some(&title));
                title_lbl.set_xalign(0.0);
                title_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                title_lbl.set_single_line_mode(true);
                if item.kind == ClipKind::Html {
                    title_lbl.set_markup(&format!(
                        "{} <span size='small' alpha='60%'>[rich]</span>",
                        glib_markup_escape(&title)
                    ));
                }
                vbox.append(&title_lbl);

                let preview_text: String = if conceal {
                    String::new()
                } else {
                    text.lines().skip(1).take(2).collect::<Vec<_>>().join("\n")
                };
                if !preview_text.is_empty() {
                    let prev = gtk4::Label::new(Some(&preview_text));
                    prev.set_xalign(0.0);
                    prev.set_lines(2);
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

        // Per-row actions: pin + delete.
        let pin_btn = gtk4::Button::from_icon_name("view-pin-symbolic");
        pin_btn.add_css_class("flat");
        pin_btn.set_tooltip_text(Some(if item.pinned { "Unpin" } else { "Pin" }));
        if item.pinned {
            pin_btn.add_css_class("accent");
        }
        let del_btn = gtk4::Button::from_icon_name("user-trash-symbolic");
        del_btn.add_css_class("flat");
        del_btn.set_tooltip_text(Some("Remove"));
        let id = item.id;
        pin_btn.connect_clicked(move |btn| {
            crate::daemon::set_pinned(id, !btn.has_css_class("accent"));
            refresh_if_visible();
        });
        del_btn.connect_clicked(move |_| {
            crate::daemon::delete_item(id);
            refresh_if_visible();
        });
        let btns = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        btns.set_valign(gtk4::Align::Center);
        btns.append(&pin_btn);
        btns.append(&del_btn);
        hbox.append(&btns);

        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&hbox));
        row
    }

    fn on_key(&mut self, key: gdk::Key, mods: gdk::ModifierType) -> glib::Propagation {
        match key {
            gdk::Key::Escape => {
                hide();
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                if let Some(row) = self.list.selected_row() {
                    if let Some(id) = row_id(&row) {
                        let plain = mods.contains(gdk::ModifierType::SHIFT_MASK);
                        hide();
                        crate::daemon::select_from_popup(id, plain);
                    }
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
                if let Some(row) = self.list.selected_row() {
                    if let Some(id) = row_id(&row) {
                        crate::daemon::delete_item(id);
                        self.rebuild();
                    }
                }
                glib::Propagation::Stop
            }
            gdk::Key::p if mods.contains(gdk::ModifierType::CONTROL_MASK) => {
                if let Some(row) = self.list.selected_row() {
                    if let Some(id) = row_id(&row) {
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
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
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
