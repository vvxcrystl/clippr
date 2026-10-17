# Development validation — 2026-10-16

Machine: KDE Plasma 6 Wayland, AMD RX 9060 XT, Mesa radeonsi VAAPI,
GStreamer 1.28.8, Rust 1.97. All Cargo commands used
`CARGO_TARGET_DIR=target/daemon`.

## Build and automated checks

- `cargo build --release -p clipprd`: passed, no compiler warnings.
- `cargo test -p clipprd`: five unit tests passed (ring bounds/keyframes, audio
  alignment, independent snapshots, ID sanitization, bounded IPC lines).
- `cargo clippy -p clipprd`: passed; reports one existing `derivable_impls`
  warning in the shared core's manual `Config::default` implementation.
- `cargo clippy -p clipprd --no-deps -- -D warnings`: passed with zero daemon
  warnings. No shared core contract was changed.
- `tests/smoke.py`: passed against the release binary. Hardware H.264, HEVC,
  AV1 and software H.264 outputs contained video and AAC audio, started at PTS
  zero on a keyframe, had no B-frames, decoded successfully, and had valid
  sidecars and JPEG thumbnails. Also exercised all IPC requests, events,
  oversized/malformed requests, duplicate instance rejection, config rebuilds,
  startup paused, pause/resume, source reset, overlay SIGTERM/reaping, Quit and
  SIGTERM socket cleanup.
- systemd unit syntax validated with the built executable substituted for the
  install path. Checking the original unit reports the expected missing
  `~/.local/bin/clipprd` executable because this task did not install it.

## Real desktop capture

The actual ScreenCast picker was accepted. Capture then resumed across restarts
using the mode-0600 restore token. Logs verified:

```
screen: video/x-raw(memory:DMABuf), format=DMA_DRM,
        drm-format=AR24:0x0200000000082305, 1920x1080, max-framerate=60/1
encoder input: video/x-raw(memory:VAMemory), format=NV12, 1920x1080
encoder: vah264enc, 10,000 kbps, no B-frames
```

PipeWire links verified the audio stream connected to the default **Audio/Sink**
monitor, rather than the microphone. KWin event callbacks populated the actual
focused app, PID and title. The script also connects the active window
caption-change signal, so the cache stays current without polling or a focus
switch; the updated script loaded successfully in the live KWin session. Host portal registration and GlobalShortcuts binding
both succeeded after supplying an executable desktop identity. A separate live
mic-plus-desktop test verified both Audio/Source and Audio/Sink input links and
saved a 1.9355-second H.264/AAC MP4 through the mixer. Its optional notification
request completed without a reported error; visual presentation was not checked.

A five-second IPC save produced a 4.450-second H.264 MP4 at 1920x1080, 60 fps
(267 video frames), plus 48 kHz stereo AAC. ffprobe confirmed PTS zero and a
keyframe on the first video packet, no B-frames, a maximum one-second interval
between keyframes, and audio starting at 0.0085 seconds. Full ffmpeg decode
passed. The atomic JSON sidecar and background JPEG thumbnail were present.
KWin reported `T3 Code (Nightly)` as the focused app and the title was
`T3 Code (Nightly) 22:52`.

## Resource measurement

One uninterrupted 30.007-second interval on the release capture path (before
the teardown-only allocator maintenance change),
using `/proc/<pid>/stat` CPU ticks and one-second RSS samples, measured:

- CPU: **1.60% of one core** (sum of user and kernel CPU time).
- RSS: **104.1 MiB mean**, 94.9–105.9 MiB observed range.
- PSS at the end: **74.4 MiB**; private resident pages: **67.3 MiB**.
- Encoded ring: **37.8 MiB**, covering 31.88 seconds of a 32-second buffer.
- PSS less encoded payload: approximately **36.6 MiB**.

This is below the task's roughly 2% investigation threshold, but does **not**
meet the architecture's strict under-1% CPU target on this machine. DMA-BUF and
VA-memory negotiation were both verified, with no system-memory video fallback.
A second 30-second thread breakdown attributed about 0.90% to the GStreamer
video streaming thread, 0.30% to audio, and 0.27% to the Mesa command-submission
thread; the daemon actor used 0.00% measured CPU.
A subsequent 30-second run of the final allocator change measured 1.80% CPU,
77.0 MiB mean RSS and 59.2 MiB ending PSS while the buffer warmed. A concurrent
synthetic codec smoke test ran during part of this second interval.
Teardown now returns freed glibc arenas once, outside the recording path. Paused
capture then measured 0.00% CPU over 10.000 seconds, with an empty
ring and no encoder; paused RSS was 60.0 MiB and PSS was 29.7 MiB
(down from 44.2 MiB PSS before the allocator trim).
These are live desktop measurements, not a game/compile workload benchmark.
The synthetic source generates raw frames on the CPU and is unsuitable for
measuring the production hardware capture path.

## Limits of this validation

Physical shortcut activation, Steam game name lookup, Hyprland/Sway/X11 focus
fallbacks, notification presentation, and injected driver/portal failures were
not exercised. Shortcuts were successfully registered with the real KDE portal;
these tests did not synthesize physical keys. The real GUI was not started by
the smoke test: it used a disposable executable to verify overlay child lifecycle.
Software fallback was exercised explicitly; automatic fallback after an injected
hardware fault and the complete 2/5/15/60-second failure schedule remain untested.
