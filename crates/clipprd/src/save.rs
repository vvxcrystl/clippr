use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    process::Command,
};

use anyhow::{bail, Context, Result};
use chrono::Local;
use clippr_core::{
    clip::{self, AppInfo, ClipMeta},
    config::Config,
    paths,
};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as app;

use crate::ring::{Packet, Snapshot};

/// Linux nice/scheduling changes apply to the calling thread, never the capture thread.
pub fn low_priority() {
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        let param = libc::sched_param { sched_priority: 0 };
        libc::sched_setscheduler(0, libc::SCHED_BATCH, &param);
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13); // IOPRIO_CLASS_IDLE
    }
}

pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 100 {
            break;
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "clip".into()
    } else {
        out.into()
    }
}

fn feed(source: &app::AppSrc, packets: Vec<Packet>, start: u64) {
    let mut packets = packets.into_iter();
    source.set_callbacks(
        app::AppSrcCallbacks::builder()
            .need_data(move |src, _| {
                if let Some(packet) = packets.next() {
                    // Copy-on-write clones the buffer header, sharing the encoded payload.
                    let mut buffer = packet.buffer;
                    let buffer_ref = buffer.make_mut();
                    buffer_ref.set_pts(gst::ClockTime::from_nseconds(packet.pts - start));
                    buffer_ref.set_dts(gst::ClockTime::from_nseconds(packet.pts - start));
                    buffer_ref.set_duration(gst::ClockTime::from_nseconds(packet.duration));
                    let _ = src.push_buffer(buffer);
                } else {
                    let _ = src.end_of_stream();
                }
            })
            .build(),
    );
}

fn mux(snapshot: Snapshot, path: &Path) -> Result<()> {
    let has_audio = !snapshot.audio.is_empty() && snapshot.audio_caps.is_some();
    let description = format!(
        "appsrc name=video format=time ! mux.video_0 {} mp4mux name=mux faststart=true ! filesink name=file",
        if has_audio {
            "appsrc name=audio format=time ! mux.audio_0"
        } else {
            ""
        }
    );
    let pipeline = gst::parse::launch(&description)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("mux is not a pipeline"))?;
    struct Stop(gst::Pipeline);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.set_state(gst::State::Null);
        }
    }
    let _stop = Stop(pipeline.clone());
    pipeline
        .by_name("file")
        .context("missing mux filesink")?
        .set_property("location", path.to_str().context("clip path is not UTF-8")?);
    let video = pipeline
        .by_name("video")
        .context("missing mux video source")?
        .downcast::<app::AppSrc>()
        .map_err(|_| anyhow::anyhow!("mux video source is not appsrc"))?;
    video.set_caps(Some(&snapshot.video_caps));
    feed(&video, snapshot.video, snapshot.start);
    if has_audio {
        let audio = pipeline
            .by_name("audio")
            .context("missing mux audio source")?
            .downcast::<app::AppSrc>()
            .map_err(|_| anyhow::anyhow!("mux audio source is not appsrc"))?;
        audio.set_caps(snapshot.audio_caps.as_ref());
        feed(&audio, snapshot.audio, snapshot.start);
    }
    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().context("mux pipeline has no bus")?;
    loop {
        let msg = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(120),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .context("mux timed out")?;
        match msg.view() {
            gst::MessageView::Eos(_) => break,
            gst::MessageView::Error(e) => bail!("mux: {} ({:?})", e.error(), e.debug()),
            _ => (),
        }
    }
    pipeline.set_state(gst::State::Null)?;
    Ok(())
}

pub fn save(
    snapshot: Snapshot,
    config: &Config,
    app: Option<AppInfo>,
    codec: String,
    fps: u32,
) -> Result<ClipMeta> {
    low_priority();
    let dir = &config.general.clips_dir;
    fs::create_dir_all(dir)?;
    let created_at = Local::now();
    let base = format!(
        "{}_{}",
        created_at.format("%Y-%m-%d_%H-%M-%S"),
        slug(app.as_ref().map(|a| a.name.as_str()).unwrap_or("clip"))
    );
    // Reserve the partial name atomically. Never overwrite an existing clip, even when
    // another process saves into this directory concurrently.
    let mut suffix = 1;
    let (id, part) = loop {
        let id = if suffix == 1 {
            base.clone()
        } else {
            format!("{base}-{suffix}")
        };
        anyhow::ensure!(clip::is_valid_id(&id), "invalid generated clip id");
        let part = dir.join(format!("{id}.mp4.part"));
        if dir.join(format!("{id}.mp4")).exists() || dir.join(format!("{id}.json")).exists() {
            suffix += 1;
            continue;
        }
        match OpenOptions::new().create_new(true).write(true).open(&part) {
            Ok(_) => break (id, part),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => suffix += 1,
            Err(e) => return Err(e.into()),
        }
    };
    let width = snapshot
        .video_caps
        .structure(0)
        .and_then(|s| s.get::<i32>("width").ok())
        .unwrap_or(0)
        .max(0) as u32;
    let height = snapshot
        .video_caps
        .structure(0)
        .and_then(|s| s.get::<i32>("height").ok())
        .unwrap_or(0)
        .max(0) as u32;
    let duration_secs = (snapshot.end - snapshot.start) as f64 / 1e9;
    let result = (|| {
        mux(snapshot, &part)?;
        let file = format!("{id}.mp4");
        let video_path = dir.join(&file);
        let size_bytes = fs::metadata(&part)?.len();
        fs::File::open(&part)?.sync_all()?;
        fs::rename(&part, &video_path)?;
        let meta = ClipMeta {
            id: id.clone(),
            title: format!(
                "{} {}",
                app.as_ref().map(|a| a.name.as_str()).unwrap_or("Clip"),
                created_at.format("%H:%M")
            ),
            file,
            created_at,
            duration_secs,
            app,
            width,
            height,
            fps,
            codec,
            size_bytes,
            favorite: false,
            tags: vec![],
            edited_from: None,
        };
        let sidecar = dir.join(format!("{id}.json"));
        let tmp = dir.join(format!("{id}.json.part"));
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(&meta)?)?;
        file.sync_all()?;
        fs::rename(tmp, sidecar)?;
        fs::File::open(dir)?.sync_all()?;
        Ok(meta)
    })();
    if result.is_err() {
        let _ = fs::remove_file(part);
        let _ = fs::remove_file(dir.join(format!("{id}.json.part")));
    }
    result
}

