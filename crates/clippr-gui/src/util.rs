//! Formatting and small helpers shared by the views.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use chrono::{DateTime, Datelike, Local};
use clippr_core::clip::AppInfo;
use clippr_core::ipc::{CaptureState, Status};
use gtk::gio;
use gtk::prelude::*;

/// `0:42`, `12:03`, `1:02:03`
pub fn duration(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// `0:42.3`, for the editor where tenths matter.
pub fn duration_precise(secs: f64) -> String {
    let tenths = (secs.max(0.0) * 10.0).round() as u64;
    format!("{}:{:02}.{}", tenths / 600, tenths / 10 % 60, tenths % 10)
}

pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// `Today, 22:41`, `Yesterday, 18:02`, `Oct 12, 09:15`, `Oct 12 2025`
pub fn date(at: &DateTime<Local>) -> String {
    let now = Local::now();
    let days = (now.date_naive() - at.date_naive()).num_days();
    match days {
        0 => format!("Today, {}", at.format("%H:%M")),
        1 => format!("Yesterday, {}", at.format("%H:%M")),
        _ if at.year() == now.year() => at.format("%b %-d, %H:%M").to_string(),
        _ => at.format("%b %-d %Y").to_string(),
    }
}

pub fn codec_name(codec: &str) -> &str {
    match codec {
        "h264" => "H.264",
        "hevc" => "HEVC",
        "av1" => "AV1",
        other => other,
    }
}

/// `30 s`, `1 min`, `1 min 30 s`
pub fn clip_length(secs: u32) -> String {
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

/// One-line recorder summary, e.g. `62 s buffered · 2560×1440 60 fps · vah264enc`.
pub fn status_line(status: &Status) -> String {
    match status.state {
        CaptureState::Starting => "Waiting for a screen to be shared".into(),
        CaptureState::Paused => "Paused. Nothing is being captured".into(),
        CaptureState::Error => status
            .last_error
            .clone()
            .unwrap_or_else(|| "Capture failed".into()),
        CaptureState::Recording => {
            let mut parts = vec![format!("{:.0} s buffered", status.buffered_secs)];
            if status.width > 0 {
                parts.push(format!("{}×{} {} fps", status.width, status.height, status.fps));
            }
            if let Some(enc) = &status.encoder {
                parts.push(enc.clone());
            }
            parts.join(" · ")
        }
    }
}

pub fn state_label(status: Option<&Status>) -> &'static str {
    match status.map(|s| s.state) {
        None => "Recorder off",
        Some(CaptureState::Starting) => "Starting",
        Some(CaptureState::Recording) => "Recording",
        Some(CaptureState::Paused) => "Paused",
        Some(CaptureState::Error) => "Error",
    }
}

/// CSS class for the status dot.
pub fn state_class(status: Option<&Status>) -> &'static str {
    match status.map(|s| s.state) {
        Some(CaptureState::Recording) => "rec",
        Some(CaptureState::Starting) => "starting",
        Some(CaptureState::Error) => "error",
        _ => "idle",
    }
}

/// Best-effort icon for the app a clip was recorded in.
pub fn app_icon(app: &AppInfo) -> Option<gio::Icon> {
    let class = app.class.as_deref()?;
    let theme = gtk::IconTheme::for_display(&gtk::gdk::Display::default()?);
    // Steam installs per-game icons as `steam_icon_<appid>`. Desktop entries usually name their
    // icon after the app id, so a theme lookup covers most other apps.
    let candidates = match class.strip_prefix("steam_app_") {
        Some(id) => vec![format!("steam_icon_{id}"), "steam".to_string()],
        None => vec![class.to_string(), class.to_lowercase()],
    };
    candidates
        .into_iter()
        .find(|name| theme.has_icon(name))
        .map(|name| gio::ThemedIcon::new(&name).upcast())
}

/// A toast that shows `text` literally. `AdwToast` parses Pango markup by default, and clip
/// titles can come from app-chosen window classes, so markup must never be interpreted.
pub fn toast(text: &str) -> adw::Toast {
    let toast = adw::Toast::new(text);
    toast.set_use_markup(false);
    toast
}

/// Path of a sibling binary installed next to this one, or the bare name for a PATH lookup.
pub fn sibling_exe(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Launch a program without waiting for it. Errors are logged, not shown.
pub fn spawn_detached(program: impl Into<PathBuf>, args: &[&str]) {
    let program = program.into();
    let result = Command::new(&program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match result {
        // Reap it on a throwaway thread so it never lingers as a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("clippr-gui: failed to launch {}: {e}", program.display()),
    }
}

/// Start the recorder through its systemd user unit.
pub fn start_daemon() {
    spawn_detached("systemctl", &["--user", "start", "clipprd.service"]);
}

/// Set a status dot's CSS class to exactly one of the state classes.
pub fn set_dot_state(dot: &gtk::Widget, class: &str) {
    for c in ["rec", "starting", "error", "idle"] {
        dot.remove_css_class(c);
    }
    dot.add_css_class(class);
}

pub fn status_dot() -> gtk::Widget {
    let dot = gtk::Box::builder().valign(gtk::Align::Center).build();
    dot.add_css_class("status-dot");
    dot.add_css_class("idle");
    dot.upcast()
}
