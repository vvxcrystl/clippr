//! KWin sends activation changes; other compositors are queried only on a save.
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use clippr_core::{clip::AppInfo, paths};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::Command,
};

#[derive(Clone, Default)]
pub struct Focus(
    pub Arc<Mutex<Option<AppInfo>>>,
    Arc<Mutex<Option<zbus::names::OwnedUniqueName>>>,
);

#[zbus::interface(name = "org.clippr.Daemon")]
impl Focus {
    fn focus_changed(
        &self,
        class: &str,
        title: &str,
        pid: &str,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
    ) {
        let owner = crate::lock(&self.1);
        if !owner
            .as_ref()
            .is_some_and(|owner| hdr.sender() == Some(owner.inner()))
        {
            tracing::debug!(sender = ?hdr.sender(), "ignoring focus metadata from non-KWin peer");
            return;
        }
        let pid = pid.parse::<u32>().unwrap_or(0);
        tracing::debug!(class, pid, "focused application changed");
        let app = if class.is_empty() && pid == 0 {
            None
        } else {
            Some(AppInfo {
                name: class.to_owned(),
                class: (!class.is_empty()).then(|| class.to_owned()),
                window_title: (!title.is_empty()).then(|| title.to_owned()),
                pid: (pid != 0).then_some(pid),
            })
        };
        *crate::lock(&self.0) = app;
    }
}

// Abort on cancellation during startup too, so a timed-out start cannot leak a watcher.
struct OwnerWatch(tokio::task::JoinHandle<()>);
impl Drop for OwnerWatch {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn watch_kwin_owner(connection: &zbus::Connection, focus: &Focus) -> Result<OwnerWatch> {
    let dbus = zbus::fdo::DBusProxy::new(connection).await?;
    // Subscribe before querying so a concurrent owner change cannot be missed.
    let mut changes = dbus
        .receive_name_owner_changed_with_args(&[(0, "org.kde.KWin")])
        .await?;
    *crate::lock(&focus.1) = dbus.get_name_owner("org.kde.KWin".try_into()?).await.ok();
    let kwin_name = zbus::names::BusName::try_from("org.kde.KWin")?;
    let watched_focus = focus.clone();
    Ok(OwnerWatch(tokio::spawn(async move {
        while changes.next().await.is_some() {
            let owner = dbus.get_name_owner(kwin_name.clone()).await.ok();
            let mut cached_owner = crate::lock(&watched_focus.1);
            if *cached_owner != owner {
                *cached_owner = owner;
                *crate::lock(&watched_focus.0) = None;
            }
        }
        *crate::lock(&watched_focus.1) = None;
        *crate::lock(&watched_focus.0) = None;
    })))
}

pub struct Kwin {
    connection: zbus::Connection,
    pub focus: Focus,
    plugin: String,
    _owner_watch: OwnerWatch,
}
impl Kwin {
    pub async fn start() -> Result<Self> {
        let focus = Focus::default();
        let connection = zbus::connection::Builder::session()?
            .name("org.clippr.Daemon")?
            .serve_at("/org/clippr/Daemon", focus.clone())?
            .build()
            .await?;
        let owner_watch = watch_kwin_owner(&connection, &focus).await?;
        let plugin = "clippr-focus".to_owned();
        let script = r#"var active = null;
function send() {
    var w = active;
    callDBus('org.clippr.Daemon', '/org/clippr/Daemon', 'org.clippr.Daemon', 'FocusChanged',
        w ? String(w.resourceClass) : '', w ? String(w.caption) : '', w ? String(w.pid) : '0');
}
function report(w) {
    if (active) { try { active.captionChanged.disconnect(send); } catch (e) {} }
    active = w;
    if (active) active.captionChanged.connect(send);
    send();
}
workspace.windowActivated.connect(report);
report(workspace.activeWindow);
"#;
        std::fs::create_dir_all(paths::state_dir())?;
        let path = paths::state_dir().join("focus.js");
        std::fs::write(&path, script)?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
        )
        .await?;
        let _: Result<bool, _> = proxy.call("unloadScript", &(&plugin,)).await;
        let id: i32 = proxy
            .call("loadScript", &(path.to_string_lossy().as_ref(), &plugin))
            .await?;
        anyhow::ensure!(id >= 0, "KWin rejected focus script");
        let script_path = format!("/Scripting/Script{id}");
        let script_proxy = zbus::Proxy::new(
            &connection,
            "org.kde.KWin",
            script_path.as_str(),
            "org.kde.kwin.Script",
        )
        .await?;
        if let Err(e) = script_proxy.call::<_, _, ()>("run", &()).await {
            let _: Result<bool, _> = proxy.call("unloadScript", &(&plugin,)).await;
            return Err(e.into());
        }
        tracing::info!("KWin focus tracking active");
        Ok(Self {
            connection,
            focus,
            plugin,
            _owner_watch: owner_watch,
        })
    }
    pub async fn stop(self) {
        if let Ok(proxy) = zbus::Proxy::new(
            &self.connection,
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
        )
        .await
        {
            let _: Result<bool, _> = proxy.call("unloadScript", &(&self.plugin,)).await;
        }
    }
}

