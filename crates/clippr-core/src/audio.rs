//! Shared source-track layout and save/export mixing rules.
use anyhow::{ensure, Result};

use crate::{
    clip::{AudioTrack, AudioTrackKind},
    config::{Audio, AudioLevels},
    edit::TrackGain,
};

impl Audio {
    /// Encoded streams retained while recording. Mix is created only when saving.
    pub fn source_tracks(&self) -> Vec<AudioTrackKind> {
        if !self.separate_tracks {
            return if self.capture_desktop || self.capture_mic {
                vec![AudioTrackKind::Mix]
            } else {
                vec![]
            };
        }
        let mut tracks = Vec::new();
        if self.capture_desktop {
            tracks.push(AudioTrackKind::Desktop);
        }
        if self.capture_mic {
            tracks.push(AudioTrackKind::Mic);
        }
        if self.capture_desktop {
            tracks.push(AudioTrackKind::Voice);
        }
        tracks
    }

    /// Match complete identities or application-name suffixes such as Discord Canary,
    /// Flatpak app ids and Electron's `WEBRTC VoiceEngine` node names. Never match a
    /// generic Electron/WebRTC identity: unrelated apps use those too.
    pub fn is_voice_app<'a>(&self, identities: impl IntoIterator<Item = &'a str>) -> bool {
        identities.into_iter().any(|identity| {
            let identity = identity.to_lowercase();
            self.voice_apps.iter().any(|app| {
                let app = app.trim().to_lowercase();
                !app.is_empty()
                    && identity.match_indices(&app).any(|(start, matched)| {
                        let boundary = |c: char| !c.is_alphanumeric();
                        (start == 0 || identity[..start].chars().next_back().is_some_and(boundary))
                            && (start + matched.len() == identity.len()
                                || identity[start + matched.len()..]
                                    .chars()
                                    .next()
                                    .is_some_and(boundary))
                    })
            })
        })
    }
}

impl AudioLevels {
    /// Percent for a source track; Mix is never scaled by default.
    pub fn percent(&self, kind: AudioTrackKind) -> u32 {
        match kind {
            AudioTrackKind::Mix => 100,
            AudioTrackKind::Desktop => self.game,
            AudioTrackKind::Mic => self.mic,
            AudioTrackKind::Voice => self.voice,
        }
    }

    /// Linear gains for [`mix_filter`].
    pub fn gains(&self) -> Vec<TrackGain> {
        [AudioTrackKind::Desktop, AudioTrackKind::Mic, AudioTrackKind::Voice]
            .into_iter()
            .map(|kind| TrackGain {
                kind,
                gain: self.percent(kind) as f32 / 100.0,
            })
            .collect()
    }
}

pub fn validate_gains(gains: &[TrackGain]) -> Result<()> {
    for (i, gain) in gains.iter().enumerate() {
        ensure!(
            gain.gain.is_finite() && gain.gain >= 0.0,
            "audio gain must be finite and nonnegative"
        );
        ensure!(
            !gains[..i].iter().any(|g| g.kind == gain.kind),
            "duplicate audio track gain"
        );
    }
    Ok(())
}

/// FFmpeg filter with output `[mix]`. Preserve the original timestamp offsets;
/// first_pts fills any initial AAC alignment gap instead of shifting a source.
pub fn mix_filter(tracks: &[AudioTrack], gains: &[TrackGain]) -> Result<String> {
    validate_gains(gains)?;
    ensure!(!tracks.is_empty(), "no audio tracks to mix");
    let gain = |kind| {
        gains
            .iter()
            .find(|g| g.kind == kind)
            .map_or(1.0, |g| g.gain)
    };
    let sources: Vec<_> = if tracks.len() > 1 && tracks[0].kind == AudioTrackKind::Mix {
        tracks[1..].iter().collect()
    } else {
        tracks.iter().collect()
    };
    let mut filter = String::new();
    for (i, track) in sources.iter().enumerate() {
        let amplitude = if tracks.len() == 1 && track.kind == AudioTrackKind::Mix {
            // Legacy clips have one combined track, also addressable as Desktop.
            gains
                .iter()
                .find(|g| g.kind == AudioTrackKind::Mix)
                .or_else(|| gains.iter().find(|g| g.kind == AudioTrackKind::Desktop))
                .map_or(1.0, |g| g.gain)
        } else {
            gain(track.kind) * gain(AudioTrackKind::Mix)
        };
        filter.push_str(&format!("[0:a:{}]aresample=48000:async=1:first_pts=0,aformat=channel_layouts=stereo,volume={amplitude}[m{i}];", track.index));
    }
    for i in 0..sources.len() {
        filter.push_str(&format!("[m{i}]"));
    }
    filter.push_str(&format!(
        "amix=inputs={}:normalize=0:duration=longest:dropout_transition=0[mix]",
        sources.len()
    ));
    Ok(filter)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_and_layout() {
        let mut audio = Audio::default();
        for identity in [
            "Discord",
            "Discord Canary",
            "DiscordCanary",
            "com.discordapp.DiscordPTB",
            "/opt/vesktop/vesktop",
            "dev.vencord.Vesktop",
            "WEBRTC VoiceEngine Discord",
        ] {
            assert!(audio.is_voice_app([identity]), "{identity}");
        }
        for identity in [
            "",
            "electron",
            "WEBRTC VoiceEngine",
            "discordant-game",
            "mydiscord",
        ] {
            assert!(!audio.is_voice_app([identity]), "{identity}");
        }
        assert_eq!(
            audio.source_tracks(),
            [AudioTrackKind::Desktop, AudioTrackKind::Voice]
        );
        audio.capture_mic = true;
        assert_eq!(
            audio.source_tracks(),
            [
                AudioTrackKind::Desktop,
                AudioTrackKind::Mic,
                AudioTrackKind::Voice
            ]
        );
        audio.capture_desktop = false;
        assert_eq!(audio.source_tracks(), [AudioTrackKind::Mic]);
        audio.separate_tracks = false;
        assert_eq!(audio.source_tracks(), [AudioTrackKind::Mix]);
        audio.capture_mic = false;
        assert!(audio.source_tracks().is_empty());
    }

    #[test]
    fn levels_shape_the_mix_without_restarting_capture() -> Result<()> {
        let levels = AudioLevels { game: 60, mic: 150, voice: 100 };
        let tracks = [
            AudioTrack::new(0, AudioTrackKind::Desktop),
            AudioTrack::new(1, AudioTrackKind::Mic),
        ];
        let filter = mix_filter(&tracks, &levels.gains())?;
        assert!(filter.contains("[0:a:0]") && filter.contains("volume=0.6["), "{filter}");
        assert!(filter.contains("volume=1.5["), "{filter}");
        // A lone source track takes its own level, not the legacy Mix/Desktop alias.
        let filter = mix_filter(&tracks[1..], &levels.gains())?;
        assert!(filter.contains("volume=1.5["), "{filter}");

        let old = crate::config::Config::default();
        let mut new = old.clone();
        new.audio.levels = levels;
        assert!(!old.needs_capture_restart(&new));
        new.audio.capture_mic = !new.audio.capture_mic;
        assert!(old.needs_capture_restart(&new));
        Ok(())
    }

}
