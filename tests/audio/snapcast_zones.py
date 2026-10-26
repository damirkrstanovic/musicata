#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Independent zone playback with real Snapserver and file-output Snapclients; isolated data."""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import struct
import subprocess
import tempfile
import time
import urllib.request
import wave


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--server', required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='musicata-zones-') as directory:
        root = Path(directory)
        library = root/'music'
        library.mkdir()
        for name, amplitude, duration in [('First', 5000, 3), ('Second', 6000, 90), ('Other', -6000, 90)]:
            with wave.open(str(library/(name+'.wav')), 'wb') as audio:
                audio.setnchannels(2)
                audio.setsampwidth(2)
                audio.setframerate(48000)
                audio.writeframes(struct.pack('<hh', amplitude, amplitude) * (48000*duration))
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        base = f'http://127.0.0.1:{port}'
        cookie = ''
        children, logs = [], []

        def launch(command, name):
            log = open(root/(name+'.log'), 'wb')
            logs.append(log)
            child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            children.append(child)
            return child

        def api(path, data=None, method=None):
            request = urllib.request.Request(base+path, data=json.dumps(data).encode() if data is not None else None,
                method=method, headers={'Content-Type':'application/json', 'Cookie':cookie})
            with urllib.request.urlopen(request, timeout=20) as response:
                body = response.read()
                return (json.loads(body) if body else None), response.headers

        def wait(check, seconds=25):
            end = time.monotonic()+seconds
            while time.monotonic()<end:
                try:
                    result = check()
                    if result:
                        return result
                except (OSError, ValueError):
                    pass
                time.sleep(.1)
            raise AssertionError('condition timed out: '+getattr(check, '__name__', repr(check)))

        def state(zone):
            return api('/api/zones/'+zone+'/state')[0]

        def command(zone, data):
            return api('/api/zones/'+zone+'/commands', data, 'POST')[0]

        def status():
            return api('/api/snapcast/status')[0]

        def pcm_sign(name, positive):
            path = root/(name+'.pcm')
            if not path.exists() or path.stat().st_size<8192:
                return False
            with path.open('rb') as source:
                source.seek(-4096, os.SEEK_END)
                samples = struct.unpack('<2048h', source.read(4096))
            mean = sum(samples)/len(samples)
            return mean>1000 if positive else mean < -1000

        def snap_pid():
            for task in (Path('/proc')/str(server.pid)/'task').iterdir():
                for pid in (task/'children').read_text().split():
                    try:
                        if (Path('/proc')/pid/'comm').read_text().strip() == 'snapserver':
                            return int(pid)
                    except FileNotFoundError:
                        pass

        signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(TimeoutError('probe deadline')))
        signal.alarm(240)
        try:
            server = launch([args.server, '--library', str(library), '--database', str(root/'musicata.db'), '--addr', f'127.0.0.1:{port}'], 'server')
            wait(lambda: api('/api/health')[0])
            _, headers = api('/api/auth/setup', {'username':'probe','password':'isolated-zone-test'}, 'POST')
            cookie = headers.get('Set-Cookie', '').split(';')[0]
            with sqlite3.connect(root/'musicata.db') as db:
                db.execute('INSERT OR REPLACE INTO settings(key,value) VALUES (?,?)', ('snapcast.fifo_path', str(root/'snapcast'/'audio.fifo')))
            api('/api/snapcast/status', {'enabled':True}, 'PATCH')
            wait(lambda: status()['health']['control']=='connected')
            for name in ['speaker-a', 'speaker-b']:
                launch(['snapclient','--host','127.0.0.1','--hostID',name,'--player', f'file:filename={root/(name+".pcm")},mode=a','--logsink','stdout'], name)
            wait(lambda: len([c for c in status()['clients'] if c['connected']])==2)
            a = api('/api/zones', {'name':'Zone A'}, 'POST')[0]['id']
            b = api('/api/zones', {'name':'Zone B'}, 'POST')[0]['id']
            for client, zone in [('speaker-a',a),('speaker-b',b)]:
                api('/api/snapcast/clients/'+client, {'zone_id':zone}, 'PATCH')
            wait(lambda: {c['zone_id'] for c in status()['clients']}=={a,b})
            tracks = wait(lambda: api('/api/tracks?limit=10')[0]['items'])
            ids = {t['title']:t['id'] for t in tracks}
            command(a, {'command':'play_tracks','track_ids':[ids['First'],ids['Second']],'start_index':0})
            command(b, {'command':'play_tracks','track_ids':[ids['Other']],'start_index':0})
            wait(lambda: pcm_sign('speaker-a', True))
            wait(lambda: pcm_sign('speaker-b', False))
            wait(lambda: state(a)['queue_position']==1)
            assert state(a)['now_playing']['track_id']==ids['Second']
            assert state(b)['now_playing']['track_id']==ids['Other']
            wait(lambda: state(a)['elapsed_seconds']>0 and state(b)['elapsed_seconds']>0)
            command(a, {'command':'seek','position_seconds':12})
            wait(lambda: state(a)['elapsed_seconds']>=12)
            command(a, {'command':'pause'})
            wait(lambda: state(a)['status']=='paused')
            assert state(b)['status']=='playing'
            command(a, {'command':'play'})
            wait(lambda: state(a)['status']=='playing')
            # Moving one client changes its audio destination without moving the other queue.
            api('/api/snapcast/clients/speaker-a', {'zone_id':b}, 'PATCH')
            wait(lambda: pcm_sign('speaker-a', False))
            wait(lambda: any({c['id'] for c in g['clients']}=={'speaker-a','speaker-b'} for g in status()['groups']))
            assert state(a)['queue_position']==1 and state(b)['queue_position']==0
            # A managed-process recovery must retain all independent streams and assignments.
            before_recovery_bytes = (root/'speaker-b.pcm').stat().st_size
            old = snap_pid()
            os.kill(old, signal.SIGKILL)
            wait(lambda: snap_pid() not in (None,old) and status()['health']['control']=='connected', 45)
            wait(lambda: len([s for s in status()['inputs'] if s['id'].startswith('Musicata-')])==2)
            wait(lambda: (root/'speaker-b.pcm').stat().st_size > before_recovery_bytes + 192000 and pcm_sign('speaker-b', False))
            for zone in [a,b]:
                command(zone, {'command':'pause'})
            # A full Musicata restart restores independent queues and durable membership.
            time.sleep(2)  # allow the deliberately asynchronous queue persistence to drain
            server.terminate()
            server.wait(timeout=15)
            server = launch([args.server, '--library', str(library), '--database', str(root/'musicata.db'), '--addr', f'127.0.0.1:{port}'], 'server-restarted')
            wait(lambda: api('/api/health')[0])
            wait(lambda: status()['health']['control']=='connected')
            wait(lambda: len([s for s in status()['inputs'] if s['id'].startswith('Musicata-')])==2)
            wait(lambda: all(c['zone_id']==b for c in status()['clients']) and len(status()['clients'])==2)
            assert state(a)['queue_position']==1 and state(b)['queue_position']==0
            assert state(a)['status']=='paused' and state(b)['status']=='paused'
            restarted_elapsed = state(b)['elapsed_seconds']
            command(b, {'command':'play'})
            wait(lambda: state(b)['elapsed_seconds'] > restarted_elapsed + 1)
            command(b, {'command':'pause'})
            api('/api/zones/'+b, method='DELETE')
            wait(lambda: all(c['zone_id'] is None for c in status()['clients']))
            print(json.dumps({'passed':True,'checks':['two different PCM streams','natural track advancement','zone progress and seek','independent pause/resume','speaker move and regroup','Snapserver recovery','Musicata restart preserves queues and membership','zone deletion'], 'limits':'Software file outputs only; physical DAC sound not verified'},indent=2))
        except BaseException:
            for log in logs:
                log.flush()
            for path in root.glob('*.log'):
                print(path.name, path.read_text(errors='replace')[-6000:])
            raise
        finally:
            signal.alarm(0)
            for child in reversed(children):
                if child.poll() is None:
                    os.killpg(child.pid, signal.SIGTERM)
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(child.pid,signal.SIGKILL)
                        child.wait(timeout=5)
            for log in logs:
                log.close()


if __name__ == '__main__':
    main()
