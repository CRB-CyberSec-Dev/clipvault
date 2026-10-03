//! Settings panel — AdwPreferencesWindow mirroring Windows'
//! Settings → Clipboard plus Linux-specific options. Every control binds
//! to the Config struct; changes save to TOML immediately and apply live
//! where possible.

use clipvault_core::config::{Config, PopupPosition, Theme};
use gtk4::prelude::*;
use libadwaita as adw;
use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

thread_local! {
    static SETTINGS: RefCell<Option<adw::PreferencesDialog>> = const { RefCell::new(None) };
}

pub fn show(config: Rc<RefCell<Config>>) {
    SETTINGS.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(build(config));
        }
        slot.as_ref().unwrap().present(None::<&gtk4::Widget>);
    });
}

fn build(config: Rc<RefCell<Config>>) -> adw::PreferencesDialog {
    let win = adw::PreferencesDialog::builder()
        .title("ClipVault Settings")
        .search_enabled(false)
        .build();

    win.add(&general_page(&config));
    win.add(&behavior_page(&config));
    win.add(&privacy_page(&config));
    win.add(&appearance_page(&config));
    win.add(&system_page(&config));
    win
}

/// Persist + notify after any change.
fn apply(config: &Rc<RefCell<Config>>, f: impl FnOnce(&mut Config)) {
    let mut cfg = config.borrow_mut();
    f(&mut cfg);
    if let Err(e) = cfg.save() {
        tracing::warn!("config save failed: {e}");
    }
}

// ---------------------------------------------------------------------
// General
// ---------------------------------------------------------------------

fn general_page(config: &Rc<RefCell<Config>>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("General")
        .icon_name("edit-paste-symbolic")
        .build();

    let clip_group = adw::PreferencesGroup::builder()
        .title("Clipboard")
        .description("Mirrors Windows Settings → System → Clipboard")
        .build();

    let history = switch_row(
        "Clipboard history",
        "Save copied items (pauses capture, keeps existing data)",
        config.borrow().history_enabled,
    );
    {
        let c = config.clone();
        history.connect_active_notify(move |row| {
            let active = row.is_active();
            apply(&c, |cfg| cfg.history_enabled = active);
            // Live-pause the backend.
            crate::daemon::pause_backend(!active);
        });
    }
    clip_group.add(&history);

    let primary = switch_row(
        "Middle-click selection (PRIMARY)",
        "Also track Linux's select-to-copy / middle-click-paste buffer",
        config.borrow().monitor_primary,
    );
    {
        let c = config.clone();
        primary.connect_active_notify(move |row| {
            let v = row.is_active();
            apply(&c, |cfg| cfg.monitor_primary = v);
        });
    }
    clip_group.add(&primary);

    let images = switch_row(
        "Capture images",
        "Store screenshots and other image data",
        config.borrow().monitor_images,
    );
    {
        let c = config.clone();
        images.connect_active_notify(move |row| {
            let v = row.is_active();
            apply(&c, |cfg| cfg.monitor_images = v);
        });
    }
    clip_group.add(&images);

    let clear = adw::ButtonRow::builder()
        .title("Clear clipboard data")
        .tooltip_text("Pinned items survive")
        .build();
    clear.connect_activated(|_| {
        crate::daemon::clear_history(false);
    });
    clip_group.add(&clear);
    page.add(&clip_group);

    // --- limits ---
    let limits = adw::PreferencesGroup::builder().title("History limits").build();

    let entries = adw::SpinRow::with_range(25.0, 5000.0, 25.0);
    entries.set_title("Maximum entries");
    entries.set_subtitle("Windows keeps 25; default here is 250");
    entries.set_value(config.borrow().max_entries as f64);
    {
        let c = config.clone();
        entries.connect_changed(move |row| {
            let v = row.value() as u32;
            apply(&c, |cfg| cfg.max_entries = v);
        });
    }
    limits.add(&entries);

    let size = adw::SpinRow::with_range(1.0, 64.0, 1.0);
    size.set_title("Maximum item size (MiB)");
    size.set_value((config.borrow().max_item_bytes / (1024 * 1024)).max(1) as f64);
    {
        let c = config.clone();
        size.connect_changed(move |row| {
            let v = (row.value() as u64) * 1024 * 1024;
            apply(&c, |cfg| cfg.max_item_bytes = v);
        });
    }
    limits.add(&size);

    let days = adw::SpinRow::with_range(0.0, 365.0, 1.0);
    days.set_title("Keep history for (days)");
    days.set_subtitle("0 = keep forever");
    days.set_value(config.borrow().max_age_days as f64);
    {
        let c = config.clone();
        days.connect_changed(move |row| {
            let v = row.value() as u32;
            apply(&c, |cfg| cfg.max_age_days = v);
        });
    }
    limits.add(&days);
    page.add(&limits);

    page
}

