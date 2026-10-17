//! Clip trimming and cutting.
//!
//! OWNER: backend (library agent). Signatures are the contract the GUI codes against.

use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

use crate::clip::ClipMeta;
use crate::library::Library;

/// A time range of the source clip to keep, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start_secs: f64,
    pub end_secs: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportRequest {
    pub source_id: String,
    /// Ranges to keep, in output order. Gaps between them are cut out.
    pub keep: Vec<Segment>,
    pub title: String,
    /// `true` re-encodes for frame-accurate cuts. `false` copies streams and snaps cut
    /// points to the nearest keyframe, which is near-instant.
    pub precise: bool,
    /// Replace the source clip instead of writing a new one.
    pub replace_source: bool,
}

/// Write the edited clip into the library and return its metadata.
/// `progress` receives values in `0.0..=1.0`. Setting `cancel` aborts and removes partial output.
pub fn export(
    lib: &Library,
    req: &ExportRequest,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> anyhow::Result<ClipMeta> {
    let _ = (lib, req, progress, cancel);
    todo!()
}
