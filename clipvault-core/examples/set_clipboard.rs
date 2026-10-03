//! Phase 3 gate: `cargo run -p clipvault-core --example set_clipboard -- "some text"`
//! takes CLIPBOARD ownership; verify in another shell with `xclip -selection clipboard -o`.

use clipvault_core::backend::{x11::X11Backend, ClipboardBackend, WatchSet};
use clipvault_core::types::{BackendCommand, BackendEvent, ClipPayload};

fn main() {
    let text = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "clipvault owns the clipboard now".to_string());
    let large: Option<String> = std::env::args()
        .nth(2)
        .map(|n| "Y".repeat(n.parse().unwrap_or(1_000_000)));

    let (ev_tx, ev_rx) = async_channel::unbounded::<BackendEvent>();
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<BackendCommand>();

    std::thread::spawn(move || {
        let mut backend = X11Backend::new().expect("connect X11");
        backend
            .run(WatchSet { clipboard: true, primary: false }, ev_tx, cmd_rx)
            .unwrap();
    });

    // Wait for Ready, then take ownership.
    while let Ok(ev) = ev_rx.recv_blocking() {
        if matches!(ev, BackendEvent::Ready(_)) {
            break;
        }
    }
    let payload = ClipPayload {
        text: large.or(Some(text)),
        html: None,
        png: None,
    };
    let size = payload.text.as_ref().map(|t| t.len()).unwrap_or(0);
    cmd_tx
        .send_blocking(BackendCommand::SetClipboard {
            payload: Box::new(payload),
            plain_text_only: false,
        })
        .unwrap();
    println!("serving {size} bytes as clipboard owner — check with: xclip -selection clipboard -o");
    println!("(Ctrl+C to quit)");

    // Print any captures (another app copying = we lose ownership).
    while let Ok(ev) = ev_rx.recv_blocking() {
        if let BackendEvent::NewClip(c) = ev {
            println!(
                "[captured] {:?} {:?}",
                c.kind,
                c.text_content.as_deref().map(|t| t.chars().take(40).collect::<String>())
            );
        }
    }
}
