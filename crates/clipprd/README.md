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
installed GUI desktop entry takes precedence. If shortcuts are unavailable, bind
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
- `capture.rs`: GStreamer pipelines, encoded appsink callbacks, error bus handler.
- `ring.rs`: bounded encoded history and keyframe-aligned snapshots.
- `save.rs`: MP4 muxing, atomic sidecars, ffmpeg thumbnails, notifications.
- `focus.rs`: event-driven KWin script; on-save Hyprland/Sway/X11 queries; Steam
  manifests and desktop names.
- `ipc.rs`: credential-checked, bounded newline-JSON Unix server.

No timer publishes status and no loop polls the desktop, keyboard, child process,
or capture bus. On a quiet desktop PipeWire can deliver fewer frames than the
configured maximum. Keyframes are requested from incoming timestamps as well as
from the encoder GOP limit, preserving one-second seek points at variable rates.
A saved clip begins at the first available keyframe at or after the requested
start, so it can be up to about one second shorter than requested.

## Verification

```sh
CARGO_TARGET_DIR=target/daemon cargo test -p clipprd
CARGO_TARGET_DIR=target/daemon cargo clippy -p clipprd --no-deps -- -D warnings
python3 crates/clipprd/tests/smoke.py target/daemon/release/clipprd
```

The smoke test runs synthetic capture with isolated XDG directories. It checks
VA H.264/HEVC/AV1 and software H.264, decodes each output with ffmpeg, verifies
keyframes/audio/duration/sidecars/thumbnails, and exercises IPC limits, events,
live config, pause/resume, source reset, duplicate-instance rejection, overlay
termination/reaping, and shutdown. It requires the corresponding VA plugins.

`--test-source` selects live `videotestsrc` and `audiotestsrc`, skips desktop
portals/hotkeys/focus tracking, and otherwise uses the same ring/mux/IPC path. Its
raw frame generator runs on the CPU; its CPU figures do not represent DMA-BUF
screen capture. For isolated live portal tests, point all four config/state/cache/
runtime XDG variables at a temporary tree and keep `PIPEWIRE_RUNTIME_DIR` pointed
at the real session's runtime directory so desktop audio can connect.

See `VALIDATION.md` for the development-machine measurements and remaining limits.
