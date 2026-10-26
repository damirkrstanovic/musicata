#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Isolated real-process health/recovery probe. No physical audio or private data.
Run in a dedicated pod: python3 tests/audio/snapcast_health.py --server target/debug/musicata-server
"""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.request
import wave


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--server', required=True)
    parser.add_argument('--observe-seconds', type=int, default=20)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='musicata-health-') as directory:
        root = Path(directory)
        library = root/'music'
        library.mkdir()
        with wave.open(str(library/'Probe.wav'), 'wb') as audio:
            audio.setnchannels(2)
            audio.setsampwidth(2)
            audio.setframerate(48000)
            audio.writeframes(b'\0\x10\0\x10' * 48000 * 10)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        base = f'http://127.0.0.1:{port}'
        cookie = ''
        children = []
        logs = []

        def launch(command, name):
            log = open(root/(name+'.log'), 'wb')
            logs.append(log)
            child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            children.append(child)
            return child

        def api(path, data=None, method=None):
            request = urllib.request.Request(base+path, data=json.dumps(data).encode() if data is not None else None,
                method=method, headers={'Content-Type':'application/json', 'Cookie':cookie})
            with urllib.request.urlopen(request, timeout=3) as response:
                body = response.read()
                return (json.loads(body) if body else None), response.headers

        def wait(check, seconds=15):
            end = time.monotonic()+seconds
            while time.monotonic() < end:
                try:
                    value = check()
                    if value:
                        return value
                except (OSError, ValueError):
                    pass
                time.sleep(.1)
            raise AssertionError('condition did not become true')

        def health():
            return api('/api/snapcast/status')[0]['health']

        def snap_pid():
            for task in (Path('/proc')/str(server.pid)/'task').iterdir():
                for pid in (task/'children').read_text().split():
                    try:
                        if (Path('/proc')/pid/'comm').read_text().strip() == 'snapserver':
                            return int(pid)
                    except FileNotFoundError:
                        pass
            return None

        def resources():
            status = (Path('/proc')/str(server.pid)/'status').read_text()
            rss = next(int(line.split()[1]) for line in status.splitlines() if line.startswith('VmRSS:'))
            return {'rss_kib':rss, 'fds':len(list((Path('/proc')/str(server.pid)/'fd').iterdir()))}

        signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(TimeoutError('probe deadline')))
        signal.alarm(240)
        try:
            server = launch([args.server, '--library', str(library), '--database', str(root/'musicata.db'),
                '--addr', f'127.0.0.1:{port}'], 'server')
            wait(lambda: api('/api/health')[0])
            _, headers = api('/api/auth/setup', {'username':'probe','password':'isolated-health-only'}, 'POST')
            cookie = headers.get('Set-Cookie', '').split(';')[0]
            with sqlite3.connect(root/'musicata.db') as db:
                db.execute('INSERT OR REPLACE INTO settings(key,value) VALUES (?,?)',
                    ('snapcast.fifo_path', str(root/'snapcast'/'audio.fifo')))
            api('/api/snapcast/status', {'enabled':True}, 'PATCH')
            wait(lambda: health()['control'] == 'connected')
            client_cmd = ['snapclient', '--host', '127.0.0.1', '--hostID', 'health-probe', '--player',
                f'file:filename={root / "audio.pcm"},mode=a', '--logsink', 'stdout']
            client = launch(client_cmd, 'client')
            wait(lambda: any(c['connected'] for c in health()['clients']))
            assert health()['physical_output'] == 'unknown'
            tracks = wait(lambda: api('/api/tracks?limit=10')[0]['items'])
            api('/api/players/snapcast-local/commands', {'command':'play_tracks','track_ids':[tracks[0]['id']],'start_index':0}, 'POST')
            wait(lambda: (root/'audio.pcm').exists() and (root/'audio.pcm').stat().st_size > 4096)
            api('/api/players/snapcast-local/commands', {'command':'pause'}, 'POST')
            queue = api('/api/players/snapcast-local/state')[0]['queue']
            before = resources()
            old = snap_pid()
            os.kill(old, signal.SIGKILL)
            wait(lambda: snap_pid() is not None and snap_pid() != old and health()['control'] == 'connected')
            assert api('/api/players/snapcast-local/state')[0]['queue'] == queue
            client.terminate()
            client.wait(timeout=5)
            wait(lambda: not any(c['connected'] for c in health()['clients']))
            client = launch(client_cmd, 'client-again')
            wait(lambda: any(c['connected'] for c in health()['clients']))
            pid = snap_pid()
            attempts = health()['attempts']
            os.kill(pid, signal.SIGSTOP)
            wait(lambda: health()['control'] == 'unavailable', 20)
            started = time.monotonic()
            for _ in range(5):
                api('/api/snapcast/status')
            assert time.monotonic()-started < 2.5, 'cached HTTP reads stalled'
            assert snap_pid() == pid and health()['attempts'] == attempts
            os.kill(pid, signal.SIGCONT)
            wait(lambda: health()['control'] == 'connected')
            # Fail repeatedly without 60 healthy seconds: budget cannot reset on flapping.
            while health()['attempts'] < 3:
                old = snap_pid()
                os.kill(old, signal.SIGKILL)
                wait(lambda: snap_pid() is not None and snap_pid() != old and health()['control'] == 'connected', 45)
            os.kill(snap_pid(), signal.SIGKILL)
            wait(lambda: health()['recovery'] == 'exhausted')
            api('/api/snapcast/health/retry', method='POST')
            wait(lambda: health()['control'] == 'connected' and health()['recovery'] == 'idle')
            samples = []
            start = time.monotonic()
            while time.monotonic()-start < args.observe_seconds:
                samples.append(resources())
                assert health()['control'] == 'connected'
                time.sleep(1)
            assert samples[-1]['fds'] <= before['fds'] + 16, samples
            print(json.dumps({'passed':True, 'observe_seconds':args.observe_seconds, 'samples':samples,
                'before':before, 'final_health':{key:health()[key] for key in ('process','control','recovery','attempts','server_version')},
                'limits':'File-output software delivery only; no physical DAC or long-term memory claim'}, indent=2))
        finally:
            signal.alarm(0)
            for child in reversed(children):
                if child.poll() is None:
                    try:
                        os.killpg(child.pid, signal.SIGCONT)
                        os.killpg(child.pid, signal.SIGTERM)
                        child.wait(timeout=5)
                    except (OSError, subprocess.TimeoutExpired):
                        try:
                            os.killpg(child.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        child.wait(timeout=5)
            for log in logs:
                log.close()


if __name__ == '__main__':
    main()
