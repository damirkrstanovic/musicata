#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Real Caddy proxy smoke test; uses a local test CA, never public ACME issuance."""
import argparse
import base64
import hashlib
import http.client
import http.server
import importlib.util
import os
from pathlib import Path
import socket
import ssl
import subprocess
import tempfile
import threading
import time

DOMAIN = 'music.example.org'
BODY = b'proxied audio bytes'


class Backend(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass

    def do_GET(self):
        if self.path == '/ws':
            key = self.headers['Sec-WebSocket-Key']
            accept = base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
            self.send_response(101)
            self.send_header('Upgrade','websocket')
            self.send_header('Connection','Upgrade')
            self.send_header('Sec-WebSocket-Accept',accept)
            self.end_headers()
            head = self.rfile.read(2)
            assert head == b'\x82\x84'
            mask = self.rfile.read(4)
            data = self.rfile.read(4)
            self.wfile.write(b'\x82\x04'+bytes(value^mask[i%4] for i,value in enumerate(data)))
            self.wfile.flush()
            return
        self.send_response(200)
        self.send_header('Content-Length',str(len(BODY)))
        self.send_header('Set-Cookie','musicata_session=fixture; HttpOnly; SameSite=Lax; Path=/')
        self.send_header('Set-Cookie','unrelated=fixture; Path=/')
        self.end_headers()
        self.wfile.write(BODY)


def check_proxy(connect, http_port, tls_port, ca):
    connection = http.client.HTTPConnection(connect,http_port,timeout=3)
    connection.request('GET','/',headers={'Host':DOMAIN})
    response = connection.getresponse()
    assert response.status == 308, response.status
    assert response.getheader('Location').startswith('https://'+DOMAIN)
    connection.close()
    context = ssl.create_default_context(cafile=str(ca))
    with socket.create_connection((connect,tls_port),timeout=3) as raw:
        with context.wrap_socket(raw,server_hostname=DOMAIN) as tls:
            connection = http.client.HTTPSConnection(DOMAIN,timeout=3)
            connection.sock = tls
            connection.request('GET','/stream')
            response = connection.getresponse()
            assert response.status == 200
            assert response.read() == BODY
            cookies = [v for k,v in response.getheaders() if k.lower()=='set-cookie']
            assert any(v.startswith('musicata_session=') and v.endswith('; Secure') for v in cookies),cookies
            assert 'unrelated=fixture; Path=/' in cookies,cookies
    with socket.create_connection((connect,tls_port),timeout=3) as raw:
        with context.wrap_socket(raw,server_hostname=DOMAIN) as tls:
            key = base64.b64encode(os.urandom(16)).decode()
            tls.sendall(f'GET /ws HTTP/1.1\r\nHost: {DOMAIN}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n'.encode())
            headers = b''
            while not headers.endswith(b'\r\n\r\n'):
                byte = tls.recv(1)
                assert byte,'WebSocket upgrade closed early'
                headers += byte
            assert headers.startswith(b'HTTP/1.1 101'),headers
            mask = b'abcd'
            tls.sendall(b'\x82\x84'+mask+bytes(v^mask[i%4] for i,v in enumerate(b'test')))
            data = b''
            while len(data)<6:
                chunk = tls.recv(6-len(data))
                assert chunk,'WebSocket tunnel closed early'
                data += chunk
            assert data == b'\x82\x04test',data
    print('HTTPS trust, redirect, media, Secure session cookie and WebSocket tunnel passed',flush=True)


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0))
        return sock.getsockname()[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--caddy',default=os.environ.get('CADDY_BIN','caddy'))
    parser.add_argument('--cloudflare',action='store_true',help='Validate DNS-01 configuration with a dummy token before the local-CA smoke test')
    parser.add_argument('--backend-only',action='store_true')
    parser.add_argument('--backend-port',type=int,default=3030)
    parser.add_argument('--check-only',action='store_true')
    parser.add_argument('--connect',default='127.0.0.1')
    parser.add_argument('--http-port',type=int,default=80)
    parser.add_argument('--tls-port',type=int,default=443)
    parser.add_argument('--ca',type=Path)
    args = parser.parse_args()
    if args.backend_only:
        http.server.ThreadingHTTPServer(('127.0.0.1',args.backend_port),Backend).serve_forever()
        return
    if args.check_only:
        check_proxy(args.connect,args.http_port,args.tls_port,args.ca)
        return
    spec = importlib.util.spec_from_file_location('installer',Path(__file__).resolve().parents[2]/'packaging/install.py')
    installer = importlib.util.module_from_spec(spec);spec.loader.exec_module(installer)
    backend = http.server.ThreadingHTTPServer(('127.0.0.1',0),Backend)
    thread = threading.Thread(target=backend.serve_forever,daemon=True);thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix='musicata-caddy-smoke-') as temporary:
            root = Path(temporary);root.chmod(0o755)
            uid,gid = (10001,10001) if os.geteuid()==0 else (os.getuid(),os.getgid())
            for path in (root/'data',root/'config'):
                path.mkdir(mode=0o750)
                if os.geteuid()==0: os.chown(path,uid,gid)
            cfg = installer.options(['--with','caddy','--tls-domain',DOMAIN,'--port',str(backend.server_port)])
            if args.cloudflare:
                cfg.update(tls_dns='cloudflare')
                dns_config=root/'DNS-Caddyfile';dns_config.write_text(installer.caddy_text(cfg))
                modules=subprocess.check_output([args.caddy,'list-modules'],text=True)
                assert 'dns.providers.cloudflare' in modules
                subprocess.run([args.caddy,'validate','--config',str(dns_config),'--adapter','caddyfile'],env=dict(os.environ,CLOUDFLARE_API_TOKEN='a'*40),check=True)
            http_port,tls_port = free_port(),free_port()
            # Test-only overrides select isolated ports and Caddy's local CA.
            test_cfg=dict(cfg,tls_dns='public')  # DNS config validated above; issuance uses only a test CA.
            text = installer.caddy_text(test_cfg).replace('admin off',f'admin off\n    http_port {http_port}\n    https_port {tls_port}').replace(DOMAIN+' {',DOMAIN+' {\n    tls internal')
            config = root/'Caddyfile';config.write_text(text);config.chmod(0o644)
            env = dict(os.environ,XDG_DATA_HOME=str(root/'data'),XDG_CONFIG_HOME=str(root/'config'))
            ca = root/'data/caddy/pki/authorities/local/root.crt'
            certificate = None
            for attempt in range(2):
                with (root/'caddy.log').open('a') as log:
                    process = subprocess.Popen([args.caddy,'run','--config',str(config),'--adapter','caddyfile'],env=env,stdout=log,stderr=log,user=uid,group=gid)
                    try:
                        end = time.monotonic()+15
                        while True:
                            try:
                                check_proxy('127.0.0.1',http_port,tls_port,ca)
                                break
                            except (OSError,FileNotFoundError):
                                if process.poll() is not None or time.monotonic()>end:
                                    raise AssertionError((root/'caddy.log').read_text())
                                time.sleep(.1)
                        if certificate is None: certificate = ca.read_bytes()
                        else: assert ca.read_bytes()==certificate,'Caddy CA did not survive restart'
                    finally:
                        process.terminate();process.wait(timeout=5)
            print('Non-root native Caddy and certificate persistence passed')
    finally:
        backend.shutdown();backend.server_close();thread.join()


if __name__=='__main__': main()