pub async fn focused(cached: Option<AppInfo>) -> Option<AppInfo> {
    let mut app = match cached {
        Some(app) => Some(app),
        None => tokio::time::timeout(Duration::from_secs(2), fallback())
            .await
            .ok()
            .flatten(),
    }?;
    if let Some(class) = &app.class {
        if let Some(id) = class
            .strip_prefix("steam_app_")
            .filter(|s| s.bytes().all(|b| b.is_ascii_digit()) && !s.is_empty())
        {
            if let Some(name) = steam_name(id) {
                app.name = name;
            }
        }
    }
    if app.class.as_deref() == Some(app.name.as_str()) {
        if let Some(name) = app.class.as_deref().and_then(desktop_name) {
            app.name = name;
        }
    }
    if app.name.is_empty() {
        if let Some(pid) = app.pid {
            app.name = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .unwrap_or_default()
                .trim()
                .into();
        }
    }
    if app.name.is_empty() {
        return None;
    }
    Some(app)
}

async fn output(program: &str, args: &[&str]) -> Result<String> {
    let result = Command::new(program)
        .args(args)
        .env_remove("XDG_ACTIVATION_TOKEN")
        .env_remove("DESKTOP_STARTUP_ID")
        .kill_on_drop(true)
        .output()
        .await?;
    anyhow::ensure!(result.status.success(), "{program} failed");
    Ok(String::from_utf8_lossy(&result.stdout).into_owned())
}

async fn fallback() -> Option<AppInfo> {
    if let Ok(signature) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        for path in [
            runtime.join("hypr").join(&signature).join(".socket.sock"),
            PathBuf::from("/tmp/hypr")
                .join(&signature)
                .join(".socket.sock"),
        ] {
            if let Ok(mut stream) = UnixStream::connect(path).await {
                stream.write_all(b"j/activewindow").await.ok()?;
                let mut data = Vec::new();
                stream.take(1024 * 1024).read_to_end(&mut data).await.ok()?;
                let value: Value = serde_json::from_slice(&data).ok()?;
                return app_from_json(&value, "class", "title");
            }
        }
    }
    if std::env::var_os("SWAYSOCK").is_some() {
        let text = output("swaymsg", &["-r", "-t", "get_tree"]).await.ok()?;
        let tree: Value = serde_json::from_str(&text).ok()?;
        fn focused_node(node: &Value) -> Option<&Value> {
            if node["focused"].as_bool() == Some(true) {
                return Some(node);
            }
            for key in ["nodes", "floating_nodes"] {
                if let Some(children) = node[key].as_array() {
                    for child in children {
                        if let Some(node) = focused_node(child) {
                            return Some(node);
                        }
                    }
                }
            }
            None
        }
        let node = focused_node(&tree)?;
        let mut app = app_from_json(node, "app_id", "name")?;
        if app.class.is_none() {
            app.class = node["window_properties"]["class"]
                .as_str()
                .map(str::to_owned);
            app.name = app.class.clone().unwrap_or_default();
        }
        return Some(app);
    }
    if std::env::var_os("DISPLAY").is_some() {
        let root = output("xprop", &["-root", "_NET_ACTIVE_WINDOW"])
            .await
            .ok()?;
        let id = root.split_whitespace().last()?;
        if id == "0x0" {
            return None;
        }
        let text = output(
            "xprop",
            &["-id", id, "WM_CLASS", "_NET_WM_NAME", "_NET_WM_PID"],
        )
        .await
        .ok()?;
        let mut app = AppInfo {
            name: String::new(),
            class: None,
            window_title: None,
            pid: None,
        };
        for line in text.lines() {
            if line.starts_with("WM_CLASS") {
                app.class = line.split('"').nth(3).map(str::to_owned);
                app.name = app.class.clone().unwrap_or_default();
            } else if line.starts_with("_NET_WM_NAME") {
                app.window_title = line.split('"').nth(1).map(str::to_owned);
            } else if line.starts_with("_NET_WM_PID") {
                app.pid = line.split('=').nth(1).and_then(|s| s.trim().parse().ok());
            }
        }
        return Some(app);
    }
    None
}

fn app_from_json(value: &Value, class: &str, title: &str) -> Option<AppInfo> {
    let class = value[class]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some(AppInfo {
        name: class.clone().unwrap_or_default(),
        class,
        window_title: value[title].as_str().map(str::to_owned),
        pid: value["pid"].as_u64().and_then(|p| p.try_into().ok()),
    })
}

