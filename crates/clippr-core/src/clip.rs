//! Clip metadata. Each `<id>.mp4` in the clips dir has a `<id>.json` sidecar holding a
//! [`ClipMeta`]. The sidecar is the source of truth for titles, tags and app info.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipMeta {
    /// File stem shared by the video and its sidecar, e.g. `2026-10-16_22-41-07_cs2`.
    /// Only `[A-Za-z0-9._-]` characters; never contains a path separator.
    pub id: String,
    /// User-facing title. Defaults to `"<app name> <local time>"`.
    pub title: String,
    /// Video file name inside the clips dir, e.g. `<id>.mp4`.
    pub file: String,
    pub created_at: DateTime<Local>,
    pub duration_secs: f64,
    /// Focused window when the clip was saved, if the compositor exposes it.
    #[serde(default)]
    pub app: Option<AppInfo>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `"h264"`, `"hevc"` or `"av1"`.
    pub codec: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Id of the clip this one was cut from.
    #[serde(default)]
    pub edited_from: Option<String>,
    /// Empty on legacy sidecars: interpret as a single Mix track when audio exists.
    #[serde(default)]
    pub audio_tracks: Vec<AudioTrack>,
    /// Source gains used to build this clip's Mix. Empty legacy metadata means unity.
    #[serde(default)]
    pub mix_gains: Vec<crate::edit::TrackGain>,
}

impl ClipMeta {
    pub fn mix_gain(&self, kind: AudioTrackKind) -> f32 {
        self.mix_gains.iter().find(|g| g.kind == kind).map_or(1.0, |g| g.gain)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    /// Human-readable name, e.g. `"Counter-Strike 2"`.
    pub name: String,
    /// Wayland app_id or X11 WM_CLASS, e.g. `"steam_app_730"`.
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub window_title: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
}

/// Returns true if `id` is safe to join onto the clips dir.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Zero-based index among audio streams, independent of the video's stream index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTrack {
    pub index: u32,
    pub kind: AudioTrackKind,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioTrackKind {
    Mix,
    Desktop,
    Mic,
    Voice,
}

impl AudioTrackKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mix => "Mix",
            Self::Desktop => "Game",
            Self::Mic => "Microphone",
            Self::Voice => "Voice chat",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        match label.to_lowercase().as_str() {
            "mix" => Some(Self::Mix),
            "game" | "game/pc" | "desktop" => Some(Self::Desktop),
            "microphone" | "mic" => Some(Self::Mic),
            "voice chat" | "voice" | "discord" => Some(Self::Voice),
            _ => None,
        }
    }
}

impl AudioTrack {
    pub fn new(index: u32, kind: AudioTrackKind) -> Self {
        Self {
            index,
            kind,
            label: kind.label().into(),
        }
    }
}
