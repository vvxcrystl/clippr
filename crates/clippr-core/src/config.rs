//! User configuration, stored as TOML at [`crate::paths::config_file`].
//!
//! Every field has a default, so a partial or missing file loads cleanly.

use std::{io::Write, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub video: Video,
    pub audio: Audio,
    pub hotkeys: Hotkeys,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Where finished clips and their sidecars are written.
    pub clips_dir: PathBuf,
    /// Length saved by the primary clip hotkey, in seconds.
    pub clip_short_secs: u32,
    /// Length saved by the secondary clip hotkey, in seconds.
    pub clip_long_secs: u32,
    /// Show a desktop notification after a clip is saved.
    pub notify_on_save: bool,
    /// Start capturing as soon as the daemon starts. When false the daemon idles until resumed.
    pub start_recording: bool,
}

impl General {
    /// Seconds of history the replay buffer must hold: the longest clip length, plus 2s
    /// of slack so a clip can start on a keyframe.
    pub fn buffer_secs(&self) -> u32 {
        self.clip_short_secs.max(self.clip_long_secs) + 2
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Low,
    Medium,
    High,
    /// Use [`Video::bitrate_kbps`].
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Native,
    P1440,
    P1080,
    P720,
}

impl Resolution {
    /// Target output height, or `None` to keep the source size.
    pub fn height(self) -> Option<u32> {
        match self {
            Resolution::Native => None,
            Resolution::P1440 => Some(1440),
            Resolution::P1080 => Some(1080),
            Resolution::P720 => Some(720),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderPref {
    /// Hardware if it works, software otherwise.
    Auto,
    Hardware,
    Software,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Video {
    pub codec: Codec,
    pub quality: Quality,
    /// Only used when `quality` is `Custom`.
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub resolution: Resolution,
    pub encoder: EncoderPref,
    pub capture_cursor: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Audio {
    /// Record what the default output device plays.
    pub capture_desktop: bool,
    pub capture_mic: bool,
    /// PipeWire node name for the microphone. `None` means the default source.
    pub mic_device: Option<String>,
    pub bitrate_kbps: u32,
}

/// Preferred key combinations, in xdg-desktop-portal GlobalShortcuts trigger syntax
/// (e.g. `"F8"`, `"SHIFT+F8"`, `"CTRL+ALT+C"`). The compositor may let the user rebind them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub clip_short: String,
    pub clip_long: String,
    pub toggle_overlay: String,
}

impl Default for General {
    fn default() -> Self {
        Self {
            clips_dir: crate::paths::default_clips_dir(),
            clip_short_secs: 30,
            clip_long_secs: 60,
            notify_on_save: true,
            start_recording: true,
        }
    }
}

impl Default for Video {
    fn default() -> Self {
        Self {
            codec: Codec::H264,
            quality: Quality::Medium,
            bitrate_kbps: 12_000,
            fps: 60,
            resolution: Resolution::Native,
            encoder: EncoderPref::Auto,
            capture_cursor: true,
        }
    }
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            capture_desktop: true,
            capture_mic: false,
            mic_device: None,
            bitrate_kbps: 160,
        }
    }
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            clip_short: "F8".into(),
            clip_long: "SHIFT+F8".into(),
            toggle_overlay: "ALT+F8".into(),
        }
    }
}

impl Config {
    /// Load from [`crate::paths::config_file`]. A missing file yields defaults.
    pub fn load() -> anyhow::Result<Self> {
        let path = crate::paths::config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write atomically (temp file + rename) to [`crate::paths::config_file`].
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&crate::paths::config_file())
    }

    fn save_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("config has no parent"))?;
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        tmp.as_file().sync_all()?;
        tmp.persist(path)?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }

    /// Bitrate the encoder should target for an output of `height` pixels.
    pub fn video_bitrate_kbps(&self, height: u32) -> u32 {
        // Medium is tuned to look clean at 60fps in fast games without bloating the buffer.
        let base = match height {
            0..=720 => 6_000,
            721..=1080 => 10_000,
            1081..=1440 => 16_000,
            _ => 28_000,
        };
        let scaled = match self.video.quality {
            Quality::Low => base * 6 / 10,
            Quality::Medium => base,
            Quality::High => base * 16 / 10,
            Quality::Custom => return self.video.bitrate_kbps.max(500),
        };
        // HEVC and AV1 reach the same quality with fewer bits.
        match self.video.codec {
            crate::config::Codec::H264 => scaled,
            _ => scaled * 7 / 10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_saves_use_independent_temp_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let path = &path;
                    scope.spawn(move || {
                        let mut config = Config::default();
                        config.hotkeys.clip_short = format!("F{i}");
                        config.save_to(path)
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap().unwrap();
            }
        });
        let config: Config = toml::from_str(&std::fs::read_to_string(path)?)?;
        assert!(config.hotkeys.clip_short.starts_with('F'));
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }
}
