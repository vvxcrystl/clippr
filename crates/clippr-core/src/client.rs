//! Blocking IPC client used by the CLI and GUI.
//!
//! OWNER: backend (library agent).

use crate::ipc::{Event, Request, Response};

pub struct Client {
    _private: (),
}

impl Client {
    /// Connect to the daemon socket. Fails fast (no retry) if the daemon is not running.
    pub fn connect() -> anyhow::Result<Self> {
        todo!()
    }

    /// Send one request and wait for its response. `Response::Error` becomes `Err`.
    pub fn request(&mut self, req: &Request) -> anyhow::Result<Response> {
        let _ = req;
        todo!()
    }

    /// Switch this connection into event mode. The iterator ends when the daemon disconnects.
    pub fn subscribe(self) -> anyhow::Result<impl Iterator<Item = anyhow::Result<Event>>> {
        Ok(std::iter::empty())
    }
}
