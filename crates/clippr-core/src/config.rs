//! User configuration, stored as TOML at [`crate::paths::config_file`].
//!
//! Omitted settings use defaults, so a partial or missing file loads cleanly.

use std::{io::Write, path::PathBuf};

use serde::{Deserialize, Serialize};

/// Maximum GOP duration, also enforced by timestamps for variable-rate capture.
pub const KEYFRAME_INTERVAL_SECS: u32 = 2;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredConfig")]
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
    /// Show a desktop notification after a clip is saved.
    pub notify_on_save: bool,
    /// Start capturing as soon as the daemon starts. When false the daemon idles until resumed.
    pub start_recording: bool,
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
    /// Save a compatibility mix followed by independently editable source tracks.
    pub separate_tracks: bool,
    /// Case-insensitive application identities excluded from Game/PC audio.
    pub voice_apps: Vec<String>,
    /// PipeWire node name for the microphone. `None` means the default source.
    pub mic_device: Option<String>,
    pub bitrate_kbps: u32,
    pub levels: AudioLevels,
}

/// Default volume of each source in the saved mix, in percent. Only clips with separate
/// tracks have sources to balance. Each saved clip keeps the gains used for its mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioLevels {
    pub game: u32,
    pub mic: u32,
    pub voice: u32,
}

impl Default for AudioLevels {
    fn default() -> Self {
        Self {
            game: 100,
            mic: 100,
            voice: 100,
        }
    }
}

/// Preferred key combinations, in xdg-desktop-portal GlobalShortcuts trigger syntax
/// (e.g. `"F8"`, `"SHIFT+F8"`, `"CTRL+ALT+C"`). The compositor may let the user rebind them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub clips: Vec<ClipBinding>,
    pub toggle_overlay: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipBinding {
    pub trigger: String,
    pub seconds: u32,
}

impl ClipBinding {
    /// Use the trigger as identity so removing a row cannot move desktop overrides
    /// or an already queued activation onto another binding.
    pub fn shortcut_id(&self) -> String {
        format!("save-{}", self.trigger)
    }
}

impl Hotkeys {
    pub fn clip_seconds(&self, shortcut_id: &str) -> Option<u32> {
        self.clips
            .iter()
            .find(|binding| binding.shortcut_id() == shortcut_id)
            .map(|binding| binding.seconds)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut triggers = std::collections::HashSet::new();
        triggers.insert(normalize_trigger(&self.toggle_overlay)?.to_ascii_lowercase());
        for binding in &self.clips {
            anyhow::ensure!(
                (1..=3600).contains(&binding.seconds),
                "clip lengths must be between 1 and 3600 seconds"
            );
            let trigger = normalize_trigger(&binding.trigger)?;
            anyhow::ensure!(
                triggers.insert(trigger.to_ascii_lowercase()),
                "shortcut {trigger} is already used"
            );
        }
        Ok(())
    }
}

/// XDG shortcut syntax. Preserve case-sensitive XKB key names such as `Return`.
pub fn normalize_trigger(trigger: &str) -> anyhow::Result<String> {
    const MODIFIERS: [&str; 5] = ["CTRL", "ALT", "SHIFT", "NUM", "LOGO"];
    let mut parts: Vec<_> = trigger.trim().split('+').collect();
    let key = parts.pop().unwrap_or_default();
    anyhow::ensure!(
        !key.is_empty() && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "enter a shortcut such as F8 or CTRL+SHIFT+F8"
    );
    let mut modifiers = Vec::new();
    for part in parts {
        let modifier = part.to_ascii_uppercase();
        let index = MODIFIERS.iter().position(|m| *m == modifier);
        let index = index.ok_or_else(|| anyhow::anyhow!("unknown shortcut modifier: {part}"))?;
        anyhow::ensure!(!modifiers.contains(&index), "duplicate modifier: {part}");
        modifiers.push(index);
    }
    modifiers.sort_unstable();
    let mut normalized: Vec<_> = modifiers
        .into_iter()
        .map(|i| MODIFIERS[i].to_owned())
        .collect();
    let key = if key.starts_with(['f', 'F']) && key[1..].parse::<u8>().is_ok() {
        key.to_ascii_uppercase()
    } else {
        key.to_owned()
    };
    normalized.push(key);
    Ok(normalized.join("+"))
}

