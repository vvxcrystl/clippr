//! Daemon IPC: newline-delimited JSON over the Unix socket at
//! [`crate::paths::socket_path`]. A client writes one [`Request`] per line and reads one
//! [`Response`] per line. After [`Request::Subscribe`] the connection stays open and the
//! daemon pushes [`Response::Event`] lines until the client disconnects.

use serde::{Deserialize, Serialize};

use crate::clip::ClipMeta;
use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Save the last `seconds` of the buffer. `None` uses `clip_short_secs`.
    Save { seconds: Option<u32> },
    GetConfig,
    /// Persist the config and apply it. Changing video settings restarts the capture pipeline,
    /// which empties the buffer.
    SetConfig { config: Config },
    /// Pause or resume capture. Paused means the pipeline is torn down and uses no GPU.
    SetPaused { paused: bool },
    /// Open the quick-settings overlay, or close it if it is open.
    ToggleOverlay,
    /// Forget the saved screen-share permission and ask the portal again.
    ResetCaptureSource,
    Subscribe,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status { status: Status },
    Config { config: Config },
    Saved { clip: ClipMeta },
    Error { message: String },
    Event { event: Event },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    ClipSaved { clip: ClipMeta },
    StatusChanged { status: Status },
    ConfigChanged { config: Config },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub state: CaptureState,
    /// Seconds of video currently held in the buffer.
    pub buffered_secs: f32,
    /// Bytes of encoded audio and video held in the buffer.
    pub buffer_bytes: u64,
    /// Encoder element in use, e.g. `"vah264enc"` or `"x264enc"`.
    pub encoder: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Most recent error, cleared when capture recovers.
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    /// Waiting for the user to pick a screen in the portal dialog.
    Starting,
    Recording,
    Paused,
    Error,
}
