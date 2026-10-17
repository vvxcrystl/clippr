//! User configuration, stored as TOML at [`crate::paths::config_file`].
//!
//! Every field has a default, so a partial or missing file loads cleanly.

use std::{io::Write, path::PathBuf};

use serde::{Deserialize, Serialize};

/// Maximum GOP duration, also enforced by timestamps for variable-rate capture.
pub const KEYFRAME_INTERVAL_SECS: u32 = 2;

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
    /// Keep a whole GOP before the requested start, plus one second for stream skew.
    pub fn buffer_secs(&self) -> u32 {
        self.clip_short_secs
            .max(self.clip_long_secs)
            .saturating_add(KEYFRAME_INTERVAL_SECS + 1)
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

impl Video {
    /// Quality ceiling for preset VA encodes: stop spending bits once this QP is
    /// reached, while VBR can raise it on difficult content to target the budget.
    /// AV1 uses a 0..255 quantizer index rather than H.264/HEVC's 0..51 QP.
    /// Custom keeps the user's bitrate without a preset quality ceiling.
    pub fn quality_quantizer(&self) -> Option<u32> {
        match (self.codec, self.quality) {
            (_, Quality::Custom) => None,
            (Codec::Av1, Quality::Low) => Some(100),
            (Codec::Av1, Quality::Medium) => Some(60),
            (Codec::Av1, Quality::High) => Some(40),
            (_, Quality::Low) => Some(26),
            (_, Quality::Medium) => Some(20),
            (_, Quality::High) => Some(16),
        }
    }
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

    /// Preset VBR budget (kbps) for `height` at the configured maximum frame rate.
    /// Flat content uses less; this remains the Settings buffer-memory estimate's
    /// input. Custom is the user's unscaled bitrate, independent of codec and fps.
    /// VA drivers can overshoot this nominal budget; it is not a memory limit.
    pub fn video_bitrate_kbps(&self, height: u32) -> u32 {
        // The table is a 60fps budget. Lower rates keep the same bits per frame; higher rates
        // stay at the 60fps cap so high-fps clips are never bigger than before. VBR with a
        // quantizer floor already spends far less than the cap on most content.
        let base = match height {
            0..=720 => 6_000u32,
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
        let scaled = match self.video.codec {
            crate::config::Codec::H264 => scaled,
            _ => scaled * 7 / 10,
        };
        (u64::from(scaled) * u64::from(self.video.fps.clamp(1, 60)) / 60).clamp(500, 200_000)
            as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_budgets_scale_down_with_fps_but_cap_at_60_and_custom_does_not() {
        let mut config = Config::default();
        for (fps, budget) in [(30, 5_000), (60, 10_000), (120, 10_000), (144, 10_000)] {
            config.video.fps = fps;
            assert_eq!(config.video_bitrate_kbps(1080), budget);
        }
        config.video.codec = Codec::Av1;
        assert_eq!(config.video_bitrate_kbps(1080), 7_000);
        config.video.quality = Quality::Custom;
        config.video.bitrate_kbps = 12_345;
        assert_eq!(config.video_bitrate_kbps(1080), 12_345);
        assert_eq!(config.video.quality_quantizer(), None);
    }

    #[test]
    fn old_partial_configs_keep_defaults_and_quality_order() -> anyhow::Result<()> {
        let mut config: Config = toml::from_str("[video]\nfps = 120\n")?;
        assert_eq!(config.video.codec, Codec::H264);
        assert_eq!(config.general.buffer_secs(), 63);
        let mut previous = (u32::MAX, 0);
        for quality in [Quality::Low, Quality::Medium, Quality::High] {
            config.video.quality = quality;
            let qp = config.video.quality_quantizer().unwrap();
            let budget = config.video_bitrate_kbps(1080);
            assert!(qp < previous.0 && budget > previous.1);
            previous = (qp, budget);
        }
        Ok(())
    }

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
