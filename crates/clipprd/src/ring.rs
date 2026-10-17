//! Encoded packet history. Never maps or copies a raw video frame.
use std::collections::VecDeque;

use anyhow::{bail, Result};
use gstreamer as gst;

#[derive(Clone)]
pub struct Packet {
    pub buffer: gst::Buffer,
    pub pts: u64,
    pub duration: u64,
    pub keyframe: bool,
}
impl Packet {
    pub fn end(&self) -> u64 {
        self.pts.saturating_add(self.duration)
    }
}

#[derive(Default)]
pub struct Ring {
    pub video: VecDeque<Packet>,
    pub audio: VecDeque<Packet>,
    pub video_caps: Option<gst::Caps>,
    pub audio_caps: Option<gst::Caps>,
    pub bytes: u64,
    pub limit_ns: u64,
}

pub struct Snapshot {
    pub video: Vec<Packet>,
    pub audio: Vec<Packet>,
    pub video_caps: gst::Caps,
    pub audio_caps: Option<gst::Caps>,
    pub start: u64,
    pub end: u64,
}

impl Ring {
    pub fn new(seconds: u32) -> Self {
        Self {
            limit_ns: u64::from(seconds) * 1_000_000_000,
            ..Self::default()
        }
    }

    pub fn push(&mut self, packet: Packet, caps: &gst::CapsRef, video: bool) {
        // A source restart/discontinuity must never leave old codec headers or timestamps.
        let queue = if video { &self.video } else { &self.audio };
        if queue.back().is_some_and(|last| packet.pts < last.pts) {
            self.clear();
        }
        if video {
            if self.video.is_empty() && !packet.keyframe {
                return;
            }
            self.video_caps = Some(caps.to_owned());
        } else {
            self.audio_caps = Some(caps.to_owned());
        }
        self.bytes += packet.buffer.size() as u64;
        if video {
            self.video.push_back(packet);
        } else {
            self.audio.push_back(packet);
        }
        self.trim();
    }

    pub fn clear(&mut self) {
        self.video.clear();
        self.audio.clear();
        self.video_caps = None;
        self.audio_caps = None;
        self.bytes = 0;
    }

    pub fn trim(&mut self) {
        let newest = self
            .video
            .back()
            .map(Packet::end)
            .unwrap_or(0)
            .max(self.audio.back().map(Packet::end).unwrap_or(0));
        let cutoff = newest.saturating_sub(self.limit_ns);
        // Drop complete GOPs only, but enforce the time bound. Until the next keyframe
        // arrives the history can briefly be empty instead of growing without bound.
        while self
            .video
            .front()
            .is_some_and(|p| p.pts < cutoff || !p.keyframe)
        {
            if let Some(packet) = self.video.pop_front() {
                self.bytes -= packet.buffer.size() as u64;
            }
        }
        let audio_cutoff = self.video.front().map(|p| p.pts).unwrap_or(cutoff);
        while self.audio.front().is_some_and(|p| p.pts < audio_cutoff) {
            if let Some(packet) = self.audio.pop_front() {
                self.bytes -= packet.buffer.size() as u64;
            }
        }
    }

    pub fn duration(&self) -> f32 {
        match (self.video.front(), self.video.back()) {
            (Some(first), Some(last)) => (last.end().saturating_sub(first.pts)) as f32 / 1e9,
            _ => 0.0,
        }
    }

