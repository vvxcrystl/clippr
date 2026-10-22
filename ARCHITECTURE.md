# clippr architecture

clippr is a replay-buffer clipper for Linux, in the spirit of Medal or ShadowPlay. The design
goal is that the always-running part costs nothing you can notice while gaming or compiling.

## Processes

| Binary | Crate | Runs | Job |
|---|---|---|---|
| `clipprd` | `crates/clipprd` | always (systemd user service) | capture, encode, ring buffer, hotkeys, saving clips |
| `clippr` | `crates/clippr-cli` | on demand | CLI client: `clippr save 30`, `clippr status`, ... |
| `clippr-gui` | `crates/clippr-gui` | on demand | library, settings, editor (`clippr-gui`) and the quick overlay (`clippr-gui --overlay`) |

The GUI is never resident. The daemon spawns `clippr-gui --overlay` when the overlay hotkey is
pressed and kills it on the next press, so the overlay costs zero RAM while hidden.

`crates/clippr-core` holds the shared contract: config (`config.rs`), clip sidecar format
(`clip.rs`), IPC protocol (`ipc.rs`), paths (`paths.rs`), plus the clip library (`library.rs`),
editor export (`edit.rs`) and IPC client (`client.rs`).

## Resource budget for clipprd

- CPU while recording: under 1% of one core on a modern desktop. The frame path must stay on
  the GPU: PipeWire DMA-BUF in, VAAPI scale/convert, VAAPI encode, encoded bytes out. No raw
  frame may be copied to system memory on the hardware path.
- RAM: the ring buffer of encoded packets plus under 40 MB of process overhead. 33s of
  1440p60 H.264 at the 16 Mbps preset budget is about 66 MB; easy scenes use less.
  VA rate control is a nominal budget, not a strict byte limit (see codec validation).
- GPU: one hardware encode session. No shaders beyond `vapostproc`.
- Idle wakeups: no polling loops. Everything is event driven (GStreamer bus, D-Bus signals,
  socket readiness).
- Paused state tears the pipeline down entirely.

## Capture pipeline (clipprd)

1. **Screen source.** xdg-desktop-portal ScreenCast via `ashpd`: monitor source, cursor mode
   from config (embedded), `PersistMode::ExplicitlyRevoked`. Save the restore token to
   `paths::state_dir()/restore_token` (mode 0600) so the picker only appears on first run.
   `ResetCaptureSource` deletes the token and asks again.
2. **Video.** GStreamer:
   `pipewiresrc fd=<portal fd> path=<node id> do-timestamp=true keepalive-time=<1000/fps>`
   → DMA-BUF caps → `vapostproc` (scale to `video.resolution`, convert to NV12, VA memory)
   → `vah264enc` / `vah265enc` / `vaav1enc` (preset VBR at
   `Config::video_bitrate_kbps(height)`, `target-percentage=100`, two-second CPB,
   `target-usage=1`; preset `min-qp` stops wasting bits on easy content). H.264/HEVC
   Low/Medium/High QP floors are 26/20/16; AV1 quantizer-index floors are 100/60/40.
   Custom keeps the unscaled user bitrate and the encoder's default rate control.
   Drivers without VBR retain their default mode with a warning. GOPs span at most
   two seconds (`KEYFRAME_INTERVAL_SECS`), enforced by both frame count and arriving
   timestamps. No H.26x B-frames; AV1 `hierarchical-level=1` disables future references
   → `h264parse`/`h265parse`/`av1parse` with `config-interval=-1` → `appsink`.
   Fallback when VAAPI fails or `encoder = software`: `videoconvert ! videoscale ! x264enc
   speed-preset=superfast tune=zerolatency` (H.264 only), capped CRF via `pass=qual`
   and the same 26/20/16 quality levels, two-second VBV; Custom retains bitrate mode.
   Preset budgets come from a 60fps table: lower capture rates scale down to keep bits per
   frame, and rates above 60 stay at the 60fps cap so high-fps clips never outgrow the old
   fixed bitrate. Resolution and capture rate are unchanged.
   Framerate cap via caps on the
   PipeWire stream (`max-framerate`), not a `videorate` element.
