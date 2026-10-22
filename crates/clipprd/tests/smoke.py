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
import shutil
import socket
import subprocess
import sys
import tempfile
import time

binary = Path(sys.argv[1]).resolve()
fps = int(os.environ.get('CLIPPR_SMOKE_FPS', '30'))
resolution = os.environ.get('CLIPPR_SMOKE_RESOLUTION', 'p720')
software_only = os.environ.get('CLIPPR_SMOKE_SOFTWARE_ONLY') == '1'

with tempfile.TemporaryDirectory(prefix='clipprd-smoke-') as tmp:
    root = Path(tmp)
    env = dict(os.environ, XDG_RUNTIME_DIR=tmp, XDG_CONFIG_HOME=tmp + '/config',
               XDG_STATE_HOME=tmp + '/state', XDG_CACHE_HOME=tmp + '/cache',
               XDG_DATA_HOME=tmp + '/data')
    config_dir = root / 'config/clippr'
    config_dir.mkdir(parents=True)
    (config_dir / 'config.toml').write_text(
        f'[general]\nclips_dir = "{tmp}/clips"\nnotify_on_save = false\n'
        '[hotkeys]\nclips = [{ trigger = "F8", seconds = 5 }, { trigger = "F9", seconds = 8 }]\n'
        f'[video]\nfps = {fps}\nresolution = "{resolution}"\n'
        + ('encoder = "software"\n' if software_only else '')
        + '[audio]\ncapture_mic = true\n')
    # The stub tests child spawning, SIGTERM and reaping without starting the GUI.
    stub_dir = root / 'bin'
    stub_dir.mkdir()
    # A workspace build may put the real GUI beside clipprd. Copy this test's
    # daemon beside its stub so current_exe cannot select that GUI instead.
    isolated_binary = stub_dir / 'clipprd'
    shutil.copy2(binary, isolated_binary)
    binary = isolated_binary
    stub = stub_dir / 'clippr-gui'
    stub.write_text('#!/usr/bin/python3\nimport os,signal\n'
                    'assert "XDG_ACTIVATION_TOKEN" not in os.environ\n'
                    'assert "DESKTOP_STARTUP_ID" not in os.environ\n'
                    'open(os.environ["OVERLAY_PID"],"w").write(str(os.getpid()))\n'
                    'signal.pause()\n')
    stub.chmod(0o755)
    env.update(PATH=str(stub_dir) + ':' + env['PATH'], OVERLAY_PID=str(root / 'overlay.pid'),
               XDG_ACTIVATION_TOKEN='stale-daemon-token', DESKTOP_STARTUP_ID='stale-startup-id')
    log = open(root / 'daemon.log', 'w+')
    daemon = subprocess.Popen([str(binary), '--test-source', '--no-tray', '--verbose'], env=env, stdout=log, stderr=log)

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
        second = subprocess.run([str(binary), '--test-source', '--no-tray'], env=env, capture_output=True, timeout=5)
        assert second.returncode != 0 and b'another clipprd' in second.stderr
        with connect() as s:
            s.sendall(b'bad json\n{"cmd":"status"}\n')
            lines = s.makefile('rb')
            assert json.loads(lines.readline())['type'] == 'error'
            assert json.loads(lines.readline())['type'] == 'status'
        with connect() as s:
            s.sendall(b'x' * (1024 * 1024 + 1))
            assert json.loads(s.makefile('rb').readline())['type'] == 'error'

        def save_and_probe(seconds, codec, default=False):
            clip = request({'cmd': 'save', 'seconds': None if default else seconds})['clip']
            path = root / 'clips' / clip['file']
            probe = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-show_streams', '-show_format', '-of', 'json', str(path)]))
            settings = request({'cmd': 'get_config'})['config']['audio']
            if settings['separate_tracks']:
                assert {g['kind']: g['gain'] for g in clip['mix_gains']} == {'desktop': settings['levels']['game'] / 100, 'mic': settings['levels']['mic'] / 100, 'voice': settings['levels']['voice'] / 100}
            kinds = ['mix'] if settings['capture_desktop'] or settings['capture_mic'] else []
            if settings['separate_tracks'] and kinds:
                kinds += (['desktop'] if settings['capture_desktop'] else []) + (['mic'] if settings['capture_mic'] else []) + (['voice'] if settings['capture_desktop'] else [])
            assert {s['codec_type'] for s in probe['streams']} == ({'video', 'audio'} if kinds else {'video'}), probe
            video = next(s for s in probe['streams'] if s['codec_type'] == 'video')
            assert video['codec_name'] == codec, probe
            assert video.get('has_b_frames', 0) == 0, probe
            assert seconds <= float(video['duration']) <= seconds + 2.1, probe
            packets = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-select_streams', 'v', '-show_packets', '-show_entries', 'packet=pts_time,dts_time,flags', '-of', 'json', str(path)]))['packets']
            assert 'K' in packets[0]['flags'] and float(packets[0]['pts_time']) == 0, packets[0]
            assert all(p['pts_time'] == p['dts_time'] for p in packets), packets[:5]
            keys = [float(p['pts_time']) for p in packets if 'K' in p['flags']]
            assert all(b-a <= 2.1 for a, b in zip(keys, keys[1:])), keys
            audio = [s for s in probe['streams'] if s['codec_type'] == 'audio']
            labels = {'mix': 'Mix', 'desktop': 'Game', 'mic': 'Microphone', 'voice': 'Voice chat'}
            assert [s['tags']['handler_name'] for s in audio] == [labels[k] for k in kinds], audio
            assert [t['kind'] for t in clip['audio_tracks']] == kinds, clip
            assert [t['index'] for t in clip['audio_tracks']] == list(range(len(kinds))), clip
            if audio: assert audio[0]['disposition']['default'] == 1
            assert [s['channels'] for s in audio] == [1 if k == 'mic' else 2 for k in kinds]
            for track in audio:
                assert 0 <= float(track['start_time']) < .05, track
                assert abs(float(track['start_time']) + float(track['duration']) - float(video['duration'])) < .08, probe
            assert video['height'] == {'p720': 720, 'p1080': 1080}[resolution], video
            assert abs(float(video['avg_frame_rate'].split('/')[0]) / float(video['avg_frame_rate'].split('/')[1]) - fps) < .1, video
            subprocess.run(['ffmpeg', '-v', 'error', '-xerror', '-i', str(path), '-map', '0', '-f', 'null', '-'], check=True, capture_output=True, timeout=20)
            assert json.loads(path.with_suffix('.json').read_text()) == clip
            wait_for(lambda: (root / 'cache/clippr/thumbs' / (clip['id'] + '.jpg')).exists())
            print(codec, 'duration=', probe['format']['duration'], 'keyframes=', keys, flush=True)
            return clip

        time.sleep(8)
        first = save_and_probe(5, 'h264', default=True)
        config = request({'cmd': 'get_config'})['config']
        config['hotkeys']['clips'].append({'trigger': 'F7', 'seconds': 300})
        request({'cmd': 'set_config', 'config': config})
        assert request({'cmd': 'get_config'})['config']['hotkeys']['clips'][-1]['seconds'] == 300
        assert request({'cmd': 'status'})['status']['buffered_secs'] >= 7
        # Changing the longest binding preserves already buffered history.
        save_and_probe(8, 'h264')
        while True:
            event = json.loads(event_file.readline())['event']
            if event['kind'] == 'clip_saved':
                assert event['clip']['id'] == first['id']
                break
        codecs = [('h264', 'h264')] if software_only else [('hevc', 'hevc'), ('av1', 'av1'), ('h264', 'h264')]
        for codec, expected in codecs:
            config = request({'cmd': 'get_config'})['config']
            config['video']['codec'] = codec
            config['video']['encoder'] = 'software' if codec == 'h264' else 'hardware'
            request({'cmd': 'set_config', 'config': config})
            wait_for(lambda: request({'cmd': 'status'})['status']['state'] == 'recording')
            time.sleep(3)
            save_and_probe(2, expected)
        for desktop, mic, separate in [(True, False, True), (False, True, True), (True, True, False), (False, False, True)]:
            config = request({'cmd': 'get_config'})['config']
            config['audio'].update(capture_desktop=desktop, capture_mic=mic, separate_tracks=separate)
            request({'cmd': 'set_config', 'config': config})
            wait_for(lambda: request({'cmd': 'status'})['status']['state'] == 'recording')
            time.sleep(3)
            save_and_probe(2, 'h264')
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
        daemon = subprocess.Popen([str(binary), '--test-source', '--no-tray'], env=env, stdout=log, stderr=log)
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
