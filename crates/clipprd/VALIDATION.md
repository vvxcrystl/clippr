# Development validation — 2026-10-16

Machine: KDE Plasma 6 Wayland, AMD RX 9060 XT, Mesa radeonsi VAAPI,
GStreamer 1.28.8, Rust 1.97. All Cargo commands used
`CARGO_TARGET_DIR=target/daemon`.

## Encoder size/quality investigation — 2026-10-16

The original files were read only and copied to `/tmp`. Geometry Dash 1/2 are
actually **1080p60**, with 1,804/1,813 frames over 30.067/30.217s; the browser
sample is variable rate (151 frames over 3.017s, nominal 120fps). No experiment
changes dimensions or drops source frames. A separate 1080p120 `testsrc2` source
covers motion/detail. Its eight-second lossless sequence was repeated by stream
copy to make a 30-second stress reference, including the loop transitions.

Each real source was decoded once to lossless `libx264 -preset ultrafast -qp 0`
Matroska and every candidate was compared with that reference. This measures
additional compression damage, not recovery of detail lost in the original.
GStreamer results use software decode of that reference, `vapostproc`, and the
same VA encoder/parser/caps as capture. These offline system-memory uploads are
benchmark inputs, not a change to the daemon's DMA-BUF path.

Metrics use the default libvmaf model, `n_threads=8:n_subsample=12`, plus standard
SSIM on every twelfth frame. Both inputs have identical frame-index timing for
scoring (avoiding Matroska millisecond rounding) and explicit BT.709 limited-range
metadata to prevent automatic color-matrix conversions in the filter graph.
Output files retain their source timing. `p01` is the nearest-rank first percentile of sampled frame VMAF
(the ceil(0.01 * N)th sorted sample), **not** the average of the worst one percent.
Raw results also retain the minimum.
The short browser source has only 13 metric samples, so its p01 is effectively
the minimum. SSIM is the pooled `All` score on those sampled frames.

Sizes below are decimal MB of MP4 video normalized to 30s, plus **0.60 MB** for
unchanged AAC 160kbps. Browser sizes are extrapolations of its three seconds,
not predictions of future desktop activity. Each old/new row uses the same
source codec: old CBR uses the previous fixed table, 1s GOP and target-usage 4;
new VBR uses a quantizer floor, 2s GOP, target-usage 1, target-percentage 100,
and a two-second CPB. The new 120fps budget is twice the 60fps budget.

Actual MP4s made by stream-copying the original AAC into the new H.264 videos:
**37,828,936 → 16,671,529 bytes (55.93% smaller)** for Geometry Dash 1, and
**38,261,617 → 17,032,006 bytes (55.49% smaller)** for Geometry Dash 2.
Against the controlled CBR re-encodes, mean VMAF changes by -0.288/-0.287.
These are offline A/B encodes of the supplied footage, not a second live game run.

### Geometry Dash 1 (30.067s, 60fps)

| Codec / settings | MB / 30s | VMAF mean | VMAF p01 | SSIM |
|---|---:|---:|---:|---:|
| H264 old CBR, 1s GOP, usage4 | 36.98 | 97.572 | 96.309 | 0.997740 |
| H264 new VBR, minQP20, 2s GOP, usage1 | 16.61 | 97.284 | 95.874 | 0.997253 |
| HEVC old CBR, 1s GOP, usage4 | 26.13 | 97.595 | 96.237 | 0.997854 |
| HEVC new VBR, minQP20, 2s GOP, usage1 | 13.85 | 97.231 | 96.257 | 0.996961 |
| AV1 old CBR, 1s GOP, usage4 | 26.12 | 97.557 | 95.120 | 0.997319 |
| AV1 new VBR, minQP60, 2s GOP, usage1 | 11.02 | 97.026 | 95.683 | 0.995686 |

### Geometry Dash 2 (30.217s, 60fps)

| Codec / settings | MB / 30s | VMAF mean | VMAF p01 | SSIM |
|---|---:|---:|---:|---:|
| H264 old CBR, 1s GOP, usage4 | 36.88 | 97.651 | 96.529 | 0.997768 |
| H264 new VBR, minQP20, 2s GOP, usage1 | 16.88 | 97.364 | 96.272 | 0.997136 |
| HEVC old CBR, 1s GOP, usage4 | 26.08 | 97.624 | 96.212 | 0.997759 |
| HEVC new VBR, minQP20, 2s GOP, usage1 | 14.27 | 97.273 | 95.940 | 0.996916 |
| AV1 old CBR, 1s GOP, usage4 | 26.03 | 97.604 | 96.503 | 0.997270 |
| AV1 new VBR, minQP60, 2s GOP, usage1 | 11.12 | 97.070 | 95.041 | 0.995548 |