3. **Audio.** With `audio.separate_tracks=true` (the default), the daemon encodes only
   source tracks: Game/PC (configured bitrate, stereo), optional Microphone (96 kb/s,
   mono), and Voice chat (96 kb/s, stereo). There is **no resident Mix encoder**.
   `fdkaacenc` falls back to `avenc_aac`; each source has its own encoded appsink.
   A native `pipewire` registry listener watches nodes, full node/client identities,
   ports, playback links and default-sink metadata. Every desktop output stream linked
   to the default sink is tapped except configured voice apps. Voice app output is
   tapped independently. Matching is case-insensitive against `application.name`,
   `application.process.binary` and `node.name`, including client-property fallbacks.
   Full identities are read before assigning a stream to Game, preventing WebRTC voice
   streams from briefly leaking there during startup. The default `voice_apps` includes
   discord, vesktop, webcord, armcord, legcord, equibop, discordcanary and discordptb.
   Names match at word/path/app-id boundaries (not arbitrary substrings).

   Each app has a native capture stream with autoconnect disabled, and additional
   non-lingering monitor links from its existing output ports. Capture nodes are
   passive; links request passive mode too, subject to the server's normal policy. No virtual sink is
   created, no playback link is removed, and defaults, routing and volumes are never
   changed. Native capture avoids GStreamer's indefinite negotiation wait for an
   unconnected `pipewiresrc`. PipeWire adapts sample rates to 48 kHz; mono is duplicated
   to stereo. Bounded, nonblocking appsrc queues feed Game and Voice `audiomixer`s on
   the common capture timeline. Permanent silence inputs keep both encoders running
   through idle periods, app exits and restarts. Audio samples are copied on the
   non-real-time registry thread; the GPU video path remains untouched. Mic capture
   uses the default/named `pipewiresrc` source. Pausing destroys taps, links and mixers.
   With separation disabled, the existing sink-monitor/mic mixer and single encoder
   remain in use.
4. **Ring buffer.** A `VecDeque` of encoded samples per stream (`Arc<[u8]>` or `gst::Buffer`,
   PTS, duration, keyframe flag). Encoders output presentation order, so DTS equals PTS;
   enabling reordering would require redesigning discontinuity detection, trimming and muxing.
   Trim from the front by PTS so the buffer holds
   `Config::buffer_secs()` of video (longest clip + GOP duration + one second of
   stream-skew slack). Trimming keeps the oldest retained video sample a
   keyframe. One AAC deque per enabled source is trimmed against that same video
   boundary; snapshots select every track against a common start/end. A timestamp
   discontinuity clears all deques together. Keep codec headers alongside each stream.
5. **Saving.** On save, take a cheap snapshot of the deques under a short lock, then mux on a
   background thread at low priority (`SCHED_IDLE` or nice 19): start at the last video
   keyframe at or before `now - seconds` (or the oldest keyframe if history is shorter),
   align audio by PTS, rebase timestamps to zero,
   write `appsrc → mp4mux → filesink` into a temporary source-track MP4. On this
   low-priority save thread, FFmpeg decodes those buffered AAC tracks, aligns initial
   gaps, scales each by `audio.levels` (percent), sums them and encodes Mix once at
   the configured bitrate. Changing levels does not restart capture. Video and source
   AAC are stream-copied into the final faststart MP4, in audio order **Mix, Game,
   Microphone, Voice chat**, omitting disabled sources. Mix alone is the default
   audio stream, with readable MP4 titles and handler names on every track. The
   temporary source file is removed automatically. Write the result to
   `<clips_dir>/<id>.mp4.part`, then rename.
   Write the sidecar `<id>.json` ([`ClipMeta`]) atomically after the video. Then emit
   `Event::ClipSaved` and, if enabled, a desktop notification (org.freedesktop.Notifications
   over D-Bus). The capture pipeline is never paused or blocked by a save.
6. **Clip id.** `YYYY-MM-DD_HH-MM-SS_<app slug>` in local time, sanitized to
   `clip::is_valid_id`, with `-2`, `-3` suffixes on collision.
7. **App metadata.** At save time, read the focused window:
   - KDE Plasma (Wayland): load a tiny KWin script over D-Bus (`org.kde.KWin /Scripting`) that
     reports `workspace.activeWindow` (resourceClass, caption, pid) back to the daemon's own
     D-Bus object. Load it once at startup, not per save.
   - Hyprland: `$HYPRLAND_INSTANCE_SIGNATURE` socket, `activewindow` JSON.
   - Sway: `swaymsg -t get_tree` focused node, or the i3 IPC socket.
   - X11: `_NET_ACTIVE_WINDOW`, `WM_CLASS`, `_NET_WM_NAME`, `_NET_WM_PID`.
   - Otherwise `None`.
   Resolve a friendly name: `steam_app_<n>` → read the game name from Steam's appmanifest if
   available, otherwise the class; read `/proc/<pid>/comm` as a last resort.
