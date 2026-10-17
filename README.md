<img src="assets/icon.svg" width="64" height="64" alt="">

# clippr

A replay-buffer clipper for Linux, along the lines of Medal or ShadowPlay. clippr keeps the last
minute or so of your screen in memory. Press a key and it saves that minute to a file.

The background recorder is meant to stay out of the way while you game or compile. Frames stay
on the GPU from capture to encode (PipeWire DMA-BUF into VAAPI). The buffer holds encoded video
in RAM, and the recorder has no polling loops. The GUI only runs while you have it open.

> Status: early development. The capture daemon is still being finished and hasn't had a
> security review yet.

## Features

- Save the last 30 seconds (F8), the last minute (Shift+F8), or any length from the overlay or CLI.
  Both lengths are configurable.
- H.264, HEVC or AV1 through VAAPI, with an x264 fallback. Quality presets or a custom bitrate,
  plus a resolution and frame-rate cap. Desktop audio and an optional microphone.
- A quick overlay (Alt+F8) to clip, pause, toggle audio sources, change quality, and rename the
  clip you just saved.
- A library window with thumbnails, search, app and favorite filters, and sorting.
- Each clip gets a title, tags, the app that was focused when you saved it, and the date.
- An editor for trimming and cutting. Split at the playhead, remove parts, drag part edges, and
  undo. Export is either an instant stream copy that snaps to keyframes or a frame-accurate GPU
  re-encode, saved as a new clip or replacing the original.

## How it works

| Binary | Runs | Job |
|---|---|---|
| `clipprd` | always, as a systemd user service | capture, encode, ring buffer, hotkeys, saving clips |
| `clippr` | on demand | CLI client |
| `clippr-gui` | on demand | library, settings, editor, and the `--overlay` panel |

`clipprd` requests the screen through xdg-desktop-portal and stores the portal's restore
token, so you only pick a screen once. Hotkeys go through the GlobalShortcuts portal. Clips are
saved as `<id>.mp4` with a `<id>.json` sidecar, by default in `~/Videos/Clippr`. See
[ARCHITECTURE.md](ARCHITECTURE.md) for the details.

## Requirements

- A Wayland compositor with xdg-desktop-portal ScreenCast support (KDE Plasma 6 is the primary
  target). GlobalShortcuts portal support for hotkeys, or bind the CLI commands in your
  compositor instead.
- PipeWire, and GStreamer 1.24 or newer with the `pipewire`, `va`, `x264`, AAC and `mp4mux` plugins.
- GTK 4.20+ and libadwaita 1.8+.
- `ffmpeg` and `ffprobe` for thumbnails and editor export.
- Rust (stable) to build.

## Install

```sh
./scripts/install.sh
systemctl --user enable --now clipprd
```

This builds in release mode and installs `clipprd`, `clippr` and `clippr-gui` to
`~/.local/bin`, plus the systemd user unit. `./scripts/uninstall.sh` removes
them.

## Usage

```sh
clippr save          # save the short clip length
clippr save 45       # save the last 45 seconds
clippr status
clippr pause | resume | toggle
clippr overlay       # open or close the quick overlay
clippr open          # open the library
clippr list [--json]
clippr reset-source  # pick a different screen
```

Settings live in `~/.config/clippr/config.toml`. Change them in the app, or edit the file while
the daemon isn't running.

## Development

```sh
cargo build
cargo test -p clippr-core
cargo clippy --workspace
```

See [AGENTS.md](AGENTS.md) for conventions.
