#!/usr/bin/env python3
"""Isolated player identity/device API regression; no hardware or credentials."""
import json
import os
from pathlib import Path
import socket
import socketserver
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

class Mpd(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True
    def __init__(self):
        super().__init__(('127.0.0.1', 0), Handler)
        self.enabled = {2: True, 8: False}
        self.names = {2: 'USB DAC', 8: 'HDMI'}
        self.changed = []
        self.output_delay = 0
        threading.Thread(target=self.serve_forever, daemon=True).start()

class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        self.wfile.write(b'OK MPD 0.23.0\n')
        try:
            for raw in self.rfile:
                command = raw.decode().strip()
                reply = ''
                if command == 'idle player mixer playlist options':
                    time.sleep(0.5)
                    reply = 'changed: player\n'
                elif command == 'status':
                    reply = 'volume: 50\nstate: stop\nplaylist: 1\nplaylistlength: 0\n'
                elif command == 'outputs':
                    reply = ''.join(f'outputid: {i}\noutputname: {name}\nplugin: alsa\noutputenabled: {int(self.server.enabled[i])}\n' for i, name in self.server.names.items())
                elif command.startswith(('enableoutput ', 'disableoutput ')):
                    verb, value = command.split()
                    self.server.enabled[int(value)] = verb == 'enableoutput'
                    self.server.changed.append(command)
                    time.sleep(self.server.output_delay)
                self.wfile.write((reply+'OK\n').encode())
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass

def main():
    binary = os.environ['MUSICATA_SERVER_BIN']
    with tempfile.TemporaryDirectory() as directory, Mpd() as first, Mpd() as second:
        root = Path(directory)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
        base = f'http://127.0.0.1:{port}'
        log = (root/'server.log').open('w')
        server = subprocess.Popen([binary, '--library', 'testdata-fixture', '--database', str(root/'test.db'), '--addr', f'127.0.0.1:{port}'], stdout=log, stderr=log)
        def api(path, method='GET', body=None, status=200):
            request = urllib.request.Request(base+path, data=json.dumps(body).encode() if body is not None else None, method=method, headers={'Content-Type':'application/json'})
            try: response = urllib.request.urlopen(request, timeout=15)
            except urllib.error.HTTPError as error: response = error
            data=response.read()
            assert response.status == status, (method,path,response.status,data.decode()[:300])
            return json.loads(data) if data else None
        try:
            for _ in range(100):
                try:
                    api('/api/health'); break
                except (OSError, AssertionError): time.sleep(.1)
            else: raise AssertionError('fixture server failed to start')
            address=f'127.0.0.1:{first.server_address[1]}'
            player=api('/api/players','POST',{'kind':'mpd','address':address,'name':'Desk'})
            pid=player['id']
            zone=api('/api/zones','POST',{'name':'Office'})
            api('/api/players/'+pid,'PATCH',{'zone_id':zone['id']})
            rebound=api('/api/players/'+pid+'/connection','PUT',{'host':'127.0.0.1','port':second.server_address[1]})
            assert rebound['id']==pid and rebound['zone_id']==zone['id']
            assert rebound['address']==f'127.0.0.1:{second.server_address[1]}'
            print('PASS connection rebind preserves player and zone')
            duplicate=api('/api/players','POST',{'kind':'mpd','address':rebound['address'],'name':'Desk renamed'})
            assert duplicate['id']==pid
            print('PASS registration at rebound address reuses identity')
            other=api('/api/players','POST',{'kind':'mpd','address':address,'name':'Other player'})
            assert other['id'] != pid
            api('/api/players/'+pid+'/connection','PUT',{'host':'127.0.0.1','port':first.server_address[1]},400)
            assert next(p for p in api('/api/players') if p['id']==pid)['address']==rebound['address']
            print('PASS address collision preserves both player identities')
            view=api('/api/players/'+pid+'/outputs')
            assert len(view['outputs'])==2 and view['selection_supported']
            chosen=api('/api/players/'+pid+'/outputs','PUT',{'name':'HDMI','plugin':'alsa'})
            assert chosen['selected']=={'name':'HDMI','plugin':'alsa'}
            assert second.enabled=={2:False,8:True} and first.enabled=={2:True,8:False}
            print('PASS output selection changes only the rebound player')
            second.names={17:'USB DAC',25:'HDMI'}; second.enabled={17:False,25:True}
            chosen=api('/api/players/'+pid+'/outputs','PUT',{'name':'USB DAC','plugin':'alsa'})
            assert second.enabled=={17:True,25:False}
            print('PASS selection resolves renumbered MPD output IDs')
            api('/api/players/'+pid+'/outputs','PUT',{'name':'Missing','plugin':'alsa'},400)
            assert second.enabled=={17:True,25:False}
            print('PASS unknown output leaves current routing unchanged')
            second.output_delay = .3
            payload=json.dumps({'name':'HDMI','plugin':'alsa'}).encode()
            with socket.create_connection(('127.0.0.1',port)) as client:
                client.sendall((f'PUT /api/players/{pid}/outputs HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {len(payload)}\r\n\r\n').encode()+payload)
                deadline=time.time()+3
                while second.enabled[17] and time.time()<deadline: time.sleep(.01)
                assert not second.enabled[17], 'request did not begin device mutation'
            time.sleep(1)
            second.output_delay=0
            view=api('/api/players/'+pid+'/outputs')
            assert second.enabled=={17:False,25:True} and view['selected']['name']=='HDMI'
            print('PASS disconnected caller does not abandon apply or persistence')
            api('/api/players/'+pid+'/outputs','PUT',{'name':'USB DAC','plugin':'alsa'})
            api('/api/players/'+pid+'/dsp/processor','PUT',{'host':'127.0.0.1','port':1234})
            view=api('/api/players/'+pid+'/outputs')
            assert not view['selection_supported'] and view['reason']
            api('/api/players/'+pid+'/outputs','PUT',{'name':'HDMI','plugin':'alsa'},400)
            assert second.enabled=={17:True,25:False}
            print('PASS processor binding prevents accidental routing bypass')
        finally:
            server.terminate()
            try: server.wait(timeout=10)
            except subprocess.TimeoutExpired: server.kill();server.wait()
            log.close()
            first.shutdown(); second.shutdown()

if __name__ == '__main__': main()
