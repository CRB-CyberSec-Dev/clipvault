//! X11 backend: XFIXES event-driven capture, selection ownership with
//! INCR serving, and XTEST paste injection.
//!
//! Capture flow:
//!   XFixesSelectSelectionInput on CLIPBOARD (+ PRIMARY when configured) →
//!   XfixesSelectionNotify → skip self → ask owner for TARGETS → pick best
//!   (image/png > text/html > UTF8_STRING > STRING) → fetch with INCR
//!   reassembly → BackendEvent::NewClip.
//!
//! Serve flow (SetClipboard):
//!   fresh timestamp (zero-length property append) → SetSelectionOwner →
//!   answer SelectionRequest events (TARGETS / TIMESTAMP / MULTIPLE / data)
//!   → payloads above the server's request limit are served via INCR
//!   (append-a-chunk on each PropertyNotify DELETE) → SelectionClear ends it.
//!
//! Gotchas honored:
//! - self-notification loop: events owned by our window are ignored
//! - password-manager hint: `x-kde.passwordManagerHint` in TARGETS ⇒ skip
//! - INCR in both directions (read: reassemble; write: chunk on delete)
//! - timestamps: always from the triggering event or the property-append
//!   trick — never CurrentTime for SetSelectionOwner

use super::traits::{BackendError, ClipboardBackend};
use super::WatchSet;
use crate::filter::PASSWORD_HINT_TARGET;
use crate::types::{BackendCommand, BackendEvent, ClipKind, ClipPayload, NewClip, Selection};
use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{
    ConnectionExt as XFixesExt, SelectionEventMask, SelectionNotifyEvent,
};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as Xproto, CreateWindowAux, EventMask, GetPropertyReply,
    Property, PropMode, SelectionClearEvent, SelectionRequestEvent, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as XTestExt;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _; // change_property8/32, maximum_request_bytes
use x11rb::CURRENT_TIME;

/// How long we wait for an owner to answer a conversion before giving up.
const CONVERT_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll timeout — commands get drained at least this often.
const CMD_POLL: Duration = Duration::from_millis(100);
/// INCR transfers get 30s overall before we abandon them.
const INCR_TIMEOUT: Duration = Duration::from_secs(30);

/// Which window currently has input focus (scratch connection — used by
/// the popup right before it grabs focus).
pub fn focused_window() -> Option<u32> {
    let (conn, _) = x11rb::connect(None).ok()?;
    let cookie = conn.get_input_focus().ok()?;
    let reply = cookie.reply().ok()?;
    Some(reply.focus)
}

/// Map a keysym (e.g. 0x76 for 'v') to a keycode on this server.
pub fn keycode_for_keysym(conn: &RustConnection, keysym: u32) -> Option<u8> {
    let setup = conn.setup();
    let (min, count) = (setup.min_keycode, setup.max_keycode - setup.min_keycode + 1);
    let cookie = conn.get_keyboard_mapping(min, count).ok()?;
    let mapping = cookie.reply().ok()?;
    let per = mapping.keysyms_per_keycode as usize;
    for (i, chunk) in mapping.keysyms.chunks(per).enumerate() {
        if chunk.contains(&keysym) {
            return Some(min + i as u8);
        }
    }
    None
}

/// Pointer position in root-window coordinates (X11 only — Wayland
/// deliberately does not expose this).
pub fn pointer_position() -> Option<(i16, i16)> {
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots[screen_num].root;
    let cookie = conn.query_pointer(root).ok()?;
    let r = cookie.reply().ok()?;
    Some((r.root_x, r.root_y))
}

x11rb::atom_manager! {
    pub Atoms: AtomCookies {
        CLIPBOARD,
        PRIMARY,
        TARGETS,
        TIMESTAMP,
        MULTIPLE,
        STRING,
        UTF8_STRING,
        TEXT_HTML: b"text/html",
        IMAGE_PNG: b"image/png",
        INCR,
        PASSWORD_HINT: b"x-kde.passwordManagerHint",
        WM_CLASS,
        CLIPVAULT_DATA,
        ATOM_PAIR,
    }
}

/// State while we own the selection to serve a history item.
struct ServingState {
    payload: ClipPayload,
    plain_text_only: bool,
    owned_at: u32,
    /// Active INCR upload to a requestor, if any.
    incr: Option<IncrServe>,
}