### Browser (3.017s, variable rate)

| Codec / settings | MB / 30s | VMAF mean | VMAF p01 | SSIM |
|---|---:|---:|---:|---:|
| H264 old CBR, 1s GOP, usage4 | 19.79 | 96.460 | 95.544 | 0.996724 |
| H264 new VBR, minQP20, 2s GOP, usage1 | 4.95 | 95.447 | 95.062 | 0.993467 |
| HEVC old CBR, 1s GOP, usage4 | 15.05 | 96.439 | 93.818 | 0.995749 |
| HEVC new VBR, minQP20, 2s GOP, usage1 | 5.57 | 96.189 | 95.971 | 0.994602 |
| AV1 old CBR, 1s GOP, usage4 | 13.83 | 95.690 | 89.162 | 0.993976 |
| AV1 new VBR, minQP60, 2s GOP, usage1 | 4.36 | 94.922 | 91.611 | 0.990994 |

### High-motion testsrc2 loop (30s, 120fps)

| Codec / settings | MB / 30s | VMAF mean | VMAF p01 | SSIM |
|---|---:|---:|---:|---:|
| H264 old CBR, 1s GOP, usage4 | 37.27 | 88.855 | 85.114 | 0.980639 |
| H264 new VBR, minQP20, 2s GOP, usage1 | 87.27 | 94.468 | 93.061 | 0.994844 |
| HEVC old CBR, 1s GOP, usage4 | 26.15 | 90.773 | 87.393 | 0.985318 |
| HEVC new VBR, minQP20, 2s GOP, usage1 | 72.56 | 93.698 | 92.052 | 0.993527 |
| AV1 old CBR, 1s GOP, usage4 | 26.32 | 89.936 | 87.214 | 0.978493 |
| AV1 new VBR, minQP60, 2s GOP, usage1 | 82.50 | 94.144 | 92.990 | 0.992716 |

### What shipped and why

- H.264 remains the default. All three VA encoders use VBR with `min-qp` rather
  than spending their entire budget on flat content. H.264/HEVC QP floors are
  Low 26 / Medium 20 / High 16; AV1 index floors are 100 / 60 / 40.
  The shared `Video::quality_quantizer()` supplies those values.
- The 60fps bitrate table is unchanged, with linear scaling by configured fps
  and the existing quality/codec multipliers. 1080p Medium H.264 budgets at
  30/60/120/144fps are 5/10/20/24 Mbps. Custom is still the unscaled user bitrate
  and keeps the previous bitrate/default-rate-control mode. No serialized config
  field was added, renamed or removed.
- `Config::video_bitrate_kbps(height)` has the same signature. It now accounts
  for fps and supplies a nominal VBR budget, also used by the GUI's existing RAM
  estimate. **It is not a hard bound**: on the 30s stress source the driver
  overshoots 20 Mbps H.264 to 23.11 Mbps, and 14 Mbps HEVC/AV1 to 19.19/21.84 Mbps.
  Settings can therefore underestimate memory for difficult content. No GUI code
  was edited. The fps-aware budget also deliberately makes high-detail 120fps
  clips larger and higher fidelity than the old fps-independent table; see the
  complete stress-source rows instead of assuming universal size savings.
- `KEYFRAME_INTERVAL_SECS=2` controls encoder GOP length and timestamp-driven
  force-key-unit requests. `Config::buffer_secs()` adds three seconds instead
  of two (one complete GOP plus one second of stream skew). Trimming remains
  bounded and starts at a keyframe. A test checks the longest requested clip
  at every phase of repeated two-second GOPs, including after trimming.
- H.264 B-frames work on this GPU (the test output has reordered PTS/DTS), but
  the ring treats decreasing PTS as discontinuity and saving explicitly writes
  DTS=PTS. They stay disabled. HEVC advertises no backward references. AV1 uses
  `hierarchical-level=1` to avoid implicit future-reference reordering as well.
