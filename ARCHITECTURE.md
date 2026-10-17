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
- RAM: the ring buffer of encoded packets plus under 40 MB of process overhead. 62s of
  1440p60 H.264 at 16 Mbps is about 125 MB.
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
   → `vah264enc` / `vah265enc` / `vaav1enc` (CBR/VBR at `Config::video_bitrate_kbps`,
   `key-int-max` = fps so there is a keyframe every second, no B-frames)
   → `h264parse`/`h265parse`/`av1parse` with `config-interval=-1` → `appsink`.
   Fallback when VAAPI fails or `encoder = software`: `videoconvert ! videoscale ! x264enc
   speed-preset=superfast tune=zerolatency` (H.264 only). Framerate cap via caps on the
   PipeWire stream (`max-framerate`), not a `videorate` element.
3. **Audio.** Desktop audio from the default sink monitor, optional mic from the default (or
   named) source, mixed with `audiomixer`, encoded with `fdkaacenc` (fallback `avenc_aac`)
   at `audio.bitrate_kbps`. Both from `pipewiresrc`. Separate appsink.
4. **Ring buffer.** A `VecDeque` of encoded samples per stream (`Arc<[u8]>` or `gst::Buffer`,
   PTS, duration, keyframe flag). Trim from the front by PTS so the buffer holds
   `General::buffer_secs()` of video. Trimming keeps the oldest retained video sample a
   keyframe. Keep codec headers (caps / codec_data) alongside.
5. **Saving.** On save, take a cheap snapshot of the deques under a short lock, then mux on a
   background thread at low priority (`SCHED_IDLE` or nice 19): start at the last video
   keyframe at or before `now - seconds` (or the oldest keyframe if history is shorter),
   align audio by PTS, rebase timestamps to zero,
   write `appsrc → mp4mux (faststart) → filesink` to `<clips_dir>/<id>.mp4.part`, then rename.
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
8. **Hotkeys.** xdg-desktop-portal GlobalShortcuts (`ashpd`), ids `clip-short`, `clip-long`,
   `toggle-overlay`, preferred triggers from `config.hotkeys`. If the portal is unavailable,
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
`h264_vaapi` (`-vaapi_device /dev/dri/renderD128`), falling back to `libx264 -preset veryfast`,
using `trim`/`atrim` + `concat` filters. Progress comes from `-progress pipe:1`.

## Config

TOML at `paths::config_file()`, all fields defaulted (see `config.rs`). The GUI writes it via
`Request::SetConfig` when the daemon runs, or directly with `Config::save` when it does not.
The daemon applies changes live; video changes rebuild the pipeline.
