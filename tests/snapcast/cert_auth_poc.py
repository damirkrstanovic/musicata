#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Throwaway certificate-auth experiment; not a production gateway.

Run only in an isolated test environment with Snapcast 0.35, openssl and websockets 15.0.1.
Creates temporary CAs/keys and loopback-only listeners; renders synthetic audio to files.
All subprocesses, listeners and private credentials are removed on exit.
"""
import argparse
import asyncio
import contextlib
import json
import math
import os
from pathlib import Path
import socket
import ssl
import struct
import subprocess
import tempfile
import time

from websockets.asyncio.client import connect
from websockets.asyncio.server import serve


def run(*args):
    return subprocess.check_output(list(map(str, args)), stderr=subprocess.PIPE, text=True).strip()


def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


class PKI:
    def __init__(self, root):
        self.root = root
        self.serial = 10
        for name in ('server-ca', 'client-ca', 'foreign-ca'):
            key, cert = self.paths(name)
            run('openssl', 'req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256',
                '-nodes', '-keyout', key, '-out', cert, '-days', '2', '-subj', '/CN='+name,
                '-addext', 'basicConstraints=critical,CA:TRUE',
                '-addext', 'keyUsage=critical,keyCertSign,cRLSign')
        self.issue('server', 'server-ca', server=True)

    def paths(self, name):
        return self.root/(name+'.key'), self.root/(name+'.crt')

    def issue(self, name, issuer='client-ca', player='player-a', server=False, days=1):
        key, cert = self.paths(name)
        csr = self.root/(name+'.csr')
        # Device-side key/CSR in this fixture; only the CSR is an issuer input.
        run('openssl', 'req', '-new', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256',
            '-nodes', '-keyout', key, '-out', csr, '-subj', '/CN='+player)
        extension = self.root/(name+'.ext')
        extension.write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n'
                             + ('extendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n' if server else
                                'extendedKeyUsage=clientAuth\nsubjectAltName=URI:urn:musicata:player:'+player+'\n'))
        ca_key, ca_cert = self.paths(issuer)
        self.serial += 1
        run('openssl', 'x509', '-req', '-in', csr, '-CA', ca_cert, '-CAkey', ca_key,
            '-set_serial', str(self.serial), '-days', str(days), '-extfile', extension, '-out', cert)
        return self.serial

    def client(self, name=None, trust='server-ca'):
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.load_verify_locations(self.paths(trust)[1])
        if name:
            key, cert = self.paths(name)
            ctx.load_cert_chain(cert, key)
        return ctx

    def server(self):
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.minimum_version = ssl.TLSVersion.TLSv1_2
        ctx.verify_mode = ssl.CERT_REQUIRED
        ctx.load_verify_locations(self.paths('client-ca')[1])  # Never system/public trust roots.
        key, cert = self.paths('server')
        ctx.load_cert_chain(cert, key)
        return ctx


class Gate:
    """Proof of feasibility only: streaming admission, identity binding and revocation."""
    def __init__(self, backend):
        self.backend = backend
        self.allowed = {}  # certificate serial -> immutable player id; DB in production
        self.sessions = {}  # serial -> live connections
        self.received_audio = {}

    def authorize(self, connection, request):
        if request.path != '/stream':
            return connection.respond(403, 'Only audio streaming is permitted\n')
        cert = connection.transport.get_extra_info('ssl_object').getpeercert()
        serial = int(cert['serialNumber'], 16)
        player = self.allowed.get(serial)
        if not player or ('URI', 'urn:musicata:player:'+player) not in cert.get('subjectAltName', ()):
            return connection.respond(403, 'Player access revoked or unknown\n')

    async def handle(self, ws):
        cert = ws.transport.get_extra_info('ssl_object').getpeercert()
        serial = int(cert['serialNumber'], 16)
        player = self.allowed.get(serial)
        if not player:  # Recheck after asynchronous handshake.
            await ws.close(1008, 'Revoked')
            return
        self.sessions.setdefault(serial, set()).add(ws)
        try:
            first = await asyncio.wait_for(ws.recv(), 3)
            if not isinstance(first, bytes) or len(first) < 30 or struct.unpack_from('<H', first)[0] != 5:
                await ws.close(1008, 'Expected Hello')
                return
            size = struct.unpack_from('<I', first, 22)[0]
            length = struct.unpack_from('<I', first, 26)[0]
            if size != len(first)-26 or length != len(first)-30:
                await ws.close(1008, 'Invalid Hello')
                return
            hello = json.loads(first[30:])
            if hello.get('ID') != player or hello.get('Instance', 1) != 1:
                await ws.close(1008, 'Certificate does not authorize this player identity')
                return
            async with connect(self.backend, proxy=None, compression=None, max_size=1048576) as upstream:
                await upstream.send(first)
                async def client_to_server():
                    async for data in ws:
                        # Only time sync and this player's info; never a second Hello or RPC.
                        if not isinstance(data, bytes) or len(data)<26 or struct.unpack_from('<H', data)[0] not in (4, 7):
                            await ws.close(1008, 'Invalid player message')
                            return
                        await upstream.send(data)
                async def server_to_client():
                    async for data in upstream:
                        if isinstance(data, bytes) and len(data)>=26 and struct.unpack_from('<H', data)[0]==2:
                            self.received_audio[player] = self.received_audio.get(player, 0)+len(data)
                        await ws.send(data)
                tasks = [asyncio.create_task(client_to_server()), asyncio.create_task(server_to_client())]
                try:
                    await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
                finally:
                    for task in tasks: task.cancel()
                    await asyncio.gather(*tasks, return_exceptions=True)
        except Exception:
            # Test assertions inspect delivery/rejection; avoid logging credential material.
            pass
        finally:
            self.sessions[serial].discard(ws)

    async def revoke(self, serial):
        self.allowed.pop(serial, None)
        await asyncio.gather(*(ws.close(1008, 'Revoked') for ws in list(self.sessions.get(serial, ()))))


async def main(args):
    checks = []
    def check(name, ok, **details):
        checks.append({'name':name, 'passed':bool(ok), **details})
        print(('PASS ' if ok else 'FAIL ')+name, flush=True)
        if not ok: raise AssertionError(name)

    os.umask(0o077)
    with tempfile.TemporaryDirectory(prefix='musicata-cert-poc-') as directory:
        root=Path(directory); pki=PKI(root)
        a=pki.issue('a'); b=pki.issue('b',player='player-b')
        pki.issue('foreign','foreign-ca'); pki.issue('expired',days=0)
        pki.issue('wrong-purpose',server=True)
        pki.issue('unknown')
        empty_roots=root/'empty-roots';empty_roots.mkdir()
        native_port, http_port, gate_port = port(), port(), port()
        fifo=root/'audio.fifo'; os.mkfifo(fifo)
        config=root/'snapserver.conf'
        config.write_text(f'''[server]
mdns_enabled = false
datadir = {root}/state
[stream]
source = pipe://{fifo}?name=fixture&sampleformat=48000:16:2&codec=pcm
buffer = 100
[tcp-control]
enabled = false
[tcp-streaming]
enabled = false
[http]
enabled = true
bind_to_address = 127.0.0.1
port = {http_port}
ssl_enabled = true
ssl_bind_to_address = 127.0.0.1
ssl_port = {native_port}
[ssl]
certificate = {pki.paths('server')[1]}
certificate_key = {pki.paths('server')[0]}
verify_clients = true
client_cert = {pki.paths('client-ca')[1]}
''')
        processes=[]; log_handles=[]
        def spawn(argv,name,env=None):
            log=(root/(name+'.log')).open('wb');log_handles.append(log)
            process=subprocess.Popen(list(map(str,argv)),stdout=log,stderr=log,env=env)
            processes.append(process);return process
        spawn([args.snapserver,'-c',config],'server')
        fd=os.open(fifo,os.O_RDWR|os.O_NONBLOCK)
        samples=b''.join(struct.pack('<hh',int(1000*math.sin(2*math.pi*i/48)),int(1000*math.sin(2*math.pi*i/48))) for i in range(960))
        async def feed():
            while True:
                with contextlib.suppress(BlockingIOError):os.write(fd,samples)
                await asyncio.sleep(.02)
        feeder=asyncio.create_task(feed())
        async def wait_for(predicate, seconds=8):
            deadline=time.monotonic()+seconds
            while time.monotonic()<deadline:
                if predicate():return True
                await asyncio.sleep(.1)
            return False
        async def probe(context,path='/jsonrpc',target=native_port):
            async with connect(f'wss://localhost:{target}{path}',ssl=context,proxy=None,open_timeout=2) as ws:
                if path=='/jsonrpc':
                    await ws.send(json.dumps({'jsonrpc':'2.0','id':1,'method':'Server.GetStatus'}))
                    result = json.loads(await asyncio.wait_for(ws.recv(),2))
                    assert result.get('id') == 1 and 'result' in result, result
                    return result
                return True
        async def denied(context,path='/jsonrpc',target=native_port):
            try:await probe(context,path,target);return False
            except Exception:return True
        def client(name,identity,target,credential='a',trust='server-ca',default_trust=None):
            output=root/(name+'.pcm')
            argv=[args.snapclient,'--hostID',identity,'--player','file:filename='+str(output),
                  '--server-cert='+str(pki.paths(trust)[1]),'--cert',pki.paths(credential)[1],
                  '--cert-key',pki.paths(credential)[0],f'wss://127.0.0.1:{target}']
            env = None if default_trust is None else {**os.environ,
                'SSL_CERT_FILE': str(pki.paths(default_trust)[1]), 'SSL_CERT_DIR': str(empty_roots)}
            return spawn(argv,name,env),output
        try:
            ready=False
            for _ in range(40):
                try:await probe(pki.client('a'));ready=True;break
                except Exception:await asyncio.sleep(.1)
            check('native trusted certificate reaches RPC',ready)
            check('native missing certificate rejected',await denied(pki.client()))
            check('native foreign CA rejected',await denied(pki.client('foreign')))
            check('native expired certificate rejected',await denied(pki.client('expired')))
            check('native wrong certificate purpose rejected',await denied(pki.client('wrong-purpose')))
            check('client rejects unrelated server CA',await denied(pki.client('a','foreign-ca')))
            # These are observed limitations, not the desired production policy.
            proc,out=client('native-spoof','someone-else',native_port)
            check('native certificate can claim unrelated hostID (gap)',await wait_for(lambda:out.exists() and out.stat().st_size>4096))
            check('native client accepts server SAN mismatch (gap)',
                  out.stat().st_size>4096 and ssl._ssl._test_decode_cert(str(pki.paths('server')[1]))['subjectAltName']==(('DNS','localhost'),), connection_host='127.0.0.1', certificate_SAN='DNS:localhost')
            proc.terminate();proc.wait(timeout=5)
            fallback,output=client('default-roots','fallback',native_port,trust='foreign-ca',default_trust='server-ca')
            check('stock client trusts default root in addition to explicit CA (gap)',await wait_for(lambda:output.exists() and output.stat().st_size>4096))
            fallback.terminate();fallback.wait(timeout=5)
            rejected,output=client('restricted-roots','rejected',native_port,trust='foreign-ca',default_trust='foreign-ca')
            await asyncio.sleep(2)
            check('stock client rejects server with restricted unrelated trust',
                  (not output.exists() or output.stat().st_size==0) and 'certificate verify failed' in (root/'restricted-roots.log').read_text())
            rejected.terminate();rejected.wait(timeout=5)
            pinned,output=client('pinned-root','pinned',native_port,default_trust='server-ca')
            check('stock client works with restricted instance CA trust',await wait_for(lambda:output.exists() and output.stat().st_size>4096))
            pinned.terminate();pinned.wait(timeout=5)
            gate=Gate(f'ws://127.0.0.1:{http_port}/stream');gate.allowed={a:'player-a',b:'player-b'}
            async with serve(gate.handle,'127.0.0.1',gate_port,ssl=pki.server(),process_request=gate.authorize,
                             compression=None,max_size=65536,open_timeout=3,close_timeout=1):
                check('gateway denies player certificate on RPC',await denied(pki.client('a'),target=gate_port))
                check('gateway missing certificate rejected',await denied(pki.client(),'/stream',gate_port))
                check('gateway foreign CA rejected',await denied(pki.client('foreign'),'/stream',gate_port))
                check('gateway expired certificate rejected',await denied(pki.client('expired'),'/stream',gate_port))
                check('gateway wrong certificate purpose rejected',await denied(pki.client('wrong-purpose'),'/stream',gate_port))
                check('gateway unknown certificate from trusted CA rejected',await denied(pki.client('unknown'),'/stream',gate_port))
                spoof,output=client('gateway-spoof','someone-else',gate_port)
                await asyncio.sleep(2)
                check('gateway binds certificate to player ID',not output.exists() or output.stat().st_size==0)
                spoof.terminate();spoof.wait(timeout=5)
                pa,oa=client('gateway-a','player-a',gate_port)
                pb,ob=client('gateway-b','player-b',gate_port,'b')
                check('stock Snapclient A renders through gateway',await wait_for(lambda:oa.exists() and oa.stat().st_size>4096))
                check('stock Snapclient B renders independently',await wait_for(lambda:ob.exists() and ob.stat().st_size>4096))
                renewed=pki.issue('renewed',player='player-a');gate.allowed[renewed]='player-a'
                check('renewal admits replacement before retiring old certificate',await probe(pki.client('renewed'),'/stream',gate_port))
                start=time.monotonic();await gate.revoke(a)
                check('revocation closes existing player session',await wait_for(lambda:not gate.sessions.get(a), 1),elapsed_ms=round((time.monotonic()-start)*1000,1))
                check('revoked certificate cannot reconnect',await denied(pki.client('a'),'/stream',gate_port))
                old=gate.received_audio.get('player-b',0);await asyncio.sleep(.5)
                check('revoking A does not interrupt B',gate.received_audio.get('player-b',0)>old)
                pr,orr=client('gateway-renewed','player-a',gate_port,'renewed')
                check('renewed stock client renders after old credential revoked',await wait_for(lambda:orr.exists() and orr.stat().st_size>4096))
                # A normal player certificate cannot be used to inject JSON-RPC into /stream.
                async with connect(f'wss://localhost:{gate_port}/stream',ssl=pki.client('b'),proxy=None) as ws:
                    await ws.send(json.dumps({'jsonrpc':'2.0','id':1,'method':'Server.GetStatus'}))
                    try:await ws.recv();closed=False
                    except Exception:closed=True
                    check('gateway rejects RPC disguised as streaming',closed)
        except Exception:
            for name in ('server','native-spoof','gateway-a','gateway-b'):
                path=root/(name+'.log')
                if path.exists():print(name+': '+path.read_text(errors='replace')[-2200:],flush=True)
            raise
        finally:
            feeder.cancel();await asyncio.gather(feeder,return_exceptions=True);os.close(fd)
            for process in processes:
                if process.poll() is None:process.terminate()
            for process in processes:
                try:process.wait(timeout=5)
                except subprocess.TimeoutExpired:process.kill();process.wait()
            for handle in log_handles:handle.close()
            Path(args.report).write_text(json.dumps({'snapserver':run(args.snapserver,'--version').splitlines()[0],
                'snapclient':run(args.snapclient,'--version').splitlines()[0], 'checks':checks,
                'limitations':['Synthetic file audio only; no physical DAC or synchronization measurement.',
                               'Enrollment HTTP API, bootstrap trust, durable registry and automatic renewal scheduling are not implemented.',
                               'Experimental Python gateway; production implementation must be Rust and fully harden protocol parsing.']},indent=2)+'\n')


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--snapserver',required=True);parser.add_argument('--snapclient',required=True)
    parser.add_argument('--report',required=True)
    asyncio.run(main(parser.parse_args()))
