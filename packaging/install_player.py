#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Install a standalone Snapclient (recommended) or MPD compatibility player."""
import argparse
import getpass
import datetime
import fcntl
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import pwd
import shlex
import shutil
import sys
import tempfile
import time
import socket
import urllib.request
import urllib.parse
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
import install as installer
import player
from usb_autowake import SnapcastStatus, MpdStatus, UsbPort, PolicyClient, private_read

CONFIG = Path('/etc/musicata/player.json')
UNIT = Path('/etc/systemd/system/musicata-snapclient.service')
MPD_UNIT = Path('/etc/systemd/system/musicata-mpd.service')
MPD_CONFIG = Path('/etc/musicata-mpd.conf')
MPD_ROOT = Path('/var/lib/musicata-mpd')
LOCK = Path('/run/lock/musicata-install.lock')
BACKUPS = Path('/var/backups/musicata-player')
USB_UNIT = Path('/etc/systemd/system/musicata-usb-autowake.service')
USB_CONFIG = Path('/etc/musicata/usb-autowake.json')
USB_CODE = Path('/opt/musicata/player/usb_autowake.py')
USB_TOKEN = Path('/etc/musicata/usb-autowake.token')
MANAGEMENT_CONFIG = Path('/etc/musicata/player-management.json')
MANAGEMENT_UNIT = Path('/etc/systemd/system/musicata-player-management.service')
MANAGEMENT_SUDOERS = Path('/etc/sudoers.d/musicata-player-management')
MANAGEMENT_ROOT = Path('/opt/musicata/player')
MANAGEMENT_STATE = Path('/var/lib/musicata-player-management/state.json')


