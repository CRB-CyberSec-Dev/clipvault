# ClipVault

A **Windows Win+V-style clipboard history manager for Linux**, written in Rust.
Popup history at your cursor, pinning, search, paste-as-plain-text, images —
plus a full settings panel. Works on **X11 and Wayland**.

![tech](https://img.shields.io/badge/GTK4-libadwaita-blue) ![license](https://img.shields.io/badge/license-MIT%2FApache--2.0-green)

## Features

| Windows Win+V | ClipVault |
|---|---|
| History popup (Win+V) | Super+V (configurable) |
| 25-item cap | 250 default, 25–5000 configurable |
| Pin items | ✅ pinned survive clear-all + restarts |
| Text / HTML / images | ✅ text, rich HTML, PNG images |
| Paste as plain text | ✅ Shift+Enter |
| Settings page | ✅ full libadwaita preferences dialog |
| Cloud sync | ❌ out of scope by design |

Plus Linux extras: optional PRIMARY (middle-click) selection tracking,
password-manager exclusion (`x-kde.passwordManagerHint` + app blocklist),
conceal-previews mode, per-item source app labels, FTS5 full-text search.

## Quick start (dev)

```bash
# system deps (Debian/Kali/Ubuntu)
sudo apt install libgtk-4-dev libadwaita-1-dev libgtk4-layer-shell-dev \
                 libdbus-1-dev wl-clipboard xdotool wtype
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

cargo build --release

# run the daemon (or install the systemd unit — see below)
./target/release/clipvault daemon
```

Then press **Super+V**. Or drive it from a shell:

```
clipvault toggle      # popup
clipvault list        # print history
clipvault select 3    # put item 3 on the clipboard
clipvault clear       # wipe history (pins survive)
clipvault settings    # settings window
clipvault status      # daemon/backend info
```

## Autostart

Settings → System → **Launch at login** writes and enables
`~/.config/systemd/user/clipvault.service`. Manually:

```bash
systemctl --user enable --now clipvault.service
journalctl --user -u clipvault -f     # logs
```

## Wayland notes (read this)

Wayland restricts what apps may do by design. ClipVault is honest about it:

| Feature | X11 | Wayland |
|---|---|---|
| Clipboard capture | ✅ XFIXES events | ✅ wlr-data-control (wlroots/KWin/GNOME 44+) |
| Popup at mouse cursor | ✅ | ❌ impossible — popup anchors **bottom-center** (layer-shell) |
| Global hotkey | ✅ built-in grab | ❌ impossible — bind your compositor to `clipvault toggle` |
| Auto-paste simulation | ✅ XTEST | wlroots only (via `wtype`) |

Compositor binding snippets for `clipvault toggle`:

- **sway** (`~/.config/sway/config`): `bindsym Mod4+v exec clipvault toggle`
- **Hyprland** (`hyprland.conf`): `bind = SUPER, V, exec, clipvault toggle`
- **KDE**: System Settings → Shortcuts → Custom → command `clipvault toggle`
- **GNOME**: Settings → Keyboard → Custom Shortcuts → `clipvault toggle`

On GNOME Wayland there is no layer-shell, so the popup is a plain centered
window. XFCE-Wayland (labwc): bind in `rc.xml`.

## Configuration

`~/.config/clipvault/config.toml` (created on first run, every key
editable from the settings panel):

```toml
hotkey = "Super+V"
popup_position = "cursor"     # cursor | center | bottom  (cursor = X11)
max_entries = 250
max_item_bytes = 4194304      # 4 MiB, like Windows
monitor_primary = false       # middle-click selection
auto_paste = true
paste_chord = "ctrl+v"        # or "shift+insert" for terminals
excluded_apps = ["keepassxc", "bitwarden", "1password"]
conceal_previews = false
theme = "system"              # system | light | dark
```

Data lives in `~/.local/share/clipvault/` — `history.db` (SQLite, WAL,
mode 0600) and `images/`. **History is unencrypted at rest**; use the
privacy toggles, retention limits, and clear-all if that matters to you.
Encryption at rest is a possible future addition.

## Architecture

```
clipvault-core (no GTK — unit-testable headless)
├── backend/x11.rs      XFIXES watch, ownership+INCR serving, XTEST paste
├── backend/wayland.rs  wlr-data-control watch/copy, wtype paste
├── storage.rs          SQLite + FTS5, blake3 dedup, retention, eviction
├── config.rs           TOML, XDG paths
└── filter.rs           size caps, password-manager heuristics
clipvault (GTK4/libadwaita app)
├── daemon.rs           glib main loop, backend threads, dispatch
├── ipc.rs              Unix-socket single instance + CLI commands
├── hotkey.rs           X11 grab (all lock-modifier variants)
├── tray.rs             StatusNotifierItem (xfce4-panel/KDE/GNOME)
└── ui/{popup,settings} layer-shell popup · AdwPreferencesDialog
```

## Testing

```bash
cargo test -p clipvault-core        # unit: storage/config/filter
./tests/xvfb_integration.sh         # headless end-to-end under Xvfb
                                    # (capture, image, INCR, serve, dedup,
                                    #  self-suppression, IPC, pin persistence)
```

## Known limitations

- Wayland: no cursor-anchored popup, no built-in hotkey (see table).
- Auto-paste timing is WM-dependent; tune `paste_delay_ms` or use
  `paste_chord = "shift+insert"` for terminals.
- `source_app` labels are best-effort (X11 WM_CLASS).

## Author

**Chamal Bandara** — Cyber Security Engineer
GitHub: [github.com/CRB-CyberSec-Dev](https://github.com/CRB-CyberSec-Dev/)

## License

MIT
