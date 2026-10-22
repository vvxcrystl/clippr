use clippr_core::{
    clip::AudioTrackKind,
    edit::{export, ExportRequest, Segment},
    library::Library,
};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

fn duration(path: &std::path::Path) -> f64 {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn media_library_and_exports() -> anyhow::Result<()> {
    // Audio export coverage does not need GPU work. An unavailable test driver
    // exercises the existing software fallback without disturbing a live recorder.
    std::env::set_var("LIBVA_DRIVER_NAME", "clippr_test_no_vaapi");
    let tmp = tempfile::tempdir()?;
    // This integration test runs alone in its process; all XDG writes are isolated.
    std::env::set_var("XDG_CACHE_HOME", tmp.path().join("cache"));
    std::env::set_var("XDG_DATA_HOME", tmp.path().join("data"));
    let clips = tmp.path().join("clips");
    let lib = Library::open(&clips);
    assert!(lib.list()?.is_empty());
    std::fs::create_dir(&clips)?;
    let video = clips.join("source.mp4");
    let result = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=640x360:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine",
            "-t",
            "6",
            "-c:v",
            "libx264",
            "-g",
            "30",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ])
        .arg(&video)
        .output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut clip = lib.list()?.remove(0);
    assert_eq!(clip.id, "source");
    assert_eq!(clip.width, 640);
    assert_eq!(clip.height, 360);
    assert!((clip.duration_secs - 6.0).abs() < 0.1);
    clip.title = "My title".into();
    clip.tags = vec!["tag".into()];
    clip.favorite = true;
    lib.update(&clip)?;
    assert_eq!(lib.list()?[0], clip);
    // A valid sidecar is authoritative; no probe of the video is needed.
    std::fs::rename(&video, clips.join("hidden"))?;
    assert_eq!(lib.list()?[0], clip);
    std::fs::rename(clips.join("hidden"), &video)?;
    assert!(lib.get("../source").is_err());
    let mut bad = clip.clone();
    bad.file = "../outside.mp4".into();
    assert!(lib.checked_video_path(&bad).is_err());
    assert!(lib.video_path(&bad).as_os_str().is_empty());
    assert!(lib.update(&bad).is_err());
    bad = clip.clone();
    bad.id = "../bad".into();
    assert!(lib.update(&bad).is_err());
    std::os::unix::fs::symlink(&video, clips.join("linked.mp4"))?;
    bad = clip.clone();
    bad.file = "linked.mp4".into();
    assert!(lib.checked_video_path(&bad).is_err());
    let thumb = lib.thumbnail(&clip)?;
    assert!(thumb.is_file());
    assert!(thumb.starts_with(tmp.path()));
    let first_modified = std::fs::metadata(&thumb)?.modified()?;
    assert_eq!(lib.thumbnail(&clip)?, thumb);
    assert_eq!(std::fs::metadata(&thumb)?.modified()?, first_modified);
    std::fs::File::options()
        .write(true)
        .open(&thumb)?
        .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))?;
    lib.thumbnail(&clip)?;
    assert!(std::fs::metadata(&thumb)?.modified()? > std::time::SystemTime::UNIX_EPOCH);
    std::fs::write(clips.join("corrupt.json"), "{")?;
    std::fs::copy(&video, clips.join("corrupt.mkv"))?;
    std::fs::copy(&video, clips.join("ignored.mp4.part"))?;
    std::fs::copy(&video, clips.join(".invalid.mp4"))?;
    let listed = lib.list()?;
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, "corrupt");
    assert!(lib.get("corrupt").is_ok());
    // Oversized sidecars are corrupt metadata: reject directly and regenerate from video.
    let sidecar = clips.join("source.json");
    let mut oversized = serde_json::to_vec(&clip)?;
    oversized.resize(1024 * 1024 + 1, b' ');
    std::fs::write(&sidecar, oversized)?;
    assert!(lib.get("source").unwrap_err().to_string().contains("1 MiB"));
    assert_eq!(lib.list()?.len(), 2);
    assert!(std::fs::metadata(&sidecar)?.len() < 1024 * 1024);
    assert!(lib.get("source").is_ok());
    lib.update(&clip)?;
    let keep = vec![
        Segment {
            start_secs: 0.0,
            end_secs: 1.0,
        },
        Segment {
            start_secs: 3.0,
            end_secs: 5.0,
        },
    ];
    let cancel = AtomicBool::new(false);
    let mut values = vec![];
    let mut req = ExportRequest {
        source_id: clip.id.clone(),
        keep,
        title: "Edited".into(),
        precise: false,
        replace_source: false,
        track_gains: vec![],
    };
    let fast = export(&lib, &req, &mut |p| values.push(p), &cancel)?;
    assert_eq!(fast.id, "source_edit");
    assert!((duration(&lib.video_path(&fast)) - 3.0).abs() < 0.35);
    assert_eq!(fast.tags, clip.tags);
    assert_eq!(fast.edited_from, Some(clip.id.clone()));
    assert!(values.iter().all(|p| (0.0..=1.0).contains(p)));
    assert_eq!(values.last(), Some(&1.0));
    req.precise = true;
    let precise = export(&lib, &req, &mut |_| (), &cancel)?;
    assert_eq!(precise.id, "source_edit-2");
    assert!((duration(&lib.video_path(&precise)) - 3.0).abs() < 0.15);
    assert_eq!((precise.width, precise.height, precise.fps), (640, 360, 30));
    assert!(Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(lib.video_path(&precise))
        .args(["-f", "null", "-"])
        .status()?
        .success());
    // Preserve the requested ordering and clamp out-of-range segments.
    req.keep = vec![
        Segment {
            start_secs: 4.0,
            end_secs: 5.0,
        },
        Segment {
            start_secs: -1.0,
            end_secs: 1.0,
        },
        Segment {
            start_secs: 2.0,
            end_secs: -1.0,
        },
    ];
    req.replace_source = true;
    let replaced = export(&lib, &req, &mut |_| (), &cancel)?;
    assert_eq!(replaced.id, "source");
    assert_eq!(replaced.created_at, clip.created_at);
    assert!((duration(&video) - 2.0).abs() < 0.15);
    assert_eq!(lib.get("source")?, replaced);
    req.replace_source = false;
    let count = lib.list()?.len();
    cancel.store(true, Ordering::Relaxed);
    assert!(export(&lib, &req, &mut |_| (), &cancel).is_err());
    cancel.store(false, Ordering::Relaxed);
    assert!(export(
        &lib,
        &req,
        &mut |p| {
            if p > 0.0 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        &cancel
    )
    .is_err());
    assert_eq!(lib.list()?.len(), count);
    assert!(!std::fs::read_dir(&clips)?.any(|e| e.unwrap().path().is_dir()));
    cancel.store(false, Ordering::Relaxed);
    req.keep = vec![Segment {
        start_secs: 5.0,
        end_secs: 1.0,
    }];
    assert!(export(&lib, &req, &mut |_| (), &cancel).is_err());
    // A silent source must work in both export modes.
    let silent = clips.join("silent.mp4");
    assert!(Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(lib.video_path(&fast))
        .args(["-c:v", "copy", "-an"])
        .arg(&silent)
        .status()?
        .success());
    lib.list()?;
    req.source_id = "silent".into();
    req.keep = vec![
        Segment {
            start_secs: 0.0,
            end_secs: 0.5,
        },
        Segment {
            start_secs: 1.0,
            end_secs: 1.5,
        },
    ];
    for precise in [false, true] {
        req.precise = precise;
        let silent_export = export(&lib, &req, &mut |_| (), &cancel)?;
        assert!((silent_export.duration_secs - 1.0).abs() < if precise { 0.15 } else { 0.7 });
    }
    lib.delete("source")?;
    assert!(!video.exists());
    assert!(!clips.join("source.json").exists());
    assert!(!thumb.exists());
    let trash = tmp.path().join("data/Trash");
    assert!(trash.join("files/source.mp4").exists());
    let info = std::fs::read_to_string(trash.join("info/source.mp4.trashinfo"))?;
    assert!(info.contains("[Trash Info]\nPath=/"));
    assert!(info.contains("DeletionDate="));
    // Repeated names reserve distinct trash entries; special path characters are escaped.
    let spaced = Library::open(tmp.path().join("space # dir"));
    std::fs::create_dir_all(spaced.dir())?;
    std::fs::copy(lib.video_path(&fast), spaced.dir().join("source.mp4"))?;
    spaced.list()?;
    spaced.delete("source")?;
    assert!(trash.join("files/source.mp4.1").exists());
    let info = std::fs::read_to_string(trash.join("info/source.mp4.1.trashinfo"))?;
    assert!(info.contains("space%20%23%20dir"));
    // Permanent deletion also cleans up orphaned metadata and cached thumbnails.
    let outside = tmp.path().join("outside.json");
    std::fs::write(&outside, "do not follow")?;
    std::os::unix::fs::symlink(&outside, spaced.dir().join("linked.json"))?;
    std::fs::write(spaced.dir().join("linked.mp4"), "video")?;
    spaced.delete_permanently("linked")?;
    assert_eq!(std::fs::read_to_string(&outside)?, "do not follow");
    assert!(!spaced.dir().join("linked.mp4").exists());
    assert!(std::fs::symlink_metadata(spaced.dir().join("linked.json")).is_err());
    spaced.delete_permanently("linked")?;
    assert!(spaced.delete_permanently("../outside").is_err());
    let mut orphan_meta = lib.get("corrupt")?;
    orphan_meta.id = "orphan-sidecar".into();
    orphan_meta.file = "orphan-sidecar.mp4".into();
    lib.update(&orphan_meta)?;
    lib.delete_permanently("orphan-sidecar")?;
    assert!(!clips.join("orphan-sidecar.json").exists());
    let corrupt_thumb = clippr_core::paths::thumbs_dir().join("corrupt.jpg");
    std::fs::write(&corrupt_thumb, "thumbnail")?;
    lib.delete_permanently("corrupt")?;
    assert!(!corrupt_thumb.exists());
    assert!(!clips.join("corrupt.mkv").exists());
    assert!(!clips.join("corrupt.json").exists());
    std::fs::write(spaced.dir().join("orphan.mkv"), "video")?;
    spaced.delete_permanently("orphan")?;
    assert!(!spaced.dir().join("orphan.mkv").exists());
    std::os::unix::fs::symlink(&outside, spaced.dir().join("unsafe.mp4"))?;
    assert!(spaced.delete_permanently("unsafe").is_err());
    assert_eq!(std::fs::read_to_string(&outside)?, "do not follow");
    Ok(())
}

