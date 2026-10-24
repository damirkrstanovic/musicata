#!/usr/bin/env python3
"""Disposable real Musicata/MPD registration and playback probe (ALSA null only)."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
import wave

if os.environ.get('MUSICATA_DISPOSABLE_AUDIO_TEST') != '1':
    raise SystemExit('Use an isolated pod with MUSICATA_DISPOSABLE_AUDIO_TEST=1')
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'packaging'))
import install_player as installer

with socket.socket() as check:
    check.bind(('127.0.0.1', 6600))
with tempfile.TemporaryDirectory(prefix='standalone-mpd-') as directory:
    root = Path(directory)
    for subdir in ('music', 'playlists'):
        (root / subdir).mkdir()
    with socket.socket() as reserve:
        reserve.bind(('127.0.0.1', 0))
        port = reserve.getsockname()[1]
    url = f'http://127.0.0.1:{port}'
    with wave.open(str(root / 'music/fixture.wav'), 'wb') as audio:
        audio.setnchannels(2)
        audio.setsampwidth(2)
        audio.setframerate(48000)
        audio.writeframes(b'\0' * 48000 * 4 * 30)
    cfg = installer.options(['--backend', 'mpd', '--output', 'plughw:CARD=Fixture,DEV=0', '--musicata-url', url])
    installer.MPD_ROOT = root
    config = installer.playback_files(cfg, os.getuid(), os.getgid())[installer.MPD_CONFIG][0]
    config = config.replace('device "plughw:CARD=Fixture,DEV=0"', 'device "null"')
    (root / 'mpd.conf').write_text(config)
    processes = []
    logs = []

    def start(name, args):
        log = (root / (name + '.log')).open('w+')
        logs.append(log)
        process = subprocess.Popen(args, stdout=log, stderr=log)
        processes.append(process)

    def api(path, payload=None):
        request = urllib.request.Request(url + path, data=json.dumps(payload).encode() if payload is not None else None,
                                         headers={'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=5) as response:
            return json.load(response)

    def wait_for(action):
        for _ in range(100):
            try:
                result = action()
                if result:
                    return result
            except (OSError, ValueError, KeyError):
                pass
            time.sleep(0.1)
        raise AssertionError('Timed out waiting for Musicata')

    try:
        start('mpd', ['mpd', '--no-daemon', str(root / 'mpd.conf')])
        installer.verify(cfg)
        start('server', [str(Path(sys.argv[1]).resolve()), '--database', str(root / 'musicata.db'),
                         '--library', str(root / 'music'), '--addr', f'127.0.0.1:{port}', '--public-url', url])
        wait_for(lambda: api('/api/health'))
        # The disposable first-run server has authentication disabled. Production setup
        # requires an administrator token; request-header behavior has separate tests.
        installer.register_mpd(cfg, 'fixture-token')
        installer.register_mpd(cfg, 'fixture-token')
        players = [p for p in api('/api/players') if p['kind'] == 'mpd']
        assert len(players) == 1 and players[0]['online']
        tracks = wait_for(lambda: api('/api/tracks?limit=1')['items'])
        track = tracks[0]
        player_id = players[0]['id']
        api(f'/api/players/{player_id}/commands', {'command': 'play_tracks', 'track_ids': [track['id']]})
        wait_for(lambda: api(f'/api/players/{player_id}/state')['status'] == 'playing')
        api(f'/api/players/{player_id}/commands', {'command': 'pause'})
        wait_for(lambda: api(f'/api/players/{player_id}/state')['status'] == 'paused')
        print('PASS: standalone MPD config, local protocol, Musicata registration/rerun and play/pause; ALSA null only')
    except Exception:
        for log in logs:
            log.flush()
            log.seek(0)
            print(log.read()[-4000:], file=sys.stderr)
        raise
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        for log in logs:
            log.close()