// Old files and IPC configs used two fixed hotkeys, with durations in [general].
// Read those at the boundary; only the new binding list is written back.
#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredConfig {
    general: StoredGeneral,
    video: Video,
    audio: Audio,
    hotkeys: StoredHotkeys,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredGeneral {
    #[serde(flatten)]
    general: General,
    clip_short_secs: Option<u32>,
    clip_long_secs: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredHotkeys {
    clips: Option<Vec<ClipBinding>>,
    clip_short: Option<String>,
    clip_long: Option<String>,
    toggle_overlay: Option<String>,
}

impl From<StoredConfig> for Config {
    fn from(stored: StoredConfig) -> Self {
        let mut hotkeys = Hotkeys::default();
        if let Some(clips) = stored.hotkeys.clips {
            hotkeys.clips = clips;
        } else if stored.hotkeys.clip_short.is_some()
            || stored.hotkeys.clip_long.is_some()
            || stored.general.clip_short_secs.is_some()
            || stored.general.clip_long_secs.is_some()
        {
            hotkeys.clips = vec![
                ClipBinding {
                    trigger: stored.hotkeys.clip_short.unwrap_or_else(|| "F8".into()),
                    seconds: stored.general.clip_short_secs.unwrap_or(30),
                },
                ClipBinding {
                    trigger: stored
                        .hotkeys
                        .clip_long
                        .unwrap_or_else(|| "SHIFT+F8".into()),
                    seconds: stored.general.clip_long_secs.unwrap_or(60),
                },
            ];
        }
        if let Some(trigger) = stored.hotkeys.toggle_overlay {
            hotkeys.toggle_overlay = trigger;
        }
        Self {
            general: stored.general.general,
            video: stored.video,
            audio: stored.audio,
            hotkeys,
        }
    }
}

impl Default for General {
    fn default() -> Self {
        Self {
            clips_dir: crate::paths::default_clips_dir(),
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
            separate_tracks: true,
            voice_apps: [
                "discord",
                "vesktop",
                "webcord",
                "armcord",
                "legcord",
                "equibop",
                "discordcanary",
                "discordptb",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            mic_device: None,
            bitrate_kbps: 160,
            levels: AudioLevels::default(),
        }
    }
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            clips: vec![ClipBinding {
                trigger: "F8".into(),
                seconds: 30,
            }],
            toggle_overlay: "ALT+F8".into(),
        }
    }
}

impl Config {
    /// CLI saves and the primary clip button use the first binding, or 30 seconds
    /// when all save hotkeys have been removed.
    pub fn default_clip_secs(&self) -> u32 {
        self.hotkeys
            .clips
            .first()
            .map_or(30, |binding| binding.seconds)
    }

    pub fn longest_clip_secs(&self) -> u32 {
        self.hotkeys
            .clips
            .iter()
            .map(|binding| binding.seconds)
            .max()
            .unwrap_or(30)
    }

    /// Keep a whole GOP before the requested start, plus one second for stream skew.
    pub fn buffer_secs(&self) -> u32 {
        self.longest_clip_secs()
            .saturating_add(KEYFRAME_INTERVAL_SECS + 1)
    }

    /// Whether moving from `self` to `new` needs a capture rebuild. Levels only shape
    /// the mix built on save, so changing them keeps the replay buffer.
    pub fn needs_capture_restart(&self, new: &Config) -> bool {
        let audio = Audio {
            levels: self.audio.levels,
            ..new.audio.clone()
        };
        new.video != self.video || audio != self.audio
    }