fn probe_streams(path: &std::path::Path) -> Vec<serde_json::Value> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success());
    serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["streams"]
        .as_array()
        .unwrap()
        .clone()
}
fn decoded_audio(path: &std::path::Path, index: u32) -> Vec<f32> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            &format!("0:a:{index}"),
            "-ac",
            "1",
            "-ar",
            "48000",
            "-f",
            "f32le",
            "-",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    out.stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect()
}
fn tone_amplitude(samples: &[f32], frequency: f64) -> f64 {
    let samples = &samples[4800..samples.len().min(48000)];
    let mut re = 0.0;
    let mut im = 0.0;
    for (i, &sample) in samples.iter().enumerate() {
        let a = i as f64 * frequency * std::f64::consts::TAU / 48000.0;
        re += f64::from(sample) * a.cos();
        im += f64::from(sample) * a.sin();
    }
    re.hypot(im) * 2.0 / samples.len() as f64
}
fn video_hash(path: &std::path::Path) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-c", "copy", "-f", "hash", "-"])
        .output()
        .unwrap();
    assert!(out.status.success());
    out.stdout
}

#[test]
fn multitrack_exports_preserve_sources_and_rebuild_only_mix() -> anyhow::Result<()> {
    std::env::set_var("LIBVA_DRIVER_NAME", "clippr_test_no_vaapi");
    use clippr_core::{clip::AudioTrackKind, edit::TrackGain};
    let tmp = tempfile::tempdir()?;
    let lib = Library::open(tmp.path());
    let input = tmp.path().join("tracks.mp4");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=128x72:rate=20",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=660:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000",
            "-filter_complex",
            "[1:a][2:a][3:a]amix=inputs=3:normalize=0[m]",
            "-map",
            "0:v",
            "-map",
            "[m]",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-map",
            "3:a",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "20",
            "-bf",
            "0",
            "-c:a",
            "aac",
            "-t",
            "4",
            "-metadata:s:a:0",
            "handler_name=Mix",
            "-metadata:s:a:1",
            "handler_name=Game",
            "-metadata:s:a:2",
            "handler_name=Microphone",
            "-metadata:s:a:3",
            "handler_name=Voice chat",
        ])
        .arg(&input)
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let source = lib.list()?.remove(0);
    assert_eq!(
        source
            .audio_tracks
            .iter()
            .map(|t| t.kind)
            .collect::<Vec<_>>(),
        [
            AudioTrackKind::Mix,
            AudioTrackKind::Desktop,
            AudioTrackKind::Mic,
            AudioTrackKind::Voice
        ]
    );
    let mut req = ExportRequest {
        source_id: source.id.clone(),
        keep: vec![Segment {
            start_secs: 0.0,
            end_secs: 4.0,
        }],
        title: "Tracks".into(),
        precise: false,
        replace_source: false,
        track_gains: vec![],
    };
    let cancel = AtomicBool::new(false);
    let unchanged = export(&lib, &req, &mut |_| (), &cancel)?;
    req.track_gains = vec![
        TrackGain {
            kind: AudioTrackKind::Mic,
            gain: 0.0,
        },
        TrackGain {
            kind: AudioTrackKind::Voice,
            gain: 0.5,
        },
    ];
    let gained = export(&lib, &req, &mut |_| (), &cancel)?;
    let unchanged_path = lib.video_path(&unchanged);
    let gained_path = lib.video_path(&gained);
    assert_eq!(video_hash(&unchanged_path), video_hash(&gained_path));
    for index in 1..4 {
        assert_eq!(
            decoded_audio(&unchanged_path, index),
            decoded_audio(&gained_path, index)
        );
    }
    let mix = decoded_audio(&gained_path, 0);
    let game = tone_amplitude(&mix, 440.0);
    let mic = tone_amplitude(&mix, 660.0);
    let voice = tone_amplitude(&mix, 880.0);
    assert!(
        game > 0.08 && mic < 0.002 && (voice / game - 0.5).abs() < 0.1,
        "{game} {mic} {voice}"
    );
    assert_eq!(gained.audio_tracks, source.audio_tracks);
    let reopened = lib.get(&gained.id)?;
    assert_eq!(reopened.mix_gain(AudioTrackKind::Mic), 0.0);
    assert_eq!(reopened.mix_gain(AudioTrackKind::Voice), 0.5);
    let mut reedit = req.clone();
    reedit.source_id = reopened.id;
    // Returning a muted source to unity must rebuild Mix, and preserve the
    // other saved gains when only one track is specified.
    reedit.track_gains = vec![TrackGain { kind: AudioTrackKind::Mic, gain: 1.0 }];
    let restored = export(&lib, &reedit, &mut |_| (), &cancel)?;
    assert_eq!(restored.mix_gain(AudioTrackKind::Mic), 1.0);
    assert_eq!(restored.mix_gain(AudioTrackKind::Voice), 0.5);
    let mix = decoded_audio(&lib.video_path(&restored), 0);
    let game = tone_amplitude(&mix, 440.0);
    assert!((tone_amplitude(&mix, 660.0) / game - 1.0).abs() < 0.1);
    assert!((tone_amplitude(&mix, 880.0) / game - 0.5).abs() < 0.1);
    reedit.source_id = restored.id.clone();
    reedit.track_gains.clear();
    let copied = export(&lib, &reedit, &mut |_| (), &cancel)?;
    assert_eq!(copied.mix_gains, restored.mix_gains);
    assert_eq!(decoded_audio(&lib.video_path(&copied), 0), decoded_audio(&lib.video_path(&restored), 0));
    req.precise = true;
    req.keep = vec![
        Segment {
            start_secs: 0.2,
            end_secs: 1.2,
        },
        Segment {
            start_secs: 2.2,
            end_secs: 3.2,
        },
    ];
    let precise = export(&lib, &req, &mut |_| (), &cancel)?;
    assert!((precise.duration_secs - 2.0).abs() < 0.12);
    let streams = probe_streams(&lib.video_path(&precise));
    assert_eq!(
        streams
            .iter()
            .filter(|s| s["codec_type"] == "audio")
            .count(),
        4
    );
    let mix = decoded_audio(&lib.video_path(&precise), 0);
    assert!(tone_amplitude(&mix, 660.0) < 0.002);
    for (index, freq) in [(1, 440.0), (2, 660.0), (3, 880.0)] {
        assert!(tone_amplitude(&decoded_audio(&lib.video_path(&precise), index), freq) > 0.08);
    }
    // Legacy sidecars, missing field, and the Desktop alias for one mixed track.
    let legacy_path = tmp.path().join("legacy.mp4");
    assert!(Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&input)
        .args(["-map", "0:v", "-map", "0:a:0", "-c", "copy"])
        .arg(&legacy_path)
        .status()?
        .success());
    lib.list()?;
    let mut old = serde_json::to_value(lib.get("legacy")?)?;
    old.as_object_mut().unwrap().remove("audio_tracks");
    std::fs::write(tmp.path().join("legacy.json"), serde_json::to_vec(&old)?)?;
    assert!(lib.get("legacy")?.audio_tracks.is_empty());
    req.source_id = "legacy".into();
    req.precise = false;
    req.keep = vec![Segment {
        start_secs: 0.0,
        end_secs: 4.0,
    }];
    req.track_gains = vec![TrackGain {
        kind: AudioTrackKind::Desktop,
        gain: 0.0,
    }];
    let muted = export(&lib, &req, &mut |_| (), &cancel)?;
    assert_eq!(muted.audio_tracks.len(), 1);
    assert!(tone_amplitude(&decoded_audio(&lib.video_path(&muted), 0), 440.0) < 0.0001);
    req.track_gains[0].gain = f32::NAN;
    assert!(export(&lib, &req, &mut |_| (), &cancel).is_err());
    Ok(())
}

#[test]
fn probing_reads_ffmpeg_mp4_track_names() -> anyhow::Result<()> {
    // ffmpeg's MP4 muxer stores per-track titles as the `name` tag, not `title`.
    let dir = tempfile::tempdir()?;
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=160x90:rate=10"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440", "-f", "lavfi", "-i", "sine=frequency=550"])
        .args(["-f", "lavfi", "-i", "sine=frequency=660", "-t", "1"])
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:a"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
        .args(["-metadata:s:a:0", "title=Mix", "-metadata:s:a:1", "title=Game"])
        .args(["-metadata:s:a:2", "title=Voice chat"])
        .arg(dir.path().join("named.mp4"))
        .status()?;
    assert!(status.success());
    let clips = Library::open(dir.path()).list()?;
    let kinds: Vec<_> = clips[0].audio_tracks.iter().map(|t| t.kind).collect();
    assert_eq!(kinds, [AudioTrackKind::Mix, AudioTrackKind::Desktop, AudioTrackKind::Voice]);
    Ok(())
}
