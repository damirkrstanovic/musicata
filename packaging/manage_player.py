#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Local-only DAC management for an existing Musicata Snapclient install."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
import install_player
import player
from usb_autowake import PolicyClient

CONFIG = install_player.CONFIG
UNIT = install_player.UNIT
LOCK = install_player.LOCK
USB_CONFIG = install_player.USB_CONFIG
STATE = Path('/var/lib/musicata-player-management/state.json')
BACKUPS = Path('/var/backups/musicata-player-management')
MANAGEMENT_CONFIG = Path('/etc/musicata/player-management.json')
ROOT_PYTHON = '/usr/bin/python3'
ROOT_WORKER = '/opt/musicata/player/manage_player.py'
SERVICE = 'musicata-snapclient.service'
MAX_BODY = 65536
SYSTEMCTL_TIMEOUT = 15
# Worst case is an apply verification followed by both apply_files and journal recovery.
ROOT_REQUEST_TIMEOUT = 270


def digest(value):
    return hashlib.sha256(value).hexdigest()


def _run(argv):
    return install_player.installer.run(argv, timeout=SYSTEMCTL_TIMEOUT)


def _succeeds(argv):
    try:
        return subprocess.run(list(map(str, argv)), stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL, timeout=SYSTEMCTL_TIMEOUT).returncode == 0
    except subprocess.TimeoutExpired:
        return False


def _regular(path, label):
    try:
        status = path.lstat()
    except FileNotFoundError as error:
        raise ValueError(label + ' is missing') from error
    if not stat.S_ISREG(status.st_mode) or path.is_symlink():
        raise ValueError(label + ' must be a regular file')
    if status.st_mode & 0o022:
        raise ValueError(label + ' must not be group or world writable')
    if os.geteuid() == 0 and status.st_uid != 0:
        raise ValueError(label + ' must be root owned')
    return status


def _owned_file(path, label, mode):
    status = _regular(path, label)
    if stat.S_IMODE(status.st_mode) != mode:
        raise ValueError(label + ' has an unsafe file mode')


def _json_file(path, label):
    _regular(path, label)
    try:
        value = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError('Invalid ' + label) from error
    if not isinstance(value, dict):
        raise ValueError('Invalid ' + label)
    return value


def _state():
    if not STATE.exists():
        return {}
    _owned_file(STATE, 'management state', 0o600)
    return _json_file(STATE, 'management state')


def _write_state(value):
    STATE.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    player.atomic_write(STATE, json.dumps(value, sort_keys=True) + '\n', 0o600)


def _output_parts(route):
    match = re.fullmatch(r'plughw:CARD=([A-Za-z0-9_-]+),DEV=([0-9]+)', route)
    if not match:
        raise ValueError('Invalid ALSA output')
    return match.groups()


def _hardware_identity(card, pcm, proc, sysfs):
    matches = [entry for entry in proc.glob('card[0-9]*')
               if (entry / 'id').is_file() and (entry / 'id').read_text().strip() == card]
    if len(matches) != 1:
        return None
    device = sysfs / matches[0].name / 'device'
    if not device.exists():
        return None
    try:
        current = device.resolve(strict=True)
    except OSError:
        return None
    usb = next((entry for entry in (current, *current.parents)
                if (entry / 'idVendor').is_file() and (entry / 'idProduct').is_file()), None)
    if usb is not None:
        try:
            vendor = (usb / 'idVendor').read_text().strip()
            product = (usb / 'idProduct').read_text().strip()
            serial = (usb / 'serial').read_text().strip() if (usb / 'serial').is_file() else ''
        except OSError:
            return None
        if (not re.fullmatch(r'[A-Za-z0-9._-]+', vendor) or
                not re.fullmatch(r'[A-Za-z0-9._-]+', product) or
                (serial and not re.fullmatch(r'[A-Za-z0-9._-]+', serial))):
            return None
        source = 'serial:' + serial if serial else 'path:' + str(usb)
        return source + '|vendor:' + vendor + '|product:' + product + '|pcm:' + pcm
    parts = current.parts
    for index, value in enumerate(parts[:-1]):
        if value == 'sound' and re.fullmatch(r'card[0-9]+', parts[index + 1]):
            current = Path(*parts[:index])
            break
    if not str(current).startswith('/'):
        return None
    return 'path:' + str(current) + '|pcm:' + pcm