    pub fn snapshot(&self, seconds: u32) -> Result<Snapshot> {
        let Some(last) = self.video.back() else {
            bail!("the replay buffer has no video yet")
        };
        let end = last.end();
        let cutoff = end.saturating_sub(u64::from(seconds) * 1_000_000_000);
        let Some(index) = self
            .video
            .iter()
            .rposition(|p| p.keyframe && p.pts <= cutoff)
            .or_else(|| self.video.iter().position(|p| p.keyframe))
        else {
            bail!("waiting for the next video keyframe");
        };
        let start = self.video[index].pts;
        Ok(Snapshot {
            video: self.video.iter().skip(index).cloned().collect(),
            audio: self
                .audio
                .iter()
                .filter(|p| p.pts >= start && p.end() <= end)
                .cloned()
                .collect(),
            video_caps: self
                .video_caps
                .clone()
                .ok_or_else(|| anyhow::anyhow!("missing video caps"))?,
            audio_caps: self.audio_caps.clone(),
            start,
            end,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(frame: u64, keyframe: bool) -> Packet {
        Packet {
            buffer: gst::Buffer::with_size(10).unwrap(),
            pts: frame * 500_000_000,
            duration: 500_000_000,
            keyframe,
        }
    }
    #[test]
    fn trims_to_keyframes_and_accounts_bytes() {
        gst::init().unwrap();
        let caps = gst::Caps::builder("video/x-h264").build();
        let mut ring = Ring::new(3);
        for frame in 0..20 {
            ring.push(packet(frame, frame % 2 == 0), &caps, true);
        }
        assert!(ring.video.front().unwrap().keyframe);
        assert!(ring.duration() <= 3.0);
        assert_eq!(ring.bytes, ring.video.len() as u64 * 10);
        let snap = ring.snapshot(2).unwrap();
        assert_eq!(snap.start, 8_000_000_000);
        assert_eq!(snap.end, 10_000_000_000);
        assert!(snap.video[0].keyframe);
    }
    #[test]
    fn ignores_leading_delta_frames_and_bounds_audio_without_video() {
        gst::init().unwrap();
        let caps = gst::Caps::new_empty_simple("video/x-h264");
        let mut ring = Ring::new(2);
        ring.push(packet(0, false), &caps, true);
        assert!(ring.video.is_empty());
        for frame in 0..20 {
            ring.push(packet(frame, true), &caps, false);
        }
        assert!(ring.audio.len() <= 4);
        assert!(ring.snapshot(2).is_err());
    }
    #[test]
    fn snapshot_aligns_audio_and_retains_independent_buffers() {
        gst::init().unwrap();
        let caps = gst::Caps::new_empty_simple("video/x-h264");
        let mut ring = Ring::new(10);
        for frame in 0..10 {
            ring.push(packet(frame, frame % 2 == 0), &caps, true);
            ring.push(packet(frame, true), &caps, false);
        }
        let snap = ring.snapshot(3).unwrap();
        assert_eq!(snap.start, 2_000_000_000);
        assert!(snap
            .audio
            .iter()
            .all(|p| p.pts >= snap.start && p.end() <= snap.end));
        ring.clear();
        assert_eq!(snap.video.len(), 6);
        assert_eq!(ring.bytes, 0);
    }

    #[test]
    fn snapshot_includes_requested_duration_or_all_available_history() {
        gst::init().unwrap();
        let caps = gst::Caps::new_empty_simple("video/x-h264");
        let mut ring = Ring::new(12);
        for frame in 0..21 {
            ring.push(packet(frame, frame % 2 == 0), &caps, true);
        }
        // The requested start (8.5 s) falls between keyframes at 8 and 9 s.
        let snap = ring.snapshot(2).unwrap();
        assert_eq!(snap.start, 8_000_000_000);
        assert_eq!(snap.end, 10_500_000_000);
        assert!(snap.end - snap.start >= 2_000_000_000);
        assert!(snap.video[0].keyframe);

        // An exact keyframe boundary must not include the preceding GOP.
        ring.push(packet(21, false), &caps, true);
        assert_eq!(ring.snapshot(2).unwrap().start, 9_000_000_000);

        // After trimming, an oversized request still saves the oldest available GOP.
        ring.limit_ns = 4_000_000_000;
        ring.trim();
        assert_eq!(ring.snapshot(20).unwrap().start, ring.video[0].pts);
    }
}