// ---------------------------------------------------------------------
// Behavior
// ---------------------------------------------------------------------

fn behavior_page(config: &Rc<RefCell<Config>>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Behavior")
        .icon_name("input-keyboard-symbolic")
        .build();

    let popup_group = adw::PreferencesGroup::builder().title("Popup").build();

    let positions = gtk4::StringList::new(&["At mouse cursor", "Screen center", "Bottom center"]);
    let pos_row = adw::ComboRow::builder()
        .title("Popup position")
        .subtitle("“At mouse cursor” requires X11 — Wayland always uses bottom center")
        .model(&positions)
        .build();
    pos_row.set_selected(match config.borrow().popup_position {
        PopupPosition::Cursor => 0,
        PopupPosition::Center => 1,
        PopupPosition::Bottom => 2,
    });
    {
        let c = config.clone();
        pos_row.connect_selected_notify(move |row| {
            let p = match row.selected() {
                0 => PopupPosition::Cursor,
                1 => PopupPosition::Center,
                _ => PopupPosition::Bottom,
            };
            apply(&c, |cfg| cfg.popup_position = p);
        });
    }
    popup_group.add(&pos_row);
    page.add(&popup_group);

    let paste_group = adw::PreferencesGroup::builder().title("Pasting").build();

    let auto = switch_row(
        "Paste automatically on selection",
        "Simulate the paste chord after you pick an item",
        config.borrow().auto_paste,
    );
    {
        let c = config.clone();
        auto.connect_active_notify(move |row| {
            let v = row.is_active();
            apply(&c, |cfg| cfg.auto_paste = v);
        });
    }
    paste_group.add(&auto);

    let chord = adw::EntryRow::builder().title("Paste chord").build();
    chord.set_text(&config.borrow().paste_chord);
    chord.set_tooltip_text(Some("e.g. ctrl+v or shift+insert (works in terminals)"));
    {
        let c = config.clone();
        chord.connect_changed(move |row| {
            let v = row.text().to_string();
            apply(&c, |cfg| cfg.paste_chord = v.clone());
        });
    }
    paste_group.add(&chord);

    let delay = adw::SpinRow::with_range(0.0, 500.0, 10.0);
    delay.set_title("Paste delay (ms)");
    delay.set_subtitle("Time between refocusing the target app and injecting the chord");
    delay.set_value(config.borrow().paste_delay_ms as f64);
    {
        let c = config.clone();
        delay.connect_changed(move |row| {
            let v = row.value() as u64;
            apply(&c, |cfg| cfg.paste_delay_ms = v);
        });
    }
    paste_group.add(&delay);
    page.add(&paste_group);

    let hk_group = adw::PreferencesGroup::builder().title("Keyboard shortcut").build();

    let hotkey = adw::EntryRow::builder().title("Global hotkey").build();
    hotkey.set_text(&config.borrow().hotkey);
    hotkey.set_tooltip_text(Some("e.g. Super+V — X11 only"));
    {
        let c = config.clone();
        hotkey.connect_changed(move |row| {
            let v = row.text().to_string();
            apply(&c, |cfg| cfg.hotkey = v.clone());
        });
    }
    hk_group.add(&hotkey);

    let de_bind = switch_row(
        "Use desktop-environment shortcut",
        "Let XFCE/your compositor own the binding (runs `clipvault toggle`). Required on Wayland.",
        config.borrow().use_de_keybinding,
    );
    {
        let c = config.clone();
        de_bind.connect_active_notify(move |row| {
            let v = row.is_active();
            apply(&c, |cfg| cfg.use_de_keybinding = v);
        });
    }
    hk_group.add(&de_bind);
    page.add(&hk_group);

    page
}

