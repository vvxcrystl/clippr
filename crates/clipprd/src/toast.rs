//! The "Clip saved" popup. Launches `clippr-toast` styled with the desktop's color scheme and
//! accent, and falls back to a desktop notification where the compositor has no layer shell.

use std::sync::OnceLock;

use clippr_core::clip::ClipMeta;
use tokio::sync::watch;
use tracing::debug;

use crate::save;

/// clippr-toast's exit codes for "can't show a popup here".
const EXIT_NO_LAYER_SHELL: i32 = 3;
const EXIT_NO_FONT: i32 = 4;

/// Bumped for every toast, so a newer one replaces the one on screen.
fn generation() -> &'static watch::Sender<u64> {
    static GENERATION: OnceLock<watch::Sender<u64>> = OnceLock::new();
    GENERATION.get_or_init(|| watch::channel(0).0)
}

pub fn show(clip: ClipMeta) {
    tokio::spawn(async move {
        let fallback = match launch(&clip).await {
            Ok(shown) => !shown,
            Err(error) => {
                debug!(%error, "clippr-toast unavailable");
                true
            }
        };
        if fallback {
            if let Err(error) = save::notify(&clip).await {
                debug!(%error, "notification unavailable");
            }
        }
    });
}

/// Run the toast to completion. `Ok(false)` means it couldn't display on this desktop.
async fn launch(clip: &ClipMeta) -> anyhow::Result<bool> {
    let (light, accent) = appearance().await;
    let program = std::env::current_exe()?.with_file_name("clippr-toast");
    let program = if program.is_file() { program } else { "clippr-toast".into() };
    let thumbnail = clippr_core::paths::thumbs_dir().join(format!("{}.jpg", clip.id));

    let mut command = tokio::process::Command::new(program);
    command.arg("--duration").arg(duration(clip.duration_secs));
    command.arg("--thumbnail").arg(thumbnail);
    if let Some(accent) = accent {
        command.arg("--accent").arg(accent);
    }
    if light {
        command.arg("--light");
    }
    command
        .arg("--")
        .arg("Clip saved")
        .arg(&clip.title)
        .env_remove("XDG_ACTIVATION_TOKEN")
        .env_remove("DESKTOP_STARTUP_ID")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn()?;

    let sender = generation();
    let mut newer = sender.subscribe();
    sender.send_modify(|g| *g += 1);
    newer.borrow_and_update();
    tokio::select! {
        status = child.wait() => {
            let code = status?.code();
            Ok(!matches!(code, Some(EXIT_NO_LAYER_SHELL | EXIT_NO_FONT)))
        }
        _ = newer.changed() => {
            // A newer clip's toast takes the spot. Killing through the handle can't hit a
            // recycled PID.
            let _ = child.kill().await;
            Ok(true)
        }
    }
}

/// `0:30`, `1:02:03`
fn duration(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// The desktop's color scheme and accent from the settings portal, as libadwaita reads them.
/// Returns (prefers light, accent as `#rrggbb`). Defaults to light without an accent, like
/// libadwaita when the portal has no preference.
async fn appearance() -> (bool, Option<String>) {
    let Ok(connection) = zbus::Connection::session().await else {
        return (true, None);
    };
    let read = |key: &'static str| {
        let connection = connection.clone();
        async move {
            let reply = connection
                .call_method(
                    Some("org.freedesktop.portal.Desktop"),
                    "/org/freedesktop/portal/desktop",
                    Some("org.freedesktop.portal.Settings"),
                    "ReadOne",
                    &("org.freedesktop.appearance", key),
                )
                .await
                .ok()?;
            reply.body().deserialize::<zbus::zvariant::OwnedValue>().ok()
        }
    };
    // 1 means "prefer dark"; 0 (no preference) and 2 (prefer light) both render light.
    let scheme = read("color-scheme").await.and_then(|v| u32::try_from(unwrap_variant(v)).ok());
    let light = scheme != Some(1);
    let accent = read("accent-color")
        .await
        .and_then(|v| <(f64, f64, f64)>::try_from(unwrap_variant(v)).ok())
        .filter(|(r, g, b)| [r, g, b].iter().all(|c| (0.0..=1.0).contains(*c)))
        .map(|(r, g, b)| {
            let byte = |c: f64| (c * 255.0).round() as u8;
            format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
        });
    (light, accent)
}

/// Some portal versions wrap the value in an extra variant.
fn unwrap_variant(value: zbus::zvariant::OwnedValue) -> zbus::zvariant::OwnedValue {
    match &*value {
        zbus::zvariant::Value::Value(inner) => inner.try_to_owned().unwrap_or(value),
        _ => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_match_the_library() {
        assert_eq!(duration(30.2), "0:30");
        assert_eq!(duration(60.0), "1:00");
        assert_eq!(duration(3723.0), "1:02:03");
    }
}
