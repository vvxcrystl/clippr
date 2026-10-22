//! System tray icon (StatusNotifierItem) showing capture state, with a menu for clipping,
//! pausing and opening the app.
//!
//! The tray lives in the daemon because the GUI is never resident. It talks to the main loop
//! through the same messages as hotkeys and IPC, and only redraws when the status or config
//! changes, so it adds no periodic wakeups.

use std::path::PathBuf;

use clippr_core::config::Config;
use clippr_core::ipc::{CaptureState, Event, Request, Response, Status};
use ksni::menu::{MenuItem, StandardItem};
use ksni::TrayMethods;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, warn};

use crate::Message;

/// Icons are written into a private theme dir advertised through `IconThemePath`. They carry
/// Plasma's `ColorScheme-Text` class, so the panel recolors the frame to match its theme.
const ICONS: [(&str, &str); 4] = [
    ("clippr-tray-recording", RECORDING_SVG),
    ("clippr-tray-paused", PAUSED_SVG),
    ("clippr-tray-starting", STARTING_SVG),
    ("clippr-tray-error", ERROR_SVG),
];

const FRAME: &str = r#"<style id="current-color-scheme" type="text/css">.ColorScheme-Text { color:#fcfcfc; }</style>
  <rect class="ColorScheme-Text" x="2.75" y="4.75" width="16.5" height="12.5" rx="3" style="fill:none;stroke:currentColor;stroke-width:1.5"/>"#;

const RECORDING_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22">
  FRAME
  <circle cx="11" cy="11" r="3.5" fill="#da4453"/>
</svg>"##;

const PAUSED_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22">
  FRAME
  <rect class="ColorScheme-Text" x="8.25" y="8" width="2" height="6" rx="0.5" style="fill:currentColor"/>
  <rect class="ColorScheme-Text" x="11.75" y="8" width="2" height="6" rx="0.5" style="fill:currentColor"/>
</svg>"#;

const STARTING_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22">
  FRAME
  <circle class="ColorScheme-Text" cx="11" cy="11" r="3" style="fill:none;stroke:currentColor;stroke-width:1.5;opacity:0.7"/>
</svg>"#;

const ERROR_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22">
  FRAME
  <rect x="10.25" y="7.25" width="1.5" height="4.75" rx="0.75" fill="#f67400"/>
  <circle cx="11" cy="14.25" r="1" fill="#f67400"/>
</svg>"##;

/// Start the tray. Failures (no StatusNotifier host, no session bus) are logged and ignored:
/// the recorder works the same without a tray.
pub fn spawn(tx: mpsc::Sender<Message>, events: broadcast::Sender<Event>) {
    tokio::spawn(async move {
        if let Err(error) = run(tx, events).await {
            warn!(%error, "tray icon unavailable");
        }
    });
}

