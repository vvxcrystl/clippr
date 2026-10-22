//! Encoded packet history. Never maps or copies a raw video frame.
use std::collections::VecDeque;

use anyhow::{bail, Context, Result};
use clippr_core::clip::AudioTrackKind;
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
    pub audio: Vec<AudioRing>,
    pub video_caps: Option<gst::Caps>,
    pub bytes: u64,
    pub limit_ns: u64,
}

pub struct AudioRing {
    pub kind: AudioTrackKind,
    pub packets: VecDeque<Packet>,
    pub caps: Option<gst::Caps>,
}

pub struct AudioSnapshot {
    pub kind: AudioTrackKind,
    pub packets: Vec<Packet>,
    pub caps: gst::Caps,
}

pub struct Snapshot {
    pub video: Vec<Packet>,
    pub audio: Vec<AudioSnapshot>,
    pub video_caps: gst::Caps,
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

    pub fn configure_audio(&mut self, kinds: &[AudioTrackKind]) {
        self.audio = kinds
            .iter()
            .map(|&kind| AudioRing {
                kind,
                packets: VecDeque::new(),
                caps: None,
            })
            .collect();
    }

    #[cfg(test)]
    pub fn push(&mut self, packet: Packet, caps: &gst::CapsRef, video: bool) {
        if !video && self.audio.is_empty() {
            self.configure_audio(&[AudioTrackKind::Mix]);
        }
        self.push_stream(packet, caps, if video { None } else { Some(0) });
    }

    pub fn push_stream(&mut self, packet: Packet, caps: &gst::CapsRef, audio: Option<usize>) {
        let video = audio.is_none();
        // A source restart/discontinuity must never leave old codec headers or timestamps.
        let queue = audio.map_or(&self.video, |i| &self.audio[i].packets);
        if queue.back().is_some_and(|last| packet.pts < last.pts) {
            self.clear();
        }
        if video {
            if self.video.is_empty() && !packet.keyframe {
                return;
            }
            self.video_caps = Some(caps.to_owned());
        } else {
            self.audio[audio.unwrap_or(0)].caps = Some(caps.to_owned());
        }
        self.bytes += packet.buffer.size() as u64;
        if video {
            self.video.push_back(packet);
        } else {
            self.audio[audio.unwrap_or(0)].packets.push_back(packet);
        }
        self.trim();
    }

    pub fn clear(&mut self) {
        self.video.clear();
        for audio in &mut self.audio {
            audio.packets.clear();
            audio.caps = None;
        }
        self.video_caps = None;
        self.bytes = 0;
    }

    pub fn trim(&mut self) {
        let newest = self.video.back().map(Packet::end).unwrap_or(0).max(
            self.audio
                .iter()
                .filter_map(|a| a.packets.back().map(Packet::end))
                .max()
                .unwrap_or(0),
        );
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
        for audio in &mut self.audio {
            while audio.packets.front().is_some_and(|p| p.pts < audio_cutoff) {
                if let Some(packet) = audio.packets.pop_front() {
                    self.bytes -= packet.buffer.size() as u64;
                }
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
                .map(|audio| {
                    let packets: Vec<_> = audio
                        .packets
                        .iter()
                        .filter(|p| p.pts >= start && p.end() <= end)
                        .cloned()
                        .collect();
                    anyhow::ensure!(!packets.is_empty(), "waiting for audio history");
                    Ok(AudioSnapshot {
                        kind: audio.kind,
                        packets,
                        caps: audio.caps.clone().context("missing audio caps")?,
                    })
                })
                .collect::<Result<_>>()?,
            video_caps: self
                .video_caps
                .clone()
                .ok_or_else(|| anyhow::anyhow!("missing video caps"))?,
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
        assert!(ring.audio[0].packets.len() <= 4);
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
        assert!(snap.audio[0]
            .packets
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

    #[test]
    fn two_second_gops_keep_longest_clip_available_through_trimming() {
        gst::init().unwrap();
        let caps = gst::Caps::new_empty_simple("video/x-h264");
        let mut config = clippr_core::config::Config::default();
        config.hotkeys.clips[0].seconds = 5;
        let mut ring = Ring::new(config.buffer_secs());
        for frame in 0..120 {
            ring.push(packet(frame, frame % 4 == 0), &caps, true);
            ring.push(packet(frame, true), &caps, false);
            if frame >= 16 {
                let snap = ring.snapshot(5).unwrap();
                assert!(snap.video[0].keyframe);
                assert!(snap.end - snap.start >= 5_000_000_000);
                assert!(ring.duration() <= config.buffer_secs() as f32);
                assert_eq!(
                    ring.bytes,
                    (ring.video.len() + ring.audio.iter().map(|a| a.packets.len()).sum::<usize>())
                        as u64
                        * 10
                );
            }
        }
    }
    #[test]
    fn source_rings_trim_and_reset_together() {
        gst::init().unwrap();
        let caps = gst::Caps::new_empty_simple("audio/mpeg");
        let mut ring = Ring::new(4);
        ring.configure_audio(&[
            AudioTrackKind::Desktop,
            AudioTrackKind::Mic,
            AudioTrackKind::Voice,
        ]);
        for frame in 0..30 {
            ring.push_stream(packet(frame, frame % 4 == 0), &caps, None);
            for i in 0..3 {
                ring.push_stream(packet(frame, true), &caps, Some(i));
            }
        }
        let snap = ring.snapshot(2).unwrap();
        assert_eq!(snap.audio.len(), 3);
        for track in &snap.audio {
            assert!(track
                .packets
                .iter()
                .all(|p| p.pts >= snap.start && p.end() <= snap.end));
            assert_eq!(track.packets.len(), snap.audio[0].packets.len());
        }
        assert_eq!(
            ring.bytes,
            (ring.video.len() + ring.audio.iter().map(|a| a.packets.len()).sum::<usize>()) as u64
                * 10
        );
        ring.push_stream(packet(0, true), &caps, Some(1));
        assert!(ring.video.is_empty());
        assert!(ring.audio[0].packets.is_empty());
        assert_eq!(ring.bytes, 10);
        assert!(!snap.video.is_empty());
    }
}