def inventory_devices(outputs, proc=Path('/proc/asound'), sys=Path('/sys/class/sound')):
    """Return ALSA devices whose physical identity is unambiguous."""
    found, identities = [], set()
    for output in outputs:
        if not isinstance(output, dict) or not isinstance(output.get('device'), str):
            continue
        try:
            card, pcm = _output_parts(output['device'])
        except ValueError:
            continue
        identity = _hardware_identity(card, pcm, Path(proc), Path(sys))
        if identity is None:
            continue
        device_id = digest(identity.encode())
        if device_id in identities:
            raise ValueError('Ambiguous physical DAC identity')
        identities.add(device_id)
        found.append({'id': device_id, 'route': output['device'],
                      'name': str(output.get('name') or output['device']), 'available': True})
    return found


def _managed(allow_usb=False, allow_dsp=False):
    _owned_file(CONFIG, 'player configuration', 0o600)
    cfg = _json_file(CONFIG, 'player configuration')
    if cfg.get('backend') != 'snapclient':
        raise ValueError('Only an existing Musicata Snapclient can manage its audio device')
    if (cfg.get('dsp') or cfg.get('dsp_enabled')) and not allow_dsp:
        raise ValueError('Audio-device selection is unavailable while DSP routing is enabled')
    if USB_CONFIG.exists() and not allow_usb:
        raise ValueError('Audio-device selection is unavailable while USB DAC wake control is enabled')
    _owned_file(UNIT, 'managed Snapclient unit', 0o644)
    expected = cfg.get('unit_digest')
    if not isinstance(expected, str) or not re.fullmatch(r'[0-9a-f]{64}', expected):
        raise ValueError('Player installation is unmanaged')
    unit = UNIT.read_bytes()
    if digest(unit) != expected:
        raise ValueError('Managed Snapclient unit was customized or removed')
    try:
        return cfg, unit.decode()
    except UnicodeDecodeError as error:
        raise ValueError('Managed Snapclient unit is invalid') from error


def _set_soundcard(unit, old, new):
    _output_parts(new)
    matches = list(re.finditer(r'(?<=--soundcard\s)\S+', unit))
    if len(matches) != 1 or matches[0].group() != old:
        raise ValueError('Managed Snapclient unit has an unexpected soundcard setting')
    return unit[:matches[0].start()] + new + unit[matches[0].end():]


def inventory_revision(devices, unit_digest):
    mapping = [{'id': item['id'], 'route': item['route'], 'available': item['available']}
               for item in devices]
    return digest(json.dumps({'devices': mapping, 'unit_digest': unit_digest}, sort_keys=True,
                             separators=(',', ':')).encode())