- Software H.264 keeps `superfast`/`zerolatency` and no B-frames, but presets now
  use capped CRF (`pass=qual`, quantizer 26/20/16, bitrate/VBV budget). No slower
  software preset was introduced. Audio encoding is unchanged.
- Precise editor exports preserve the source codec on VAAPI, using the shared
  Medium VBR/QP/GOP model and passthrough frame timing. Failure still falls back
  to H.264 `libx264 -preset veryfast -crf 20`, now with the shared bitrate/VBV
  budget. A full real-source export check confirmed 1920x1080, all 1,804 frames,
  60fps and the original codec for H.264, HEVC and AV1. Re-encoding is lossy:
  compared with captured candidates, the additional mean VMAF loss is
  0.384 / 0.486 / 0.809 points. The AV1 first-percentile loss is 1.808 points
  (minimum loss 2.870 points); it is the weakest result, rather than a claim
  of lossless editing.

### Full Geometry Dash 1 precise exports

Compared against the same original lossless reference, after capturing with the
new settings and then re-encoding the complete captured video once more:

| Codec | MB / 30s | VMAF mean | VMAF p01 | VMAF min | SSIM |
|---|---:|---:|---:|---:|---:|
| H264 | 16.07 | 96.900 | 94.278 | 94.246 | 0.996259 |
| HEVC | 13.90 | 96.745 | 95.456 | 94.878 | 0.996078 |
| AV1 | 10.44 | 96.217 | 93.875 | 90.304 | 0.994638 |

### Adapter capabilities and alternatives measured

`vainfo -a` and the installed GStreamer 1.28.8 property enums expose CBR, VBR,
CQP and QVBR for all three codecs. ICQ is absent. `bitrate`, `cpb-size`,
`target-percentage`, QP bounds and target-usage are available; there is no separate
GStreamer max-bitrate property. Maximum VBR bitrate is derived from bitrate and
target-percentage. Drivers without VBR keep their default mode with a warning.

Full raw candidate results, including CBR/VBR/CQP/QVBR and codec/quality sweeps,
are in [tests/quality-results.json](tests/quality-results.json). Prototypes use
FFmpeg `h264_vaapi`, `hevc_vaapi` and `av1_vaapi`; the shipped candidates were
also encoded with GStreamer rather than assuming those adapters are identical.

CQP did well on easy material but has no bitrate control: H.264 QP22 used
12.03 MB/30s on the first eight seconds of Geometry Dash, but 172.82 MB/30s
on the eight-second stress source. QVBR also overspent on hard content. For
example, full Geometry Dash H.264 QVBR20 used 21.48 MB/30s versus VBR-floor20's
16.61 MB/30s, at essentially the same mean VMAF (97.526 versus 97.284).
The VBR floor was the better size/fidelity compromise.

For the eight-second Geometry Dash prototype, QVBR22/usage1/10Mbps with
1/2/3s GOPs used 18.96/16.39/15.69 MB per 30s at mean VMAF
97.177/97.082/97.034. Two seconds captures most of the gain; a third second
saves only another 4.3% while making seeks and history slack worse. On the
120fps stress prototype, 1/2/3s GOPs saved only 91.40/90.64/90.15 MB per 30s.

At QVBR22/20Mbps, H.264 B2 increased Geometry Dash size from 20.08 to
24.03 MB/30s, while reducing stress-source size only from 89.66 to 87.99 MB/30s.
That does not justify redesigning the ring/mux timestamp model in this change.
Usage1 reduced the same flat-content prototype from 20.08 to 15.07 MB/30s
at slightly higher mean VMAF; usage4 and usage7 produced identical outputs.

On the full Geometry Dash 1 source the shipped codecs are approximately
quality matched: H.264/HEVC/AV1 mean VMAF 97.284/97.231/97.026 at
16.61/13.85/11.02 MB/30s. AV1 is another 34% smaller than the new H.264 here.
It is only 5.5% smaller on the 30s stress source at similar mean quality;
HEVC is the smaller alternative on that source. AV1 is not universally best
on this hardware and these settings.

### Reproducing an individual comparison

Use only copied clips and isolated output paths. Do not run parallel hardware
benchmarks while gaming or assessing live capture: they compete for the same
GPU. For a 60fps H.264 Medium candidate, the encoder properties are:

