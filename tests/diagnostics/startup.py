#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Missing managed Snapcast process must remain diagnosable before router startup."""
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.request
binary=os.environ['MUSICATA_SERVER_BIN']
with tempfile.TemporaryDirectory(prefix='musicata-startup-diagnostic-') as directory:
    db=Path(directory)/'musicata.db'
    subprocess.run([binary,'--library','testdata-fixture','--database',str(db),'--scan-once'],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    with sqlite3.connect(db) as connection:
        connection.execute("INSERT OR REPLACE INTO bool_settings(key,value) VALUES('snapcast.enabled',1),('snapcast.manage_server',1)")
        connection.execute("INSERT OR REPLACE INTO settings(key,value) VALUES('snapcast.server_binary','/definitely-missing/snapserver'),('snapcast.fifo_path',?)",(str(Path(directory)/'test.fifo'),))
    with socket.socket() as reservation:
        reservation.bind(('127.0.0.1',0));port=reservation.getsockname()[1]
    process=subprocess.Popen([binary,'--library','testdata-fixture','--database',str(db),'--addr',f'127.0.0.1:{port}'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    try:
        time.sleep(3)
        with urllib.request.urlopen(f'http://127.0.0.1:{port}/api/health',timeout=2) as response:assert response.status==200
        with sqlite3.connect(str(db)+'.diagnostics.db') as connection:
            count=connection.execute("SELECT COUNT(*) FROM incidents WHERE json_extract(data,'$.event.category')='snapcast.startup'").fetchone()[0]
        assert count>=1,'startup failure must be retained before router construction'
        print('Managed Snapcast startup failure retained; server remains available.')
    finally:
        process.terminate();process.wait(timeout=5)