struct IncrServe {
    requestor: u32,
    property: Atom,
    /// Property type the chunks carry (e.g. UTF8_STRING).
    type_: Atom,
    data: Vec<u8>,
    offset: usize,
}

pub struct X11Backend {
    conn: RustConnection,
    window: u32,
    atoms: Atoms,
    atom_names: HashMap<Atom, String>,
    serving: Option<ServingState>,
    paused: bool,
    /// keycode cache: keysym → keycode
    keycodes: HashMap<u32, u8>,
}

impl X11Backend {
    pub fn new() -> Result<Self, BackendError> {
        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| BackendError::Connect(e.to_string()))?;

        // XFIXES 2.0 introduced selection monitoring.
        let ver = conn
            .xfixes_query_version(2, 0)
            .map_err(|e| BackendError::MissingCapability(format!("XFIXES: {e}")))?
            .reply()
            .map_err(|e| BackendError::MissingCapability(format!("XFIXES: {e}")))?;
        if ver.major_version < 2 {
            return Err(BackendError::MissingCapability(format!(
                "XFIXES {}.{} < 2.0",
                ver.major_version, ver.minor_version
            )));
        }

        let atoms = Atoms::new(&conn)
            .map_err(|e| BackendError::Connect(e.to_string()))?
            .reply()
            .map_err(|e| BackendError::Connect(e.to_string()))?;

        let screen = &conn.setup().roots[screen_num];
        let window = conn.generate_id().map_err(|e| BackendError::Other(e.into()))?;
        conn.create_window(
            0,
            window,
            screen.root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| BackendError::Connect(e.to_string()))?;
        conn.flush().map_err(|e| BackendError::Connect(e.to_string()))?;