fn desktop_name(class: &str) -> Option<String> {
    if class.contains('/') || class.contains('\\') || class.starts_with('.') {
        return None;
    }
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let mut roots: Vec<_> = home.into_iter().collect();
    roots.extend(
        std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".into())
            .split(':')
            .map(PathBuf::from),
    );
    for root in roots {
        let Ok(text) =
            std::fs::read_to_string(root.join("applications").join(format!("{class}.desktop")))
        else {
            continue;
        };
        if let Some(name) = text.lines().find_map(|line| line.strip_prefix("Name=")) {
            return Some(name.to_owned());
        }
    }
    None
}

fn steam_name(id: &str) -> Option<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let mut roots = vec![
        home.join(".steam/steam"),
        home.join(".local/share/Steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
    ];
    for root in roots.clone() {
        if let Ok(folders) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            for line in folders.lines() {
                let fields: Vec<_> = line.split('"').collect();
                if fields.get(1) == Some(&"path") {
                    if let Some(path) = fields.get(3) {
                        roots.push(PathBuf::from(path.replace("\\\\", "\\")));
                    }
                }
            }
        }
    }
    for root in roots {
        let Ok(manifest) =
            std::fs::read_to_string(root.join(format!("steamapps/appmanifest_{id}.acf")))
        else {
            continue;
        };
        for line in manifest.lines() {
            let fields: Vec<_> = line.split('"').collect();
            if fields.get(1) == Some(&"name") {
                return fields.get(3).map(|s| s.to_string());
            }
        }
    }
    None
}

pub fn validate_config(config: &clippr_core::config::Config) -> Result<()> {
    anyhow::ensure!(
        (1..=240).contains(&config.video.fps),
        "video.fps must be between 1 and 240"
    );
    anyhow::ensure!(
        (1..=3600).contains(&config.general.clip_short_secs)
            && (1..=3600).contains(&config.general.clip_long_secs),
        "clip lengths must be between 1 and 3600 seconds"
    );
    anyhow::ensure!(
        (8..=512).contains(&config.audio.bitrate_kbps),
        "audio bitrate must be between 8 and 512 kbps"
    );
    anyhow::ensure!(
        (500..=200_000).contains(&config.video.bitrate_kbps),
        "video bitrate must be between 500 and 200000 kbps"
    );
    config
        .general
        .clips_dir
        .to_str()
        .context("clips directory must be UTF-8")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test(flavor = "current_thread")]
    async fn only_current_kwin_owner_can_report_focus() -> Result<()> {
        // A private bus keeps the real session, KWin, and daemon untouched.
        let mut bus = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = bus.stdout.take().context("missing bus stdout")?;
        let mut address = String::new();
        BufReader::new(stdout).read_line(&mut address).await?;
        let connect = || zbus::connection::Builder::address(address.trim());
        let focus = Focus::default();
        let daemon = connect()?
            .serve_at("/org/clippr/Daemon", focus.clone())?
            .build()
            .await?;
        let kwin = connect()?.name("org.kde.KWin")?.build().await?;
        let outsider = connect()?.build().await?;
        let _watch = watch_kwin_owner(&daemon, &focus).await?;
        async fn report(
            peer: &zbus::Connection,
            daemon: &zbus::Connection,
            title: &str,
        ) -> Result<()> {
            let proxy = zbus::Proxy::new(
                peer,
                daemon
                    .unique_name()
                    .context("missing unique name")?
                    .as_str(),
                "/org/clippr/Daemon",
                "org.clippr.Daemon",
            )
            .await?;
            proxy
                .call::<_, _, ()>("FocusChanged", &("game", title, "123"))
                .await?;
            Ok(())
        }
        report(&outsider, &daemon, "spoof").await?;
        assert!(crate::lock(&focus.0).is_none());
        report(&kwin, &daemon, "accepted").await?;
        assert_eq!(
            crate::lock(&focus.0)
                .as_ref()
                .unwrap()
                .window_title
                .as_deref(),
            Some("accepted")
        );
        kwin.release_name("org.kde.KWin").await?;
        outsider.request_name("org.kde.KWin").await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if crate::lock(&focus.1).as_ref().map(|n| n.as_str())
                    == outsider.unique_name().map(|n| n.as_str())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(crate::lock(&focus.0).is_none());
        report(&kwin, &daemon, "former owner").await?;
        assert!(crate::lock(&focus.0).is_none());
        report(&outsider, &daemon, "new owner").await?;
        assert_eq!(
            crate::lock(&focus.0)
                .as_ref()
                .unwrap()
                .window_title
                .as_deref(),
            Some("new owner")
        );
        bus.kill().await?;
        bus.wait().await?;
        Ok(())
    }
}
