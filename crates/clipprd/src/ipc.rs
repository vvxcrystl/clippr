use std::{
    fs::File,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::UnixStream as StdStream,
        },
    },
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use clippr_core::{
    ipc::{Event, Request, Response},
    paths,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{broadcast, mpsc, oneshot, Semaphore},
};

use crate::Message;
const MAX_LINE: usize = 1024 * 1024;
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Socket {
    pub listener: Option<UnixListener>,
    path: PathBuf,
    _lock: File,
}
impl Socket {
    pub fn bind() -> Result<Self> {
        let path = paths::socket_path();
        std::fs::create_dir_all(path.parent().context("socket has no parent")?)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension("sock.lock"))?;
        anyhow::ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another clipprd holds the socket lock"
        );
        if StdStream::connect(&path).is_ok() {
            bail!("another daemon is listening at {}", path.display());
        }
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                anyhow::ensure!(
                    meta.file_type().is_socket() && meta.uid() == unsafe { libc::geteuid() },
                    "refusing to remove an unowned or non-socket file"
                );
                std::fs::remove_file(&path)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener: Some(listener),
            path,
            _lock: lock,
        })
    }
}
impl Drop for Socket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub async fn serve(
    listener: UnixListener,
    tx: mpsc::Sender<Message>,
    events: broadcast::Sender<Event>,
) {
    let slots = Arc::new(Semaphore::new(32));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        if stream.peer_cred().map(|p| p.uid()).ok() != Some(unsafe { libc::geteuid() }) {
            continue;
        }
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let tx = tx.clone();
        let events = events.subscribe();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = connection(stream, tx, events).await {
                tracing::debug!(error = %e, "IPC client disconnected");
            }
        });
    }
}

async fn line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                bail!("request must end with a newline")
            };
        }
        let len = buf
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(buf.len());
        anyhow::ensure!(line.len() + len <= MAX_LINE, "request exceeds 1 MiB");
        line.extend_from_slice(&buf[..len]);
        let done = line.last() == Some(&b'\n');
        reader.consume(len);
        if done {
            return Ok(Some(line));
        }
    }
}
async fn send<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, response: &Response) -> Result<()> {
    let mut bytes = serde_json::to_vec(response)?;
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(10), writer.write_all(&bytes)).await??;
    Ok(())
}
async fn connection(
    stream: UnixStream,
    tx: mpsc::Sender<Message>,
    events: broadcast::Receiver<Event>,
) -> Result<()> {
    connection_with_deadlines(
        stream,
        tx,
        events,
        FIRST_REQUEST_TIMEOUT,
        IDLE_REQUEST_TIMEOUT,
    )
    .await
}

async fn connection_with_deadlines(
    stream: UnixStream,
    tx: mpsc::Sender<Message>,
    mut events: broadcast::Receiver<Event>,
    first_timeout: Duration,
    idle_timeout: Duration,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut deadline = first_timeout;
    loop {
        // A single deadline covers the whole line, including a trickle of partial bytes.
        let bytes = match tokio::time::timeout(deadline, line(&mut reader)).await? {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(()),
            Err(e) => {
                send(
                    &mut writer,
                    &Response::Error {
                        message: e.to_string(),
                    },
                )
                .await?;
                return Ok(());
            }
        };
        deadline = idle_timeout;
        let request = match serde_json::from_slice::<Request>(&bytes) {
            Ok(req) => req,
            Err(e) => {
                send(
                    &mut writer,
                    &Response::Error {
                        message: e.to_string(),
                    },
                )
                .await?;
                continue;
            }
        };
        let subscribe = request == Request::Subscribe;
        let quit = request == Request::Quit;
        let (reply, response) = oneshot::channel();
        tx.send(Message::Request(Box::new(request), reply)).await?;
        send(&mut writer, &response.await?).await?;
        if quit {
            let _ = tx.send(Message::Shutdown).await;
            return Ok(());
        }
        if subscribe {
            let mut byte = [0];
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Ok(event) => send(&mut writer, &Response::Event { event }).await?,
                        // A slow subscriber must reconnect for a fresh status instead of
                        // retaining an unbounded backlog or silently losing saved events.
                        Err(_) => return Ok(()),
                    },
                    _ = reader.read(&mut byte) => return Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "current_thread")]
    async fn idle_connections_release_their_slots() {
        for partial in [false, true] {
            let (mut client, server) = UnixStream::pair().unwrap();
            let (tx, mut requests) = mpsc::channel(1);
            let (events, _) = broadcast::channel(1);
            let slots = Arc::new(Semaphore::new(1));
            let permit = slots.clone().acquire_owned().await.unwrap();
            let task = tokio::spawn(async move {
                let _permit = permit;
                connection_with_deadlines(
                    server,
                    tx,
                    events.subscribe(),
                    Duration::from_millis(30),
                    Duration::from_millis(60),
                )
                .await
            });
            if partial {
                client.write_all(b"{\"cmd\":\"status\"}\n").await.unwrap();
                let Message::Request(_, reply) = requests.recv().await.unwrap() else {
                    panic!("expected request");
                };
                reply.send(Response::Ok).unwrap();
                let mut reader = BufReader::new(&mut client);
                assert!(line(&mut reader).await.unwrap().is_some());
                // A partial second line must still expire.
                client.write_all(b"{").await.unwrap();
            }
            assert!(tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .is_err());
            assert_eq!(slots.available_permits(), 1);
            let mut byte = [0];
            assert_eq!(client.read(&mut byte).await.unwrap(), 0);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bounded_lines_and_multiple_requests() {
        let mut reader = BufReader::new(&b"{\"cmd\":\"status\"}\n{\"cmd\":\"quit\"}\n"[..]);
        assert_eq!(
            serde_json::from_slice::<Request>(&line(&mut reader).await.unwrap().unwrap()).unwrap(),
            Request::Status
        );
        assert_eq!(
            serde_json::from_slice::<Request>(&line(&mut reader).await.unwrap().unwrap()).unwrap(),
            Request::Quit
        );
        assert!(line(&mut reader).await.unwrap().is_none());
        let data = vec![b'x'; MAX_LINE + 1];
        assert!(line(&mut BufReader::new(data.as_slice())).await.is_err());
    }
}