def options(argv, saved=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--pair-code', action='store_true', help='Prompt privately for a one-time device-management pairing code')
    parser.add_argument('--backend', choices=('snapclient', 'mpd'))
    parser.add_argument('--mpd-bind', help='MPD local IPv4 address; default loopback, port 6600')
    parser.add_argument('--confirm-trusted-network', action='store_true', default=None,
                        help='Acknowledge unauthenticated MPD control on an isolated trusted network')
    parser.add_argument('--server', help='Snapserver host (port 1704; control port 1705)')
    parser.add_argument('--output', help='Stable ALSA output; otherwise select interactively')
    parser.add_argument('--user', help='Non-root playback account (default: musicata)')
    parser.add_argument('--check', action='store_true')
    parser.add_argument('--yes', action='store_true')
    parser.add_argument('--usb-device', help='Explicit sysfs USB device to bind (optional)')
    parser.add_argument('--musicata-url', help='Musicata HTTP(S) URL for registration or USB policy')
    parser.add_argument('--token-file', help='Private root-owned Musicata API token file')
    parser.add_argument('--confirm-isolated-usb', action='store_true', help='Confirm this DAC port has independently verified isolated control')
    parser.add_argument('--remove-usb', action='store_true', help='Stop USB control, restore the DAC port, and remove only its managed files')
    result = {'server': '', 'output': '', 'identity': '', 'user': 'musicata',
              'backend': 'snapclient', 'mpd_bind': '127.0.0.1', 'confirm_trusted_network': False}
    result.update(saved or {})
    result.update({key: value for key, value in vars(parser.parse_args(argv)).items() if value is not None})
    if result['server']:
        player.checked(result['server'], r'[A-Za-z0-9:][A-Za-z0-9.:-]*', 'Snapserver host')
    if result['output']:
        player.checked(result['output'], r'plughw:CARD=[A-Za-z0-9_-]+,DEV=[0-9]+', 'stable ALSA output')
    player.checked(result['user'], r'[a-z_][a-z0-9_-]*', 'service account')
    if result['user'] == 'root':
        raise ValueError('Use a non-root playback account')
    if saved and result['user'] != saved['user']:
        raise ValueError('Changing the existing service account requires migration')
    if saved and result['backend'] != saved.get('backend', 'snapclient'):
        raise ValueError('Changing playback backend requires migration')
    if result['backend'] == 'mpd':
        address = ipaddress.IPv4Address(result['mpd_bind'])
        private = any(address in ipaddress.IPv4Network(network) for network in
                      ('10.0.0.0/8', '172.16.0.0/12', '192.168.0.0/16'))
        if not address.is_loopback and not private:
            raise ValueError('MPD requires a specific loopback or private IPv4 address')
        if not address.is_loopback and not result['confirm_trusted_network']:
            raise ValueError('Remote MPD requires --confirm-trusted-network; control is unauthenticated')
        if address.is_loopback and result.get('musicata_url'):
            host = urllib.parse.urlsplit(result['musicata_url']).hostname
            try:
                local_server = ipaddress.ip_address(host).is_loopback
            except ValueError:
                local_server = host == 'localhost'
            if not local_server:
                raise ValueError('A remote Musicata server requires an explicit reachable --mpd-bind and --confirm-trusted-network; loopback would address the server itself')
        if result.get('usb_device'):
            raise ValueError('Direct MPD cannot use USB disconnect automation; use MPD through Snapcast on the server')
        if saved and (result['mpd_bind'] != saved.get('mpd_bind') or
                      result.get('musicata_url') != saved.get('musicata_url')):
            raise ValueError('Changing the MPD address or Musicata server requires migration')
    return result


def dependencies(family, backend='snapclient'):
    base = ['alsa-utils', 'ca-certificates']
    if backend == 'mpd':
        accounts = {'debian': 'passwd', 'fedora': 'shadow-utils', 'arch': 'shadow'}
        return base + ['mpd', 'util-linux', accounts[family]] if family in accounts else []
    if family == 'debian':
        return base + ['snapclient', 'util-linux', 'passwd']
    build = ['git', 'cmake', 'make', 'pkgconf']
    if family == 'arch':
        return base + build + ['gcc', 'boost', 'alsa-lib', 'flac', 'libvorbis', 'opus', 'openssl', 'libsoxr', 'avahi', 'expat', 'shadow', 'util-linux']
    if family == 'fedora':
        return base + build + ['gcc-c++', 'boost-devel', 'alsa-lib-devel', 'flac-devel', 'libvorbis-devel', 'opus-devel', 'openssl-devel', 'soxr-devel', 'avahi-devel', 'expat-devel', 'shadow-utils', 'util-linux']
    return []


def generic_command(cfg):
    if cfg.get('backend') == 'mpd':
        binary = shutil.which('mpd')
        if not binary:
            raise installer.InstallError('Generic Linux needs an existing MPD executable; see docs/install-linux.md.')
        print('Save this configuration with writable music/playlists/state paths for your account:')
        print(player.mpd_config(cfg['output'], root=MPD_ROOT, bind=cfg['mpd_bind']))
        print('Register the reachable address in Musicata after configuring and starting MPD.')
        return shlex.join([binary, '--no-daemon', str(MPD_CONFIG)])
    binary = shutil.which('snapclient')
    if not binary:
        raise installer.InstallError('Generic Linux needs an existing Snapclient executable. Install Snapcast 0.35.0 with BUILD_SERVER=OFF; see docs/install-linux.md.')
    return shlex.join([binary, '--host', cfg['server'], '--hostID', cfg['identity'], '--soundcard', cfg['output']])


def verify(cfg):
    if cfg.get('backend') == 'mpd':
        observer = MpdStatus(cfg['mpd_bind'], 6600)
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                try:
                    if observer.observe() is not None:
                        return
                except (OSError, ValueError):
                    pass
                time.sleep(1)
        finally:
            observer.close()
        raise installer.InstallError('MPD did not answer on the configured address; check musicata-mpd.service.')
    observer = SnapcastStatus(cfg['server'], 1705, cfg['identity'])
    deadline = time.monotonic() + 30
    try:
        while time.monotonic() < deadline:
            try:
                observer.observe()
                if observer.client_connected:
                    return
            except (OSError, ValueError, KeyError):
                pass
            time.sleep(1)
    finally:
        observer.close()
    raise installer.InstallError('Player did not connect to Snapserver. Check server availability, ports 1704/1705, DAC permissions and the musicata-snapclient journal.')


def playback_files(cfg, uid, gid):
    if cfg['backend'] == 'mpd':
        return {MPD_UNIT: (player.mpd_unit(uid, gid, MPD_CONFIG), 0o644),
                MPD_CONFIG: (player.mpd_config(cfg['output'], root=MPD_ROOT, bind=cfg['mpd_bind']), 0o644)}
    return {UNIT: (player.snapclient_unit(uid, gid, cfg['output'], cfg['server'],
                  cfg['identity'], binary=shutil.which('snapclient')), 0o644)}


def registration_request(cfg, token, payload=None):
    client = PolicyClient(cfg['musicata_url'], token)
    url = client.url.removesuffix('/api/snapcast/usb-wake') + '/api/players'
    request = urllib.request.Request(url,
        data=json.dumps(payload).encode() if payload is not None else None,
        headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    with client.opener.open(request, timeout=5) as response:
        data = response.read(262145)
    if len(data) > 262144:
        raise ValueError('Musicata player response exceeded byte limit')
    result = json.loads(data)
    if payload is None:
        if not isinstance(result, list) or not all(isinstance(item, dict) for item in result):
            raise ValueError('Invalid Musicata player list')
    elif not isinstance(result, dict) or not isinstance(result.get('id'), str):
        raise ValueError('Invalid Musicata registration response')
    return result


def register_mpd(cfg, token):
    address = cfg['mpd_bind'] + ':6600'
    players = registration_request(cfg, token)
    existing = next((item for item in players if item.get('kind') == 'mpd' and item.get('address') == address), None)
    registered = existing or registration_request(cfg, token, {'kind': 'mpd', 'address': address,
                                                               'name': 'MPD (' + address + ')'})
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if any(item.get('id') == registered['id'] and item.get('online') is True
               for item in registration_request(cfg, token)):
            return
        time.sleep(1)
    raise installer.InstallError('MPD is registered but Musicata cannot reach it. Check bind address, routing and port 6600; rerun to verify without duplicating the player.')


def usb_files(cfg):
    if not cfg.get('usb_device'):
        return {}
    if not cfg.get('confirm_isolated_usb') or not cfg.get('musicata_url') or not cfg.get('token_file'):
        raise installer.InstallError('USB binding requires --confirm-isolated-usb, --musicata-url and --token-file; verify isolated port control before opting in.')
    port = UsbPort.discover(cfg['usb_device'])
    # An explicit USB path must also be the parent of the selected ALSA card.
    card_id = cfg['output'].split('CARD=', 1)[1].split(',', 1)[0]
    cards = [card for card in Path('/proc/asound').glob('card[0-9]*')
             if (card / 'id').read_text().strip() == card_id]
    if len(cards) != 1 or not (Path('/sys/class/sound') / cards[0].name / 'device').resolve().is_relative_to(port.device):
        raise installer.InstallError('Selected USB device does not own the selected ALSA output')
    token = private_read(cfg['token_file']).strip()
    PolicyClient(cfg['musicata_url'], token).observe()
    binding = {'device': str(port.device), 'disable_path': str(port.disable_path),
               'identity': list(port.identity), 'musicata_url': cfg['musicata_url'],
               'token_file': str(USB_TOKEN), 'snapserver': cfg['server'], 'client_id': cfg['identity']}
    player.checked(str(port.disable_path), r'/[A-Za-z0-9_./:+-]+', 'USB control path')
    unit = f'''[Unit]
Description=Musicata USB DAC auto-wake
After=network-online.target musicata-snapclient.service
Wants=network-online.target
[Service]
ExecStart=/usr/bin/python3 {USB_CODE} {USB_CONFIG}
Restart=on-failure
RestartSec=5
User=root
UMask=0077
NoNewPrivileges=true
CapabilityBoundingSet=
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ReadOnlyPaths=/sys
ReadWritePaths={port.disable_path}
[Install]
WantedBy=multi-user.target
'''
    return {USB_UNIT: (unit, 0o644), USB_CONFIG: (json.dumps(binding, indent=2) + '\n', 0o600),
            USB_TOKEN: (token + '\n', 0o600), USB_CODE: (Path(__file__).with_name('usb_autowake.py').read_text(), 0o644)}


def restore_usb():
    """Retain recovery data until the old port has actually been restored."""
    if not USB_CONFIG.exists() or not USB_CODE.exists():
        raise installer.InstallError('No complete managed USB controller installation found.')
    if installer.succeeds(['systemctl', 'is-active', '--quiet', 'musicata-usb-autowake.service']):
        installer.run(['systemctl', 'stop', 'musicata-usb-autowake.service'])
    installer.run(['/usr/bin/python3', str(USB_CODE), str(USB_CONFIG), '--restore'])


def remove_usb():
    restore_usb()
    if USB_UNIT.exists():
        installer.run(['systemctl', 'disable', 'musicata-usb-autowake.service'])
    for path in (USB_UNIT, USB_CONFIG, USB_CODE, USB_TOKEN):
        path.unlink(missing_ok=True)
    installer.run(['systemctl', 'daemon-reload'])


def apply_usb(files, backup):
    was_active = installer.succeeds(['systemctl', 'is-active', '--quiet', 'musicata-usb-autowake.service'])
    old_config = USB_CONFIG.read_text() if USB_CONFIG.exists() else None
    recovery_failed = False
    def restore_new_port():
        nonlocal recovery_failed
        try:
            installer.run(['/usr/bin/python3', str(USB_CODE), str(USB_CONFIG), '--restore'])
        except Exception:
            recovery_failed = True
            raise
    if USB_UNIT.exists() and not USB_CONFIG.exists():
        raise installer.InstallError('Existing USB service has no managed binding; reconcile it before installation.')
    if USB_CONFIG.exists():
        # Stop and restore even when the binding is unchanged; subsequent failures
        # must not strand a disconnected DAC behind a lost/invalid configuration.
        restore_usb()
    try:
        player.apply_files(files, 'musicata-usb-autowake.service', backup,
                           installer.run, installer.succeeds,
                           lambda: installer.run(['/usr/bin/python3', str(USB_CODE), str(USB_CONFIG), '--check']),
                           before_restore=restore_new_port)
    except Exception:
        if was_active and not recovery_failed and USB_CONFIG.exists() and USB_CONFIG.read_text() == old_config:
            installer.run(['systemctl', 'start', 'musicata-usb-autowake.service'])
        raise


def management_files(cfg, credentials, uid, gid):
    player.checked(cfg['user'], r'[a-z_][a-z0-9_-]*', 'management account')
    unit = f"""[Unit]
Description=Musicata player device management
After=network-online.target
Wants=network-online.target
[Service]
User={uid}
Group={gid}
ExecStart=/usr/bin/python3 /opt/musicata/player/manage_player.py --daemon
Restart=on-failure
RestartSec=5
UMask=0077
PrivateTmp=true
ProtectHome=true
ProtectSystem=full
ReadWritePaths=/etc/musicata /etc/systemd/system /var/lib/musicata-player-management /var/backups/musicata-player-management /run/lock
[Install]
WantedBy=multi-user.target
"""
    sudoers = (f"{cfg['user']} ALL=(root) NOPASSWD: /usr/bin/python3 "
               "/opt/musicata/player/manage_player.py --local\n")
    files = {MANAGEMENT_UNIT: (unit, 0o644), MANAGEMENT_SUDOERS: (sudoers, 0o440),
             MANAGEMENT_CONFIG: (json.dumps(credentials) + '\n', 0o640)}
    for name in ('manage_player.py','install_player.py','install.py','player.py','usb_autowake.py'):
        files[MANAGEMENT_ROOT/name] = (Path(__file__).with_name(name).read_text(),0o644)
    return files


def pair_management(cfg, saved):
    if os.geteuid() != 0:
        raise installer.InstallError('Run the player installer with sudo to pair device management.')
    os.umask(0o022)
    if not saved or saved.get('backend', 'snapclient') != 'snapclient':
        raise installer.InstallError('Device management requires an existing Musicata-managed Snapclient installation.')
    if not cfg.get('musicata_url'):
        raise installer.InstallError('Pairing requires --musicata-url.')
    if not shutil.which('sudo') or not shutil.which('visudo'):
        raise installer.InstallError('Install sudo before enabling device management.')
    for path in (CONFIG, UNIT):
        metadata = path.lstat()
        if path.is_symlink() or metadata.st_uid != 0 or metadata.st_mode & 0o022:
            raise installer.InstallError('Managed playback files must be root-owned and not writable by other accounts.')
    if hashlib.sha256(UNIT.read_bytes()).hexdigest() != saved.get('unit_digest'):
        raise installer.InstallError('Playback service was customized; reconcile it before pairing management.')
    account = pwd.getpwnam(saved['user'])
    if account.pw_uid == 0:
        raise installer.InstallError('Management requires a non-root playback account.')
    client = PolicyClient(cfg['musicata_url'], '')
    # Validate all local installation content before consuming the one-time pairing code.
    files = management_files(saved, {}, account.pw_uid, account.pw_gid)
    for path in files:
        for parent in (path, *path.parents):
            if parent.exists() and (parent.is_symlink() or parent.stat().st_uid != 0 or parent.stat().st_mode & 0o022):
                raise installer.InstallError('Management installation paths must be owned and protected by root.')
    with tempfile.NamedTemporaryFile(mode='w', prefix='musicata-sudoers-') as candidate:
        candidate.write(files[MANAGEMENT_SUDOERS][0]); candidate.flush()
        installer.run(['visudo','-cf',candidate.name])
    if not cfg['yes']:
        if not sys.stdin.isatty() or input('Enable device management for this player? [y/N] ').lower() not in ('y','yes'):
            return 1
    if not sys.stdin.isatty():
        raise installer.InstallError('Pairing requires an interactive terminal for the private code prompt.')
    pairing_code = getpass.getpass('Musicata pairing code: ').strip()
    player.checked(pairing_code, r'[0-9a-f]{64}', 'pairing code')
    was_active = installer.succeeds(['systemctl','is-active','--quiet',MANAGEMENT_UNIT.name])
    installer.run(['systemctl','stop',MANAGEMENT_UNIT.name]) if MANAGEMENT_UNIT.exists() else None
    enrolled = False
    try:
        import manage_player
        # Recover a prior interrupted change before resetting this host's management generation.
        with open(LOCK, 'w') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            manage_player.recover_interrupted_apply()
            if json.loads(CONFIG.read_text()) != saved:
                raise installer.InstallError('Playback configuration changed during pairing; retry.')
            url = client.url.removesuffix('/api/snapcast/usb-wake') + '/api/managed-players/enroll'
            request = urllib.request.Request(url, data=json.dumps({'pairing_token':pairing_code,
                'client_id':saved['identity']}).encode(),headers={'Content-Type':'application/json'})
            try:
                with client.opener.open(request,timeout=5) as response:
                    data=response.read(65537)
                if len(data)>65536: raise ValueError('Oversized pairing response')
                enrolled_result=json.loads(data)
                player.checked(enrolled_result['token'],r'[0-9a-f]{64}','management credential')
                if enrolled_result['client_id'] != saved['identity']: raise ValueError('Player identity mismatch')
            except Exception:
                raise installer.InstallError('Pairing failed. Check the server URL, player identity and unexpired code.') from None
            credentials={'server_url':cfg['musicata_url'],'client_id':saved['identity'],'token':enrolled_result['token']}
            # Retain the new credential if starting the service fails: the old one is revoked.
            player.atomic_write(MANAGEMENT_CONFIG,json.dumps(credentials)+'\n',0o640)
            os.chown(MANAGEMENT_CONFIG,0,account.pw_gid)
            enrolled=True
            MANAGEMENT_STATE.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
            MANAGEMENT_STATE.unlink(missing_ok=True)
            Path('/var/backups/musicata-player-management').mkdir(parents=True,exist_ok=True,mode=0o700)
            files=management_files(saved,credentials,account.pw_uid,account.pw_gid)
            for path,(text,mode) in files.items():
                player.atomic_write(path,text,mode)
                os.chown(path,0,account.pw_gid if path==MANAGEMENT_CONFIG else 0)
            installer.run(['systemctl','daemon-reload'])
            installer.run(['systemctl','enable','--now',MANAGEMENT_UNIT.name])
            if not installer.succeeds(['systemctl','is-active','--quiet',MANAGEMENT_UNIT.name]):
                raise installer.InstallError('Management is paired but its service did not start. Check its journal; the credential is retained.')
    except Exception:
        if was_active and not enrolled:
            installer.run(['systemctl','start',MANAGEMENT_UNIT.name])
        raise
    print('Device management paired. Playback configuration and the current device are unchanged.')
    return 0


def main(argv=None):
    saved = json.loads(CONFIG.read_text()) if CONFIG.exists() else None
    argv = sys.argv[1:] if argv is None else argv
    if not saved and sys.stdin.isatty() and not any(arg == '--backend' or arg.startswith('--backend=') for arg in argv) and '--check' not in argv and '--remove-usb' not in argv:
        choice = input('Playback backend: Snapclient [s, recommended] or MPD compatibility [m]: ').strip().lower()
        if choice not in ('', 's', 'm'):
            raise installer.InstallError('Choose Snapclient or MPD.')
        argv = list(argv) + ['--backend', 'mpd' if choice == 'm' else 'snapclient']
    cfg = options(argv, saved)
    if cfg.get('pair_code'):
        return pair_management(cfg, saved)
    if cfg['remove_usb']:
        if os.geteuid() != 0:
            raise installer.InstallError('Run with sudo to remove USB control.')
        with open(LOCK, 'w') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            remove_usb()
        print('USB controller removed; the bound port is enabled. Snapclient remains installed.')
        return 0
    if cfg['backend'] == 'snapclient' and not cfg['server'] and sys.stdin.isatty() and not cfg['check']:
        cfg['server'] = input('Musicata / Snapserver host: ').strip()
        player.checked(cfg['server'], r'[A-Za-z0-9:][A-Za-z0-9.:-]*', 'Snapserver host')
    if cfg['backend'] == 'snapclient' and not cfg['server']:
        raise installer.InstallError('Select the Musicata / Snapserver host with --server or interactively.')
    token = None
    if cfg['backend'] == 'mpd':
        if USB_CONFIG.exists():
            raise installer.InstallError('Remove existing USB automation before configuring direct MPD.')
        if not cfg.get('musicata_url') or not cfg.get('token_file'):
            raise installer.InstallError('MPD registration requires --musicata-url and --token-file (administrator API token).')
        token = private_read(cfg['token_file']).strip()
        # Check credentials and URL before modifying local services.
        registration_request(cfg, token)
    try:
        outputs = player.parse_outputs(installer.run(['aplay', '-l']))
    except FileNotFoundError:
        outputs = player.proc_outputs()
    if not outputs:
        raise installer.InstallError('No playback outputs found. Connect the DAC and check audio permissions.')
    if not cfg['output'] and sys.stdin.isatty() and not cfg['check']:
        for index, output in enumerate(outputs, 1):
            print(f"{index}. {output['name']} ({output['device']})")
        selection = input('Select output number (blank cancels): ').strip()
        if selection.isdigit() and 1 <= int(selection) <= len(outputs):
            cfg['output'] = outputs[int(selection) - 1]['device']
    if cfg['output'] not in [output['device'] for output in outputs]:
        raise installer.InstallError('Select an available stable output; no default or replacement DAC is chosen automatically.')
    if not cfg['identity'] and cfg['backend'] == 'mpd':
        cfg['identity'] = str(uuid.uuid5(uuid.NAMESPACE_URL, 'musicata:mpd:' + cfg['mpd_bind']))
    if not cfg['identity']:
        machine_id = Path('/etc/machine-id').read_text().strip()
        player.checked(machine_id, r'[0-9a-f]{32}', 'machine identity')
        cfg['identity'] = str(uuid.uuid5(uuid.NAMESPACE_URL, 'musicata:snapclient:' + machine_id))
    release = installer.parse_os_release(Path('/etc/os-release').read_text())
    try:
        family = installer.distro_family(release)
    except installer.InstallError:
        family = 'generic'
    if not Path('/run/systemd/system').is_dir() or family == 'generic':
        print('Manual startup only; no system service or boot integration was installed:')
        print(generic_command(cfg))
        print('Run as your audio-capable non-root account; configure startup with your init system.')
        return 0
    unit_path = MPD_UNIT if cfg['backend'] == 'mpd' else UNIT
    service = unit_path.name
    if not saved and (unit_path.exists() or (cfg['backend'] == 'mpd' and MPD_CONFIG.exists())):
        raise installer.InstallError('Existing playback configuration is unmanaged here; use its original installer.')
    if saved and (not unit_path.is_file() or hashlib.sha256(unit_path.read_bytes()).hexdigest() != saved.get('unit_digest')):
        raise installer.InstallError('The managed service was customized or removed. Preserve and reconcile those changes before reconfiguring.')
    if saved and cfg['backend'] == 'mpd' and (not MPD_CONFIG.is_file() or
            hashlib.sha256(MPD_CONFIG.read_bytes()).hexdigest() != saved.get('config_digest')):
        raise installer.InstallError('The MPD configuration was customized; reconcile it before reconfiguring.')
    for other in ('snapclient.service', 'mpd.service', 'mpd.socket', 'musicata-mpd.service', 'musicata-snapclient.service'):
        if other == service:
            continue
        if installer.succeeds(['systemctl', 'is-active', '--quiet', other]) or installer.succeeds(['systemctl', 'is-enabled', '--quiet', other]):
            raise installer.InstallError(other + ' can claim audio outside this setup; resolve the conflict first.')
    if cfg['backend'] == 'mpd' and not saved:
        with socket.socket() as probe:
            probe.bind((cfg['mpd_bind'], 6600))
    missing = [name for name in dependencies(family, cfg['backend']) if not installer.package_installed(family, name)]
    print('Player:', cfg['identity'], '; server:', cfg.get('musicata_url') if cfg['backend'] == 'mpd' else cfg['server'], '; output:', cfg['output'])
    print('Dependencies:', ', '.join(missing) or 'already installed')
    usb = usb_files(cfg)
    if cfg['check']:
        return int(bool(missing or not shutil.which(cfg['backend'])))
    if os.geteuid() != 0:
        raise installer.InstallError('Run with sudo to install the player service.')
    if not cfg['yes']:
        if not sys.stdin.isatty() or input('Apply player setup? [y/N] ').lower() not in ('y', 'yes'):
            return 1
    with open(LOCK, 'w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if (json.loads(CONFIG.read_text()) if CONFIG.exists() else None) != saved:
            raise installer.InstallError('Installation changed; review the setup again.')
        installer.install_packages(family, missing)
        if cfg['backend'] == 'snapclient':
            with tempfile.TemporaryDirectory(prefix='musicata-player-') as directory:
                installer.install_snapcast(family, Path(directory), clients_only=True)
        try:
            account = pwd.getpwnam(cfg['user'])
        except KeyError:
            if cfg['user'] != 'musicata':
                raise installer.InstallError('The requested service account does not exist.')
            installer.run(['useradd', '--system', '--user-group', '--no-create-home', '--shell', '/usr/sbin/nologin', 'musicata'])
            account = pwd.getpwnam(cfg['user'])
        if account.pw_uid == 0:
            raise installer.InstallError('Playback account must not have UID 0.')
        available = player.parse_outputs(installer.run(['runuser', '-u', cfg['user'], '-G', 'audio', '--', 'aplay', '-l']))
        if cfg['output'] not in [output['device'] for output in available]:
            raise installer.InstallError('The service account cannot access the selected output.')
        files = playback_files(cfg, account.pw_uid, account.pw_gid)
        unit = files[unit_path][0]
        if cfg['backend'] == 'mpd':
            for path in (MPD_ROOT, MPD_ROOT / 'music', MPD_ROOT / 'playlists'):
                if path.is_symlink():
                    raise installer.InstallError('MPD state path must not be a symlink')
                path.mkdir(parents=True, exist_ok=True, mode=0o750)
                os.chown(path, account.pw_uid, account.pw_gid)
        manifest = {key: value for key, value in cfg.items() if key not in (
            'yes', 'check', 'pair_code', 'remove_usb', 'usb_device', 'musicata_url', 'token_file', 'confirm_isolated_usb')}
        if cfg['backend'] == 'mpd':
            manifest['musicata_url'] = cfg['musicata_url']
            manifest['config_digest'] = hashlib.sha256(files[MPD_CONFIG][0].encode()).hexdigest()
        manifest['unit_digest'] = hashlib.sha256(unit.encode()).hexdigest()
        backup = BACKUPS / datetime.datetime.now().strftime('%Y%m%d-%H%M%S-%f')
        files[CONFIG] = (json.dumps(manifest, indent=2) + '\n', 0o600)
        player.apply_files(files, service, backup, installer.run, installer.succeeds,
                           lambda: verify(cfg))
        if cfg['backend'] == 'mpd':
            try:
                register_mpd(cfg, token)
            except Exception as error:
                raise installer.InstallError('MPD is installed and answering locally, but Musicata registration/verification failed. '
                                             'The local setup is retained; fix connectivity/credentials and rerun. '
                                             'An offline registration may remain in Musicata: ' + str(error)) from error
        if usb:
            try:
                apply_usb(usb, backup / 'usb')
            except Exception as error:
                raise installer.InstallError('Snapclient was installed and connected, but optional USB setup failed. '
                                             'Its previous files were retained/restored; review the USB service before retrying: ' + str(error)) from error
    print('MPD registered and online in Musicata; no audible test was started.' if cfg['backend'] == 'mpd' else
          'Player connected. Select its Snapcast group in Musicata; no audible test was started.')
    print('Backup:', backup)
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (installer.InstallError, OSError, ValueError, KeyError) as error:
        print('Player setup error:', error, file=sys.stderr)
        sys.exit(1)
