# Development validation — 2026-10-16

Machine: KDE Plasma 6 Wayland, AMD RX 9060 XT, Mesa radeonsi VAAPI,
GStreamer 1.28.8, Rust 1.97. All Cargo commands used
`CARGO_TARGET_DIR=target/daemon`.

## Follow-up validation

- Saving now chooses the last keyframe at or before the requested start, or the
  oldest available keyframe when history is shorter. A new unit test covers a
  cutoff between keyframes, an exact boundary, and a request longer than history.
  The architecture and daemon README describe the corrected rule.
- A live 1080p60 VA H.264/AAC save for 10 seconds on the final build contained
  **10.283333 seconds** of video (617 frames), starting at zero on a keyframe;
  audio started at 0.0185 seconds. Full ffmpeg decode passed, and adjacent
  keyframes were no more than 1.1 seconds apart. Before saving, the full ring
  contained 40,173,161 bytes over 31.65 seconds.
- `cargo test --workspace`: passed (seven daemon unit tests, shared-core unit
  and media/export tests). `cargo clippy -p clipprd --no-deps -- -D warnings`:
  passed with no warnings. No GUI or core code changed.
- The release smoke test passed for VA H.264, HEVC, AV1 and software H.264,
  checking **video stream duration >= requested duration**. Observed durations
  were 5.066667, 2.033333, 2.733333 and 2.066667 seconds respectively for
  requests of 5, 2, 2 and 2 seconds. Decode, audio, keyframes, sidecars,
  thumbnails, IPC and lifecycle checks also passed.
- Shortcut options now forward their nonempty string `activation_token` to the
  overlay's `XDG_ACTIVATION_TOKEN`. A unit test checks the explicit token versus
  no token; the smoke test starts the daemon with stale activation/startup
  credentials and asserts the IPC-launched overlay inherits neither. All daemon
  child-command sites discard inherited `XDG_ACTIVATION_TOKEN` and
  `DESKTOP_STARTUP_ID`. Physical shortcut activation and resulting GTK focus
  remain unverified.
- The packaged library launcher and the daemon use the same portal identity,
  `dev.clippr.Clippr`; the standalone hidden fallback derives from that packaged
  template. Both desktop entries (packaged and generated fallback) passed
  `desktop-file-validate`. `bash -n` passed for install/uninstall. The unit passed
  `systemd-analyze --user verify` with its executable replaced by the built binary.
  Install/uninstall were not run, and the live installation was not changed.
  In particular, KDE shortcut display-name/icon integration after installation
  remains unverified: the real portal cannot see a desktop entry created only
  in the isolated daemon's `XDG_DATA_HOME`, so it rejected shortcut registration
  during these runs.

### Follow-up CPU investigation

All capture runs used isolated config/state/cache/runtime/data directories, the
real PipeWire runtime for audio, 1920x1080 at 60 fps, VA H.264 at 10 Mbps, and
desktop stereo FDK AAC at 160 kbps. DMA-BUF source and VA-memory encoder-input
negotiation were checked again. The production encoded callbacks already keep
`gst::Buffer` references; they do not copy packet payloads. Trimming checks queue
fronts, and there is no per-frame logging or focus polling to remove.

The baseline was measured over 45.001 seconds using `/proc/PID/task/TID/schedstat`
CPU-runtime deltas (nanosecond resolution). This includes kernel time and reports
percentages of one core. The actor and all other listed worker threads were idle.

- `screen:src`: **0.994%**, 363.95 context switches/s.
- `desktop:src`: **0.496%**, 97.95 context switches/s.
- Mesa `clipprd:cs0`: **0.272%**, 124.60 context switches/s.
- Two `pipewire-main-l` loops: **0.043% + 0.040%**.
- Daemon actor and remaining workers: **0.000%**.
- Total: **1.845%**.

A temporary build increased video keepalive from 16 to 100 ms and requested
`node.latency=2048/48000` for audio sources. The installed PipeWire 1.6.9 source
has no `latency-time` or `buffer-time` properties. `pw-dump` confirmed the audio
latency preference reached the stream; it did not change the shared graph's
observed roughly 47 audio wakeups/s. The portal continued supplying 60 fps, as
confirmed by the saved video's packet count, even with the slower keepalive.
Over 45.001 seconds this build used **1.761%**: video **0.934%**, audio **0.496%**,
Mesa submission **0.252%**, PipeWire loops **0.041% + 0.038%**, actor **0.000%**.
The small difference does not establish a repeatable improvement. Both tuning
changes were removed; the media path and configured capture fidelity are retained.

The final build, with both experiments removed, measured **1.819%** over
45.001 seconds (process-wide tick counter: **1.844%**):

- `screen:src`: **0.994%**, 365.21 context switches/s.
- `desktop:src`: **0.491%**, 102.18 context switches/s.
- Mesa `clipprd:cs0`: **0.251%**, 121.80 context switches/s.
- Two `pipewire-main-l` loops: **0.042% + 0.040%**.
- Daemon actor and remaining workers: **0.000%**.

There is no demonstrated CPU improvement over the baseline. The difference
between the summed scheduler runtime and process tick counters is small in
absolute CPU time and does not affect the under-1% conclusion.
After pausing and tearing capture down, a ten-second measurement used
**0.0004%** of one core (0.00% rounded). The isolated daemon was then shut down
through its own socket; no pre-existing process or live socket was touched.

The strict **under-1% target remains unmet**. Streaming video plus Mesa submission
alone measured about 1.2% before adding audio and the PipeWire loops. Around
1.8% is the observed cost at 60 fps on this stack, not a proven irreducible floor.
These thread totals cannot distinguish individual driver/GStreamer functions
from the daemon callbacks running on those threads. A local extracted `perf`
binary could count task time, but the host rejected sampling events (including
user-only task-clock, cpu-clock and cycles); no system security settings were
changed. Further attribution requires working function sampling. Reducing the
capture rate or disabling audio was not used to claim success.

`tests/profile_cpu.py PID --seconds 45` reproduces the read-only thread
measurement, includes a process-wide `/proc/PID/stat` cross-check, and rejects
changing thread sets. Run it after the ring fills, without concurrent saves or
builds. The synthetic source is still unsuitable for hardware CPU measurements.

## Previous-round results

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
