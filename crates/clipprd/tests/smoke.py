#!/usr/bin/env python3
"""Real GStreamer smoke test; no portal picker or desktop recording.

Run after building: python3 crates/clipprd/tests/smoke.py target/daemon/release/clipprd
Uses an isolated config, socket and clip library. Requires ffmpeg/ffprobe and VA
encoders for the H.264/HEVC/AV1 hardware checks on the development machine.
"""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time

binary = Path(sys.argv[1]).resolve()

with tempfile.TemporaryDirectory(prefix='clipprd-smoke-') as tmp:
    root = Path(tmp)
    env = dict(os.environ, XDG_RUNTIME_DIR=tmp, XDG_CONFIG_HOME=tmp + '/config',
               XDG_STATE_HOME=tmp + '/state', XDG_CACHE_HOME=tmp + '/cache')
    config_dir = root / 'config/clippr'
    config_dir.mkdir(parents=True)
    (config_dir / 'config.toml').write_text(
        f'[general]\nclips_dir = "{tmp}/clips"\nclip_short_secs = 5\n'
        'clip_long_secs = 8\nnotify_on_save = false\n'
        '[video]\nfps = 30\nresolution = "p720"\n')
    # The stub tests child spawning, SIGTERM and reaping without starting the GUI.
    stub_dir = root / 'bin'
    stub_dir.mkdir()
    stub = stub_dir / 'clippr-gui'
    stub.write_text('#!/usr/bin/python3\nimport os,signal\n'
                    'open(os.environ["OVERLAY_PID"],"w").write(str(os.getpid()))\n'
                    'signal.pause()\n')
    stub.chmod(0o755)
    env.update(PATH=str(stub_dir) + ':' + env['PATH'], OVERLAY_PID=str(root / 'overlay.pid'))
    log = open(root / 'daemon.log', 'w+')
    daemon = subprocess.Popen([str(binary), '--test-source', '--verbose'], env=env, stdout=log, stderr=log)

    def connect():
        s = socket.socket(socket.AF_UNIX)
        s.settimeout(15)
        s.connect(tmp + '/clippr.sock')
        return s

    def request(req):
        with connect() as s:
            s.sendall(json.dumps(req).encode() + b'\n')
            result = json.loads(s.makefile('rb').readline())
        assert result['type'] != 'error', result
        return result

    def wait_for(condition, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if condition():
                return
            if daemon.poll() is not None:
                raise AssertionError('daemon exited')
            time.sleep(.05)
        raise AssertionError('condition timed out')

    try:
        wait_for(lambda: (root / 'clippr.sock').exists())
        assert (root / 'clippr.sock').stat().st_mode & 0o777 == 0o600
        subscriber = connect()
        subscriber.sendall(b'{"cmd":"subscribe"}\n')
        event_file = subscriber.makefile('rb')
        assert json.loads(event_file.readline())['type'] == 'event'
        wait_for(lambda: request({'cmd': 'status'})['status']['state'] == 'recording')
        second = subprocess.run([str(binary), '--test-source'], env=env, capture_output=True, timeout=5)
        assert second.returncode != 0 and b'another clipprd' in second.stderr
        with connect() as s:
            s.sendall(b'bad json\n{"cmd":"status"}\n')
            lines = s.makefile('rb')
            assert json.loads(lines.readline())['type'] == 'error'
            assert json.loads(lines.readline())['type'] == 'status'
        with connect() as s:
            s.sendall(b'x' * (1024 * 1024 + 1))
            assert json.loads(s.makefile('rb').readline())['type'] == 'error'

        def save_and_probe(seconds, codec):
            clip = request({'cmd': 'save', 'seconds': seconds})['clip']
            path = root / 'clips' / clip['file']
            probe = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-show_streams', '-show_format', '-of', 'json', str(path)]))
            assert {s['codec_type'] for s in probe['streams']} == {'video', 'audio'}, probe
            video = next(s for s in probe['streams'] if s['codec_type'] == 'video')
            assert video['codec_name'] == codec, probe
            assert video.get('has_b_frames', 0) == 0, probe
            assert seconds <= float(video['duration']) <= seconds + 1.1, probe
            packets = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-select_streams', 'v', '-show_packets', '-show_entries', 'packet=pts_time,flags', '-of', 'json', str(path)]))['packets']
            assert 'K' in packets[0]['flags'] and float(packets[0]['pts_time']) == 0, packets[0]
            keys = [float(p['pts_time']) for p in packets if 'K' in p['flags']]
            assert all(b-a <= 1.1 for a, b in zip(keys, keys[1:])), keys
            subprocess.run(['ffmpeg', '-v', 'error', '-i', str(path), '-f', 'null', '-'], check=True, capture_output=True, timeout=20)
            assert json.loads(path.with_suffix('.json').read_text()) == clip
            wait_for(lambda: (root / 'cache/clippr/thumbs' / (clip['id'] + '.jpg')).exists())
            print(codec, 'duration=', probe['format']['duration'], 'keyframes=', keys, flush=True)
            return clip

        time.sleep(6)
        first = save_and_probe(5, 'h264')
        while True:
            event = json.loads(event_file.readline())['event']
            if event['kind'] == 'clip_saved':
                assert event['clip']['id'] == first['id']
                break
        for codec, expected in [('hevc', 'hevc'), ('av1', 'av1'), ('h264', 'h264')]:
            config = request({'cmd': 'get_config'})['config']
            config['video']['codec'] = codec
            config['video']['encoder'] = 'software' if codec == 'h264' else 'hardware'
            request({'cmd': 'set_config', 'config': config})
            wait_for(lambda: request({'cmd': 'status'})['status']['state'] == 'recording')
            time.sleep(3)
            save_and_probe(2, expected)
        request({'cmd': 'set_paused', 'paused': True})
        paused = request({'cmd': 'status'})['status']
        assert paused['state'] == 'paused' and paused['buffer_bytes'] == 0 and paused['encoder'] is None
        request({'cmd': 'set_paused', 'paused': False})
        wait_for(lambda: request({'cmd': 'status'})['status']['state'] == 'recording')
        request({'cmd': 'reset_capture_source'})
        wait_for(lambda: request({'cmd': 'status'})['status']['state'] == 'recording')
        request({'cmd': 'toggle_overlay'})
        wait_for(lambda: (root / 'overlay.pid').exists())
        overlay_pid = int((root / 'overlay.pid').read_text())
        request({'cmd': 'toggle_overlay'})
        wait_for(lambda: not Path(f'/proc/{overlay_pid}').exists())
        config = request({'cmd': 'get_config'})['config']
        config['general']['start_recording'] = False
        request({'cmd': 'set_config', 'config': config})
        request({'cmd': 'quit'})
        assert daemon.wait(timeout=10) == 0
        assert not (root / 'clippr.sock').exists()
        daemon = subprocess.Popen([str(binary), '--test-source'], env=env, stdout=log, stderr=log)
        wait_for(lambda: (root / 'clippr.sock').exists())
        assert request({'cmd': 'status'})['status']['state'] == 'paused'
        daemon.send_signal(signal.SIGTERM)
        assert daemon.wait(timeout=10) == 0
        assert not (root / 'clippr.sock').exists()
        print('IPC, subscriptions, live codec rebuilds, pause/resume, reset, overlay, shutdown: PASS')
    except BaseException:
        log.flush()
        log.seek(0)
        print(log.read(), file=sys.stderr)
        raise
    finally:
        subscriber.close() if 'subscriber' in locals() else None
        if daemon.poll() is None:
            daemon.terminate()
            daemon.wait(timeout=10)