        Ok(Self {
            conn,
            window,
            atoms,
            atom_names: HashMap::new(),
            serving: None,
            paused: false,
            keycodes: HashMap::new(),
        })
    }

    // ------------------------------------------------------------------
    // main loop
    // ------------------------------------------------------------------

    fn selection_atom(&self, sel: Selection) -> Atom {
        match sel {
            Selection::Clipboard => self.atoms.CLIPBOARD,
            Selection::Primary => self.atoms.PRIMARY,
        }
    }

    fn atom_name(&mut self, atom: Atom) -> String {
        if let Some(n) = self.atom_names.get(&atom) {
            return n.clone();
        }
        let name = self
            .conn
            .get_atom_name(atom)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
            .unwrap_or_else(|| format!("atom#{atom}"));
        self.atom_names.insert(atom, name.clone());
        name
    }

    /// Drain already-queued events, then poll the X fd up to CMD_POLL.
    fn wait(&self) -> Result<Vec<Event>, BackendError> {
        let mut out = Vec::new();
        while let Some(ev) = self
            .conn
            .poll_for_event()
            .map_err(|e| BackendError::Connect(e.to_string()))?
        {
            out.push(ev);
        }
        if !out.is_empty() {
            return Ok(out);
        }

        self.conn.flush().map_err(|e| BackendError::Connect(e.to_string()))?;
        let fd = self.conn.stream().as_raw_fd();
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd points to a valid single-element array.
        let r = unsafe { libc::poll(&mut pfd, 1, CMD_POLL.as_millis() as libc::c_int) };
        if r > 0 {
            while let Some(ev) = self
                .conn
                .poll_for_event()
                .map_err(|e| BackendError::Connect(e.to_string()))?
            {
                out.push(ev);
            }
        } else if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(BackendError::Connect(format!("poll: {err}")));
            }
        }
        Ok(out)
    }

    fn handle_command(&mut self, cmd: BackendCommand) -> bool {
        match cmd {
            BackendCommand::Shutdown => return false,
            BackendCommand::SetClipboard {
                payload,
                plain_text_only,
            } => {
                if let Err(e) = self.set_clipboard(*payload, plain_text_only) {
                    tracing::error!("SetClipboard failed: {e}");
                }
            }
            BackendCommand::SimulatePaste { chord } => {
                if let Err(e) = self.simulate_paste(&chord) {
                    tracing::error!("SimulatePaste failed: {e}");
                }
            }
            BackendCommand::SetPaused(p) => self.paused = p,
            BackendCommand::RefocusWindow(w) => {
                if w != 0 && w != self.window {
                    // Fresh timestamp — the server may reject CurrentTime.
                    if let Ok(t) = self.fresh_timestamp() {
                        let _ = self.conn.set_input_focus(
                            x11rb::protocol::xproto::InputFocus::POINTER_ROOT,
                            w,
                            t,
                        );
                        let _ = self.conn.flush();
                    }
                }
            }
        }
        true
    }

    // ------------------------------------------------------------------
    // capture
    // ------------------------------------------------------------------

    fn fetch_target(
        &mut self,
        selection: Atom,
        target: Atom,
        timestamp: u32,
    ) -> Result<Option<Vec<u8>>, BackendError> {
        let prop = self.atoms.CLIPVAULT_DATA;
        let _ = self.conn.delete_property(self.window, prop);
        self.conn
            .convert_selection(self.window, selection, target, prop, timestamp)
            .map_err(|e| BackendError::Other(e.into()))?;
        self.conn.flush().map_err(|e| BackendError::Connect(e.to_string()))?;

        let deadline = Instant::now() + CONVERT_TIMEOUT;
        loop {
            let ev = self
                .conn
                .wait_for_event()
                .map_err(|e| BackendError::Connect(e.to_string()))?;
            // While blocked here, keep serving any active ownership —
            // some apps re-request as a side effect of our own converts.
            if let Event::SelectionRequest(req) = &ev {
                self.serve_request(req);
                continue;
            }
            if let Event::SelectionNotify(sn) = ev {
                if sn.requestor == self.window && sn.selection == selection {
                    if sn.property == x11rb::NONE {
                        return Ok(None); // owner refused
                    }
                    break;
                }
            }
            if Instant::now() > deadline {
                tracing::warn!("selection conversion timed out");
                return Ok(None);
            }
        }

        let reply = self
            .conn
            .get_property(true, self.window, prop, AtomEnum::NONE, 0, u32::MAX / 4)
            .map_err(|e| BackendError::Other(e.into()))?
            .reply()
            .map_err(|e| BackendError::Other(e.into()))?;

        if reply.type_ == self.atoms.INCR {
            return Ok(Some(self.read_incr(prop)?));
        }
        Ok(Some(reply.value))
    }

    /// Reassemble an INCR transfer: chunks arrive via PropertyNotify; an
    /// empty chunk ends it.
    fn read_incr(&mut self, prop: Atom) -> Result<Vec<u8>, BackendError> {
        let mut data = Vec::new();
        let deadline = Instant::now() + INCR_TIMEOUT;
        loop {
            let ev = self
                .conn
                .wait_for_event()
                .map_err(|e| BackendError::Connect(e.to_string()))?;
            match ev {
                Event::PropertyNotify(pn)
                    if pn.window == self.window
                        && pn.atom == prop
                        && pn.state == Property::NEW_VALUE =>
                {
                    let chunk: GetPropertyReply = self
                        .conn
                        .get_property(true, self.window, prop, AtomEnum::NONE, 0, u32::MAX / 4)
                        .map_err(|e| BackendError::Other(e.into()))?
                        .reply()
                        .map_err(|e| BackendError::Other(e.into()))?;
                    if chunk.value.is_empty() {
                        break; // final empty chunk = done
                    }
                    data.extend_from_slice(&chunk.value);
                }
                _ => {}
            }
            if Instant::now() > deadline {
                return Err(BackendError::Other(anyhow::anyhow!(
                    "INCR read timed out after {} bytes",
                    data.len()
                )));
            }
        }
        Ok(data)
    }

    /// Decode the TARGETS list and decide what to fetch.
    fn pick_target(&mut self, selection: Atom, timestamp: u32) -> Option<(Atom, ClipKind)> {
        let raw = self
            .fetch_target(selection, self.atoms.TARGETS, timestamp)
            .ok()??;
        if raw.len() % 4 != 0 {
            return None;
        }
        let offered: Vec<Atom> = raw
            .chunks_exact(4)
            .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let names: Vec<String> = offered.iter().map(|&a| self.atom_name(a)).collect();

        if names.iter().any(|n| n == PASSWORD_HINT_TARGET) {
            tracing::info!("skipping clip: password-manager hint present");
            return None;
        }

        for (atom, kind) in [
            (self.atoms.IMAGE_PNG, ClipKind::Image),
            (self.atoms.TEXT_HTML, ClipKind::Html),
            (self.atoms.UTF8_STRING, ClipKind::Text),
            (self.atoms.STRING, ClipKind::Text),
        ] {
            if offered.contains(&atom) {
                return Some((atom, kind));
            }
        }
        tracing::debug!("no supported target in {:?}", names);
        None
    }

    /// Best-effort: which app owns the selection (WM_CLASS of owner window).
    fn source_app(&self, owner: u32) -> Option<String> {
        let reply = self
            .conn
            .get_property(false, owner, self.atoms.WM_CLASS, AtomEnum::STRING, 0, 64)
            .ok()?
            .reply()
            .ok()?;
        let text = String::from_utf8_lossy(&reply.value);
        text.split('\0')
            .filter(|s| !s.is_empty())
            .nth(1)
            .map(str::to_string)
            .or_else(|| text.split('\0').find(|s| !s.is_empty()).map(str::to_string))
    }

    fn on_selection_notify(
        &mut self,
        ev: &SelectionNotifyEvent,
        events: &async_channel::Sender<BackendEvent>,
    ) {
        if self.paused {
            return;
        }
        let which = if ev.selection == self.atoms.CLIPBOARD {
            Selection::Clipboard
        } else if ev.selection == self.atoms.PRIMARY {
            Selection::Primary
        } else {
            return;
        };
        let _ = which;

        // Self-change suppression: we own it after a SetClipboard.
        if ev.owner == self.window || ev.owner == 0 {
            return;
        }

        let timestamp = ev.timestamp;
        let source_app = self.source_app(ev.owner);
        let Some((target, kind)) = self.pick_target(ev.selection, timestamp) else {
            return;
        };
        let Ok(Some(bytes)) = self.fetch_target(ev.selection, target, timestamp) else {
            return;
        };

        let clip = match kind {
            ClipKind::Image => NewClip {
                kind,
                text_content: None,
                html_content: None,
                byte_size: bytes.len() as u64,
                image_png: Some(bytes),
                source_app,
            },
            ClipKind::Html => {
                let html = String::from_utf8_lossy(&bytes).into_owned();
                let plain = self
                    .fetch_target(ev.selection, self.atoms.UTF8_STRING, timestamp)
                    .ok()
                    .flatten()
                    .map(|b| String::from_utf8_lossy(&b).into_owned());
                NewClip {
                    kind,
                    text_content: plain.clone().or_else(|| Some(html.clone())),
                    html_content: Some(html),
                    image_png: None,
                    byte_size: bytes.len() as u64,
                    source_app,
                }
            }
            ClipKind::Text => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                NewClip {
                    kind,
                    text_content: Some(text),
                    html_content: None,
                    image_png: None,
                    byte_size: bytes.len() as u64,
                    source_app,
                }
            }
        };

        tracing::debug!(kind = ?clip.kind, size = clip.byte_size, "captured clip");
        let _ = events.send_blocking(BackendEvent::NewClip(clip));
    }

    // ------------------------------------------------------------------
    // serving (we own the clipboard)
    // ------------------------------------------------------------------

    /// Grab a fresh server timestamp via a zero-length property append on
    /// our window (its PropertyNotify carries the time). Never CurrentTime
    /// for SetSelectionOwner — the server can reject or misorder it.
    fn fresh_timestamp(&self) -> Result<u32, BackendError> {
        let prop = self.atoms.CLIPVAULT_DATA;
        self.conn
            .change_property8(PropMode::APPEND, self.window, prop, AtomEnum::STRING, &[])
            .map_err(|e| BackendError::Other(e.into()))?;
        self.conn.flush().map_err(|e| BackendError::Connect(e.to_string()))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let ev = self
                .conn
                .wait_for_event()
                .map_err(|e| BackendError::Connect(e.to_string()))?;
            if let Event::PropertyNotify(pn) = ev {
                if pn.window == self.window && pn.atom == prop {
                    return Ok(pn.time);
                }
            }
            if Instant::now() > deadline {
                return Ok(CURRENT_TIME); // degraded fallback
            }
        }
    }

    fn set_clipboard(
        &mut self,
        payload: ClipPayload,
        plain_text_only: bool,
    ) -> Result<(), BackendError> {
        let time = self.fresh_timestamp()?;
        self.conn
            .set_selection_owner(self.window, self.atoms.CLIPBOARD, time)
            .map_err(|e| BackendError::Other(e.into()))?;
        self.conn.flush().map_err(|e| BackendError::Connect(e.to_string()))?;

        // Verify the server actually gave it to us.
        let owner = self
            .conn
            .get_selection_owner(self.atoms.CLIPBOARD)
            .map_err(|e| BackendError::Other(e.into()))?
            .reply()
            .map_err(|e| BackendError::Other(e.into()))?;
        if owner.owner != self.window {
            return Err(BackendError::Other(anyhow::anyhow!(
                "failed to acquire CLIPBOARD ownership"
            )));
        }

        self.serving = Some(ServingState {
            payload,
            plain_text_only,
            owned_at: time,
            incr: None,
        });
        tracing::debug!("now serving clipboard");
        Ok(())
    }

    /// Data bytes + property format for a requested target, if we serve it.
    /// Returns (type atom, format, bytes).
    fn serve_data(&self, target: Atom) -> Option<(Atom, u8, Vec<u8>)> {
        let s = self.serving.as_ref()?;
        let p = &s.payload;
        if target == self.atoms.TARGETS {
            let mut targets: Vec<Atom> =
                vec![self.atoms.TARGETS, self.atoms.TIMESTAMP, self.atoms.MULTIPLE];
            if p.text.is_some() {
                targets.push(self.atoms.UTF8_STRING);
                targets.push(AtomEnum::STRING.into());
            }
            if !s.plain_text_only && p.html.is_some() {
                targets.push(self.atoms.TEXT_HTML);
            }
            if !s.plain_text_only && p.png.is_some() {
                targets.push(self.atoms.IMAGE_PNG);
            }
            let bytes: Vec<u8> = targets.iter().flat_map(|a| a.to_ne_bytes()).collect();
            return Some((u32::from(AtomEnum::ATOM), 32, bytes));
        }
        if target == self.atoms.TIMESTAMP {
            return Some((u32::from(AtomEnum::INTEGER), 32, s.owned_at.to_ne_bytes().to_vec()));
        }
        if target == self.atoms.UTF8_STRING || target == u32::from(AtomEnum::STRING) {
            return p
                .text
                .as_ref()
                .map(|t| (target, 8, t.clone().into_bytes()));
        }
        if !s.plain_text_only {
            if target == self.atoms.TEXT_HTML {
                return p
                    .html
                    .clone()
                    .map(|h| (target, 8, h.into_bytes()));
            }
            if target == self.atoms.IMAGE_PNG {
                return p.png.clone().map(|b| (target, 8, b));
            }
        }
        None
    }

    /// Answer one SelectionRequest.
    fn serve_request(&mut self, req: &SelectionRequestEvent) {
        let reply_prop = |p: Atom| {
            if p == x11rb::NONE {
                self.atoms.CLIPVAULT_DATA
            } else {
                p
            }
        };
        let property = reply_prop(req.property);

        // MULTIPLE: batch of (target, property) pairs in the requestor's
        // property. GTK pastes use this.
        if req.target == self.atoms.MULTIPLE {
            self.serve_multiple(req, property);
            return;
        }

        match self.serve_data(req.target) {
            Some((type_, format, data)) => {
                let max = self.conn.maximum_request_bytes().saturating_sub(1024);
                if data.len() > max {
                    // INCR: advertise size, then chunk on each DELETE.
                    let ok = self
                        .conn
                        .change_property32(
                            PropMode::REPLACE,
                            req.requestor,
                            property,
                            self.atoms.INCR,
                            &[data.len() as u32],
                        )
                        .is_ok();
                    if ok {
                        if let Some(s) = self.serving.as_mut() {
                            s.incr = Some(IncrServe {
                                requestor: req.requestor,
                                property,
                                type_,
                                data,
                                offset: 0,
                            });
                        }
                    }
                } else {
                    let _ = match format {
                        32 => self.conn.change_property32(
                            PropMode::REPLACE,
                            req.requestor,
                            property,
                            type_,
                            &data
                                .chunks_exact(4)
                                .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                                .collect::<Vec<_>>(),
                        ),
                        _ => self.conn.change_property8(
                            PropMode::REPLACE,
                            req.requestor,
                            property,
                            type_,
                            &data,
                        ),
                    };
                }
                self.send_selection_done(req, property);
            }
            None => self.send_selection_done(req, x11rb::NONE),
        }
        let _ = self.conn.flush();
    }

    fn serve_multiple(&mut self, req: &SelectionRequestEvent, property: Atom) {
        let raw = self
            .conn
            .get_property(false, req.requestor, property, self.atoms.ATOM_PAIR, 0, u32::MAX / 4)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.value)
            .unwrap_or_default();
        let mut pairs: Vec<u32> = raw
            .chunks_exact(4)
            .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let mut changed = Vec::new();
        for pair in pairs.chunks_exact_mut(2) {
            let (target, prop) = (pair[0], pair[1]);
            match self.serve_data(target) {
                Some((type_, fmt, data)) if data.len() <= 1024 * 1024 => {
                    let r = if fmt == 32 {
                        self.conn.change_property32(
                            PropMode::REPLACE,
                            req.requestor,
                            prop,
                            type_,
                            &data
                                .chunks_exact(4)
                                .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                                .collect::<Vec<_>>(),
                        )
                    } else {
                        self.conn.change_property8(
                            PropMode::REPLACE,
                            req.requestor,
                            prop,
                            type_,
                            &data,
                        )
                    };
                    if r.is_err() {
                        pair[1] = x11rb::NONE;
                        changed.push(());
                    }
                }
                // Oversized entries inside MULTIPLE: refuse rather than
                // implementing nested INCR (rare; requestor retries singly).
                _ => {
                    pair[1] = x11rb::NONE;
                    changed.push(());
                }
            }
        }
        if !changed.is_empty() {
            let bytes: Vec<u8> = pairs.iter().flat_map(|a| a.to_ne_bytes()).collect();
            let words: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let _ = self.conn.change_property32(
                PropMode::REPLACE,
                req.requestor,
                property,
                self.atoms.ATOM_PAIR,
                &words,
            );
        }
        self.send_selection_done(req, property);
        let _ = self.conn.flush();
    }

    /// Continue an INCR upload: each PropertyNotify DELETE from the
    /// requestor means "send the next chunk"; when all data has been
    /// delivered, the next DELETE gets a zero-length chunk (end marker).
    fn continue_incr(&mut self, pn: &x11rb::protocol::xproto::PropertyNotifyEvent) {
        let Some(s) = self.serving.as_mut() else { return };
        let Some(incr) = s.incr.as_mut() else { return };
        if pn.window != incr.requestor || pn.atom != incr.property {
            return;
        }
        let (req, prop, type_) = (incr.requestor, incr.property, incr.type_);
        let max = self.conn.maximum_request_bytes().saturating_sub(1024);

        let chunk: Vec<u8> = if incr.offset < incr.data.len() {
            let end = (incr.offset + max).min(incr.data.len());
            let c = incr.data[incr.offset..end].to_vec();
            incr.offset = end;
            c
        } else {
            // All chunks delivered and deleted → terminal empty chunk.
            s.incr = None;
            Vec::new()
        };
        let _ = self
            .conn
            .change_property8(PropMode::APPEND, req, prop, type_, &chunk);
        let _ = self.conn.flush();
    }

    fn send_selection_done(&self, req: &SelectionRequestEvent, property: Atom) {
        let ev = x11rb::protocol::xproto::SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property,
        };
        let _ = self
            .conn
            .send_event(false, req.requestor, EventMask::NO_EVENT, ev);
    }

    // ------------------------------------------------------------------
    // paste injection (XTEST)
    // ------------------------------------------------------------------

    fn keycode_for(&mut self, keysym: u32) -> Option<u8> {
        if let Some(&kc) = self.keycodes.get(&keysym) {
            return Some(kc);
        }
        let setup = self.conn.setup();
        let (min, count) = (
            setup.min_keycode,
            setup.max_keycode - setup.min_keycode + 1,
        );
        let mapping = self
            .conn
            .get_keyboard_mapping(min, count)
            .ok()?
            .reply()
            .ok()?;
        let per = mapping.keysyms_per_keycode as usize;
        for (i, chunk) in mapping.keysyms.chunks(per).enumerate() {
            if chunk.contains(&keysym) {
                let kc = min + i as u8;
                self.keycodes.insert(keysym, kc);
                return Some(kc);
            }
        }
        None
    }

    /// Inject a chord like "ctrl+v" or "shift+insert" via XTEST.
    fn simulate_paste(&mut self, chord: &str) -> Result<(), BackendError> {
        const XK_CONTROL_L: u32 = 0xffe3;
        const XK_SHIFT_L: u32 = 0xffe1;
        const XK_ALT_L: u32 = 0xffe9;
        const XK_SUPER_L: u32 = 0xffeb;
        const XK_INSERT: u32 = 0xff63;
        const XK_RETURN: u32 = 0xff0d;

        let mut mods: Vec<u32> = Vec::new();
        let mut key: Option<u32> = None;
        for part in chord.split('+') {
            match part.trim().to_lowercase().as_str() {
                "ctrl" | "control" => mods.push(XK_CONTROL_L),
                "shift" => mods.push(XK_SHIFT_L),
                "alt" => mods.push(XK_ALT_L),
                "super" | "meta" | "win" => mods.push(XK_SUPER_L),
                "insert" | "ins" => key = Some(XK_INSERT),
                "enter" | "return" => key = Some(XK_RETURN),
                s if s.len() == 1 => key = Some(s.chars().next().unwrap() as u32),
                _ => {}
            }
        }
        let Some(key) = key else {
            return Err(BackendError::Other(anyhow::anyhow!(
                "unparseable paste chord: {chord}"
            )));
        };

        // Resolve all keycodes first (mutably borrows self for the cache),
        // then inject with a closure that only borrows the connection.
        let mod_kcs: Vec<u8> = mods.iter().filter_map(|m| self.keycode_for(*m)).collect();
        let key_kc = self
            .keycode_for(key)
            .ok_or_else(|| BackendError::Other(anyhow::anyhow!("no keycode for {key:#x}")))?;

        let conn = &self.conn;
        let press = |kc: u8, down: bool| -> Result<(), BackendError> {
            conn.xtest_fake_input(
                    if down {
                        x11rb::protocol::xproto::KEY_PRESS_EVENT
                    } else {
                        x11rb::protocol::xproto::KEY_RELEASE_EVENT
                    },
                    kc,
                    CURRENT_TIME,
                    x11rb::NONE,
                    0,
                    0,
                    0,
                )
                .map_err(|e| BackendError::Other(e.into()))
                .map(|_| ())
        };

        for &kc in &mod_kcs {
            press(kc, true)?;
        }
        press(key_kc, true)?;
        press(key_kc, false)?;
        for &kc in mod_kcs.iter().rev() {
            press(kc, false)?;
        }
        self.conn.flush().map_err(|e| BackendError::Connect(e.to_string()))?;
        tracing::debug!("injected paste chord {chord}");
        Ok(())
    }
}