```text
filesrc location=reference.mkv ! matroskademux ! h264parse !
avdec_h264 max-threads=4 ! vapostproc !
video/x-raw(memory:VAMemory),format=NV12 !
vah264enc rate-control=vbr min-qp=20 bitrate=10000
 target-percentage=100 cpb-size=20000 target-usage=1
 key-int-max=120 b-frames=0 ! h264parse config-interval=-1 !
video/x-h264,stream-format=avc,alignment=au ! mp4mux ! filesink location=candidate.mp4
```

In FFmpeg the analogous prototype uses `-rc_mode VBR -qmin 20 -quality 1
-b:v 10M -maxrate 10M -bufsize 20M -g 120 -bf 0`. Decode candidate/reference
with bounded threads; set identical BT.709 limited-range metadata and
`setpts=N/(60*TB)` on each branch, then score with
`libvmaf=n_threads=8:n_subsample=12:shortest=1:log_fmt=json:log_path=metrics.json`.
Split both branches and select every twelfth frame for `ssim=shortest=1`.
Use 120 instead of 60 for the synthetic reference. None of these scoring
filters are applied to saved candidate videos.

### End-to-end checks

- Required `cargo test --workspace` and
  `cargo clippy --workspace --exclude clippr-gui --all-targets -- -D warnings` pass.
  Cargo uses `CARGO_TARGET_DIR=target/daemon`.
- Isolated release smoke tests pass at 720p30 and 1080p120 for VA H.264, HEVC,
  AV1 and software H.264. At 1080p120, five-second H.264 and two-second codec
  saves contained 6.050 / 3.050 / 3.050 / 3.050 seconds, with two-second
  keyframe gaps. Checks include clean full decode, keyframe/PTS zero at the
  start, PTS=DTS for every packet, unchanged dimensions/fps, audio start within
  50ms and stream ends within 80ms, sidecars/thumbnails, IPC/lifecycle behavior.
- The old animated ball fixture itself measured only 155fps before any encoding
  and could not sustain the complete 1080p120 pipeline. The test source now uses
  cached NV12 color bars (about 8,888fps generation alone). It does not lower
  resolution or fps. Quality/headroom is separately checked with high-motion
  lossless media. The production DMA-BUF path is unchanged.
- The 30s/3,600-frame high-motion source encoded in 16.15s H.264, 10.99s HEVC,
  and 10.89s AV1 with the final settings, including CPU reference decode/upload.
  That is at least 1.86x / 2.73x / 2.75x real-time throughput on these runs.
  It is a throughput check, not a measurement of GPU utilization during gaming.

Live CPU checks used copied restore tokens and isolated config/state/cache/runtime/
data directories, real PipeWire audio, 1920x1080 with a **60fps maximum**, H.264
and desktop AAC160. DMA-BUF source and VA-memory encoder input were verified.
After a 35s warmup, stable-thread `schedstat` measurements over 45s gave:

| Capture path | Sum of thread CPU / one core | Process tick cross-check |
|---|---:|---:|
| Baseline | 1.818% | 1.822% |
| New settings | 1.271% | 1.267% |

The new binary used `--no-tray` to match the baseline without the independently
added tray feature. Source delivery was variable rate: the verification saves
averaged 51.86fps before and 46.41fps after. Therefore these runs show **no
observed CPU increase**, but do not establish a causal 30% encoder-CPU improvement
at identical frame delivery. The strict under-1% architecture target is still
unmet. The initial 13s-warmup samples were rejected when thread sets changed;
they were discarded, not averaged into these results. Both final live saves
started at zero on a keyframe with PTS=DTS, decoded cleanly, and contained
5.283s / 5.925s of video for five-second requests, with audio aligned within one
AAC frame at the start and less than 30ms at the end. Each isolated daemon was
shut down using its own socket, with captured-PID termination only as cleanup.


### User-reported stutter during benchmarking

The supplied 23:45:53 clip came from the old running installed daemon, not this
build: `/proc/<service PID>/exe`, the current installed file, and the new binary
had distinct SHA-256 hashes. The service still held a deleted executable from
23:32; no install or service restart was performed here. Its 30.217s video has
907 frames and a repeating 16.7/16.7/66.7ms cadence (about 30fps despite a 60fps
maximum). Audio packet timestamps are continuous at 1,024 samples/48kHz;
continuous timestamps do not prove the audible signal has no glitches.
Concurrent hardware prototypes may have contributed to contention; they were
stopped on the report and remaining tests ran serially after the user finished
gaming. This is not proof that bitrate settings fix that pre-existing capture
cadence. No real clip, config, socket or portal token was overwritten.

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

