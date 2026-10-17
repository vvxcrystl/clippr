//! Blocking newline-delimited JSON IPC client.
use crate::ipc::{Event, Request, Response};
use anyhow::{bail, Context};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub struct Client {
    stream: BufReader<UnixStream>,
}
impl Client {
    pub fn connect() -> anyhow::Result<Self> {
        Self::from_stream(
            UnixStream::connect(crate::paths::socket_path()).context(
                "clipprd is not running; start it with `systemctl --user start clipprd`",
            )?,
        )
    }
    fn from_stream(stream: UnixStream) -> anyhow::Result<Self> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self {
            stream: BufReader::new(stream),
        })
    }
    fn send(&mut self, req: &Request) -> anyhow::Result<()> {
        serde_json::to_writer(self.stream.get_mut(), req)?;
        self.stream.get_mut().write_all(b"\n")?;
        self.stream.get_mut().flush()?;
        Ok(())
    }
    pub fn request(&mut self, req: &Request) -> anyhow::Result<Response> {
        self.send(req)?;
        read_response(&mut self.stream)?.context("daemon disconnected")
    }
    pub fn subscribe(mut self) -> anyhow::Result<impl Iterator<Item = anyhow::Result<Event>>> {
        self.stream.get_ref().set_read_timeout(None)?;
        self.send(&Request::Subscribe)?;
        let mut ended = false;
        Ok(std::iter::from_fn(move || {
            if ended {
                return None;
            }
            loop {
                match read_response(&mut self.stream) {
                    Ok(Some(Response::Event { event })) => return Some(Ok(event)),
                    Ok(Some(Response::Ok)) => continue,
                    Ok(Some(_)) => {
                        ended = true;
                        return Some(Err(anyhow::anyhow!("unexpected subscription response")));
                    }
                    Ok(None) => {
                        ended = true;
                        return None;
                    }
                    Err(e) => {
                        ended = true;
                        return Some(Err(e));
                    }
                }
            }
        }))
    }
}
fn read_response(reader: &mut impl BufRead) -> anyhow::Result<Option<Response>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    match serde_json::from_str(&line)? {
        Response::Error { message } => bail!("{message}"),
        response => Ok(Some(response)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requests_and_events() -> anyhow::Result<()> {
        let (client, server) = UnixStream::pair()?;
        let worker = std::thread::spawn(move || {
            let mut server = BufReader::new(server);
            let mut line = String::new();
            server.read_line(&mut line).unwrap();
            assert_eq!(
                serde_json::from_str::<Request>(&line).unwrap(),
                Request::Status
            );
            server
                .get_mut()
                .write_all(b"{\"type\":\"error\",\"message\":\"test error\"}\n")
                .unwrap();
            line.clear();
            server.read_line(&mut line).unwrap();
            assert_eq!(
                serde_json::from_str::<Request>(&line).unwrap(),
                Request::Subscribe
            );
            server.get_mut().write_all(b"{\"type\":\"ok\"}\n{\"type\":\"event\",\"event\":{\"kind\":\"config_changed\",\"config\":{}}}\n").unwrap();
        });
        let mut client = Client::from_stream(client)?;
        assert!(client
            .request(&Request::Status)
            .unwrap_err()
            .to_string()
            .contains("test error"));
        let events: Vec<_> = client.subscribe()?.collect();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Ok(Event::ConfigChanged { .. })));
        worker.join().unwrap();
        Ok(())
    }
}
