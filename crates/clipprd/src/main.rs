mod capture;
mod focus;
mod ipc;
mod portal;
mod ring;
mod save;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use clippr_core::{
    clip::ClipMeta,
    config::{Config, EncoderPref},
    ipc::{CaptureState, Event, Request, Response, Status},
    paths,
};
use tokio::{
    process::Child,
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};
use tracing::{info, warn};

enum Message {
    Request(Box<Request>, oneshot::Sender<Response>),
    Hotkey {
        id: String,
        activation_token: Option<String>,
    },
    Saved(Box<Saved>),
    Shutdown,
}
struct Saved {
    result: Result<ClipMeta>,
    config: Config,
    reply: Option<oneshot::Sender<Response>>,
}
struct SetupError {
    error: anyhow::Error,
    hardware: bool,
}
enum CaptureEvent {
    Ready(u64),
    Failed(u64, String, bool),
}

struct Daemon {
    config: Config,
    ring: Arc<Mutex<ring::Ring>>,
    capture: Option<capture::Capture>,
    setup: Option<JoinHandle<std::result::Result<capture::Capture, SetupError>>>,
    hotkeys: Option<JoinHandle<()>>,
    overlay: Option<Child>,
    kwin: Option<focus::Kwin>,
    events: broadcast::Sender<Event>,
    tx: mpsc::Sender<Message>,
    capture_tx: mpsc::UnboundedSender<CaptureEvent>,
    state: CaptureState,
    error: Option<String>,
    paused: bool,
    test: bool,
    software: bool,
    generation: u64,
    failures: usize,
    recording_since: Option<Instant>,
    retry: Option<Instant>,
    watchdog: Option<Instant>,
    saves: usize,
}
impl Daemon {
    fn status(&self) -> Status {
        let ring = lock(&self.ring);
        Status {
            version: clippr_core::VERSION.into(),
            state: self.state,
            buffered_secs: ring.duration(),
            buffer_bytes: ring.bytes,
            encoder: self.capture.as_ref().map(|c| c.encoder.clone()),
            width: self.capture.as_ref().map(|c| c.width).unwrap_or(0),
            height: self.capture.as_ref().map(|c| c.height).unwrap_or(0),
            fps: self.config.video.fps,
            last_error: self.error.clone(),
        }
    }
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
    fn state_changed(&self) {
        self.emit(Event::StatusChanged {
            status: self.status(),
        });
    }
    fn stop_capture(&mut self) {
        self.generation += 1;
        if let Some(setup) = self.setup.take() {
            setup.abort();
        }
        self.capture.take();
        self.retry = None;
        self.watchdog = None;
        self.recording_since = None;
        // New history prevents an aborted startup's callbacks touching a new generation.
        self.ring = Arc::new(Mutex::new(ring::Ring::new(
            self.config.general.buffer_secs(),
        )));
        // Encoded history can leave large freed arenas behind. Return those pages
        // once at teardown; never run allocator maintenance on the frame path.
        #[cfg(target_env = "gnu")]
        unsafe {
            libc::malloc_trim(0);
        }
    }
    fn start_capture(&mut self) {
        self.stop_capture();
        self.state = if self.paused {
            CaptureState::Paused
        } else {
            CaptureState::Starting
        };
        self.error = None;
        if !self.paused {
            let config = self.config.clone();
            let ring = self.ring.clone();
            let events = self.capture_tx.clone();
            let generation = self.generation;
            let test = self.test;
            let software = self.software || config.video.encoder == EncoderPref::Software;
            self.setup = Some(tokio::spawn(async move {
                let screen = if test {
                    None
                } else {
                    Some(portal::screen(&config).await.map_err(|error| SetupError {
                        error: error.context("ScreenCast portal"),
                        hardware: false,
                    })?)
                };
                capture::build(&config, screen, test, software, ring, events, generation).map_err(
                    |error| SetupError {
                        error,
                        hardware: !software,
                    },
                )
            }));
        }
        self.state_changed();
    }
    fn failed(&mut self, error: String, allow_fallback: bool) {
        if self
            .recording_since
            .is_some_and(|since| since.elapsed() >= Duration::from_secs(30))
        {
            self.failures = 0;
        }
        let try_software =
            self.config.video.encoder == EncoderPref::Auto && !self.software && allow_fallback;
        self.stop_capture();
        self.state = CaptureState::Error;
        warn!(%error, "capture failed");
        self.error = Some(error);
        if try_software {
            self.software = true;
            self.retry = Some(Instant::now());
            warn!("auto encoder will retry with software H.264");
        } else {
            let seconds = [2, 5, 15, 60][self.failures.min(3)];
            self.failures = self.failures.saturating_add(1);
            self.retry = Some(Instant::now() + Duration::from_secs(seconds));
        }
        self.state_changed();
    }
    fn bind_hotkeys(&mut self) {
        if let Some(task) = self.hotkeys.take() {
            task.abort();
        }
        if self.test {
            return;
        }
        let config = self.config.clone();
        let tx = self.tx.clone();
        self.hotkeys = Some(tokio::spawn(async move {
            if let Err(e) = portal::hotkeys(config, tx).await {
                warn!(error = %e, "global shortcuts unavailable; compositor bindings can call clippr");
            }
        }));
    }
    async fn toggle_overlay(&mut self, activation_token: Option<&str>) -> Result<()> {
        if let Some(mut child) = self.overlay.take() {
            if child.try_wait()?.is_none() {
                if let Some(pid) = child.id() {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                }
                // SIGCHLD-driven reaping, with a bounded grace period for an unresponsive UI.
                tokio::spawn(async move {
                    if tokio::time::timeout(Duration::from_secs(2), child.wait())
                        .await
                        .is_err()
                    {
                        let _ = child.kill().await;
                    }
                });
                return Ok(());
            }
        }
        let adjacent = std::env::current_exe()?.with_file_name("clippr-gui");
        let program = if adjacent.is_file() {
            adjacent
        } else {
            "clippr-gui".into()
        };
        self.overlay = Some(
            overlay_command(&program, activation_token)
                .spawn()
                .context("launching clippr-gui --overlay")?,
        );
        Ok(())
    }
    fn save(&mut self, seconds: Option<u32>, reply: Option<oneshot::Sender<Response>>) {
        let result = (|| {
            anyhow::ensure!(self.saves < 2, "two clips are already being saved");
            let seconds = seconds.unwrap_or(self.config.general.clip_short_secs);
            anyhow::ensure!(
                seconds > 0 && seconds <= 3600,
                "save length must be between 1 and 3600 seconds"
            );
            let capture = self.capture.as_ref().context("capture is not running")?;
            let snapshot = lock(&self.ring).snapshot(seconds)?;
            Ok((snapshot, capture.codec.clone()))
        })();
        let (snapshot, codec) = match result {
            Ok(value) => value,
            Err(e) => {
                if let Some(reply) = reply {
                    let _ = reply.send(Response::Error {
                        message: format!("{e:#}"),
                    });
                } else {
                    warn!(error = %e, "cannot save clip");
                }
                return;
            }
        };
        self.saves += 1;
        let config = self.config.clone();
        let cached = self.kwin.as_ref().and_then(|k| lock(&k.focus.0).clone());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let app = focus::focused(cached).await;
            let worker_config = config.clone();
            let (done, result) = oneshot::channel();
            let spawn = std::thread::Builder::new()
                .name("clippr-save".into())
                .spawn(move || {
                    let result = save::save(
                        snapshot,
                        &worker_config,
                        app,
                        codec,
                        worker_config.video.fps,
                    );
                    let _ = done.send(result);
                });
            let result = match spawn {
                Ok(thread) => {
                    drop(thread);
                    result.await.unwrap_or_else(|e| Err(e.into()))
                }
                Err(e) => Err(e.into()),
            };
            let _ = tx
                .send(Message::Saved(Box::new(Saved {
                    result,
                    config,
                    reply,
                })))
                .await;
        });
    }
    fn saved(&mut self, saved: Saved) {
        let Saved {
            result,
            config,
            reply,
        } = saved;
        self.saves -= 1;
        match result {
            Ok(clip) => {
                info!(id = %clip.id, duration = clip.duration_secs, "clip saved");
                self.emit(Event::ClipSaved { clip: clip.clone() });
                if let Some(reply) = reply {
                    let _ = reply.send(Response::Saved { clip: clip.clone() });
                }
                let thumb_clip = clip.clone();
                let notify = config.general.notify_on_save;
                let _ = std::thread::Builder::new()
                    .name("clippr-thumbnail".into())
                    .spawn(move || {
                        if let Err(e) = save::thumbnail(&thumb_clip, &config) {
                            warn!(error = %e, "thumbnail failed");
                        }
                    });
                if notify {
                    tokio::spawn(async move {
                        if let Err(e) = save::notify(&clip).await {
                            tracing::debug!(error = %e, "notification unavailable");
                        }
                    });
                }
            }
            Err(e) => {
                warn!(error = %e, "save failed");
                if let Some(reply) = reply {
                    let _ = reply.send(Response::Error {
                        message: format!("{e:#}"),
                    });
                }
            }
        }
    }
    async fn request(&mut self, req: Request, reply: oneshot::Sender<Response>) {
        let response: Result<Response> = async {
            Ok(match req {
                Request::Status => Response::Status {
                    status: self.status(),
                },
                Request::Subscribe => Response::Event {
                    event: Event::StatusChanged {
                        status: self.status(),
                    },
                },
                Request::GetConfig => Response::Config {
                    config: self.config.clone(),
                },
                Request::SetConfig { config } => {
                    focus::validate_config(&config)?;
                    config.save()?;
                    let restart =
                        config.video != self.config.video || config.audio != self.config.audio;
                    let rebind = config.hotkeys != self.config.hotkeys;
                    self.config = config;
                    if restart {
                        self.software = false;
                        self.failures = 0;
                        self.start_capture();
                    } else {
                        let mut ring = lock(&self.ring);
                        ring.limit_ns =
                            u64::from(self.config.general.buffer_secs()) * 1_000_000_000;
                        ring.trim();
                    }
                    if rebind {
                        self.bind_hotkeys();
                    }
                    self.emit(Event::ConfigChanged {
                        config: self.config.clone(),
                    });
                    Response::Ok
                }
                Request::SetPaused { paused } => {
                    if self.paused != paused {
                        self.paused = paused;
                        self.failures = 0;
                        self.start_capture();
                    }
                    Response::Ok
                }
                Request::ToggleOverlay => {
                    self.toggle_overlay(None).await?;
                    Response::Ok
                }
                Request::ResetCaptureSource => {
                    match std::fs::remove_file(paths::state_dir().join("restore_token")) {
                        Ok(()) => (),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                        Err(e) => return Err(e.into()),
                    }
                    self.paused = false;
                    self.software = false;
                    self.failures = 0;
                    self.start_capture();
                    Response::Ok
                }
                Request::Quit => Response::Ok,
                Request::Save { .. } => unreachable!(),
            })
        }
        .await;
        let _ = reply.send(response.unwrap_or_else(|e| Response::Error {
            message: format!("{e:#}"),
        }));
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut verbose = false;
    let mut test = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("clipprd {}", clippr_core::VERSION);
                return Ok(());
            }
            "--verbose" | "-v" => verbose = true,
            "--test-source" => test = true,
            "--help" | "-h" => {
                println!(
                    "clipprd [--version] [--verbose] [--test-source]\n--test-source uses synthetic video/audio; never opens the capture picker or binds hotkeys."
                );
                return Ok(());
            }
            _ => anyhow::bail!("unknown flag: {arg}"),
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                if verbose {
                    "clipprd=debug".into()
                } else {
                    "info".into()
                }
            }),
        )
        .with_target(false)
        .init();
    unsafe {
        libc::umask(0o077);
        // Also use background priority when launched without the systemd unit.
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
    let config = Config::load()?;
    focus::validate_config(&config)?;
    let mut socket = ipc::Socket::bind()?;
    gstreamer::init()?;
    let (tx, mut rx) = mpsc::channel(64);
    let (capture_tx, mut capture_rx) = mpsc::unbounded_channel();
    let (events, _) = broadcast::channel(64);
    let server = tokio::spawn(ipc::serve(
        socket
            .listener
            .take()
            .context("socket listener is missing")?,
        tx.clone(),
        events.clone(),
    ));
    let kwin = if !test
        && std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .contains("KDE")
    {
        match tokio::time::timeout(Duration::from_secs(3), focus::Kwin::start()).await {
            Ok(Ok(kwin)) => Some(kwin),
            Ok(Err(e)) => {
                warn!(error = %e, "KWin focus tracking unavailable");
                None
            }
            Err(e) => {
                warn!(error = %e, "KWin focus tracking timed out");
                None
            }
        }
    } else {
        None
    };
    let mut daemon = Daemon {
        paused: !config.general.start_recording,
        ring: Arc::new(Mutex::new(ring::Ring::new(config.general.buffer_secs()))),
        config,
        capture: None,
        setup: None,
        hotkeys: None,
        overlay: None,
        kwin,
        events,
        tx,
        capture_tx,
        state: CaptureState::Paused,
        error: None,
        test,
        software: false,
        generation: 0,
        failures: 0,
        recording_since: None,
        retry: None,
        watchdog: None,
        saves: 0,
    };
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    if !test {
        if let Err(error) = portal::prepare_identity() {
            warn!(%error, "portal desktop identity unavailable");
        }
    }
    daemon.bind_hotkeys();
    daemon.start_capture();
    info!(socket = %paths::socket_path().display(), test_source = test, "clipprd ready");
    loop {
        tokio::select! {
            _ = interrupt.recv() => break,
            _ = terminate.recv() => break,
            Some(message) = rx.recv() => match message {
                Message::Request(req, reply) => match *req {
                    Request::Save { seconds } => daemon.save(seconds, Some(reply)),
                    req => daemon.request(req, reply).await,
                },
                Message::Hotkey { id, activation_token } => match id.as_str() {
                    "clip-short" => daemon.save(Some(daemon.config.general.clip_short_secs), None),
                    "clip-long" => daemon.save(Some(daemon.config.general.clip_long_secs), None),
                    "toggle-overlay" => if let Err(e) = daemon.toggle_overlay(activation_token.as_deref()).await { warn!(error = %e, "overlay failed"); },
                    _ => (),
                },
                Message::Saved(saved) => {
                    daemon.saved(*saved);
                }
                Message::Shutdown => break,
            },
            Some(event) = capture_rx.recv() => match event {
                CaptureEvent::Ready(generation) if generation == daemon.generation => {
                    daemon.state = CaptureState::Recording;
                    daemon.error = None;
                    daemon.recording_since = Some(Instant::now());
                    daemon.watchdog = None;
                    daemon.state_changed();
                }
                CaptureEvent::Failed(generation, error, fallback) if generation == daemon.generation => daemon.failed(error, fallback),
                _ => (),
            },
            result = async { match daemon.setup.as_mut() { Some(setup) => setup.await, None => std::future::pending().await } }, if daemon.setup.is_some() => {
                // Leave setup present until failed() decides whether auto can fall back.
                match result {
                    Ok(Ok(capture)) => {
                        info!(encoder = %capture.encoder, width = capture.width, height = capture.height, "capture pipeline started");
                        daemon.setup = None;
                        daemon.capture = Some(capture);
                        if daemon.state != CaptureState::Recording { daemon.watchdog = Some(Instant::now() + Duration::from_secs(15)); }
                        daemon.state_changed();
                    }
                    Ok(Err(e)) => daemon.failed(format!("{:#}", e.error), e.hardware),
                    Err(e) => daemon.failed(format!("capture setup failed: {e}"), false),
                }
            },
            _ = deadline(daemon.retry), if daemon.retry.is_some() => daemon.start_capture(),
            _ = deadline(daemon.watchdog), if daemon.watchdog.is_some() => { let missing_video = lock(&daemon.ring).video.is_empty(); daemon.failed("capture did not produce all enabled streams in 15 seconds".into(), missing_video); },
            _ = async { match daemon.overlay.as_mut() { Some(child) => child.wait().await, None => std::future::pending().await } }, if daemon.overlay.is_some() => { daemon.overlay.take(); },
        }
    }
    info!("shutting down");
    daemon.stop_capture();
    if let Some(task) = daemon.hotkeys.take() {
        task.abort();
    }
    if let Some(mut child) = daemon.overlay.take() {
        let _ = child.kill().await;
    }
    if let Some(kwin) = daemon.kwin.take() {
        kwin.stop().await;
    }
    // Let cancelled portal futures run their session-close guards before runtime teardown.
    tokio::task::yield_now().await;
    // Finish snapshots already accepted by IPC before exiting. Capture is already
    // stopped, and waiting on the channel costs no wakeups while the muxers work.
    let shutdown_limit = Instant::now() + Duration::from_secs(125);
    while daemon.saves != 0 {
        match tokio::time::timeout_at(shutdown_limit, rx.recv()).await {
            Ok(Some(Message::Saved(saved))) => daemon.saved(*saved),
            Ok(Some(Message::Request(_, reply))) => {
                let _ = reply.send(Response::Error {
                    message: "daemon is shutting down".into(),
                });
            }
            Ok(Some(_)) => (),
            _ => {
                warn!("timed out finishing clip saves during shutdown");
                break;
            }
        }
    }
    tokio::task::yield_now().await;
    server.abort();
    drop(socket);
    Ok(())
}
async fn deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}

fn overlay_command(
    program: &std::path::Path,
    activation_token: Option<&str>,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command
        .arg("--overlay")
        // Startup credentials belong to one activation, never the daemon's lifetime.
        .env_remove("XDG_ACTIVATION_TOKEN")
        .env_remove("DESKTOP_STARTUP_ID")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(token) = activation_token {
        command.env("XDG_ACTIVATION_TOKEN", token);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_uses_only_the_current_shortcut_activation_token() {
        for token in [None, Some("current-shortcut-token")] {
            let command = overlay_command(std::path::Path::new("clippr-gui"), token);
            let env: std::collections::HashMap<_, _> = command.as_std().get_envs().collect();
            assert_eq!(
                env[std::ffi::OsStr::new("XDG_ACTIVATION_TOKEN")],
                token.map(std::ffi::OsStr::new)
            );
            assert_eq!(env[std::ffi::OsStr::new("DESKTOP_STARTUP_ID")], None);
        }
    }
}

// A poisoned mutex still contains valid packet history; errors must not abort capture.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