## Separate audio tracks — 2026-10-17

The real service, socket, config, restore token, clips and audio routing were not
modified. Daemon runs used isolated XDG config/state/cache/runtime/data directories,
`--test-source --no-tray`. Audio graph validation uses a completely private PipeWire
server with no hardware/device discovery modules; its null sink is test-only.

The native per-app capture test verifies simultaneous 440/660 Hz game streams,
880 Hz voice output identified by its full `application.process.binary=Discord`
while its application name is `WEBRTC VoiceEngine`, exclusion from Game, 44.1/48 kHz
adaptation, mono duplication, voice app removal/restart, short-lived streams,
default-sink changes and preserving ordinary playback links after recorder cleanup.
PipeWire may invalidate an object between announcement and bind: both ENOENT and
ESTALE are normal registry races, reconciled by removal events, rather than capture
failures. This fixed a failure found by running the complete suite concurrently.
Tests also use the stock link factory policy: capture nodes are passive; a requested
`link.passive` flag may be ignored by the server and derived from its node policies.
No server policy is overridden.

Core media fixtures verify ffprobe track discovery, legacy missing metadata,
fast and precise multitrack exports, muting mic/attenuating Voice only in Mix,
bit-identical copied video/source audio in fast mode, unchanged source volumes in
precise mode, single-track Desktop gain compatibility and invalid gain rejection.
Ring tests cover three source deques, byte accounting, shared trim/snapshot bounds
and synchronized discontinuity reset. Required workspace test and Clippy commands
pass, including the GUI (its existing editor edit was left untouched).

Initial hardware smoke runs produced correctly ordered/titled, decodable Mix/Game/
Microphone/Voice tracks for H.264, HEVC, AV1 and software H.264. **Hardware validation
was stopped after GPU resets**, including a reported FFmpeg VAAPI context loss during
export testing; KWin showed the user reset notifications. Some hardware validation
ran concurrently with the first benchmark, so those initial CPU/RAM observations
are discarded. This is not a clean hardware stability sign-off. No further GPU work
was performed after the report. Core audio media tests now select an unavailable
test VAAPI driver and exercise software fallback. The final software-only smoke
also checks desktop-only, mic-only, single-mix and video-only configurations,
all track names/default selection/channels, decoding, alignment, sidecars,
subscriptions, IPC/lifecycle behavior and cleanup.

A fresh **software video / synthetic audio** release comparison used 720p30,
desktop + mic enabled, configured audio 160 kb/s, 65 s warmup followed by 20 s
summed `/proc/PID/task/TID/schedstat` CPU measurement. Saving happened afterward.
It never opened a portal or audio device. This deliberately does not repeat the
live hardware baseline of ~1.8% and does not measure native per-app capture cost.

- Separation off: **14.65%** of one CPU core; RSS **83.40 MiB**, PSS **75.29 MiB**;
  encoded ring **1,727,742 bytes**. Saving a requested 60 s (61.1 s keyframe-aligned)
  clip took **0.080 s**.
- Separation on: **14.98%** of one CPU core; RSS **93.28 MiB**, PSS **85.23 MiB**;
  encoded ring **3,193,171 bytes**. Saving 60 s (61.1 s keyframe-aligned) took
  **0.642 s**, including decode/mix/AAC encode/remux.
- Observed growth: **0.33 percentage points CPU**, **9.94 MiB PSS**, including
  **1.40 MiB** more encoded history. These short synthetic observations include
  software-video/allocator overhead and are not a production resource guarantee.
- Final tracks: stereo Mix, stereo Game, mono Microphone, stereo Voice chat.
  Mix started at 0; sources started at 0.016 s; all ended within 7 ms of video.
  Every audio/video stream decoded successfully.

Remaining: real Discord/Vesktop voice calls, physical mic timing, live per-app
CPU/RAM at the desktop hardware path, and hardware stability after the observed
resets. No claim is made that the strict under-1% target is met. Browser tabs share
browser process identity; individual web Discord tabs cannot be distinguished.
Surround layouts are not validated; current taps capture FL/FR or duplicate MONO.