// ---------------------------------------------------------------------
// Privacy
// ---------------------------------------------------------------------

fn privacy_page(config: &Rc<RefCell<Config>>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Privacy")
        .icon_name("dialog-password-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder().title("Sensitive data").build();

    let pw = switch_row(
        "Ignore password managers",
        "Skip clips marked sensitive (KDE password hint) and the app list below",
        config.borrow().exclude_password_managers,
    );
    {
        let c = config.clone();
        pw.connect_active_notify(move |row| {
            let v = row.is_active();
            apply(&c, |cfg| cfg.exclude_password_managers = v);
        });
    }
    group.add(&pw);

    let conceal = switch_row(
        "Conceal previews",
        "Mask item text in the popup (like Windows' password masking)",
        config.borrow().conceal_previews,
    );
    {
        let c = config.clone();
        conceal.connect_active_notify(move |row| {
            let v = row.is_active();
            apply(&c, |cfg| cfg.conceal_previews = v);
        });
    }
    group.add(&conceal);

    // Excluded apps editor.
    let expander = adw::ExpanderRow::builder()
        .title("Excluded applications")
        .subtitle("Clips from these apps are never stored (matches WM_CLASS / app name)")
        .build();

    let entry = adw::EntryRow::builder()
        .title("Add application (e.g. keepassxc)")
        .show_apply_button(true)
        .build();
    {
        let c = config.clone();
        let exp = expander.clone();
        entry.connect_apply(move |row| {
            let name = row.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            apply(&c, |cfg| {
                if !cfg.excluded_apps.iter().any(|a| a == &name) {
                    cfg.excluded_apps.push(name.clone());
                }
            });
            row.set_text("");
            rebuild_app_list(&exp, &c);
        });
    }
    expander.add_row(&entry);

    // Existing entries rendered as removable rows.
    let names = config.borrow().excluded_apps.clone();
    for name in names {
        expander.add_row(&app_row(&name, config, &expander));
    }

    group.add(&expander);
    page.add(&group);
    page
}

fn app_row(name: &str, config: &Rc<RefCell<Config>>, expander: &adw::ExpanderRow) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(name).build();
    let remove = gtk4::Button::from_icon_name("user-trash-symbolic");
    remove.add_css_class("flat");
    remove.set_valign(gtk4::Align::Center);
    let n = name.to_string();
    let c = config.clone();
    let exp = expander.clone();
    let row_ref = row.clone();
    remove.connect_clicked(move |_| {
        apply(&c, |cfg| cfg.excluded_apps.retain(|a| a != &n));
        exp.remove(&row_ref);
    });
    row.add_suffix(&remove);
    row
}

fn rebuild_app_list(expander: &adw::ExpanderRow, config: &Rc<RefCell<Config>>) {
    let names = config.borrow().excluded_apps.clone();
    if let Some(last) = names.last() {
        expander.add_row(&app_row(last, config, expander));
    }
}

// ---------------------------------------------------------------------
// Appearance
// ---------------------------------------------------------------------

fn appearance_page(config: &Rc<RefCell<Config>>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Appearance")
        .icon_name("preferences-desktop-theme-symbolic")
        .build();

    let group = adw::PreferencesGroup::new();

    let themes = gtk4::StringList::new(&["System", "Light", "Dark"]);
    let theme_row = adw::ComboRow::builder()
        .title("Theme")
        .model(&themes)
        .build();
    theme_row.set_selected(match config.borrow().theme {
        Theme::System => 0,
        Theme::Light => 1,
        Theme::Dark => 2,
    });
    {
        let c = config.clone();
        theme_row.connect_selected_notify(move |row| {
            let (t, scheme) = match row.selected() {
                1 => (Theme::Light, adw::ColorScheme::ForceLight),
                2 => (Theme::Dark, adw::ColorScheme::ForceDark),
                _ => (Theme::System, adw::ColorScheme::Default),
            };
            adw::StyleManager::default().set_color_scheme(scheme);
            apply(&c, |cfg| cfg.theme = t);
        });
    }
    group.add(&theme_row);

    let tray = switch_row(
        "Show tray icon",
        "Status icon in the system tray",
        config.borrow().show_tray,
    );
    {
        let c = config.clone();
        tray.connect_active_notify(move |row| {
            let v = row.is_active();
            apply(&c, |cfg| cfg.show_tray = v);
        });
    }
    group.add(&tray);

    page.add(&group);
    page
}

