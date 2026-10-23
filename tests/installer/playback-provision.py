#!/usr/bin/env python3
"""Real-process provisioning probe; only run in a disposable cluster pod.

Uses ALSA's null output, never physical audio. Requires a built Musicata server,
MPD, Snapserver and Snapclient. Owns localhost ports 6600/1704/1705/1780 and a
temporary /var/lib/musicata-mpd; refuses existing state or occupied ports.
"""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import wave

if os.environ.get('MUSICATA_DISPOSABLE_AUDIO_TEST') != '1':
    raise SystemExit('Run only in an isolated disposable pod with MUSICATA_DISPOSABLE_AUDIO_TEST=1')
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'packaging'))
import install as installer
from usb_autowake import SnapcastStatus

mpd_root = Path('/var/lib/musicata-mpd')
if mpd_root.exists():
    raise SystemExit('Existing MPD state must not be modified by this probe')
for port in (6600, 1704, 1705, 1780):
    with socket.socket() as check:
        check.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        check.bind(('127.0.0.1', port))


def eventually(action, timeout=15):
    deadline = time.monotonic() + timeout
    while True:
        try:
            value = action()
            if value:
                return value
        except (OSError, ValueError, KeyError):
            pass
        if time.monotonic() >= deadline:
            raise AssertionError('Timed out waiting for playback state')
        time.sleep(0.1)



with tempfile.TemporaryDirectory(prefix='musicata-provision-') as directory:
    root = Path(directory)
    processes, logs = [], []
    observer = SnapcastStatus('127.0.0.1', 1705, 'provision-fixture')
    with socket.socket() as reserve:
        reserve.bind(('127.0.0.1', 0))
        http_port = reserve.getsockname()[1]
    url = f'http://127.0.0.1:{http_port}'

    def api(path, payload=None, method=None):
        request = urllib.request.Request(url + path,
            data=json.dumps(payload).encode() if payload is not None else None,
            method=method or ('PATCH' if payload is not None else 'GET'),
            headers={'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=15) as response:
            return json.load(response)

    def launch(name, argv, env=None):
        log = (root / (name + '.log')).open('w+')
        logs.append(log)
        process = subprocess.Popen(argv, stdout=log, stderr=log, env=env)
        processes.append(process)
        return process

    server_args = [str(Path(sys.argv[1]).resolve()), '--database', str(root/'musicata.db'),
                   '--library', str(root/'library'), '--addr', f'127.0.0.1:{http_port}']
    (root/'library').mkdir()
    (root/'runtime').mkdir()
    server_env = dict(os.environ, TMPDIR=str(root/'runtime'), MUSICATA_MPD='127.0.0.1:6600',
                      MUSICATA_PUBLIC_URL=url)
    try:
        server = launch('server', server_args, server_env)
        eventually(lambda: api('/api/health'))
        assert api('/api/snapcast/status', {'enabled': True})['running']
        try:
            api('/api/snapcast/status', {'mpd_enabled': True})
            raise AssertionError('Missing MPD FIFO was accepted')
        except urllib.error.HTTPError as error:
            assert error.code == 400
        assert api('/api/snapcast/status')['mpd_enabled'] is False

        cfg = installer.options(['--mode', 'native', '--with', 'mpd,snapclient',
                                 '--alsa-device', 'plughw:CARD=Fixture,DEV=0'])
        cfg['snapclient_id'] = 'provision-fixture'
        installer.MPD_CONFIG = root/'mpd.conf'
        installer.MPD_UNIT = root/'mpd.service'
        installer.PLAYBACK_OVERRIDE = root/'playback.conf'
        installer.configure_mpd(cfg, os.getuid(), os.getgid(), {})
        assert api('/api/snapcast/status', {'mpd_enabled': True})['mpd_enabled']
        with wave.open(str(root/'library/fixture.wav'), 'wb') as audio:
            audio.setnchannels(2)
            audio.setsampwidth(2)
            audio.setframerate(48000)
            audio.writeframes(b'\0' * 48000 * 4 * 30)
        launch('mpd', ['mpd', '--no-daemon', str(installer.MPD_CONFIG)])
        launch('snapclient', ['snapclient', '--host', '127.0.0.1', '--hostID', cfg['snapclient_id'],
                              '--player', 'alsa', '--soundcard', 'null'])
        installer.verify_mpd()
        installer.verify_snapclient(cfg)
        track = eventually(lambda: api('/api/tracks?limit=1')['items'])[0]
        mpd_player = next(player for player in api('/api/players') if player['kind'] == 'mpd')
        def command(name, **kwargs):
            return api('/api/players/' + mpd_player['id'] + '/commands', dict(command=name, **kwargs), 'POST')
        eventually(lambda: api('/api/players/' + mpd_player['id'] + '/state'))
        command('play_tracks', track_ids=[track['id']])
        eventually(lambda: observer.observe() == ('MPD', 'playing') and observer.client_connected)
        for change in ({'buffer_ms': 500}, {'mpd_enabled': False}):
            try:
                api('/api/snapcast/status', change)
                raise AssertionError('A managed restart was allowed during MPD playback')
            except urllib.error.HTTPError as error:
                assert error.code == 400
        assert api('/api/snapcast/status')['applied_buffer_ms'] == 1000
        command('pause')
        eventually(lambda: observer.observe() == ('MPD', 'idle'))
        assert api('/api/snapcast/status', {'buffer_ms': 500})['applied_buffer_ms'] == 500
        installer.verify_snapclient(cfg)
        command('play')
        eventually(lambda: observer.observe() == ('MPD', 'playing'))
        command('pause')
        eventually(lambda: observer.observe() == ('MPD', 'idle'))
        assert not api('/api/snapcast/status', {'mpd_enabled': False})['mpd_enabled']
        status = api('/api/snapcast/status')
        assert 'MPD' not in [stream['id'] for stream in status['inputs']]
        assert 'Musicata' in [stream['id'] for stream in status['inputs']]
        api('/api/snapcast/status', {'mpd_enabled': True})
        server.terminate()
        server.wait(timeout=10)
        observer.close()
        launch('server-restart', server_args, server_env)
        eventually(lambda: api('/api/snapcast/status').get('running'))
        assert api('/api/snapcast/status')['mpd_enabled']
        installer.verify_snapclient(cfg)
        print('PASS: real MPD FIFO provisioning, Snapclient routing, live input toggle, buffer restart and persisted server restart; ALSA null only')
    except Exception:
        for log in logs:
            log.seek(0)
            print(log.name, log.read()[-12000:], file=sys.stderr)
        raise
    finally:
        observer.close()
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        for log in logs:
            log.close()
        shutil.rmtree(mpd_root, ignore_errors=True)
