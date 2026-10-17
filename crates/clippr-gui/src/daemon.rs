//! Async wrappers around the blocking IPC client, so the UI thread never waits on a socket.

use std::time::Duration;

use anyhow::{anyhow, bail};
use clippr_core::client::Client;
use clippr_core::config::Config;
use clippr_core::ipc::{Event, Request, Response, Status};
use gtk::gio;

/// Send one request on a fresh connection from a worker thread.
pub async fn call(req: Request) -> anyhow::Result<Response> {
    gio::spawn_blocking(move || Client::connect()?.request(&req))
        .await
        .unwrap_or_else(|_| Err(anyhow!("IPC worker panicked")))
}

pub enum Update {
    Online { status: Status, config: Config },
    Event(Event),
    Offline,
}

/// Follow the daemon for as long as the receiver lives. Emits `Online` with a fresh snapshot
/// on every (re)connect, then events, and `Offline` once when the connection drops.
pub fn watch() -> async_channel::Receiver<Update> {
    let (tx, rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("clippr-ipc".into())
        .spawn(move || {
            let mut online = true; // so the first failure reports Offline
            while !tx.is_closed() {
                let mut session = || -> anyhow::Result<()> {
                    let mut client = Client::connect()?;
                    let Response::Status { status } = client.request(&Request::Status)? else {
                        bail!("unexpected reply to status");
                    };
                    let Response::Config { config } = client.request(&Request::GetConfig)? else {
                        bail!("unexpected reply to get_config");
                    };
                    online = true;
                    tx.send_blocking(Update::Online { status, config })?;
                    for event in client.subscribe()? {
                        tx.send_blocking(Update::Event(event?))?;
                    }
                    Ok(())
                };
                let _ = session();
                if online {
                    online = false;
                    if tx.send_blocking(Update::Offline).is_err() {
                        break;
                    }
                }
                // Only runs while a GUI window is open, so a slow retry costs nothing.
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("spawn IPC thread");
    rx
}
