//! The clips directory: listing, metadata edits, deletion and thumbnails.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::clip::{is_valid_id, ClipMeta};
use anyhow::{bail, ensure, Context};

/// No supported trash location could accept this clip. Callers may offer permanent deletion.
#[derive(Debug)]
pub struct TrashUnavailable {
    pub path: PathBuf,
    pub reason: String,
}

impl std::fmt::Display for TrashUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "can't move {} to the trash: {}",
            self.path.display(),
            self.reason
        )
    }
}

impl std::error::Error for TrashUnavailable {}

pub struct Library {
    dir: PathBuf,
}

impl Library {
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// All clips, newest first. Existing valid sidecars never invoke ffprobe.
    pub fn list(&self) -> anyhow::Result<Vec<ClipMeta>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let files: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        let mut clips = Vec::new();
        let mut seen = HashSet::new();
        for path in &files {
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| is_valid_id(s))
            else {
                continue;
            };
            if let Ok(clip) = self.get(id) {
                if clip.file.ends_with(".part") {
                    continue;
                }
                seen.insert(id.to_owned());
                clips.push(clip);
            }
        }
        for path in files {
            if !matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("mp4" | "mkv")
            ) {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| is_valid_id(s))
            else {
                continue;
            };
            if seen.contains(id) || path.symlink_metadata()?.file_type().is_symlink() {
                continue;
            }
            match probe(&path, id).and_then(|clip| {
                self.update(&clip)?;
                Ok(clip)
            }) {
                Ok(clip) => {
                    seen.insert(id.to_owned());
                    clips.push(clip);
                }
                Err(e) => eprintln!("clippr: skipping {}: {e:#}", path.display()),
            }
        }
        clips.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(clips)
    }

    pub fn get(&self, id: &str) -> anyhow::Result<ClipMeta> {
        ensure!(is_valid_id(id), "invalid clip id");
        let path = self.dir.join(format!("{id}.json"));
        const MAX_SIDECAR: u64 = 1024 * 1024;
        let meta = fs::symlink_metadata(&path)?;
        ensure!(meta.is_file(), "sidecar is not a regular file");
        ensure!(meta.len() <= MAX_SIDECAR, "sidecar exceeds 1 MiB");
        // Also bound the read if the file grows after the metadata check.
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(MAX_SIDECAR + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= MAX_SIDECAR, "sidecar exceeds 1 MiB");
        let clip: ClipMeta = serde_json::from_slice(&bytes)?;
        ensure!(clip.id == id, "sidecar id mismatch");
        self.checked_video_path(&clip)?;
        Ok(clip)
    }

    /// Returns an empty path for invalid metadata. Use checked_video_path for diagnostics.
    pub fn video_path(&self, clip: &ClipMeta) -> PathBuf {
        self.checked_video_path(clip).unwrap_or_default()
    }

    pub fn checked_video_path(&self, clip: &ClipMeta) -> anyhow::Result<PathBuf> {
        ensure!(is_valid_id(&clip.id), "invalid clip id");
        self.checked_video_filename(&clip.file)
    }

    fn checked_video_filename(&self, filename: &str) -> anyhow::Result<PathBuf> {
        let mut parts = Path::new(filename).components();
        ensure!(
            matches!(parts.next(), Some(Component::Normal(_)))
                && parts.next().is_none()
                && !filename.contains('/')
                && !filename.contains('\\'),
            "invalid video filename"
        );
        let path = self.dir.join(filename);
        match fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(meta.is_file(), "video is not a regular file"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        Ok(path)
    }

    pub fn update(&self, clip: &ClipMeta) -> anyhow::Result<()> {
        self.checked_video_path(clip)?;
        fs::create_dir_all(&self.dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer_pretty(&mut tmp, clip)?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.dir.join(format!("{}.json", clip.id)))?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        let clip = self.get(id)?;
        let video = self.checked_video_path(&clip)?;
        let sidecar = self.dir.join(format!("{id}.json"));
        trash_pair(&video, &sidecar)?;
        remove_if_present(&crate::paths::thumbs_dir().join(format!("{id}.jpg")))
    }

    /// Permanently remove a clip, including an orphaned video or sidecar.
    /// Symlinked sidecars are unlinked without reading their targets.
    pub fn delete_permanently(&self, id: &str) -> anyhow::Result<()> {
        ensure!(is_valid_id(id), "invalid clip id");
        let sidecar = self.dir.join(format!("{id}.json"));
        let mut videos = Vec::new();
        match fs::symlink_metadata(&sidecar) {
            Ok(meta) if meta.is_file() => videos.push(self.checked_video_path(&self.get(id)?)?),
            Ok(meta) if meta.file_type().is_symlink() => (),
            Ok(_) => bail!("sidecar is not a regular file"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        // Without a readable sidecar, only the conventional clip filenames are known.
        if videos.is_empty() {
            for extension in ["mp4", "mkv"] {
                videos.push(self.checked_video_filename(&format!("{id}.{extension}"))?);
            }
        }
        let mut paths: Vec<_> = videos.iter().map(PathBuf::as_path).collect();
        paths.push(&sidecar);
        let mut existing = Vec::new();
        for path in paths {
            match fs::symlink_metadata(path) {
                Ok(_) => existing.push(path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        with_recovery_links(&existing, || {
            for path in &existing {
                remove_if_present(path)?;
            }
            Ok(())
        })?;
        remove_if_present(&crate::paths::thumbs_dir().join(format!("{id}.jpg")))
    }

    pub fn thumbnail(&self, clip: &ClipMeta) -> anyhow::Result<PathBuf> {
        let video = self.checked_video_path(clip)?;
        let dir = crate::paths::thumbs_dir();
        fs::create_dir_all(&dir)?;
        let output = dir.join(format!("{}.jpg", clip.id));
        let modified = fs::metadata(&video)?.modified()?;
        if fs::metadata(&output)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t >= modified)
        {
            return Ok(output);
        }
        let tmp = tempfile::Builder::new().suffix(".jpg").tempfile_in(&dir)?;
        let result = Command::new("nice")
            .args(["-n", "19", "ffmpeg", "-nostdin", "-v", "error", "-y", "-ss"])
            .arg((clip.duration_secs / 2.0).clamp(0.0, 1.0).to_string())
            .arg("-i")
            .arg(video)
            .args(["-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "4"])
            .arg(tmp.path())
            .output()?;
        ensure!(
            result.status.success(),
            "thumbnail failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        tmp.persist(&output)?;
        Ok(output)
    }
}

pub(crate) fn probe(path: &Path, id: &str) -> anyhow::Result<ClipMeta> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-i",
        ])
        .arg(path)
        .output()?;
    ensure!(
        output.status.success(),
        "ffprobe: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let video = value["streams"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["codec_type"] == "video"))
        .context("no video stream")?;
    let fps = video["avg_frame_rate"].as_str().unwrap_or("0/1");
    let (n, d) = fps.split_once('/').unwrap_or((fps, "1"));
    let fps = (n.parse::<f64>().unwrap_or(0.0) / d.parse::<f64>().unwrap_or(1.0)).round() as u32;
    let meta = fs::metadata(path)?;
    Ok(ClipMeta {
        id: id.into(),
        title: id.into(),
        file: path
            .file_name()
            .context("missing filename")?
            .to_str()
            .context("non-UTF8 filename")?
            .into(),
        created_at: meta.modified()?.into(),
        duration_secs: value["format"]["duration"]
            .as_str()
            .context("missing duration")?
            .parse()?,
        app: None,
        width: video["width"].as_u64().unwrap_or(0) as u32,
        height: video["height"].as_u64().unwrap_or(0) as u32,
        fps,
        codec: video["codec_name"].as_str().unwrap_or("unknown").into(),
        size_bytes: meta.len(),
        favorite: false,
        tags: vec![],
        edited_from: None,
        mix_gains: vec![],
        audio_tracks: value["streams"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["codec_type"] == "audio")
            .enumerate()
            .map(|(index, stream)| {
                use crate::clip::{AudioTrack, AudioTrackKind};
                // ffmpeg's MP4 muxer stores a track title as `name`; other muxers use
                // `title` or the handler name. "SoundHandler" is the generic default.
                let label = ["title", "name", "handler_name"]
                    .iter()
                    .filter_map(|key| stream["tags"][*key].as_str())
                    .find(|s| !s.is_empty() && *s != "SoundHandler");
                let kind = label
                    .and_then(AudioTrackKind::from_label)
                    .unwrap_or(AudioTrackKind::Mix);
                AudioTrack {
                    index: index as u32,
                    kind,
                    label: label.unwrap_or(kind.label()).into(),
                }
            })
            .collect(),
    })
}

fn remove_if_present(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn mount_top(path: &Path) -> anyhow::Result<PathBuf> {
    mount_top_with(path, |p| Ok(fs::metadata(p)?.dev()))
}

fn mount_top_with(
    path: &Path,
    device: impl Fn(&Path) -> anyhow::Result<u64>,
) -> anyhow::Result<PathBuf> {
    let dev = device(path)?;
    let mut top = path.parent().context("missing parent")?.to_path_buf();
    while let Some(parent) = top.parent() {
        if device(parent)? != dev {
            break;
        }
        top = parent.to_path_buf();
    }
    Ok(top)
}

fn private_dir(path: &Path, uid: u32) -> anyhow::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && meta.uid() == uid,
        "unsafe trash directory {}",
        path.display()
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn top_trashes(top: &Path, uid: u32) -> Vec<PathBuf> {
    let shared = top.join(".Trash");
    let mut roots = Vec::new();
    if fs::symlink_metadata(&shared).is_ok_and(|m| m.is_dir() && m.mode() & 0o1000 != 0) {
        roots.push(shared.join(uid.to_string()));
    }
    roots.push(top.join(format!(".Trash-{uid}")));
    roots
}

fn trash_pair(video: &Path, sidecar: &Path) -> anyhow::Result<()> {
    let video = fs::canonicalize(video)?;
    let sidecar = fs::canonicalize(sidecar)?;
    let uid = unsafe { libc::geteuid() };
    let mut reasons = Vec::new();
    let mut candidates = Vec::new();
    if let Some(data) = dirs::data_dir() {
        let root = data.join("Trash");
        match fs::create_dir_all(&data) {
            Ok(()) => candidates.push((root, None)),
            Err(e) => reasons.push(format!("home trash: {e}")),
        }
    }
    match mount_top(&video) {
        Ok(top) => {
            for root in top_trashes(&top, uid) {
                candidates.push((root, Some(top.clone())));
            }
        }
        Err(e) => reasons.push(format!("mount trash: {e:#}")),
    }
    try_trashes(&video, &sidecar, uid, candidates, reasons, gio_trash_pair)
}

fn try_trashes(
    video: &Path,
    sidecar: &Path,
    uid: u32,
    candidates: Vec<(PathBuf, Option<PathBuf>)>,
    mut reasons: Vec<String>,
    fallback: impl FnOnce(&Path, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for (root, top) in candidates {
        match trash_pair_in(video, sidecar, &root, top.as_deref(), uid) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // Never try another strategy after an unsuccessful rollback.
                ensure!(
                    video.try_exists()? && sidecar.try_exists()?,
                    "trash rollback failed: {e:#}"
                );
                reasons.push(format!("{}: {e:#}", root.display()));
            }
        }
    }
    match fallback(video, sidecar) {
        Ok(()) => return Ok(()),
        Err(e) => reasons.push(format!("gio trash: {e:#}")),
    }
    Err(TrashUnavailable {
        path: video.to_path_buf(),
        reason: reasons.join("; "),
    }
    .into())
}

fn trash_pair_in(
    video: &Path,
    sidecar: &Path,
    root: &Path,
    top: Option<&Path>,
    uid: u32,
) -> anyhow::Result<()> {
    private_dir(root, uid)?;
    let files = root.join("files");
    let info = root.join("info");
    private_dir(&files, uid)?;
    private_dir(&info, uid)?;
    let dev = fs::metadata(&files)?.dev();
    ensure!(
        fs::metadata(video)?.dev() == dev && fs::metadata(sidecar)?.dev() == dev,
        "trash is on a different device"
    );
    let (dest, infopath) = trash_into(video, &files, &info, top)?;
    if let Err(e) = trash_into(sidecar, &files, &info, top) {
        fs::rename(&dest, video).context(format!("rollback failed after {e:#}"))?;
        remove_if_present(&infopath)?;
        return Err(e);
    }
    Ok(())
}

fn trash_into(
    path: &Path,
    files: &Path,
    info: &Path,
    top: Option<&Path>,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let original = match top {
        Some(top) => path.strip_prefix(top)?,
        None => path,
    };
    let encoded: String = original
        .as_os_str()
        .as_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let name = path
        .file_name()
        .context("missing filename")?
        .to_string_lossy();
    for suffix in 0.. {
        let name = if suffix == 0 {
            name.to_string()
        } else {
            format!("{name}.{suffix}")
        };
        let dest = files.join(&name);
        if fs::symlink_metadata(&dest).is_ok() {
            continue;
        }
        let infopath = info.join(format!("{name}.trashinfo"));
        let mut record = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&infopath)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        };
        let result = (|| -> anyhow::Result<()> {
            writeln!(
                record,
                "[Trash Info]\nPath={encoded}\nDeletionDate={}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S")
            )?;
            record.sync_all()?;
            fs::rename(path, &dest)?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&infopath);
            return Err(e);
        }
        return Ok((dest, infopath));
    }
    bail!("cannot allocate trash name")
}

fn gio_trash_pair(video: &Path, sidecar: &Path) -> anyhow::Result<()> {
    gio_trash_pair_with(video, sidecar, |paths| {
        let output = Command::new("gio")
            .arg("trash")
            .arg("--")
            .args(paths)
            .output()?;
        ensure!(
            output.status.success(),
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    })
}

fn gio_trash_pair_with(
    video: &Path,
    sidecar: &Path,
    run: impl FnOnce(&[&Path]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    with_recovery_links(&[video, sidecar], || run(&[video, sidecar]))
}

fn with_recovery_links(
    paths: &[&Path],
    run: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let Some(first) = paths.first() else {
        return run();
    };
    // gio exposes no rollback destination and may move only one argument. Hard links
    // also make permanent deletion recoverable without copying video data.
    let backup = tempfile::Builder::new()
        .prefix(".clippr-delete-")
        .tempdir_in(first.parent().context("missing parent")?)?;
    for (index, path) in paths.iter().enumerate() {
        fs::hard_link(path, backup.path().join(index.to_string()))?;
    }
    if let Err(e) = run() {
        for (index, path) in paths.iter().enumerate() {
            if matches!(fs::symlink_metadata(path), Err(ref e) if e.kind() == std::io::ErrorKind::NotFound)
            {
                if let Err(restore) = fs::rename(backup.path().join(index.to_string()), path) {
                    let retained = backup.keep();
                    bail!(
                        "rollback failed after {e:#}: {restore}; recovery files retained in {}",
                        retained.display()
                    );
                }
            }
        }
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn pair(dir: &Path) -> (PathBuf, PathBuf) {
        let video = dir.join("clip #.mp4");
        let sidecar = dir.join("clip #.json");
        fs::write(&video, b"video").unwrap();
        fs::write(&sidecar, b"sidecar").unwrap();
        (video, sidecar)
    }

    #[test]
    fn injected_mount_boundary_and_topdir_selection() -> anyhow::Result<()> {
        let top = mount_top_with(Path::new("/mount/clips/video.mp4"), |path| {
            Ok(if path == Path::new("/") { 1 } else { 2 })
        })?;
        assert_eq!(top, Path::new("/mount"));
        let tmp = tempfile::tempdir()?;
        let uid = unsafe { libc::geteuid() };
        let private = tmp.path().join(format!(".Trash-{uid}"));
        assert_eq!(top_trashes(tmp.path(), uid), vec![private.clone()]);
        let shared = tmp.path().join(".Trash");
        fs::create_dir(&shared)?;
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777))?;
        assert_eq!(top_trashes(tmp.path(), uid), vec![private.clone()]);
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777))?;
        assert_eq!(
            top_trashes(tmp.path(), uid),
            vec![shared.join(uid.to_string()), private.clone()]
        );
        fs::remove_dir(&shared)?;
        let target = tmp.path().join("target");
        fs::create_dir(&target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o1777))?;
        symlink(target, &shared)?;
        assert_eq!(top_trashes(tmp.path(), uid), vec![private.clone()]);
        symlink(&shared, &private)?;
        assert!(private_dir(&private, uid).is_err());
        assert!(private_dir(&tmp.path().join("wrong-owner"), uid.wrapping_add(1)).is_err());
        Ok(())
    }

    #[test]
    fn topdir_paths_are_relative_and_collisions_preserve_entries() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let (video, sidecar) = pair(tmp.path());
        let uid = unsafe { libc::geteuid() };
        let root = tmp.path().join(format!(".Trash-{uid}"));
        trash_pair_in(&video, &sidecar, &root, Some(tmp.path()), uid)?;
        let info = fs::read_to_string(root.join("info/clip #.mp4.trashinfo"))?;
        assert!(info.contains("\nPath=clip%20%23.mp4\n"));
        pair(tmp.path());
        trash_pair_in(&video, &sidecar, &root, Some(tmp.path()), uid)?;
        assert_eq!(fs::read(root.join("files/clip #.mp4"))?, b"video");
        assert_eq!(fs::read(root.join("files/clip #.mp4.1"))?, b"video");
        assert_eq!(fs::metadata(root)?.mode() & 0o777, 0o700);
        Ok(())
    }

    #[test]
    fn sidecar_info_failure_rolls_video_back() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let (video, old_sidecar) = pair(tmp.path());
        // The source fits NAME_MAX, but adding .trashinfo does not.
        let sidecar = tmp.path().join("s".repeat(250));
        fs::rename(old_sidecar, &sidecar)?;
        let root = tmp.path().join("Trash");
        let err = trash_pair_in(&video, &sidecar, &root, None, unsafe { libc::geteuid() });
        assert!(err.is_err());
        assert_eq!(fs::read(&video)?, b"video");
        assert_eq!(fs::read(&sidecar)?, b"sidecar");
        assert_eq!(fs::read_dir(root.join("files"))?.count(), 0);
        assert_eq!(fs::read_dir(root.join("info"))?.count(), 0);
        Ok(())
    }

    #[test]
    fn unavailable_downcasts_and_keeps_fallback_diagnostics() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let (video, sidecar) = pair(tmp.path());
        let unsafe_root = tmp.path().join("unsafe");
        symlink(tmp.path(), &unsafe_root)?;
        let err = try_trashes(
            &video,
            &sidecar,
            unsafe { libc::geteuid() },
            vec![(unsafe_root, None)],
            vec![],
            |_, _| bail!("exit status: 1: simulated gio stderr"),
        )
        .unwrap_err();
        let typed = err.downcast_ref::<TrashUnavailable>().unwrap();
        assert_eq!(typed.path, video);
        assert!(typed.reason.contains("simulated gio stderr"));
        assert!(err.to_string().contains("can't move"));
        assert!(sidecar.exists());
        assert!(video.exists());
        Ok(())
    }

    #[test]
    fn gio_partial_failure_and_permanent_failure_restore_both_files() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let (video, sidecar) = pair(tmp.path());
        let err = gio_trash_pair_with(&video, &sidecar, |paths| {
            fs::remove_file(paths[0])?;
            bail!("sidecar failed")
        });
        assert!(err.is_err());
        assert_eq!(fs::read(&video)?, b"video");
        assert_eq!(fs::read(&sidecar)?, b"sidecar");
        let err = with_recovery_links(&[&video, &sidecar], || {
            fs::remove_file(&video)?;
            bail!("sidecar remove failed")
        });
        assert!(err.is_err());
        assert_eq!(fs::read(&video)?, b"video");
        assert_eq!(fs::read(&sidecar)?, b"sidecar");
        assert_eq!(fs::read_dir(tmp.path())?.count(), 2);
        Ok(())
    }

    #[test]
    fn rejects_trash_files_on_another_device() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let (video, sidecar) = pair(tmp.path());
        // Only create inside an owned temporary directory on the other device.
        if let Ok(other) = tempfile::tempdir_in("/dev/shm") {
            if fs::metadata(other.path())?.dev() != fs::metadata(tmp.path())?.dev() {
                let err = trash_pair_in(
                    &video,
                    &sidecar,
                    &other.path().join("Trash"),
                    None,
                    unsafe { libc::geteuid() },
                )
                .unwrap_err();
                assert!(err.to_string().contains("different device"));
                assert!(video.exists());
                assert!(sidecar.exists());
            }
        }
        Ok(())
    }
}