impl ClipboardBackend for X11Backend {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn run(
        &mut self,
        watch: WatchSet,
        events: async_channel::Sender<BackendEvent>,
        commands: async_channel::Receiver<BackendCommand>,
    ) -> Result<(), BackendError> {
        for sel in watch.selections() {
            self.conn
                .xfixes_select_selection_input(
                    self.window,
                    self.selection_atom(sel),
                    SelectionEventMask::SET_SELECTION_OWNER
                        | SelectionEventMask::SELECTION_WINDOW_DESTROY
                        | SelectionEventMask::SELECTION_CLIENT_CLOSE,
                )
                .map_err(|e| BackendError::Other(e.into()))?;
        }
        // We must see SelectionRequest + SelectionClear; they go to the
        // selection owner (us) automatically. PropertyNotify comes from our
        // window mask.
        self.conn.flush().map_err(|e| BackendError::Connect(e.to_string()))?;
        let _ = events.send_blocking(BackendEvent::Ready("x11"));

        loop {
            while let Ok(cmd) = commands.try_recv() {
                if !self.handle_command(cmd) {
                    return Ok(());
                }
            }
            for ev in self.wait()? {
                match ev {
                    Event::XfixesSelectionNotify(sn) => self.on_selection_notify(&sn, &events),
                    Event::SelectionRequest(req) => self.serve_request(&req),
                    Event::SelectionClear(sc) => {
                        if sc.selection == self.atoms.CLIPBOARD {
                            self.serving = None;
                        }
                        let _: &SelectionClearEvent = &sc;
                    }
                    Event::PropertyNotify(pn) if pn.state == Property::DELETE => {
                        self.continue_incr(&pn)
                    }
                    _ => {}
                }
            }
        }
    }
}
