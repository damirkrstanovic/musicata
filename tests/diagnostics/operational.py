#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Isolated real-server diagnostics injection; run after a fresh server build."""
import io
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.request
import zipfile

binary = os.environ['MUSICATA_SERVER_BIN']
fixture = Path('testdata-fixture').resolve()
with tempfile.TemporaryDirectory(prefix='musicata-diagnostics-') as directory:
    root = Path(directory)
    database = root/'musicata.db'
    with socket.socket() as reservation:
        reservation.bind(('127.0.0.1', 0))
        port = reservation.getsockname()[1]
    base = f'http://127.0.0.1:{port}'
    cookie = ''
    process = None
    log = (root/'bootstrap.log').open('wb')

    def request(path, body=None, auth=None):
        headers = {'authorization':f'Bearer {auth}'} if auth else {'cookie': cookie}
        if body is not None:
            headers['content-type'] = 'application/json'
        req = urllib.request.Request(base+path, data=None if body is None else json.dumps(body).encode(), headers=headers)
        return urllib.request.urlopen(req, timeout=5)

    def api(path, body=None):
        with request(path, body) as response:
            return json.load(response)

    def wait(predicate, seconds=12):
        deadline = time.monotonic()+seconds
        while time.monotonic() < deadline:
            try:
                if predicate():
                    return
            except (OSError, ValueError):
                pass
            time.sleep(.05)
        raise AssertionError('condition timed out')

    def start():
        return subprocess.Popen([binary, '--library', str(fixture), '--database', str(database), '--addr', f'127.0.0.1:{port}'], stdout=log, stderr=log)

    def export():
        api('/api/diagnostics/export', {})
        wait(lambda: api('/api/diagnostics')['export']['ready'])
        with request('/api/diagnostics/export/download') as response:
            payload = response.read(16*1024*1024+1)
        assert len(payload) <= 16*1024*1024
        archive = zipfile.ZipFile(io.BytesIO(payload))
        assert len(archive.namelist()) == 5
        result = {name:json.loads(archive.read(name)) for name in archive.namelist()}
        text = json.dumps(result)
        assert 'private-secret' not in text and '/home/private' not in text
        return result

    try:
        process = start()
        wait(lambda: api('/api/health'))
        with request('/api/auth/setup', {'username':'test','password':'diagnostics-test-only'}) as response:
            cookie = response.headers['set-cookie'].split(';')[0]
        wait(lambda: api('/api/diagnostics')['health']['available'])
        def rss_bytes():
            for line in Path(f"/proc/{process.pid}/status").read_text().splitlines():
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])*1024
            return 0
        output=api('/api/players',{'kind':'native','address':'diagnostics-test','issue_token':True})
        output_id=output['id']; output_token=output['auth_token']
        baseline_rss=rss_bytes()
        baseline = []
        for _ in range(20):
            begin = time.monotonic()
            api('/api/players/browser-local/commands', {'command':'stop'})
            baseline.append(time.monotonic()-begin)
        store = sqlite3.connect(str(database)+'.diagnostics.db', timeout=.1)
        store.execute('BEGIN IMMEDIATE')
        report = {'category':'native.audio','component':'native','action':'failure','context':{},'message':'permission denied /home/private https://private.invalid/?token=private-secret'}
        with request(f'/api/players/{output_id}/diagnostics', report, output_token) as response:
            assert response.status == 202
        wait(lambda: not api('/api/diagnostics')['health']['available'])
        latency = []
        for _ in range(400):
            begin = time.monotonic()
            api('/api/players/browser-local/commands', {'command':'stop'})
            latency.append(time.monotonic()-begin)
        assert max(latency) < 1, max(latency)
        assert sum(latency)/len(latency) < .25
        blocked_rss=rss_bytes()
        assert blocked_rss < baseline_rss+32*1024*1024
        time.sleep(2.2)  # allow the independent writer to drain retained queue evidence
        bundle = export()
        assert bundle['context.json']['recorder']['storage_gaps'] >= 1
        assert any(row['category']=='native.audio' for row in bundle['context.json']['pending_evidence_may_overlap_history']['incidents'])
        store.rollback()
        store.close()
        wait(lambda: api('/api/diagnostics')['health']['available'])
        bundle = export()
        assert any(row['category']=='native.audio' for row in bundle['incidents.json'])
        assert bundle['manifest.json']['storage_gaps'] >= 1
        assert bundle['manifest.json']['dropped_events'] > 0
        api('/api/diagnostics/debug', {'enabled':True})
        api('/api/health')
        def has_detail():
            api('/api/health')  # reuse the quiet callsite while the outage backlog drains
            with sqlite3.connect(str(database)+'.diagnostics.db') as reader:
                return reader.execute("SELECT COUNT(*) FROM transitions WHERE json_extract(data,'$.event.action')='detail'").fetchone()[0]>0
        try:
            wait(has_detail)
        except AssertionError:
            print('Detail health:',api('/api/diagnostics')['health'])
            with sqlite3.connect(str(database)+'.diagnostics.db') as reader:
                print('Retained actions:',reader.execute("SELECT json_extract(data,'$.event.action'),COUNT(*) FROM transitions GROUP BY 1").fetchall())
            raise
        process.terminate()
        process.wait(timeout=8)
        process = start()
        wait(lambda: api('/api/diagnostics')['health']['available'])
        assert api('/api/diagnostics')['health']['detail_remaining_seconds'] == 0
        bundle = export()
        assert any(row['category']=='native.audio' for row in bundle['incidents.json'])
        size = Path(str(database)+'.diagnostics.db').stat().st_size
        wal = Path(str(database)+'.diagnostics.db-wal')
        assert size <= 64*1024*1024
        assert not wal.exists() or wal.stat().st_size <= 8*1024*1024
        print(json.dumps({'baseline_rss_bytes':baseline_rss,'blocked_rss_bytes':blocked_rss,'controls':len(latency),'baseline_mean_ms':round(1000*sum(baseline)/len(baseline),2),'blocked_writer_mean_ms':round(1000*sum(latency)/len(latency),2),'blocked_writer_max_ms':round(1000*max(latency),2),'database_bytes':size,'dropped_events':bundle['manifest.json']['dropped_events'],'restart_retained':True}))
    finally:
        if process and process.poll() is None:
            process.terminate()
            process.wait(timeout=8)
        log.close()