    /// Load from [`crate::paths::config_file`]. A missing file yields defaults.
    pub fn load() -> anyhow::Result<Self> {
        let path = crate::paths::config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let config: Self = toml::from_str(&text)?;
                config.hotkeys.validate()?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write atomically (temp file + rename) to [`crate::paths::config_file`].
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&crate::paths::config_file())
    }

    fn save_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        self.hotkeys.validate()?;
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
        (u64::from(scaled) * u64::from(self.video.fps.clamp(1, 60)) / 60).clamp(500, 200_000) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_bindings_default_migrate_and_roundtrip() -> anyhow::Result<()> {
        let defaults: Config = toml::from_str("")?;
        assert_eq!(
            defaults.hotkeys.clips,
            [ClipBinding {
                trigger: "F8".into(),
                seconds: 30
            }]
        );
        assert_eq!(defaults.buffer_secs(), 33);
        let old: Config = toml::from_str(
            r#"
            [general]
            clip_short_secs = 45
            clip_long_secs = 300
            notify_on_save = false
            [hotkeys]
            clip_short = "F9"
            clip_long = "F7"
        "#,
        )?;
        assert!(!old.general.notify_on_save);
        assert_eq!(old.hotkeys.clips[0].trigger, "F9");
        assert_eq!(old.default_clip_secs(), 45);
        assert_eq!(old.hotkeys.clips[1].trigger, "F7");
        assert_eq!(old.buffer_secs(), 303);
        let text = toml::to_string_pretty(&old)?;
        assert!(!text.contains("clip_short"));
        assert_eq!(toml::from_str::<Config>(&text)?, old);
        assert_eq!(
            serde_json::from_value::<Config>(serde_json::to_value(&old)?)?,
            old
        );
        let empty: Config = toml::from_str("[hotkeys]\nclips = []\n")?;
        assert!(empty.hotkeys.clips.is_empty());
        assert_eq!(empty.default_clip_secs(), 30);
        assert_eq!(empty.buffer_secs(), 33);
        Ok(())
    }

    #[test]
    fn custom_clip_bindings_validate_and_keep_their_identity() -> anyhow::Result<()> {
        let mut config = Config::default();
        config.hotkeys.clips.extend([
            ClipBinding {
                trigger: "F9".into(),
                seconds: 45,
            },
            ClipBinding {
                trigger: "F7".into(),
                seconds: 300,
            },
        ]);
        config.hotkeys.validate()?;
        assert_eq!(config.buffer_secs(), 303);
        let id = config.hotkeys.clips[2].shortcut_id();
        let removed_id = config.hotkeys.clips[1].shortcut_id();
        assert_eq!(config.hotkeys.clip_seconds(&removed_id), Some(45));
        config.hotkeys.clips.remove(1);
        assert_eq!(config.hotkeys.clips[1].shortcut_id(), id);
        assert_eq!(config.hotkeys.clip_seconds(&id), Some(300));
        assert_eq!(config.hotkeys.clip_seconds(&removed_id), None);
        assert!(!Config::default().needs_capture_restart(&config));
        for seconds in [0, 3601, u32::MAX] {
            config.hotkeys.clips[1].seconds = seconds;
            assert!(config.hotkeys.validate().is_err());
        }
        config.hotkeys.clips[1].seconds = 300;
        config.hotkeys.clips[1].trigger = "alt+f8".into();
        assert!(config.hotkeys.validate().is_err());
        config.hotkeys.clips[1].trigger = "shift+ctrl+F9".into();
        config.hotkeys.clips[0].trigger = "CTRL+SHIFT+f9".into();
        assert!(config.hotkeys.validate().is_err());
        assert_eq!(normalize_trigger("shift+ctrl+Return")?, "CTRL+SHIFT+Return");
        for trigger in ["", "CTRL+", "CTRL+CTRL+F8", "F8+F9", "<Control>F8"] {
            assert!(normalize_trigger(trigger).is_err(), "{trigger}");
        }
        Ok(())
    }

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
        assert_eq!(config.buffer_secs(), 33);
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
                        config.hotkeys.clips[0].trigger = format!("F{i}");
                        config.save_to(path)
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap().unwrap();
            }
        });
        let config: Config = toml::from_str(&std::fs::read_to_string(path)?)?;
        assert!(config.hotkeys.clips[0].trigger.starts_with('F'));
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }
}
