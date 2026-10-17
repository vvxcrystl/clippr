# clippr

clippr is a very lightweight replay-buffer screen clipper for Linux, in the spirit of Medal and ShadowPlay. The always-running daemon must be unnoticeable while gaming. The primary target is KDE Plasma on Wayland with AMD VAAPI, but it should work on other Wayland compositors and on X11.

See `ARCHITECTURE.md` for the capture pipeline, IPC, clip library, and export details.


## Design notes (taken from T3 Code's AGENTS.md)

I like ambitious ideas, simple systems, and software that feels obvious. Do not preserve complexity just because it already exists. Do not introduce machinery because it looks architecturally impressive. Understand the real constraint, then fight for the smallest model that makes the correct behavior unsurprising.

Channel both "measure twice, cut once" and "yagni". Fight scope creep. Try to honor the dev's intent in both a minimal and realistic fashion.

The rest of this document is meant to help you navigate the codebase and make changes effectively. Think of these instructions less as "hard rules", more as "good defaults". The developer's preferences should be able to override anything here.

## Where things live

This is a Rust workspace.

- `crates/clippr-core` is the shared contract: config, IPC protocol, clip sidecar format, paths, clip library, editor export, and the IPC client.
- `crates/clipprd` is the capture daemon: xdg-desktop-portal ScreenCast, PipeWire, GStreamer VAAPI encode, the in-RAM ring buffer, GlobalShortcuts hotkeys, and the Unix socket IPC.
- `crates/clippr-cli` is the `clippr` command line client.
- `crates/clippr-gui` is the GTK4 and libadwaita app: library, settings, editor, and the `--overlay` quick-settings mode. It is never resident.

Build and check with `cargo build`, `cargo test -p clippr-core`, and `cargo clippy --workspace`. `scripts/install.sh` does the release build and installs to `~/.local/bin` plus a systemd user unit.

## The three ways to hurt yourself

1. **Killing by pattern.** Never `pkill -f`, `pgrep | kill`, or `kill` a PID you found by matching a name, path, or worktree string. Your own agent process has this worktree's path in its argv, and this machine runs several other agents and dev processes at once. That includes `clipprd` and `clippr-gui`, which may be the developer's real running instances. Kill only a PID you captured at spawn.
2. **Writing to the live install.** The developer's real `clipprd`, its socket (`paths::socket_path()`), `config.toml`, portal restore token, and clips directory are in use while you work. Reading them and copying from them are fine. Never start a second daemon against the real socket, never overwrite the real config or restore token, never delete or rewrite real clips, and do not run `scripts/install.sh` or `scripts/uninstall.sh` unless asked. `paths.rs` follows the XDG variables, so point `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, and `XDG_RUNTIME_DIR` at a temp directory when you need an isolated run.
3. **Stepping on other agents.** Several agents work in this checkout at the same time, each owning specific crates or files. Stay in yours. Use your own `CARGO_TARGET_DIR` (`target/daemon`, `target/lib`, or `target/gui`) so builds do not lock each other out. Only `git add` the files you own, never `git add -A` or `git commit -a`, and do not reset, stash, or checkout files you did not change.

## Taste

- Complexity belongs at the adapter boundary. Orchestration stays pure, UI stays dumb.
- Keep shared rules in `crates/clippr-core`. The daemon, CLI, and GUI parse input, call shared logic, and map errors. Do not duplicate config, IPC, clip format, or library logic in a second crate. UI code lives only in `crates/clippr-gui`, and frontend and backend work are split between agents, so change the core contract deliberately and say so.
- Validate at the boundary, not in the caller's good manners. The IPC socket is mode 0600, checks `SO_PEERCRED`, and rejects requests over 1 MiB. Clip ids go through `clip::is_valid_id` before they touch a path. Run ffmpeg, ffprobe, and gio with an argument vector, never through a shell.
- Write files atomically: write to a `.part` file or temp file, then rename. Write the sidecar JSON after the video. Never hold the ring buffer lock while muxing or doing I/O.
- Reuse widgets and shared styles in `crates/clippr-gui`. Add reusable appearance options to the widget; keep page layout with its caller. Avoid copying widgets for small visual differences.
- Inferred types over annotations. `unwrap` and `expect` on a runtime path in `clipprd` are the enemy. Return errors with `anyhow` and let the daemon recover.
- Comments should explain intent, constraints, or how to use a function. Update them when code moves or behavior changes. Skip narration of obvious statements.
- Protect the daemon's resource budget: under 1% of one core while recording, the encoded ring buffer plus under 40 MB of overhead, one hardware encode session, and no polling loops. Everything is event driven (GStreamer bus, D-Bus signals, socket readiness). On the hardware path no raw frame is copied to system memory. Saving never pauses or blocks capture, and paused state tears the pipeline down entirely.
- Keep the GUI responsive. Run exports, thumbnail generation, and other slow work off the UI thread, keep exports cancellable, and make loading and error states match the actual operation. Avoid animations that repaint continuously. The GUI is never resident, so do not add background services or tray behavior to it.
- Treat these as defaults. If one conflicts with the task, explain the tradeoff and follow the developer's stated intent.

## Check the installed media stack

GStreamer element names and properties (`vapostproc`, `vah264enc`, `vah265enc`, `vaav1enc`) differ between versions and drivers, and the portal and shortcut behavior differs between compositors. Check what the installed GStreamer and portal actually provide before relying on a property or signal from memory.

## Pull requests

Use conventional commit titles in plain language: `type(scope): description`. For example, `fix(clipprd): keep a keyframe at the start of the ring buffer` or `feat(gui): show export progress in the editor`. Choose a scope that names the affected area, such as `core`, `clipprd`, `cli`, or `gui`.

End every PR description with `Authored by: [model name]`, using the name of the model that wrote the PR, for example `Authored by: Claude Sonnet 5.5`.
