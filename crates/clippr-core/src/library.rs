//! The clips directory: listing, metadata edits, deletion and thumbnails.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::clip::{is_valid_id, ClipMeta};
use anyhow::{bail, ensure, Context};

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
        ensure!(
            !fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "sidecar is a symlink"
        );
        let clip: ClipMeta = serde_json::from_slice(&fs::read(path)?)?;
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
        let mut parts = Path::new(&clip.file).components();
        ensure!(
            matches!(parts.next(), Some(Component::Normal(_)))
                && parts.next().is_none()
                && !clip.file.contains('/')
                && !clip.file.contains('\\'),
            "invalid video filename"
        );
        let path = self.dir.join(&clip.file);
        if let Ok(meta) = fs::symlink_metadata(&path) {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "video is not a regular file"
            );
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
        trash(&self.checked_video_path(&clip)?)?;
        trash(&self.dir.join(format!("{id}.json")))?;
        match fs::remove_file(crate::paths::thumbs_dir().join(format!("{id}.jpg"))) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        Ok(())
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
    })
}

fn trash(path: &Path) -> anyhow::Result<()> {
    let root = dirs::data_dir().context("no data directory")?.join("Trash");
    let files = root.join("files");
    let info = root.join("info");
    for dir in [&root, &files, &info] {
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    if fs::metadata(path)?.dev() != fs::metadata(&files)?.dev() {
        let status = Command::new("gio")
            .arg("trash")
            .arg("--")
            .arg(path)
            .status()?;
        ensure!(status.success(), "gio trash failed for {}", path.display());
        return Ok(());
    }
    let absolute = fs::canonicalize(path)?;
    let encoded: String = absolute
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
        if dest.exists() {
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
            fs::rename(path, dest)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(infopath);
        }
        return result;
    }
    bail!("cannot allocate trash name")
}