def inventory():
    try:
        cfg, unit = _managed(allow_usb=True, allow_dsp=True)
    except (ValueError, OSError):
        return {'inventory_revision': digest(b'unavailable'), 'devices': [],
                'selected_device_id': None, 'selection_supported': False,
                'reason': 'Audio device management is unavailable on this player.'}
    try:
        text = subprocess.run(['runuser', '-u', cfg['user'], '-G', 'audio', '--', 'aplay', '-l'],
                              check=True, text=True, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, timeout=10).stdout
        outputs = player.parse_outputs(text)
    except (KeyError, OSError, subprocess.SubprocessError, ValueError):
        return {'inventory_revision': inventory_revision([], digest(unit.encode())), 'devices': [],
                'selected_device_id': None, 'selection_supported': False,
                'reason': 'Audio device inventory is unavailable.'}
    try:
        devices = inventory_devices(outputs)
    except ValueError:
        return {'inventory_revision': inventory_revision([], digest(unit.encode())), 'devices': [],
                'selected_device_id': None, 'selection_supported': False,
                'reason': 'Audio device inventory is ambiguous.'}
    selected = next((item['id'] for item in devices if item['route'] == cfg.get('output')), None)
    unit_digest = digest(unit.encode())
    dsp = bool(cfg.get('dsp') or cfg.get('dsp_enabled'))
    supported = not USB_CONFIG.exists() and not dsp
    return {'inventory_revision': inventory_revision(devices, unit_digest), 'devices': devices,
            'selected_device_id': selected, 'selection_supported': supported,
            'reason': ('USB DAC wake control owns the current output.' if USB_CONFIG.exists() else
                       'DSP routing owns the current audio device.' if dsp else None)}


def recover_interrupted_apply():
    state = _state()
    pending = state.get('pending')
    if not pending:
        return state
    try:
        if (not isinstance(pending, dict) or not isinstance(pending.get('previous'), dict) or
                not isinstance(pending.get('intended'), dict) or not isinstance(pending.get('service'), dict)):
            raise ValueError('Invalid pending management recovery')
        previous, intended, service = pending['previous'], pending['intended'], pending['service']
        if (not all(isinstance(version.get(key), str) for version in (previous, intended)
                    for key in ('unit', 'config', 'unit_digest', 'config_digest')) or
                not all(isinstance(service.get(key), bool) for key in ('active', 'enabled'))):
            raise ValueError('Invalid pending management recovery')
        _owned_file(UNIT, 'managed Snapclient unit', 0o644)
        _owned_file(CONFIG, 'player configuration', 0o600)
        current = {'unit_digest': digest(UNIT.read_bytes()), 'config_digest': digest(CONFIG.read_bytes())}
        if (current['unit_digest'] not in (previous['unit_digest'], intended['unit_digest']) or
                current['config_digest'] not in (previous['config_digest'], intended['config_digest'])):
            raise ValueError('Managed files changed after the interrupted request')
        player.atomic_write(UNIT, previous['unit'], 0o644)
        player.atomic_write(CONFIG, previous['config'], 0o600)
        previous_cfg = json.loads(previous['config'])
        if not isinstance(previous_cfg, dict):
            raise ValueError('Invalid pending player configuration')
        _run(['systemctl', 'daemon-reload'])
        _run(['systemctl', 'enable' if service['enabled'] else 'disable', SERVICE])
        if service['active']:
            _run(['systemctl', 'restart', SERVICE])
            install_player.verify(previous_cfg)
        else:
            _run(['systemctl', 'stop', SERVICE])
    except Exception as error:
        state['status'] = 'failed'
        state['error'] = 'Unverified rollback; local recovery is still required'
        _write_state(state)
        raise RuntimeError(state['error']) from error
    state.pop('pending', None)
    state['status'] = 'failed'
    state['error'] = 'Recovered interrupted audio-device change'
    _write_state(state)
    return state


def _restore_service_state(service):
    _run(['systemctl', 'enable' if service['enabled'] else 'disable', SERVICE])
    _run(['systemctl', 'start' if service['active'] else 'stop', SERVICE])


