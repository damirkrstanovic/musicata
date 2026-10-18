#!/usr/bin/env python3
"""Exercise profiling in the actual shipped binary, using an isolated fixture DB.

Usage: python3 tests/diagnostics/profiles.py /path/to/musicata-server
Run optimized glibc and native-architecture musl builds: debug-only checks miss unwind bugs.
"""
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

binary = str(pathlib.Path(sys.argv[1]).resolve())
fixture = pathlib.Path(__file__).resolve().parents[2] / "testdata-fixture"
with tempfile.TemporaryDirectory(prefix="musicata-profile-smoke-") as temp:
    db = pathlib.Path(temp) / "test.db"
    subprocess.run([binary, "--scan-once", "--library", str(fixture), "--database", str(db)],
                   check=True, stdout=subprocess.DEVNULL)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    cookie = ""
    def api(path, body=None):
        request = urllib.request.Request(
            f"http://127.0.0.1:{port}" + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={"Content-Type": "application/json", "Cookie": cookie},
        )
        return urllib.request.urlopen(request, timeout=10)
    with open(pathlib.Path(temp) / "server.log", "w") as log:
        process = subprocess.Popen([binary, "--no-scan", "--database", str(db),
                                    "--addr", f"127.0.0.1:{port}"], stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 20
            while True:
                assert process.poll() is None, "server exited during startup"
                try:
                    with api("/api/auth/setup", {"username": "probe", "password": "fixture-only-password"}) as response:
                        cookie = response.headers["Set-Cookie"].split(";")[0]
                    break
                except (urllib.error.URLError, ConnectionError):
                    assert time.monotonic() < deadline, "server did not bind"
                    time.sleep(0.1)
            with api("/api/diagnostics") as response:
                capability = json.load(response)["profiles"]
            for kind in ("cpu", "heap"):
                supported = capability["supported" if kind == "cpu" else "heap_supported"]
                if not supported:
                    try:
                        api("/api/diagnostics/profile", {"kind": kind, "seconds": 2})
                        raise AssertionError("unsupported capture was accepted")
                    except urllib.error.HTTPError as error:
                        assert error.code == 400
                    assert process.poll() is None
                    print(f"PASS {kind}: unsupported capability safely rejected")
                    continue
                with api("/api/diagnostics/profile", {"kind": kind, "seconds": 5}):
                    pass
                held = []
                if kind == "heap":
                    # Keep bounded request-body buffers live across the heap snapshot.
                    # Bodies remain incomplete, so no playlist is ever created.
                    for _ in range(64):
                        connection = socket.create_connection(("127.0.0.1", port), timeout=3)
                        held.append(connection)
                        header = (f"POST /api/playlists HTTP/1.1\r\nHost: localhost\r\n"
                                  f"Cookie: {cookie}\r\nContent-Type: application/json\r\n"
                                  "Content-Length: 1048576\r\n\r\n")
                        connection.sendall(header.encode() + b'{"name":"' + b'x' * 262144)
                deadline = time.monotonic() + 20
                while True:
                    # Allocation and CPU work during capture, without external providers.
                    with api("/api/tracks?limit=200") as response:
                        response.read()
                    with api("/api/diagnostics") as response:
                        status = json.load(response)["profiles"]
                    if not status["running"]:
                        break
                    assert time.monotonic() < deadline, "capture did not expire"
                    time.sleep(0.005)
                assert status[kind + "_ready"], status
                with api("/api/diagnostics/profile/" + kind) as response:
                    profile = response.read()
                assert 0 < len(profile) <= 2 * 1024 * 1024
                if kind == "cpu":
                    assert b"<svg" in profile and b"musicata" in profile, "missing CPU application symbols"
                else:
                    data = json.loads(profile)
                    assert "before" in data and "after" in data
                    assert data["after"], "heap capture missed held request allocations"
                    assert any(any(label in frame for label in ("axum", "tokio", "musicata"))
                               for stack in data["after"] for frame in stack["frames"]), "missing allocation-site symbols"
                    assert b"MAPPED_LIBRARIES" not in profile and str(temp).encode() not in profile
                for connection in held:
                    connection.close()
                assert process.poll() is None, "profiling terminated the server"
                print(f"PASS {kind}: automatic expiry, bounded download, responsive server")
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
