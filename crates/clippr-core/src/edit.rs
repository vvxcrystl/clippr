//! Clip trimming and cutting with cancellable ffmpeg exports.
use crate::clip::{AudioTrack, AudioTrackKind, ClipMeta};
use crate::config::{Codec, Config, KEYFRAME_INTERVAL_SECS};
use crate::library::{probe, Library};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
/// A time range of the source clip to keep, in seconds.
pub struct Segment {
    pub start_secs: f64,
    pub end_secs: f64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportRequest {
    pub source_id: String,
    /// Ranges to keep, in output order.
    pub keep: Vec<Segment>,
    pub title: String,
    /// Re-encode for frame-accurate cuts; otherwise copy streams at keyframes.
    pub precise: bool,
    /// Replace the source video and metadata while retaining its id.
    pub replace_source: bool,
    #[serde(default)]
    pub track_gains: Vec<TrackGain>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackGain {
    pub kind: AudioTrackKind,
    /// Linear amplitude; zero mutes, one preserves the original amplitude.
    pub gain: f32,
}

/// Export kept ranges. Progress stays in 0..=1; cancellation removes temporary output.
pub fn export(
    lib: &Library,
    req: &ExportRequest,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> anyhow::Result<ClipMeta> {
    ensure!(!cancel.load(Ordering::Relaxed), "export cancelled");
    crate::audio::validate_gains(&req.track_gains)?;
    let source = lib.get(&req.source_id)?;
    let input = lib.checked_video_path(&source)?;
    ensure!(
        source.duration_secs.is_finite() && source.duration_secs > 0.0,
        "invalid source duration"
    );
    let segments: Vec<_> = req
        .keep
        .iter()
        .filter_map(|s| {
            if !s.start_secs.is_finite() || !s.end_secs.is_finite() {
                return None;
            }
            let start_secs = s.start_secs.clamp(0.0, source.duration_secs);
            let end_secs = s.end_secs.clamp(0.0, source.duration_secs);
            (end_secs > start_secs).then_some(Segment {
                start_secs,
                end_secs,
            })
        })
        .collect();
    ensure!(!segments.is_empty(), "no nonempty segments to export");
    let total: f64 = segments.iter().map(|s| s.end_secs - s.start_secs).sum();
    let probed = probe(&input, &source.id)?;
    let audio_count = probed.audio_tracks.len();
    let tracks = if source.audio_tracks.is_empty() {
        probed.audio_tracks
    } else {
        source.audio_tracks.clone()
    };
    ensure!(
        tracks.len() == audio_count,
        "audio track metadata does not match the file"
    );
    ensure!(
        tracks.iter().enumerate().all(|(i, t)| t.index == i as u32),
        "invalid audio track indices"
    );
    crate::audio::validate_gains(&source.mix_gains)?;
    let mut mix_gains = source.mix_gains.clone();
    for gain in &req.track_gains {
        if let Some(existing) = mix_gains.iter_mut().find(|g| g.kind == gain.kind) {
            *existing = gain.clone();
        } else {
            mix_gains.push(gain.clone());
        }
    }
    let remix = !tracks.is_empty() && req.track_gains.iter().any(|g| g.gain != source.mix_gain(g.kind));
    let output_tracks = if remix && tracks.len() > 1 && tracks[0].kind != AudioTrackKind::Mix {
        std::iter::once(AudioTrack::new(0, AudioTrackKind::Mix))
            .chain(tracks.iter().enumerate().map(|(i, t)| AudioTrack {
                index: i as u32 + 1,
                ..t.clone()
            }))
            .collect()
    } else {
        tracks.clone()
    };
    let work = tempfile::tempdir_in(lib.dir())?;
    let output = work.path().join(
        if req.replace_source && input.extension().is_some_and(|e| e == "mkv") {
            "output.mkv"
        } else {
            "output.mp4"
        },
    );
    progress(0.0);
    let mut cut_progress = |p| progress(p * if remix { 0.9f32 } else { 1.0f32 });
    let cut_progress = &mut cut_progress as &mut dyn FnMut(f32);
    if req.precise {
        // Preserve the source codec on hardware rather than expanding HEVC/AV1
        // into H.264. Exports use the same Medium budget/quality rules as capture.
        let mut encoding = Config::default();
        encoding.video.codec = match source.codec.as_str() {
            "hevc" => Codec::Hevc,
            "av1" => Codec::Av1,
            _ => Codec::H264,
        };
        encoding.video.fps = source.fps.clamp(1, 240);
        for hardware in [true, false] {
            let mut filter = String::new();
            let mut labels = String::new();
            for (i, s) in segments.iter().enumerate() {
                filter.push_str(&format!(
                    "[0:v:0]trim=start={}:end={},setpts=PTS-STARTPTS[v{i}];",
                    s.start_secs, s.end_secs
                ));
                labels.push_str(&format!("[v{i}]"));
                for track in &tracks {
                    let t = track.index;
                    filter.push_str(&format!("[0:a:{t}]aresample=async=1:first_pts=0,apad,atrim=start={}:end={},asetpts=PTS-STARTPTS[a{i}_{t}];", s.start_secs, s.end_secs));
                    labels.push_str(&format!("[a{i}_{t}]"));
                }
            }
            let audio_labels: String = tracks.iter().map(|t| format!("[a{}]", t.index)).collect();
            filter.push_str(&format!(
                "{labels}concat=n={}:v=1:a={}[base]{audio_labels};[base]{}[v]",
                segments.len(),
                tracks.len(),
                if hardware {
                    "format=nv12,hwupload"
                } else {
                    "format=yuv420p"
                }
            ));
            let mut cmd = ffmpeg();
            if hardware {
                cmd.args(["-vaapi_device", "/dev/dri/renderD128"]);
            }
            cmd.arg("-i")
                .arg(&input)
                .args(["-filter_complex", &filter, "-map", "[v]"]);
            for track in &tracks {
                cmd.args(["-map", &format!("[a{}]", track.index)]);
            }
            cmd.args(["-c:a", "aac", "-b:a", "160k"]);
            track_metadata(&mut cmd, &tracks);
            if hardware {
                let codec = match encoding.video.codec {
                    Codec::H264 => "h264_vaapi",
                    Codec::Hevc => "hevc_vaapi",
                    Codec::Av1 => "av1_vaapi",
                };
                cmd.args(["-c:v", codec, "-rc_mode", "VBR", "-quality", "1", "-qmin"])
                    .arg(encoding.video.quality_quantizer().unwrap_or(20).to_string());
            } else {
                encoding.video.codec = Codec::H264;
                cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"]);
            }
            let bitrate = encoding.video_bitrate_kbps(source.height);
            cmd.args([
                "-b:v",
                &format!("{bitrate}k"),
                "-maxrate",
                &format!("{bitrate}k"),
            ])
            .args(["-bufsize", &format!("{}k", bitrate * 2), "-bf", "0", "-g"])
            .arg((encoding.video.fps * KEYFRAME_INTERVAL_SECS).to_string())
            .args(["-fps_mode", "passthrough", "-movflags", "+faststart"])
            .arg(&output);
            match run(cmd, total, cut_progress, cancel) {
                Ok(()) => break,
                Err(e) if hardware && !cancel.load(Ordering::Relaxed) => {
                    eprintln!("clippr: VAAPI export failed, using libx264: {e:#}");
                }
                Err(e) => return Err(e),
            }
        }
    } else {
        let mut manifest = std::fs::File::create(work.path().join("concat.txt"))?;
        let mut elapsed = 0.0;
        for (i, s) in segments.iter().enumerate() {
            let name = format!("segment{i}.mp4");
            let duration = s.end_secs - s.start_secs;
            let mut cmd = ffmpeg();
            cmd.arg("-ss")
                .arg(s.start_secs.to_string())
                .arg("-i")
                .arg(&input)
                .arg("-t")
                .arg(duration.to_string())
                .args([
                    "-map",
                    "0:v:0",
                    "-map",
                    "0:a?",
                    "-c",
                    "copy",
                    "-avoid_negative_ts",
                    "make_zero",
                ])
                .arg(work.path().join(&name));
            run(
                cmd,
                duration,
                &mut |p| cut_progress(((elapsed + f64::from(p) * duration) / total * 0.8) as f32),
                cancel,
            )?;
            elapsed += duration;
            writeln!(manifest, "file '{name}'")?;
        }
        drop(manifest);
        let mut cmd = ffmpeg();
        cmd.args(["-f", "concat", "-safe", "1", "-i"])
            .arg(work.path().join("concat.txt"))
            .args([
                "-map",
                "0:v:0",
                "-map",
                "0:a?",
                "-c",
                "copy",
                "-movflags",
                "+faststart",
            ]);
        track_metadata(&mut cmd, &tracks);
        cmd.arg(&output);
        run(cmd, total, &mut |p| cut_progress(0.8 + p * 0.2), cancel)?;
    }
    if remix {
        let cut = work.path().join("cut.mp4");
        std::fs::rename(&output, &cut)?;
        let mut cmd = ffmpeg();
        cmd.arg("-i").arg(&cut).args([
            "-filter_complex",
            &crate::audio::mix_filter(&tracks, &mix_gains)?,
            "-map",
            "0:v:0",
            "-map",
            "[mix]",
        ]);
        for track in tracks
            .iter()
            .skip(usize::from(tracks[0].kind == AudioTrackKind::Mix))
        {
            if tracks.len() > 1 {
                cmd.args(["-map", &format!("0:a:{}", track.index)]);
            }
        }
        cmd.args([
            "-c",
            "copy",
            "-c:a:0",
            "aac",
            "-b:a:0",
            "160k",
            "-movflags",
            "+faststart",
        ]);
        track_metadata(&mut cmd, &output_tracks);
        cmd.arg(&output);
        run(cmd, total, &mut |p| progress(0.9 + p * 0.09), cancel)?;
    }
    ensure!(!cancel.load(Ordering::Relaxed), "export cancelled");
    let id = if req.replace_source {
        source.id.clone()
    } else {
        let base = format!("{}_edit", source.id);
        let mut candidate = base.clone();
        let mut suffix = 2;
        loop {
            ensure!(
                crate::clip::is_valid_id(&candidate),
                "edited clip id exceeds maximum length"
            );
            if !lib.dir().join(format!("{candidate}.json")).exists()
                && !lib.dir().join(format!("{candidate}.mp4")).exists()
                && !lib.dir().join(format!("{candidate}.mkv")).exists()
            {
                break candidate;
            }
            candidate = format!("{base}-{suffix}");
            suffix += 1;
        }
    };
    let mut clip = probe(&output, &id)?;
    clip.audio_tracks = output_tracks;
    clip.mix_gains = mix_gains;
    clip.title = req.title.clone();
    clip.app = source.app.clone();
    clip.tags = source.tags.clone();
    clip.favorite = source.favorite;
    clip.edited_from = Some(source.id.clone());
    let destination = if req.replace_source {
        input
    } else {
        lib.dir().join(format!("{id}.mp4"))
    };
    clip.file = destination
        .file_name()
        .context("missing filename")?
        .to_str()
        .context("non UTF8 filename")?
        .into();
    if req.replace_source {
        clip.created_at = source.created_at;
    }
    ensure!(!cancel.load(Ordering::Relaxed), "export cancelled");
    // Back up a replaced source until its sidecar is committed, allowing rollback on failure.
    let backup = work.path().join("original");
    if req.replace_source {
        std::fs::hard_link(&destination, &backup)?;
    }
    if req.replace_source {
        std::fs::rename(&output, &destination)?;
    } else {
        std::fs::hard_link(&output, &destination)?;
    }
    if let Err(e) = lib.update(&clip) {
        if req.replace_source {
            std::fs::rename(backup, &destination)?;
        } else {
            let _ = std::fs::remove_file(&destination);
        }
        return Err(e);
    }
    progress(1.0);
    Ok(clip)
}

fn track_metadata(cmd: &mut Command, tracks: &[AudioTrack]) {
    for track in tracks {
        let index = track.index;
        cmd.args([
            &format!("-metadata:s:a:{index}"),
            &format!("title={}", track.label),
            &format!("-metadata:s:a:{index}"),
            &format!("handler_name={}", track.label),
            &format!("-disposition:a:{index}"),
            &String::from(if index == 0 { "default" } else { "0" }),
        ]);
    }
}

fn ffmpeg() -> Command {
    let mut cmd = Command::new("nice");
    cmd.args([
        "-n",
        "10",
        "ffmpeg",
        "-nostdin",
        "-v",
        "error",
        "-y",
        "-progress",
        "pipe:1",
        "-nostats",
        "-filter_complex_threads",
        "1",
        "-threads",
        "1",
    ]);
    cmd
}

fn run(
    mut cmd: Command,
    total: f64,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "export cancelled");
    let mut errors = tempfile::tempfile()?;
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(errors.try_clone()?)
        .spawn()
        .context("starting ffmpeg")?;
    let stdout = child.stdout.take().context("missing progress pipe")?;
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(time) = line
                .strip_prefix("out_time_us=")
                .and_then(|s| s.parse::<f64>().ok())
            {
                if tx.send(time).is_err() {
                    break;
                }
            }
        }
    });
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            anyhow::bail!("export cancelled");
        }
        for time in rx.try_iter() {
            progress((time / 1_000_000.0 / total).clamp(0.0, 0.99) as f32);
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let _ = reader.join();
    if !status.success() {
        use std::io::{Seek, SeekFrom};
        errors.seek(SeekFrom::Start(0))?;
        let mut message = String::new();
        errors.read_to_string(&mut message)?;
        anyhow::bail!("ffmpeg failed: {message}");
    }
    progress(1.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_kills_running_ffmpeg() -> anyhow::Result<()> {
        let cancel = AtomicBool::new(false);
        let mut cmd = ffmpeg();
        cmd.args([
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10",
            "-t",
            "60",
            "-f",
            "null",
            "-",
        ]);
        let started = std::time::Instant::now();
        let result = run(
            cmd,
            60.0,
            &mut |p| {
                if p > 0.0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(5));
        Ok(())
    }
}