def apply_selection(revision, device_id, expected_inventory_revision, devices):
    if (type(revision) is not int or not 1 <= revision <= (2**64 - 1) or
            not isinstance(device_id, str) or not re.fullmatch(r'[0-9a-f]{64}', device_id) or
            not isinstance(expected_inventory_revision, str) or
            not re.fullmatch(r'[0-9a-f]{64}', expected_inventory_revision)):
        raise ValueError('Invalid audio-device selection request')
    state = _state()
    if revision <= state.get('attempted_revision', 0):
        return state
    cfg, unit = _managed()
    actual_revision = inventory_revision(devices, digest(unit.encode()))
    if expected_inventory_revision != actual_revision:
        raise ValueError('Audio-device inventory changed; refresh before applying')
    selected = [item for item in devices if item.get('id') == device_id and item.get('available') is True]
    if len(selected) != 1:
        raise ValueError('Requested audio device is unavailable or ambiguous')
    next_unit = _set_soundcard(unit, cfg.get('output'), selected[0]['route'])
    next_cfg = dict(cfg)
    next_cfg['output'], next_cfg['unit_digest'] = selected[0]['route'], digest(next_unit.encode())
    old_config = CONFIG.read_text()
    next_config = json.dumps(next_cfg, indent=2) + '\n'
    service = {'active': _succeeds(['systemctl', 'is-active', '--quiet', SERVICE]),
               'enabled': _succeeds(['systemctl', 'is-enabled', '--quiet', SERVICE])}
    state.update({'attempted_revision': revision, 'status': 'idle', 'error': None,
                  'pending': {'previous': {'unit': unit, 'config': old_config,
                                           'unit_digest': digest(unit.encode()), 'config_digest': digest(old_config.encode())},
                              'intended': {'unit': next_unit, 'config': next_config,
                                           'unit_digest': digest(next_unit.encode()), 'config_digest': digest(next_config.encode())},
                              'service': service}})
    _write_state(state)
    files = {UNIT: (next_unit, 0o644), CONFIG: (next_config, 0o600)}
    try:
        player.apply_files(files, SERVICE, BACKUPS / str(revision), _run, _succeeds,
                           lambda: install_player.verify(next_cfg))
        _restore_service_state(service)
    except Exception as error:
        try:
            state = recover_interrupted_apply()
        except Exception as recovery:
            raise RuntimeError('Audio-device change failed with unverified rollback') from recovery
        state.update({'attempted_revision': revision, 'status': 'failed',
                      'error': 'Audio-device change failed; previous configuration was restored'})
        _write_state(state)
        raise RuntimeError(state['error']) from error
    state = _state()
    state.update({'attempted_revision': revision, 'applied_revision': revision,
                  'status': 'applied', 'error': None})
    state.pop('pending', None)
    _write_state(state)
    return state


def report_payload(snapshot, state):
    unsupported = not snapshot['selection_supported']
    return {'inventory_revision': snapshot['inventory_revision'],
            'devices': [{key: item[key] for key in ('id', 'name', 'available')} for item in snapshot['devices']],
            'selected_device_id': snapshot['selected_device_id'],
            'selection_supported': snapshot['selection_supported'],
            'attempted_revision': state.get('attempted_revision', 0),
            'applied_revision': state.get('applied_revision', 0),
            'status': 'unsupported' if unsupported else state.get('status', 'idle'),
            'error': snapshot.get('reason') if unsupported else state.get('error')}


def _record_preflight_failure(revision, error):
    if type(revision) is not int or not 1 <= revision <= (2**64 - 1):
        return
    state = _state()
    if revision <= state.get('attempted_revision', 0):
        return
    state.update({'attempted_revision': revision, 'status': 'failed',
                  'error': 'Audio-device change was refused: ' + str(error)})
    _write_state(state)


