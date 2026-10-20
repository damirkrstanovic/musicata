#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Optional, isolated Linux integration: real Snapserver + two file-output clients.
Run in the development cluster, with snapserver/snapclient on PATH. No audio hardware
or existing installation is used. All children are time-limited and cleaned up.
"""
import argparse
import json
import math
import os
from pathlib import Path
import signal
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import wave


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--server', required=True, help='freshly built Musicata server binary')
    parser.add_argument('--load-workers', type=int, default=0, choices=range(5),
                        help='isolated CPU load workers (0–4)')
    parser.add_argument('--continuity-seconds', type=int, default=4, choices=range(4, 61),
                        help='capture duration for each continuity check (4–60)')
    args = parser.parse_args()
    deadline_seconds = 70 + 3 * args.continuity_seconds
    root = Path(tempfile.mkdtemp(prefix='musicata-snapcast-probe-'))
    library = root/'music'
    library.mkdir()
    for name, level in [('First', 10000), ('Second', -10000)]:
        with wave.open(str(library/(name+'.wav')), 'wb') as audio:
            audio.setnchannels(2)
            audio.setsampwidth(2)
            audio.setframerate(48000)
            audio.writeframes(struct.pack('<hh', level, level) * (48000 * 20))
    for rate, name in [(44100, 'Resampled'), (48000, 'Compressed')]:
        source = library/(name+'.wav')
        with wave.open(str(source), 'wb') as audio:
            audio.setnchannels(2)
            audio.setsampwidth(2)
            audio.setframerate(rate)
            audio.writeframes(b''.join(struct.pack('<hh', sample, sample) for sample in
                              (int(10000*math.sin(2*math.pi*440*i/rate))
                               for i in range(rate*(args.continuity_seconds+12)))))
        if name == 'Compressed':
            subprocess.run(['ffmpeg', '-v', 'error', '-i', str(source), str(source.with_suffix('.mp3'))], check=True, timeout=15)
            source.unlink()
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    # Snapcast uses its standard stream/control ports, exclusively inside this test pod.
    for snap_port in (1704, 1705, 1780):
        with socket.socket() as sock:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind(('0.0.0.0', snap_port))
    base = f'http://127.0.0.1:{port}'
    cookie = ''
    children, logs = [], []

    def launch(command, name):
        log = open(root/(name+'.log'), 'wb')
        logs.append(log)
        child = subprocess.Popen(['timeout', '--signal=TERM', '--kill-after=3s',
                                  f'{deadline_seconds+5}s', *command],
                                 stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        children.append(child)
        return child

    def api(path, data=None, method=None):
        body = json.dumps(data).encode() if data is not None else None
        request = urllib.request.Request(base+path, data=body, method=method,
                                         headers={'Content-Type': 'application/json', 'Cookie': cookie})
        with urllib.request.urlopen(request, timeout=12) as response:
            payload = response.read()
            return json.loads(payload) if payload else None, response.headers

    def wait_for(check, seconds=8):
        end = time.monotonic()+seconds
        while time.monotonic() < end:
            try:
                value = check()
                if value:
                    return value
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(.01)
        raise AssertionError('timed out waiting for integration condition')

    def command(value):
        started = time.monotonic()
        api('/api/players/snapcast-local/commands', value, 'POST')
        return time.monotonic()-started

    def pcm_start(path, offset, positive):
        if not path.exists():
            return None
        with path.open('rb') as audio:
            audio.seek(offset)
            samples = audio.read()
        for index in range(0, len(samples)-3, 4):
            value = struct.unpack_from('<h', samples, index)[0]
            if (value > 1000) if positive else (value < -1000):
                return offset+index+4
        return None

    signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(TimeoutError('probe exceeded deadline')))
    signal.alarm(deadline_seconds)
    try:
        for index in range(args.load_workers):
            launch([sys.executable, '-c', 'while True: pass'], f'cpu-load-{index}')
        launch([args.server, '--library', str(library), '--database', str(root/'musicata.db'),
                '--addr', f'127.0.0.1:{port}'], 'musicata')
        wait_for(lambda: api('/api/health')[0])
        _, headers = api('/api/auth/setup', {'username': 'probe', 'password': 'isolated-probe-only'}, 'POST')
        cookie = headers.get('Set-Cookie', '').split(';')[0]
        tracks = wait_for(lambda: (lambda x: x if len(x) == 4 else None)(api('/api/tracks?limit=10')[0]['items']))
        by_title = {track['title']: track['id'] for track in tracks}
        ids = [by_title[name] for name in ('First', 'Second')]
        # Isolated fixture bootstrap only; product users use Settings.
        with sqlite3.connect(root/'musicata.db') as db:
            db.execute('INSERT OR REPLACE INTO settings(key,value) VALUES (?,?)',
                       ('snapcast.fifo_path', str(root/'snapcast'/'musicata.fifo')))
        api('/api/snapcast/status', {'enabled': True}, 'PATCH')
        captures = [root/'a.pcm', root/'b.pcm']
        for name, capture in zip(('probe-a', 'probe-b'), captures):
            launch(['snapclient', '--host', '127.0.0.1', '--port', '1704', '--hostID', name,
                    '--player', f'file:filename={capture},mode=a', '--logsink', 'stdout'], name)
        wait_for(lambda: len([c for c in api('/api/snapcast/status')[0]['clients'] if c['connected']]) == 2)
        for client in api('/api/snapcast/status')[0]['clients']:
            api(f"/api/snapcast/clients/{client['id']}/volume", {'percent': 80}, 'POST')
        measurements = []
        for buffer_ms in (1000, 250):
            status, _ = api('/api/snapcast/status', {'buffer_ms': buffer_ms}, 'PATCH')
            assert status['applied_buffer_ms'] == buffer_ms, status
            wait_for(lambda: len([c for c in api('/api/snapcast/status')[0]['clients'] if c['connected']]) == 2)
            assert all(c['volume_percent'] == 80 for c in api('/api/snapcast/status')[0]['clients'])
            offsets = [p.stat().st_size if p.exists() else 0 for p in captures]
            play_ack = command({'command': 'play_tracks', 'track_ids': ids, 'start_index': 0})
            starts = [wait_for(lambda p=p, offset=offset: pcm_start(p, offset, True)) for p, offset in zip(captures, offsets)]
            # Give the bounded preloader time to prepare the second track.
            time.sleep(.3)
            offsets = [p.stat().st_size for p in captures]
            started = time.monotonic()
            next_ack = command({'command': 'next'})
            transitions, observed = [], []
            for path, offset in zip(captures, offsets):
                transitions.append(wait_for(lambda p=path, offset=offset: pcm_start(p, offset, False)))
                observed.append(time.monotonic()-started)
            frame_counts = [(end-start)//4 for start, end in zip(starts, transitions)]
            assert abs(frame_counts[0]-frame_counts[1]) <= 48, frame_counts
            assert play_ack < .5 and next_ack < .5, (play_ack, next_ack)
            measurements.append({'buffer_ms': buffer_ms, 'play_ack_ms': round(play_ack*1000, 1),
                                 'next_ack_ms': round(next_ack*1000, 1),
                                 'file_output_transition_ms': [round(v*1000, 1) for v in observed],
                                 'client_transition_difference_frames': abs(frame_counts[0]-frame_counts[1])})
        assert measurements[1]['file_output_transition_ms'][0] < measurements[0]['file_output_transition_ms'][0] - 300, measurements
        continuity = []
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            radio_port = sock.getsockname()[1]
        launch([sys.executable, '-m', 'http.server', str(radio_port), '--bind', '127.0.0.1', '--directory', str(library)], 'radio')
        time.sleep(.2)
        station, _ = api('/api/radio', {'name': 'Probe radio', 'stream_url': f'http://127.0.0.1:{radio_port}/Compressed.mp3'}, 'POST')
        for name in ('Compressed', 'Resampled', 'Radio'):
            if name == 'Radio':
                command({'command': 'play_stream', 'url': f"/api/radio/{station['id']}/stream", 'title': 'Probe radio'})
            else:
                command({'command': 'play_tracks', 'track_ids': [by_title[name]], 'start_index': 0})
            time.sleep(1.5) # Exclude the transport transition and decoder startup.
            offsets = [p.stat().st_size for p in captures]
            started = time.monotonic()
            time.sleep(args.continuity_seconds)
            snapshots = []
            for path, offset in zip(captures, offsets):
                with path.open('rb') as audio:
                    audio.seek(offset)
                    pcm = audio.read()
                snapshots.append((path, pcm, time.monotonic()-started))
            # Capture both clients before analyzing either: a long PCM analysis must
            # not extend the second client's measurement window.
            for path, pcm, duration in snapshots:
                frames = len(pcm)//4
                zeros = sum(left == 0 and right == 0 for left, right in struct.iter_unpack('<hh', pcm))
                run = longest = 0
                for left, right in struct.iter_unpack('<hh', pcm):
                    run = run + 1 if left == 0 and right == 0 else 0
                    longest = max(longest, run)
                item = {'source': name, 'client': path.name, 'audio_seconds': round(frames/48000, 3),
                        'wall_seconds': round(duration, 3), 'zero_fraction': round(zeros/max(1, frames), 4),
                        'longest_silence_ms': round(longest/48, 3)}
                continuity.append(item)
                assert abs(frames/48000-duration) < .15, item
                assert zeros/max(1, frames) < .02, item
                assert longest <= 48, item  # waveform zero crossings, never a silent 20 ms chunk
        command({'command': 'stop'})
        results = {'artifacts': str(root), 'load_workers': args.load_workers,
                   'measurements': measurements, 'continuity': continuity}
        (root/'results.json').write_text(json.dumps(results, indent=2)+'\n')
        print(json.dumps(results, indent=2))
    finally:
        signal.alarm(0)
        for child in reversed(children):
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=4)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
        for log in logs:
            log.close()
        print('Probe artifacts:', root)


if __name__ == '__main__':
    main()
