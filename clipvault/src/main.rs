//! clipvault: Win+V-style clipboard manager for Linux.
//!
//! One binary, several modes:
//!   clipvault daemon    — run the background service (what systemd starts)
//!   clipvault toggle    — show/hide the history popup (bind this to Super+V)
//!   clipvault settings  — open the settings window
//!   clipvault select N  — put history item N on the clipboard
//!   clipvault list      — print history (reads the DB directly)
//!   clipvault clear     — clear history (pinned items survive)
//!   clipvault status    — daemon/backend info

use clap::{Parser, Subcommand};

mod daemon;
mod hotkey;
mod ipc;
mod tray;
mod ui;

#[derive(Parser)]
#[command(name = "clipvault", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the background daemon (foreground; systemd manages it).
    Daemon,
    /// Toggle the history popup (sends to the running daemon).
    Toggle,
    /// Show the history popup.
    Show,
    /// Hide the history popup.
    Hide,
    /// Open the settings window.
    Settings,
    /// Put a history item on the clipboard.
    Select {
        id: i64,
        /// Offer plain text only (strip HTML/image targets).
        #[arg(long)]
        plain: bool,
    },
    /// List history items (reads the DB directly; daemon not required).
    List,
    /// Clear history (pinned items survive unless --all).
    Clear {
        #[arg(long)]
        all: bool,
    },
    /// Show daemon/backend status.
    Status,
    /// Debug: run the clipboard backend and print captured clips.
    CaptureDebug,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clipvault=info,clipvault_core=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Command::Toggle) {
        Command::Daemon => daemon::run(),
        Command::List => list(),
        Command::CaptureDebug => capture_debug(),
        other => {
            let cmd = match other {
                Command::Toggle => ipc::IpcCommand::Toggle,
                Command::Show => ipc::IpcCommand::Show,
                Command::Hide => ipc::IpcCommand::Hide,
                Command::Settings => ipc::IpcCommand::Settings,
                Command::Select { id, plain } => ipc::IpcCommand::Select { id, plain },
                Command::Clear { all } => ipc::IpcCommand::Clear { all },
                Command::Status => ipc::IpcCommand::Status,
                _ => unreachable!(),
            };
            if !ipc::try_send(&cmd)? {
                eprintln!("clipvault daemon is not running — start it with `clipvault daemon`");
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

/// `clipvault list`: read-only view straight from the DB.
fn list() -> anyhow::Result<()> {
    use clipvault_core::config::Config;
    use clipvault_core::storage::Storage;
    use clipvault_core::types::ClipKind;

    let st = Storage::open(&Config::db_path()?, &Config::images_dir()?)?;
    for it in st.list(100)? {
        let pin = if it.pinned { "📌" } else { "  " };
        let kind = match it.kind {
            ClipKind::Text => "text ",
            ClipKind::Html => "html ",
            ClipKind::Image => "image",
        };
        let preview: String = it
            .text_content
            .as_deref()
            .unwrap_or("<binary>")
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(70)
            .collect();
        // Ignore EPIPE so `clipvault list | head` doesn't panic.
        use std::io::Write;
        let _ = writeln!(
            std::io::stdout(),
            "{pin} {:>4} [{kind}] {preview}",
            it.id
        );
    }
    Ok(())
}

/// Debug helper: watch the clipboard and print every captured clip.
fn capture_debug() -> anyhow::Result<()> {
    use clipvault_core::backend::{x11::X11Backend, ClipboardBackend, WatchSet};
    use clipvault_core::types::BackendEvent;

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
    Ok(())
}
