//! Well-known file locations. Every process resolves paths through here.

use std::path::PathBuf;

const APP: &str = "clippr";

/// `$XDG_CONFIG_HOME/clippr`
pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| home().join(".config")).join(APP)
}

/// `$XDG_CONFIG_HOME/clippr/config.toml`
pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// `$XDG_STATE_HOME/clippr`. Holds the portal restore token and logs.
pub fn state_dir() -> PathBuf {
    dirs::state_dir().unwrap_or_else(|| home().join(".local/state")).join(APP)
}

/// `$XDG_CACHE_HOME/clippr/thumbs`. Thumbnails are disposable and regenerated on demand.
pub fn thumbs_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(|| home().join(".cache")).join(APP).join("thumbs")
}

/// `$XDG_RUNTIME_DIR/clippr.sock`, falling back to the state dir when there is no runtime dir.
pub fn socket_path() -> PathBuf {
    dirs::runtime_dir().unwrap_or_else(state_dir).join("clippr.sock")
}

/// Default clips directory: `$XDG_VIDEOS_DIR/Clippr`.
pub fn default_clips_dir() -> PathBuf {
    dirs::video_dir().unwrap_or_else(|| home().join("Videos")).join("Clippr")
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}
