# clipprd

Event-driven Linux replay-buffer daemon. The normal video path requires PipeWire
DMA-BUF input and VA memory at the encoder. It never maps a raw GPU frame. Encoded
packets are retained by reference in RAM; each save snapshots those references and
muxes on a dedicated nice-19 thread without interrupting capture.

## Run

```sh
CARGO_TARGET_DIR=target/daemon cargo build --release -p clipprd
target/daemon/release/clipprd
# Debug negotiated source and encoder memory features:
target/daemon/release/clipprd --verbose
```

The first ScreenCast request can open a monitor picker. Its restore token is
stored in `$XDG_STATE_HOME/clippr/restore_token` with mode 0600. GlobalShortcuts
binds `clip-short`, `clip-long`, and `toggle-overlay`. Modern portals need a
matching desktop identity; when no packaged `dev.clippr.Clippr.desktop` exists,
the daemon creates a hidden entry in the user's applications directory. An
installed GUI desktop entry takes precedence. `packaging/dev.clippr.Clippr.desktop`
serves both the library launcher and the daemon's explicitly registered portal app
ID, `dev.clippr.Clippr`, including when launched by systemd. No second desktop
identity is needed. If shortcuts are unavailable, bind
`clippr save` / `clippr overlay` in the compositor.

`general.start_recording=false` starts paused. Pausing destroys the pipeline.
Video/audio config changes restart it; hotkey changes replace the shortcut
session. Error retries wait 2, 5, 15, then 60 seconds. Auto encoder mode falls back
to software H.264 when the hardware pipeline fails. Software mode has a much
higher CPU cost and does not provide the hardware resource budget.

Install the binary in `~/.local/bin/clipprd` and the unit from
`packaging/clipprd.service` in `~/.config/systemd/user/`, then run:

```sh
systemctl --user daemon-reload
systemctl --user enable --now clipprd.service
```

The unit runs after, and stops with, the graphical session. Manual launches also
set nice 10; saving and thumbnail generation use nice 19 and idle I/O priority.

## Code layout

- `main.rs`: current-thread Tokio actor, config, lifecycle, overlay children.
- `portal.rs`: ScreenCast permissions, private token, GlobalShortcuts sessions.
- `capture.rs`: GStreamer pipelines, source-track AAC encoders, encoded appsinks.
- `audio.rs`: event-driven PipeWire discovery and per-application monitor taps.
- `ring.rs`: bounded encoded history and keyframe-aligned snapshots.
- `save.rs`: MP4 muxing, atomic sidecars, ffmpeg thumbnails, notifications.
- `focus.rs`: event-driven KWin script; on-save Hyprland/Sway/X11 queries; Steam
  manifests and desktop names.
- `ipc.rs`: credential-checked, bounded newline-JSON Unix server.

No timer publishes status and no loop polls the desktop, keyboard, child process,
or capture bus. On a quiet desktop PipeWire can deliver fewer frames than the
configured maximum. Keyframes are requested from incoming timestamps as well as
from the encoder GOP limit, preserving two-second seek points at variable rates.
A saved clip begins at the last available keyframe at or before the requested
start, using the ring's three seconds of slack. It includes at least the requested
duration whenever that much history is available; otherwise it saves all available
history from the oldest keyframe.

Preset encodes use VBR with a quality ceiling (QP floor) so flat scenes do not fill
the bitrate budget. Budgets scale with configured maximum fps; Custom remains the
user's bitrate. The `Config::video_bitrate_kbps(height)` API is unchanged and remains
the Settings memory estimate input. See `VALIDATION.md` for measured savings,
codec comparisons and the VA driver's bitrate overshoot caveat.

## Verification

```sh
CARGO_TARGET_DIR=target/daemon cargo test -p clipprd
CARGO_TARGET_DIR=target/daemon cargo clippy -p clipprd --no-deps -- -D warnings
python3 crates/clipprd/tests/smoke.py target/daemon/release/clipprd
# After live capture has filled its ring (percentages are of one CPU core):
python3 crates/clipprd/tests/profile_cpu.py <daemon-pid> --seconds 45
```

The smoke test runs synthetic capture with isolated XDG directories. It checks
VA H.264/HEVC/AV1 and software H.264, decodes each output with ffmpeg, verifies
keyframes/audio/duration/sidecars/thumbnails, and exercises IPC limits, events,
live config, pause/resume, source reset, duplicate-instance rejection, overlay
termination/reaping, and shutdown. It requires the corresponding VA plugins.

`--test-source` selects live NV12 color bars (`videotestsrc`) and `audiotestsrc`, skips desktop
portals/hotkeys/focus tracking, and otherwise uses the same ring/mux/IPC path. Its
raw frame generator runs on the CPU; its CPU figures do not represent DMA-BUF
screen capture. For isolated live portal tests, point all four config/state/cache/
runtime XDG variables at a temporary tree and keep `PIPEWIRE_RUNTIME_DIR` pointed
at the real session's runtime directory so desktop audio can connect.
Also isolate `XDG_DATA_HOME` if the standalone desktop-identity fallback must not
write to the installed applications directory. The running desktop portal reads
its own data paths, so it will not find a desktop entry created only inside that
sandbox; shortcut registration requires the normally installed launcher.

See `VALIDATION.md` for the development-machine measurements and remaining limits.

## Separate audio tracks

`[audio] separate_tracks = true` is the default. Saved MP4s contain Mix first (and
marked default), Game, optional Microphone, then Voice chat. Disable desktop capture
to omit both Game and Voice; disable mic capture to omit Microphone. Disabling
separate tracks retains the previous single mixed-track path.

`[audio.levels]` sets each source's volume in the saved Mix, in percent, for example
`game = 60` to keep game audio under the microphone. Source tracks stay as recorded, and
each new clip stores the levels used for its Mix. The editor reads those saved levels,
independent of current defaults. Levels have no effect without separate tracks.

Game includes output streams playing to the default sink except voice applications.
`voice_apps` matches application name, process binary and node name without case
sensitivity, including full client identities for WebRTC streams. Defaults are
`discord`, `vesktop`, `webcord`, `armcord`, `legcord`, `equibop`, plus Discord's
`discordcanary` and `discordptb` binary/app-id variants. Add another app identity to
this list to exclude it from Game and put it on Voice. A browser running Discord
normally identifies as the browser; distinguishing individual browser tabs is not
supported by PipeWire's application identity.

The recorder creates additional monitor links into one passive capture stream per app,
without creating virtual sinks, rerouting playback or changing defaults/volumes.
Game and Voice use continuous silence-backed mixers, so absent/restarting apps do
not reset timestamps. Mono and 44.1/48 kHz inputs are supported. Only source tracks
are encoded while recording: Game at the configured stereo bitrate, mic at 96 kb/s
mono, Voice at 96 kb/s stereo. Mix is decoded/mixed/encoded once on the low-priority
save thread; video and source AAC are copied. All sources share video-aligned ring
trimming and snapshot boundaries. Export track gains change only the Mix.

The private PipeWire unit test starts its own server with no hardware/device
modules. It checks two-app mixing, binary-based WebRTC classification, exclusion,
mono/rate adaptation, restarts, default changes and playback-link preservation.
It does not touch the session server. Core media tests explicitly disable VAAPI so
audio export validation does not schedule GPU work beside a running recorder.
The smoke script supports `CLIPPR_SMOKE_SOFTWARE_ONLY=1` for the same reason.
