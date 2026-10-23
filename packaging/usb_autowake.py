#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Portable USB binding and playback observation for opt-in DAC standby.

Discovery never establishes electrical power-switching safety. Installation must
separately confirm that the selected hardware supports isolated port control.
"""
from dataclasses import dataclass
import json
import math
import os
from pathlib import Path
import socket
import stat
import sys
import time
import urllib.request
import urllib.parse


class Controller:
    def __init__(self, port):
        self.port = port
        self.gate = None

    def step(self, policy, observation, now):
        if policy is None:
            self.gate = None
            return
        if not policy['enabled']:
            self.gate = None
            self.port.set_enabled(True)
            return
        if self.gate is None or self.gate.idle_seconds != policy['idle_seconds']:
            self.gate = IdleGate(policy['idle_seconds'])
        decision = self.gate.observe(dict([observation]) if observation else {}, now)
        if decision is not None:
            self.port.set_enabled(decision)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError('Musicata policy endpoint must not redirect')


class PolicyClient:
    def __init__(self, base_url, token):
        parsed = urllib.parse.urlsplit(base_url)
        if (parsed.scheme not in ('http', 'https') or not parsed.hostname
                or parsed.username or parsed.password or parsed.query or parsed.fragment):
            raise ValueError('Use an HTTP(S) Musicata URL without credentials, query or fragment')
        self.url = base_url.rstrip('/') + '/api/snapcast/usb-wake'
        self.token = token
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())

    def observe(self):
        request = urllib.request.Request(self.url, headers={'Authorization': 'Bearer ' + self.token})
        with self.opener.open(request, timeout=3) as response:
            data = response.read(65537)
        if len(data) > 65536:
            raise ValueError('Musicata policy response exceeded byte limit')
        policy = json.loads(data)
        if (not isinstance(policy, dict) or type(policy.get('enabled')) is not bool
                or type(policy.get('idle_seconds')) is not int
                or not 1 <= policy['idle_seconds'] <= 3600):
            raise ValueError('Invalid Musicata USB policy')
        return policy


def device_identity(device):
    identity = tuple((device / name).read_text().strip()
                     for name in ('idVendor', 'idProduct', 'bDeviceClass'))
    if identity[2] == '09':
        raise ValueError('USB hubs cannot be selected as playback devices')
    serial = device / 'serial'
    return identity + (serial.read_text().strip() if serial.exists() else '',)


@dataclass
class UsbPort:
    device: Path
    disable_path: Path
    identity: tuple

    @classmethod
    def discover(cls, device, sysfs_root=Path('/sys')):
        root = Path(sysfs_root).resolve(strict=True)
        device = Path(device).resolve(strict=True)
        if not device.is_relative_to(root / 'devices'):
            raise ValueError('Selected device is outside the sysfs device tree')
        identity = device_identity(device)
        if not identity[3]:
            raise ValueError('USB auto-wake requires a device serial; unattended replacement detection is unavailable')
        # Kernel USB port objects link to the attached device. Do not infer a
        # physical controller path from PCI, USB bus numbering or a DAC model.
        ports = [path.parent for path in (root / 'devices').rglob('device')
                 if path.is_symlink() and path.resolve() == device
                 and (path.parent / 'disable').exists()]
        if len(ports) != 1:
            raise ValueError('Selected device needs exactly one controllable USB port')
        binding = cls(device, ports[0] / 'disable', identity)
        binding.validate_path()
        return binding

    def validate_path(self):
        if self.disable_path.resolve(strict=True) != self.disable_path:
            raise ValueError('USB control path changed; select the device again')

    def set_enabled(self, enabled):
        self.validate_path()
        # Identity files disappear after disconnect. Only the already-bound
        # control may be re-enabled; never rediscover a substitute target here.
        fd = os.open(self.disable_path, os.O_RDWR | os.O_NOFOLLOW)
        with os.fdopen(fd, 'r+') as control:
            state = control.read().strip()
            if state not in ('0', '1'):
                raise ValueError('Unknown USB control state')
            desired = '0' if enabled else '1'
            if state != desired:
                if not enabled and ((self.disable_path.parent / 'device').resolve() != self.device
                                    or device_identity(self.device) != self.identity):
                    raise ValueError('USB device identity or topology changed')
                control.seek(0)
                control.write(desired + '\n')


class IdleGate:
    """Only continuous, known idle from every configured source may disconnect."""
    def __init__(self, idle_seconds):
        if not math.isfinite(idle_seconds) or idle_seconds <= 0:
            raise ValueError('Idle delay must be a positive finite number')
        self.idle_seconds = idle_seconds
        self.sources = None
        self.since = None

    def observe(self, states, now):
        sources = frozenset(states)
        if sources != self.sources:
            self.since = None
            self.sources = sources
        if 'playing' in states.values():
            self.since = None
            return True
        if not states or any(state != 'idle' for state in states.values()):
            self.since = None
            return None
        if self.since is None or now < self.since:
            self.since = now
        return False if now - self.since >= self.idle_seconds else None


class StatusConnection:
    """One TCP session with a total deadline and byte budget per observation."""
    def __init__(self, host, port, timeout=3, byte_limit=1024 * 1024):
        self.host, self.port = host, port
        self.timeout, self.byte_limit = timeout, byte_limit
        self.connection = None
        self.buffer = b''

    def close(self):
        if self.connection is not None:
            self.connection.close()
        self.connection = None
        self.buffer = b''

    def begin(self):
        self.deadline = time.monotonic() + self.timeout
        self.remaining = self.byte_limit
        fresh = self.connection is None
        if fresh:
            self.connection = socket.create_connection((self.host, self.port), self.timeout)
        return fresh

    def wait_timeout(self):
        wait = self.deadline - time.monotonic()
        if wait <= 0:
            raise TimeoutError('Playback status response timed out')
        self.connection.settimeout(wait)

    def send(self, data):
        self.wait_timeout()
        self.connection.sendall(data)

    def line(self):
        while True:
            self.wait_timeout()
            if b'\n' in self.buffer:
                line, self.buffer = self.buffer.split(b'\n', 1)
                self.remaining -= len(line) + 1
                if self.remaining < 0:
                    raise ValueError('Playback response exceeded byte limit')
                return line.decode('utf-8').rstrip('\r')
            if len(self.buffer) >= self.remaining:
                raise ValueError('Playback response exceeded byte limit')
            chunk = self.connection.recv(min(4096, self.remaining - len(self.buffer)))
            if not chunk:
                raise OSError('Playback server closed the connection')
            self.buffer += chunk


class SnapcastStatus(StatusConnection):
    def __init__(self, host, port, client_id, **kwargs):
        super().__init__(host, port, **kwargs)
        self.client_id = client_id
        self.request_id = 0
        self.client_connected = False

    def observe(self):
        try:
            self.begin()
            self.request_id += 1
            request = {'id': self.request_id, 'jsonrpc': '2.0', 'method': 'Server.GetStatus'}
            self.send((json.dumps(request) + '\r\n').encode())
            while True:
                reply = json.loads(self.line())
                if not isinstance(reply, dict):
                    raise ValueError('Invalid Snapcast response')
                if 'id' not in reply:
                    continue
                if reply['id'] != self.request_id or reply.get('error'):
                    raise ValueError('Snapcast response ID or server error')
                server = reply['result']['server']
                groups = [g for g in server['groups'] if any(
                    c['id'] == self.client_id for c in g['clients'])]
                if len(groups) != 1:
                    raise ValueError('Player must belong to exactly one Snapcast group')
                self.client_connected = any(c['id'] == self.client_id and c.get('connected') is True
                                            for c in groups[0]['clients'])
                streams = [s for s in server['streams'] if s['id'] == groups[0]['stream_id']]
                if len(streams) != 1 or streams[0]['status'] not in ('playing', 'idle'):
                    raise ValueError('Assigned Snapcast stream state is unknown')
                return streams[0]['id'], streams[0]['status']
        except Exception:
            self.close()
            raise


class MpdStatus(StatusConnection):
    def observe(self):
        try:
            if self.begin() and not self.line().startswith('OK MPD '):
                raise ValueError('Invalid MPD greeting')
            self.send(b'status\n')
            states = []
            while True:
                line = self.line()
                if line == 'OK':
                    break
                if line.startswith('ACK'):
                    raise ValueError('MPD status query failed')
                if line.startswith('state: '):
                    states.append(line[7:])
            if len(states) != 1 or states[0] not in ('play', 'pause', 'stop'):
                raise ValueError('MPD playback state is unknown')
            return 'playing' if states[0] == 'play' else 'idle'
        except Exception:
            self.close()
            raise


def private_read(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd) as source:
        info = os.fstat(source.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o077:
            raise ValueError('USB controller configuration must be a root-owned private file')
        data = source.read(65537)
    if len(data) > 65536:
        raise ValueError('USB controller configuration is too large')
    return data


def load_binding(config):
    device = Path(config['device'])
    control = Path(config['disable_path'])
    for path in (device, control):
        if not path.is_absolute() or not path.is_relative_to('/sys/devices') or '..' in path.parts:
            raise ValueError('USB binding must remain within /sys/devices')
    identity = config['identity']
    if (control.name != 'disable' or not isinstance(identity, list) or len(identity) != 4
            or not all(isinstance(part, str) and part for part in identity) or identity[2] == '09'):
        raise ValueError('Invalid saved USB binding')
    port = UsbPort(device, control, tuple(identity))
    port.validate_path()
    return port


def main(argv=None):
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('config', help='Root-owned private host binding JSON')
    parser.add_argument('--check', action='store_true', help='Validate binding and server access without changing USB')
    parser.add_argument('--restore', action='store_true', help='Re-enable the bound port before removing the service')
    args = parser.parse_args(argv)
    config = json.loads(private_read(args.config))
    port = load_binding(config)
    if args.restore:
        port.set_enabled(True)
        return
    policy_client = PolicyClient(config['musicata_url'], private_read(config['token_file']).strip())
    playback = SnapcastStatus(config['snapserver'], 1705, config['client_id'])
    if args.check:
        try:
            policy_client.observe()
            playback.observe()
        finally:
            playback.close()
        return
    controller = Controller(port)
    policy, next_policy, last_warning = None, 0, -float('inf')
    try:
        while True:
            now = time.monotonic()
            try:
                if now >= next_policy:
                    policy = None
                    next_policy = now + 15
                    policy = policy_client.observe()
                observation = playback.observe() if policy and policy['enabled'] else None
                controller.step(policy, observation, time.monotonic())
            except (OSError, ValueError, KeyError, TypeError) as error:
                controller.gate = None
                if now - last_warning >= 60:
                    # Do not print URLs, tokens, or raw remote response bodies.
                    print('USB auto-wake observation/control unavailable: ' + type(error).__name__, file=sys.stderr, flush=True)
                    last_warning = now
            time.sleep(1)
    finally:
        playback.close()


if __name__ == '__main__':
    main()