def local_request(request):
    if not isinstance(request, dict):
        raise ValueError('Invalid local management request')
    LOCK.parent.mkdir(parents=True, exist_ok=True)
    with open(LOCK, 'w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            recover_interrupted_apply()
        except RuntimeError:
            if request.get('action') != 'inventory':
                raise
            snapshot = {'inventory_revision': digest(b'unavailable'), 'devices': [],
                        'selected_device_id': None, 'selection_supported': False,
                        'reason': 'Audio device management recovery needs local attention.'}
            return report_payload(snapshot, _state())
        if request.get('action') == 'inventory':
            snapshot = inventory()
            return report_payload(snapshot, _state())
        if request.get('action') != 'apply':
            raise ValueError('Unknown local management action')
        snapshot = inventory()
        if not snapshot['selection_supported']:
            _record_preflight_failure(request.get('revision'), snapshot['reason'])
            raise ValueError(snapshot['reason'])
        try:
            state = apply_selection(request.get('revision'), request.get('device_id'),
                                    request.get('inventory_revision'), snapshot['devices'])
        except ValueError as error:
            _record_preflight_failure(request.get('revision'), error)
            raise
        return report_payload(inventory(), state)


def management_config():
    status = MANAGEMENT_CONFIG.lstat()
    if (not stat.S_ISREG(status.st_mode) or MANAGEMENT_CONFIG.is_symlink() or
            stat.S_IMODE(status.st_mode) != 0o640 or status.st_uid != 0 or
            status.st_gid != os.getegid()):
        raise ValueError('Player management configuration must be root owned mode 0640 for this playback group')
    cfg = _json_file(MANAGEMENT_CONFIG, 'player management configuration')
    if len(json.dumps(cfg)) > MAX_BODY or not all(isinstance(cfg.get(key), str) and cfg[key]
                                                  for key in ('server_url', 'client_id', 'token')):
        raise ValueError('Invalid player management configuration')
    PolicyClient(cfg['server_url'], cfg['token'])
    return cfg


def _local_call(request):
    body = json.dumps(request, separators=(',', ':')).encode()
    if len(body) > MAX_BODY:
        raise ValueError('Local management request exceeded byte limit')
    response = subprocess.run(['sudo', '-n', ROOT_PYTHON, ROOT_WORKER, '--local'], input=body,
                              stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                              check=True, timeout=ROOT_REQUEST_TIMEOUT).stdout
    if len(response) > MAX_BODY:
        raise ValueError('Local management response exceeded byte limit')
    value = json.loads(response)
    if not isinstance(value, dict):
        raise ValueError('Invalid local management response')
    return value


def daemon_once():
    cfg = management_config()
    snapshot = _local_call({'action': 'inventory'})
    client = PolicyClient(cfg['server_url'], cfg['token'])
    url = client.url.removesuffix('/api/snapcast/usb-wake') + '/api/managed-players/report'
    body = json.dumps(snapshot, separators=(',', ':')).encode()
    if len(body) > MAX_BODY:
        raise ValueError('Player report exceeded byte limit')
    request = urllib.request.Request(url, data=body, method='POST', headers={
        'Authorization': 'Bearer ' + cfg['token'], 'Content-Type': 'application/json'})
    with client.opener.open(request, timeout=5) as response:
        data = response.read(MAX_BODY + 1)
    if len(data) > MAX_BODY:
        raise ValueError('Player management response exceeded byte limit')
    desired = json.loads(data)
    if not isinstance(desired, dict):
        raise ValueError('Invalid player management response')
    revision = desired.get('revision')
    if type(revision) is int and revision > snapshot.get('attempted_revision', 0):
        _local_call({'action': 'apply', 'revision': revision, 'device_id': desired.get('device_id'),
                     'inventory_revision': desired.get('inventory_revision')})


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument('--local', action='store_true')
    group.add_argument('--daemon', action='store_true')
    args = parser.parse_args(argv)
    if args.local:
        if os.geteuid() != 0:
            raise ValueError('Local management actions require root')
        raw = sys.stdin.buffer.read(MAX_BODY + 1)
        if len(raw) > MAX_BODY:
            raise ValueError('Local management request exceeded byte limit')
        sys.stdout.write(json.dumps(local_request(json.loads(raw)), separators=(',', ':')) + '\n')
        return 0
    while True:
        try:
            daemon_once()
        except Exception:
            pass
        time.sleep(5)


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except Exception as error:
        print('Player management failed: ' + str(error), file=sys.stderr)
        raise SystemExit(1)