async fn run(tx: mpsc::Sender<Message>, events: broadcast::Sender<Event>) -> anyhow::Result<()> {
    // Subscribe before taking the snapshot so no change can slip in between.
    let mut rx = events.subscribe();
    let status = query_status(&tx).await;
    let config = query_config(&tx).await.unwrap_or_default();
    let icon_dir = write_icons().unwrap_or_else(|error| {
        warn!(%error, "could not write tray icons, using fallback names");
        PathBuf::new()
    });

    let tray = ClipprTray { tx: tx.clone(), status, config, icon_dir };
    // Plasma's tray may start after the recorder; keep the item alive and register late.
    let handle = tray.assume_sni_available(true).spawn().await?;

    loop {
        match rx.recv().await {
            Ok(Event::StatusChanged { status }) => {
                handle.update(|t| t.status = Some(status)).await;
            }
            Ok(Event::ConfigChanged { config }) => {
                handle.update(|t| t.config = config).await;
            }
            Ok(Event::ClipSaved { .. }) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let status = query_status(&tx).await;
                handle.update(|t| t.status = status).await;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    handle.shutdown().await;
    Ok(())
}

async fn request(tx: &mpsc::Sender<Message>, req: Request) -> Option<Response> {
    let (reply, rx) = oneshot::channel();
    tx.send(Message::Request(Box::new(req), reply)).await.ok()?;
    rx.await.ok()
}

async fn query_status(tx: &mpsc::Sender<Message>) -> Option<Status> {
    match request(tx, Request::Status).await? {
        Response::Status { status } => Some(status),
        _ => None,
    }
}

async fn query_config(tx: &mpsc::Sender<Message>) -> Option<Config> {
    match request(tx, Request::GetConfig).await? {
        Response::Config { config } => Some(config),
        _ => None,
    }
}

/// Write the tray SVGs into `$XDG_STATE_HOME/clippr/tray-icons`, skipping unchanged files.
fn write_icons() -> std::io::Result<PathBuf> {
    let dir = clippr_core::paths::state_dir().join("tray-icons");
    std::fs::create_dir_all(&dir)?;
    for (name, svg) in ICONS {
        let path = dir.join(format!("{name}.svg"));
        let svg = svg.replace("FRAME", FRAME);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(svg.as_str()) {
            let tmp = dir.join(format!(".{name}.svg.tmp"));
            std::fs::write(&tmp, svg)?;
            std::fs::rename(tmp, path)?;
        }
    }
    Ok(dir)
}

struct ClipprTray {
    tx: mpsc::Sender<Message>,
    /// `None` until the first status arrives.
    status: Option<Status>,
    config: Config,
    icon_dir: PathBuf,
}

impl ClipprTray {
    fn state(&self) -> CaptureState {
        self.status.as_ref().map_or(CaptureState::Starting, |s| s.state)
    }

    /// Queue a message for the main loop without blocking the D-Bus task.
    fn send(&self, message: Message) {
        if self.tx.try_send(message).is_err() {
            debug!("daemon busy; dropped a tray action");
        }
    }

    fn hotkey(&self, id: &str) {
        self.send(Message::Hotkey { id: id.into(), activation_token: None });
    }

    fn set_paused(&self, paused: bool) {
        let (reply, _) = oneshot::channel();
        self.send(Message::Request(Box::new(Request::SetPaused { paused }), reply));
    }

    fn save(&self, seconds: Option<u32>) {
        let (reply, _) = oneshot::channel();
        self.send(Message::Request(Box::new(Request::Save { seconds }), reply));
    }

    fn state_line(&self) -> String {
        match &self.status {
            None => "Starting".into(),
            Some(s) => match s.state {
                CaptureState::Starting => "Waiting for a screen to be shared".into(),
                CaptureState::Paused => "Paused".into(),
                CaptureState::Error => {
                    format!("Error: {}", s.last_error.as_deref().unwrap_or("capture failed"))
                }
                CaptureState::Recording if s.width > 0 => {
                    format!("Recording {}×{} at {} fps", s.width, s.height, s.fps)
                }
                CaptureState::Recording => "Recording".into(),
            },
        }
    }
}

impl ksni::Tray for ClipprTray {
    fn id(&self) -> String {
        "dev.clippr.Clippr".into()
    }

    fn title(&self) -> String {
        "Clippr".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn icon_theme_path(&self) -> String {
        self.icon_dir.to_string_lossy().into_owned()
    }

    fn icon_name(&self) -> String {
        match self.state() {
            CaptureState::Recording => "clippr-tray-recording",
            CaptureState::Paused => "clippr-tray-paused",
            CaptureState::Starting => "clippr-tray-starting",
            CaptureState::Error => "clippr-tray-error",
        }
        .into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Clippr".into(),
            // The description may be rendered as HTML, and error text can contain anything.
            description: escape_markup(&self.state_line()),
            ..Default::default()
        }
    }

    /// Left click opens the library.
    fn activate(&mut self, _x: i32, _y: i32) {
        open_gui(&[]);
    }

    /// Middle click saves the default clip length.
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        if self.state() == CaptureState::Recording {
            self.save(None);
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let recording = self.state() == CaptureState::Recording;
        let paused = self.state() == CaptureState::Paused;
        let mut items = vec![
            StandardItem {
                label: self.state_line().replace('_', "__"),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ];
        for binding in &self.config.hotkeys.clips {
            let seconds = binding.seconds;
            items.push(StandardItem {
                label: format!("Save last {}", clip_length(seconds)),
                icon_name: "media-record".into(),
                enabled: recording,
                activate: Box::new(move |t: &mut Self| t.save(Some(seconds))),
                ..Default::default()
            }.into());
        }
        items.extend([
            MenuItem::Separator,
            StandardItem {
                label: if paused { "Resume recording" } else { "Pause recording" }.into(),
                icon_name: if paused {
                    "media-playback-start"
                } else {
                    "media-playback-pause"
                }
                .into(),
                activate: Box::new(move |t: &mut Self| t.set_paused(!paused)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quick overlay".into(),
                icon_name: "view-preview".into(),
                activate: Box::new(|t: &mut Self| t.hotkey("toggle-overlay")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open Clippr".into(),
                icon_name: "dev.clippr.Clippr".into(),
                activate: Box::new(|_: &mut Self| open_gui(&[])),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Settings".into(),
                icon_name: "configure".into(),
                activate: Box::new(|_: &mut Self| open_gui(&["--settings"])),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit recorder".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| t.send(Message::Shutdown)),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }

    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        // Keep the item alive so it reappears when the panel restarts.
        true
    }
}

/// Launch the GUI detached from the daemon, reaped in the background.
fn open_gui(args: &[&str]) {
    let program = std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name("clippr-gui"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| "clippr-gui".into());
    let result = tokio::process::Command::new(&program)
        .args(args)
        // Startup credentials belong to one activation, never the daemon's lifetime.
        .env_remove("XDG_ACTIVATION_TOKEN")
        .env_remove("DESKTOP_STARTUP_ID")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match result {
        Ok(mut child) => {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Err(error) => warn!(%error, program = %program.display(), "could not launch clippr-gui"),
    }
}

/// `30 s`, `1 min`, `1 min 30 s`
fn clip_length(secs: u32) -> String {
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_embed_the_frame_and_are_valid_xml_ish() {
        for (name, svg) in ICONS {
            let svg = svg.replace("FRAME", FRAME);
            assert!(!svg.contains("FRAME"), "{name}");
            assert!(svg.starts_with("<svg") && svg.trim_end().ends_with("</svg>"), "{name}");
            assert!(svg.contains("ColorScheme-Text"), "{name}");
        }
    }

    #[test]
    fn clip_lengths_read_naturally() {
        assert_eq!(clip_length(30), "30 s");
        assert_eq!(clip_length(60), "1 min");
        assert_eq!(clip_length(90), "1 min 30 s");
    }

    #[test]
    fn tooltip_text_is_escaped() {
        assert_eq!(escape_markup("<a href=x>&</a>"), "&lt;a href=x&gt;&amp;&lt;/a&gt;");
    }
}
