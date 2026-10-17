use std::{os::fd::OwnedFd, os::unix::fs::OpenOptionsExt};

use anyhow::{Context, Result};
use ashpd::desktop::{
    global_shortcuts::{GlobalShortcuts, NewShortcut},
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    PersistMode, Session, SessionPortal,
};
use clippr_core::{config::Config, paths};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::Message;

// The library desktop entry also gives the host daemon its portal identity.
const APP_ID: &str = "dev.clippr.Clippr";

pub struct ManagedSession<T: SessionPortal + Send + Sync + 'static>(std::sync::Arc<Session<T>>);
impl<T: SessionPortal + Send + Sync + 'static> std::ops::Deref for ManagedSession<T> {
    type Target = Session<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T: SessionPortal + Send + Sync + 'static> Drop for ManagedSession<T> {
    fn drop(&mut self) {
        let session = self.0.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = session.close().await;
            });
        }
    }
}

pub struct Screen {
    pub fd: OwnedFd,
    pub node: u32,
    pub width: u32,
    pub height: u32,
    pub _session: ManagedSession<Screencast>,
}

pub async fn screen(config: &Config) -> Result<Screen> {
    let proxy = Screencast::with_connection(connection().await?).await?;
    let session = ManagedSession(std::sync::Arc::new(
        proxy.create_session(Default::default()).await?,
    ));
    let token_path = paths::state_dir().join("restore_token");
    let token = std::fs::read_to_string(&token_path).ok();
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_sources(Some(SourceType::Monitor.into()))
                .set_multiple(false)
                .set_cursor_mode(if config.video.capture_cursor {
                    CursorMode::Embedded
                } else {
                    CursorMode::Hidden
                })
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref()),
        )
        .await?
        .response()?;
    let response = proxy
        .start(&session, None, Default::default())
        .await?
        .response()?;
    let stream = response
        .streams()
        .first()
        .context("portal returned no monitor")?;
    let (width, height) = stream.size().unwrap_or((1920, 1080));
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;
    if let Some(token) = response.restore_token() {
        std::fs::create_dir_all(paths::state_dir())?;
        let tmp = token_path.with_extension("tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        use std::io::Write;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        std::fs::rename(tmp, token_path)?;
    }
    Ok(Screen {
        fd,
        node: stream.pipe_wire_node_id(),
        width: width.max(1) as u32,
        height: height.max(1) as u32,
        _session: session,
    })
}

pub async fn hotkeys(config: Config, tx: mpsc::Sender<Message>) -> Result<()> {
    let proxy = GlobalShortcuts::with_connection(connection().await?).await?;
    let session = ManagedSession(std::sync::Arc::new(
        proxy.create_session(Default::default()).await?,
    ));
    let shortcuts = [
        NewShortcut::new("clip-short", "Save a short clip")
            .preferred_trigger(config.hotkeys.clip_short.as_str()),
        NewShortcut::new("clip-long", "Save a long clip")
            .preferred_trigger(config.hotkeys.clip_long.as_str()),
        NewShortcut::new("toggle-overlay", "Toggle clippr overlay")
            .preferred_trigger(config.hotkeys.toggle_overlay.as_str()),
    ];
    let session_path = serde_json::to_value(&*session)?
        .as_str()
        .context("invalid shortcut session path")?
        .to_owned();
    let mut activated = proxy.receive_activated().await?;
    proxy
        .bind_shortcuts(&session, &shortcuts, None, Default::default())
        .await?
        .response()?;
    tracing::info!("global shortcuts bound");
    while let Some(event) = activated.next().await {
        if event.session_handle().as_str() == session_path
            && tx
                .send(Message::Hotkey {
                    id: event.shortcut_id().to_owned(),
                    activation_token: event
                        .options()
                        .get("activation_token")
                        .and_then(|value| <&str>::try_from(value).ok())
                        .filter(|token| !token.is_empty())
                        .map(str::to_owned),
                })
                .await
                .is_err()
        {
            break;
        }
    }
    Ok(())
}

/// Modern portals require an installed desktop identity even for host applications.
/// Supply a hidden identity only when the GUI package has not installed one yet.
pub fn prepare_identity() -> Result<()> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
        .context("no user data directory")?;
    let relative = format!("applications/{APP_ID}.desktop");
    let installed = data_home.join(&relative).exists()
        || std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".into())
            .split(':')
            .any(|root| std::path::Path::new(root).join(&relative).exists());
    if !installed {
        let path = data_home.join(relative);
        std::fs::create_dir_all(data_home.join("applications"))?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                use std::io::Write;
                let executable = std::env::current_exe()?
                    .to_string_lossy()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('`', "\\`")
                    .replace('$', "\\$");
                // A standalone daemon may have no GUI binary. Reuse the packaged
                // identity, but hide it and give the portal an existing executable.
                let desktop = include_str!("../../../packaging/dev.clippr.Clippr.desktop")
                    .replace("Exec=clippr-gui", &format!("Exec=\"{executable}\""));
                file.write_all(desktop.as_bytes())?;
                file.write_all(b"NoDisplay=true\n")?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

async fn connection() -> Result<zbus::Connection> {
    let connection = zbus::Connection::session().await?;
    // A fresh portal connection also recovers registration after a portal restart.
    // Older portals lack Registry; ScreenCast still works without it there.
    if let Err(error) =
        ashpd::register_host_app_with_connection(connection.clone(), APP_ID.parse()?).await
    {
        tracing::debug!(%error, "portal host registration unavailable");
    }
    Ok(connection)
}