8. **Hotkeys.** xdg-desktop-portal GlobalShortcuts (`ashpd`), one `save-<trigger>` id per
   `config.hotkeys.clips` entry (`trigger`, `seconds`) plus `toggle-overlay`. F8 saves 30 seconds
   by default. Legacy short/long configs migrate on read. The longest binding sizes the ring,
   and changing bindings resizes it without restarting capture. Trigger-based ids keep desktop
   overrides and queued activations from moving to another row when a binding is removed.
   If the portal is unavailable,
   log it and rely on the CLI (`clippr save`, `clippr overlay`) bound in the compositor.
9. **Errors.** Pipeline errors set `CaptureState::Error` + `last_error`, then retry with
   backoff (2s, 5s, 15s, then every 60s). Hardware encoder failure falls back to software
   only when `encoder = auto`.

## IPC

Newline-delimited JSON on `paths::socket_path()` (see `ipc.rs`). The socket is created with
mode 0600; the daemon refuses peers whose `SO_PEERCRED` uid differs from its own. Requests
larger than 1 MiB are rejected. One request per line; `Subscribe` turns the connection into
an event stream.

## Clip library

`<clips_dir>/<id>.mp4` + `<id>.json`. Thumbnails cached at `paths::thumbs_dir()/<id>.jpg`,
generated with `ffmpeg -ss <min(1,dur/2)> -i <clip> -frames:v 1 -vf scale=480:-2` at nice 19.
External tools are always invoked with an argument vector, never through a shell.

## Editor export

`edit::export` uses `ffmpeg`. Fast mode stream-copies each kept segment (cut points snap to
keyframes) and concatenates them with the concat demuxer. Precise mode re-encodes with
the source codec's VAAPI encoder (`-vaapi_device /dev/dri/renderD128`), using the shared
Medium VBR budget, QP floor and two-second GOP, no frame reordering, and passthrough frame
timing. Hardware failure falls back to H.264 `libx264 -preset veryfast -crf 20` with the
shared H.264 bitrate/VBV budget. `trim`/`atrim` + `concat` filters preserve precise cuts.
Progress comes from `-progress pipe:1`.

All audio streams are retained in both modes. `ExportRequest::track_gains` changes
only Mix: when a requested gain differs from the saved gain, rebuild it from the
cut source tracks, leaving source-track volumes unchanged. Clip sidecars retain `mix_gains`, the source gains
used for the saved Mix. The editor reads these rather than current recorder defaults;
legacy clips without the field use unity. Exports retain these gains and rebuild Mix
when a requested gain differs, including a reset to unity. Fast exports still stream-copy video and
source AAC; only Mix is encoded. Precise exports trim/concatenate every audio stream
and re-encode AAC for the precise cut, then rebuild Mix if requested. Legacy single
tracks accept Mix or Desktop gain. Nonfinite/negative and duplicate gains are rejected.
Cancellation applies to cutting and remixing, and temporary files roll back together.

The additive shared contract is:

```rust
Audio { separate_tracks: bool, voice_apps: Vec<String>, levels: AudioLevels, /* existing fields */ }
pub struct AudioLevels { pub game: u32, pub mic: u32, pub voice: u32 } // percent, default 100
ClipMeta { audio_tracks: Vec<AudioTrack>, mix_gains: Vec<TrackGain>, /* existing fields */ }
pub struct AudioTrack { pub index: u32, pub kind: AudioTrackKind, pub label: String }
pub enum AudioTrackKind { Mix, Desktop, Mic, Voice } // snake_case serde
ExportRequest { track_gains: Vec<TrackGain>, /* existing fields */ }
pub struct TrackGain { pub kind: AudioTrackKind, pub gain: f32 }
```

Indices are zero-based **audio** stream indices. Both new vectors have serde defaults;
an empty legacy `audio_tracks` means one Mix track when the file has audio. Library
probing fills tracks from ffprobe titles/handler names for files without sidecars.
`edit::export` retains its existing signature. Shared `Audio::source_tracks() ->
Vec<AudioTrackKind>` and `Audio::is_voice_app<'a>(identities: impl IntoIterator<Item =
&'a str>) -> bool` define layout and matching. `AudioTrack::new(index: u32, kind:
AudioTrackKind) -> AudioTrack`, `AudioTrackKind::label(self) -> &'static str` and
`AudioTrackKind::from_label(label: &str) -> Option<AudioTrackKind>` define naming.

## Config

TOML at `paths::config_file()`, all fields defaulted (see `config.rs`). The GUI writes it via
`Request::SetConfig` when the daemon runs, or directly with `Config::save` when it does not.
The daemon applies changes live; video changes rebuild the pipeline.
