#!/usr/bin/env python3
"""Real MPD intent check with a nonexistent ALSA target; never opens real audio."""
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import wave
import json

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'packaging'))
from usb_autowake import MpdStatus, StatusConnection
from player import mpd_config


with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    (root / 'music').mkdir()
    (root / 'playlists').mkdir()
    with wave.open(str(root / 'music/fixture.wav'), 'wb') as output:
        output.setnchannels(2)
        output.setsampwidth(2)
        output.setframerate(48000)
        output.writeframes(b'\0' * 48000 * 4 * 30)
    with socket.socket() as reserve:
        reserve.bind(('127.0.0.1', 0))
        port = reserve.getsockname()[1]
    config = root / 'mpd.conf'
    use_fifo = '--fifo' in sys.argv
    config.write_text(mpd_config('hw:CARD=MusicataMissingFixture,DEV=0', root,
                      fifo=root / 'snapcast.fifo' if use_fifo else None).replace(
                          'port "6600"', f'port "{port}"'))
    snapserver = None
    control = None
    if use_fifo:
        import os
        os.mkfifo(root / 'snapcast.fifo', 0o600)
        with socket.socket() as reserve, socket.socket() as stream_reserve:
            reserve.bind(('127.0.0.1', 0))
            stream_reserve.bind(('127.0.0.1', 0))
            control_port = reserve.getsockname()[1]
            stream_port = stream_reserve.getsockname()[1]
        snapconfig = root / 'snapserver.conf'
        snapconfig.write_text(f'''[server]
datadir = {root}
[stream]
port = {stream_port}
source = pipe://{root}/snapcast.fifo?name=MPD&sampleformat=48000:16:2&mode=read
[tcp]
enabled = true
port = {control_port}
[http]
enabled = false
''')
        snapserver = subprocess.Popen(['snapserver', '--config', str(snapconfig)],
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        control = StatusConnection('127.0.0.1', control_port)

    def stream_state():
        control.begin()
        control.send(b'{"id":1,"jsonrpc":"2.0","method":"Server.GetStatus"}\n')
        while True:
            response = json.loads(control.line())
            if response.get('id') == 1:
                return next(s['status'] for s in response['result']['server']['streams'] if s['id'] == 'MPD')
    with (root / 'mpd.log').open('w+') as log:
        process = subprocess.Popen(['mpd', '--no-daemon', str(config)], stdout=log, stderr=log)
        client = MpdStatus('127.0.0.1', port)
        try:
            for attempt in range(50):
                try:
                    client.observe()
                    break
                except OSError:
                    time.sleep(0.1)
            else:
                raise RuntimeError('Test MPD did not start')
            with socket.create_connection(('127.0.0.1', port), timeout=3) as connection:
                with connection.makefile('rwb') as stream:
                    assert stream.readline().startswith(b'OK MPD ')

                    def command(text):
                        stream.write((text + '\n').encode())
                        stream.flush()
                        lines = []
                        while True:
                            line = stream.readline()
                            if line == b'OK\n':
                                return lines
                            if not line or line.startswith(b'ACK'):
                                raise RuntimeError(repr(line))
                            lines.append(line)

                    command('update')
                    for _ in range(50):
                        if not any(line.startswith(b'updating_db:') for line in command('status')):
                            break
                        time.sleep(0.1)
                    command('add "fixture.wav"')
                    command('play')
                    states = []
                    for _ in range(5):
                        time.sleep(1)
                        states.append(client.observe())
                        if use_fifo:
                            assert stream_state() == 'playing', 'Snapcast did not see MPD playback'
                    if use_fifo:
                        assert states == ['playing'] * 5, states
                    else:
                        assert 'idle' in states, 'Recheck whether this MPD supports absent-output intent'
                    command('pause 1')
                    assert client.observe() == 'idle'
                    if use_fifo:
                        for _ in range(50):
                            if stream_state() == 'idle':
                                break
                            time.sleep(0.1)
                        else:
                            raise AssertionError('Snapcast did not become idle after MPD pause')
                    command('stop')
                    assert client.observe() == 'idle'
            print('PASS: MPD → Snapcast preserves playback without a DAC; pause becomes idle'
                  if use_fifo else 'PASS: reproduced lost MPD intent with absent ALSA; direct polling must not control USB')
        except Exception:
            log.seek(0)
            print(log.read(), file=sys.stderr)
            raise
        finally:
            client.close()
            if control:
                control.close()
            if snapserver:
                snapserver.terminate()
                snapserver.wait(timeout=5)
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