// ---------------------------------------------------------------------
// System
// ---------------------------------------------------------------------

fn system_page(config: &Rc<RefCell<Config>>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("System")
        .icon_name("emblem-system-symbolic")
        .build();

    let group = adw::PreferencesGroup::new();

    let autostart = switch_row(
        "Launch at login",
        "systemd user service (graphical session)",
        config.borrow().launch_at_login && systemd_unit_exists(),
    );
    {
        let c = config.clone();
        autostart.connect_active_notify(move |row| {
            let v = row.is_active();
            apply(&c, |cfg| cfg.launch_at_login = v);
            if let Err(e) = set_autostart(v) {
                tracing::error!("autostart toggle failed: {e}");
            }
        });
    }
    group.add(&autostart);

    let about = adw::ActionRow::builder()
        .title("Diagnostics")
        .subtitle(format!(
            "v{} · backend: {} · data: {}",
            env!("CARGO_PKG_VERSION"),
            std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "?".into()),
            Config::data_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".into())
        ))
        .build();
    group.add(&about);

    page.add(&group);

    // --- About ---
    let about_group = adw::PreferencesGroup::builder().title("About").build();

    let author = adw::ActionRow::builder()
        .title("Chamal Bandara")
        .subtitle("Cyber Security Engineer")
        .build();
    let gh = gtk4::LinkButton::builder()
        .uri("https://github.com/CRB-CyberSec-Dev/")
        .label("GitHub")
        .valign(gtk4::Align::Center)
        .build();
    author.add_suffix(&gh);
    about_group.add(&author);

    let about_btn = adw::ButtonRow::builder().title("About ClipVault").build();
    about_btn.connect_activated(|_| {
        let dlg = adw::AboutDialog::builder()
            .application_name("ClipVault")
            .application_icon("clipvault")
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name("Chamal Bandara")
            .developers(vec!["Chamal Bandara https://github.com/CRB-CyberSec-Dev/"])
            .comments("Win+V-style clipboard history manager for Linux.\nBuilt by Chamal Bandara, Cyber Security Engineer.")
            .website("https://github.com/CRB-CyberSec-Dev/")
            .license_type(gtk4::License::MitX11)
            .build();
        dlg.present(None::<&gtk4::Widget>);
    });
    about_group.add(&about_btn);

    page.add(&about_group);
    page
}

fn switch_row(title: &str, subtitle: &str, active: bool) -> adw::SwitchRow {
    adw::SwitchRow::builder()
        .title(title)
        .subtitle(subtitle)
        .active(active)
        .build()
}

// ---------------------------------------------------------------------
// systemd user unit
// ---------------------------------------------------------------------

fn unit_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        std::path::PathBuf::from(home)
            .join(".config/systemd/user/clipvault.service"),
    )
}

fn systemd_unit_exists() -> bool {
    unit_path().map(|p| p.exists()).unwrap_or(false)
}

fn set_autostart(enable: bool) -> anyhow::Result<()> {
    use anyhow::Context;
    let path = unit_path().context("no HOME")?;
    if enable {
        let exe = std::env::current_exe().context("current_exe")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            &path,
            format!(
                "[Unit]\n\
                 Description=ClipVault clipboard manager\n\
                 PartOf=graphical-session.target\n\
                 After=graphical-session.target\n\n\
                 [Service]\n\
                 ExecStart={} daemon\n\
                 Restart=on-failure\n\
                 RestartSec=2\n\n\
                 [Install]\n\
                 WantedBy=graphical-session.target\n",
                exe.display()
            ),
        )?;
        run_systemctl(&["--user", "daemon-reload"])?;
        run_systemctl(&["--user", "enable", "--now", "clipvault.service"])?;
    } else {
        let _ = run_systemctl(&["--user", "disable", "--now", "clipvault.service"]);
        let _ = std::fs::remove_file(&path);
        let _ = run_systemctl(&["--user", "daemon-reload"]);
    }
    Ok(())
}

fn run_systemctl(args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("systemctl")
        .args(args)
        .output()?;
    if !out.status.success() {
        anyhow::bail!("systemctl {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}
