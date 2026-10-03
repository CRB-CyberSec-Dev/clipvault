//! Phase 1 gate: `cargo run -p clipvault-core --example capture_debug`
//! then copy text/image anywhere — each clip prints here.

use clipvault_core::backend::{x11::X11Backend, ClipboardBackend, WatchSet};
use clipvault_core::types::BackendEvent;

fn main() {
    let (ev_tx, ev_rx) = async_channel::unbounded::<BackendEvent>();
    let (_cmd_tx, cmd_rx) = async_channel::unbounded();

    std::thread::spawn(move || {
        let mut backend = X11Backend::new().expect("connect X11");
        if let Err(e) = backend.run(
            WatchSet {
                clipboard: true,
                primary: true,
            },
            ev_tx,
            cmd_rx,
        ) {
            eprintln!("backend error: {e}");
        }
    });

    println!("watching clipboard — copy something (Ctrl+C to quit)");
    while let Ok(ev) = ev_rx.recv_blocking() {
        match ev {
            BackendEvent::Ready(name) => println!("[ready] backend: {name}"),
            BackendEvent::Error(e) => eprintln!("[error] {e}"),
            BackendEvent::NewClip(c) => {
                let preview = c
                    .text_content
                    .as_deref()
                    .map(|t| t.chars().take(60).collect::<String>())
                    .unwrap_or_else(|| "<binary>".into());
                println!(
                    "[clip] kind={:?} bytes={} source={:?} text={:?}",
                    c.kind, c.byte_size, c.source_app, preview
                );
            }
        }
    }
}
