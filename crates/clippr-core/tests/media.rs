use clippr_core::{
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
    // Exercise the cross-filesystem failure path without invoking the user's trash service.
    if let Ok(other_fs) = tempfile::tempdir_in("/dev/shm") {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(other_fs.path())?.dev() != std::fs::metadata(&clips)?.dev() {
            let bin = tmp.path().join("bin");
            std::fs::create_dir(&bin)?;
            std::fs::copy("/usr/bin/false", bin.join("gio"))?;
            std::env::set_var("PATH", &bin);
            std::env::set_var("XDG_DATA_HOME", other_fs.path());
            assert!(lib.delete("corrupt").is_err());
            assert!(clips.join("corrupt.mkv").exists());
            assert!(clips.join("corrupt.json").exists());
        }
    }
    Ok(())
}
