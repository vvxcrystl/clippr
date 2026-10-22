#!/usr/bin/env python3
"""Exercise editor playback, saved gains and shortcut settings on an isolated GTK Broadway display."""
import os
from pathlib import Path
import subprocess
import tempfile

with tempfile.TemporaryDirectory(prefix="clippr-editor-") as directory:
    root = Path(directory)
    env = dict(os.environ, CLIPPR_ISOLATED_GUI_TEST="1", GDK_BACKEND="broadway",
               BROADWAY_DISPLAY=":77", GSK_RENDERER="cairo", GTK_A11Y="none", GDK_DEBUG="no-portals")
    for kind in ("CONFIG", "STATE", "CACHE", "RUNTIME", "DATA"):
        path = root / kind.lower()
        path.mkdir(mode=0o700)
        env[f"XDG_{kind}_HOME" if kind != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(path)
    clips = root / "clips"
    clips.mkdir()
    config = root / "config/clippr"
    config.mkdir()
    (config / "config.toml").write_text(
        f'[general]\nclips_dir = "{clips}"\n[audio.levels]\ngame = 90\n')
    subprocess.run([
        "ffmpeg", "-v", "error", "-f", "lavfi", "-i", "testsrc=size=160x90:rate=20",
        "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "6",
        "-map", "0:v", "-map", "1:a", "-map", "1:a", "-c:v", "libx264",
        "-pix_fmt", "yuv420p", "-c:a", "aac", "-metadata:s:a:0", "handler_name=Mix",
        "-metadata:s:a:1", "handler_name=Game", str(clips / "fixture.mp4")], check=True)
    # No service activation: GTK must not start host portals or FUSE mounts.
    bus_config = root / "bus.conf"
    bus_config.write_text(
        f'<busconfig><type>session</type><listen>unix:tmpdir={root}</listen>'
        '<policy context="default"><allow send_destination="*" eavesdrop="true"/>'
        '<allow eavesdrop="true"/><allow own="*"/></policy></busconfig>')
    display = subprocess.Popen(["gtk4-broadwayd", "--unixsocket", str(root / "http.sock"), ":77"], env=env)
    try:
        subprocess.run([
            "dbus-run-session", f"--config-file={bus_config}", "--", "cargo", "test", "-p", "clippr-gui",
            "editor_audio_and_shortcut_settings", "--", "--ignored", "--nocapture",
            "--test-threads=1"], env=env, check=True)
    finally:
        display.terminate()
        display.wait(timeout=5)