pub fn thumbnail(clip: &ClipMeta, config: &Config) -> Result<()> {
    low_priority();
    fs::create_dir_all(paths::thumbs_dir())?;
    let final_path = paths::thumbs_dir().join(format!("{}.jpg", clip.id));
    let part = final_path.with_extension("jpg.part");
    let status = Command::new("ffmpeg")
        .env_remove("XDG_ACTIVATION_TOKEN")
        .env_remove("DESKTOP_STARTUP_ID")
        .args(["-v", "error", "-nostdin", "-y", "-threads", "1", "-ss"])
        .arg((clip.duration_secs / 2.0).min(1.0).to_string())
        .arg("-i")
        .arg(config.general.clips_dir.join(&clip.file))
        .args([
            "-frames:v",
            "1",
            "-vf",
            "scale=480:-2",
            "-threads",
            "1",
            "-f",
            "image2",
        ])
        .arg(&part)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .output()?;
    if !status.status.success() {
        let _ = fs::remove_file(part);
        bail!("thumbnail: {}", String::from_utf8_lossy(&status.stderr));
    }
    fs::rename(part, final_path)?;
    Ok(())
}

/// Remove abandoned save files without traversing directories or following symlinks.
pub fn sweep_parts(dir: &Path, now: std::time::SystemTime) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".mp4.part") && !name.ends_with(".json.part") {
            continue;
        }
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_file()
            && now
                .duration_since(meta.modified()?)
                .is_ok_and(|age| age > std::time::Duration::from_secs(3600))
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn notification_text(text: &str, max_chars: usize) -> String {
    let mut escaped = String::new();
    for c in text.chars().take(max_chars) {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(c),
        }
    }
    escaped
}

pub async fn notify(clip: &ClipMeta) -> Result<()> {
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    let hints: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> =
        std::collections::HashMap::new();
    let _: u32 = proxy
        .call(
            "Notify",
            &(
                "clippr",
                0u32,
                "video-x-generic",
                notification_text("Clip saved", 200),
                notification_text(&clip.title, 200),
                Vec::<&str>::new(),
                hints,
                5000i32,
            ),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaped_bounded_notifications() {
        assert_eq!(
            notification_text("<&>\"'", 200),
            "&lt;&amp;&gt;&quot;&apos;"
        );
        assert_eq!(notification_text(&"é".repeat(201), 200), "é".repeat(200));
        assert_eq!(notification_text("<&", 1), "&lt;");
    }

    #[test]
    fn sweep_only_old_regular_clip_parts() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let now = std::time::SystemTime::now();
        for name in [
            "old.mp4.part",
            "old.json.part",
            "fresh.mp4.part",
            "other.part",
            "clip.mp4",
        ] {
            let path = dir.path().join(name);
            fs::write(&path, b"test")?;
            if !name.starts_with("fresh") {
                fs::File::options().write(true).open(&path)?.set_times(
                    fs::FileTimes::new().set_modified(now - std::time::Duration::from_secs(3601)),
                )?;
            }
        }
        fs::create_dir(dir.path().join("directory.mp4.part"))?;
        std::os::unix::fs::symlink(
            dir.path().join("other.part"),
            dir.path().join("link.mp4.part"),
        )?;
        sweep_parts(dir.path(), now)?;
        for name in ["old.mp4.part", "old.json.part"] {
            assert!(!dir.path().join(name).exists());
        }
        for name in [
            "fresh.mp4.part",
            "other.part",
            "clip.mp4",
            "directory.mp4.part",
            "link.mp4.part",
        ] {
            assert!(dir.path().join(name).symlink_metadata().is_ok());
        }
        Ok(())
    }

    #[test]
    fn safe_clip_slugs() {
        for name in [
            "../../bad/path",
            "你好",
            "",
            ".",
            "Counter-Strike 2™",
            "a\0b\nc",
            &"x".repeat(400),
        ] {
            let id = format!("2026-10-16_22-41-07_{}", slug(name));
            assert!(clip::is_valid_id(&id), "{id}");
            assert!(id.len() < 200);
            assert!(!id.contains('/'));
        }
        assert_eq!(slug("Counter-Strike 2"), "counter-strike-2");
    }
}
