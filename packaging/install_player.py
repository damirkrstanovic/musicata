#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Install a standalone Musicata Snapclient player without installing a server."""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pwd
import shlex
import shutil
import sys
import tempfile
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
import install as installer
import player
from usb_autowake import SnapcastStatus, UsbPort, PolicyClient, private_read

CONFIG = Path('/etc/musicata/player.json')
UNIT = Path('/etc/systemd/system/musicata-snapclient.service')
USB_UNIT = Path('/etc/systemd/system/musicata-usb-autowake.service')
USB_CONFIG = Path('/etc/musicata/usb-autowake.json')
USB_CODE = Path('/opt/musicata/player/usb_autowake.py')
USB_TOKEN = Path('/etc/musicata/usb-autowake.token')


def options(argv, saved=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--server', help='Snapserver host (port 1704; control port 1705)')
    parser.add_argument('--output', help='Stable ALSA output; otherwise select interactively')
    parser.add_argument('--user', help='Non-root playback account (default: musicata)')
    parser.add_argument('--check', action='store_true')
    parser.add_argument('--yes', action='store_true')
    parser.add_argument('--usb-device', help='Explicit sysfs USB device to bind (optional)')
    parser.add_argument('--musicata-url', help='Musicata HTTP(S) URL for USB policy')
    parser.add_argument('--token-file', help='Private root-owned Musicata API token file')
    parser.add_argument('--confirm-isolated-usb', action='store_true', help='Confirm this DAC port has independently verified isolated control')
    parser.add_argument('--remove-usb', action='store_true', help='Stop USB control, restore the DAC port, and remove only its managed files')
    result = {'server': '', 'output': '', 'identity': '', 'user': 'musicata'}
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
    return result


def dependencies(family):
    base = ['alsa-utils', 'ca-certificates']
    if family == 'debian':
        return base + ['snapclient', 'util-linux', 'passwd']
    build = ['git', 'cmake', 'make', 'pkgconf']
    if family == 'arch':
        return base + build + ['gcc', 'boost', 'alsa-lib', 'flac', 'libvorbis', 'opus', 'openssl', 'libsoxr', 'avahi', 'expat', 'shadow', 'util-linux']
    if family == 'fedora':
        return base + build + ['gcc-c++', 'boost-devel', 'alsa-lib-devel', 'flac-devel', 'libvorbis-devel', 'opus-devel', 'openssl-devel', 'soxr-devel', 'avahi-devel', 'expat-devel', 'shadow-utils', 'util-linux']
    return []


def generic_command(cfg):
    binary = shutil.which('snapclient')
    if not binary:
        raise installer.InstallError('Generic Linux needs an existing Snapclient executable. Install Snapcast 0.35.0 with BUILD_SERVER=OFF; see docs/install-linux.md.')
    return shlex.join([binary, '--host', cfg['server'], '--hostID', cfg['identity'], '--soundcard', cfg['output']])


def verify(cfg):
    import time
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


def main(argv=None):
    saved = json.loads(CONFIG.read_text()) if CONFIG.exists() else None
    cfg = options(sys.argv[1:] if argv is None else argv, saved)
    if cfg['remove_usb']:
        if os.geteuid() != 0:
            raise installer.InstallError('Run with sudo to remove USB control.')
        with open('/run/lock/musicata-install.lock', 'w') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            remove_usb()
        print('USB controller removed; the bound port is enabled. Snapclient remains installed.')
        return 0
    if not cfg['server'] and sys.stdin.isatty() and not cfg['check']:
        cfg['server'] = input('Musicata / Snapserver host: ').strip()
        player.checked(cfg['server'], r'[A-Za-z0-9:][A-Za-z0-9.:-]*', 'Snapserver host')
    if not cfg['server']:
        raise installer.InstallError('Select the Musicata / Snapserver host with --server or interactively.')
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
    if not saved and UNIT.exists():
        raise installer.InstallError('An existing musicata-snapclient service is unmanaged here; use its original installer.')
    if saved and (not UNIT.is_file() or hashlib.sha256(UNIT.read_bytes()).hexdigest() != saved.get('unit_digest')):
        raise installer.InstallError('The managed service was customized or removed. Preserve and reconcile those changes before reconfiguring.')
    for service in ('snapclient.service', 'mpd.service', 'mpd.socket', 'musicata-mpd.service'):
        if installer.succeeds(['systemctl', 'is-active', '--quiet', service]) or installer.succeeds(['systemctl', 'is-enabled', '--quiet', service]):
            raise installer.InstallError(service + ' can claim audio outside this setup; resolve the conflict first.')
    missing = [name for name in dependencies(family) if not installer.package_installed(family, name)]
    print('Player:', cfg['identity'], '; server:', cfg['server'], '; output:', cfg['output'])
    print('Dependencies:', ', '.join(missing) or 'already installed')
    usb = usb_files(cfg)
    if cfg['check']:
        return int(bool(missing or not shutil.which('snapclient')))
    if os.geteuid() != 0:
        raise installer.InstallError('Run with sudo to install the player service.')
    if not cfg['yes']:
        if not sys.stdin.isatty() or input('Apply player setup? [y/N] ').lower() not in ('y', 'yes'):
            return 1
    with open('/run/lock/musicata-install.lock', 'w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if (json.loads(CONFIG.read_text()) if CONFIG.exists() else None) != saved:
            raise installer.InstallError('Installation changed; review the setup again.')
        installer.install_packages(family, missing)
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
        unit = player.snapclient_unit(account.pw_uid, account.pw_gid, cfg['output'], cfg['server'],
                                      cfg['identity'], binary=shutil.which('snapclient'))
        manifest = {key: value for key, value in cfg.items() if key not in (
            'yes', 'check', 'remove_usb', 'usb_device', 'musicata_url', 'token_file', 'confirm_isolated_usb')}
        manifest['unit_digest'] = hashlib.sha256(unit.encode()).hexdigest()
        backup = Path('/var/backups/musicata-player') / datetime.datetime.now().strftime('%Y%m%d-%H%M%S-%f')
        player.apply_files({UNIT: (unit, 0o644), CONFIG: (json.dumps(manifest, indent=2) + '\n', 0o600)},
                           'musicata-snapclient.service', backup, installer.run, installer.succeeds,
                           lambda: verify(cfg))
        if usb:
            try:
                apply_usb(usb, backup / 'usb')
            except Exception as error:
                raise installer.InstallError('Snapclient was installed and connected, but optional USB setup failed. '
                                             'Its previous files were retained/restored; review the USB service before retrying: ' + str(error)) from error
    print('Player connected. Select its Snapcast group in Musicata; no audible test was started.')
    print('Backup:', backup)
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (installer.InstallError, OSError, ValueError, KeyError) as error:
        print('Player setup error:', error, file=sys.stderr)
        sys.exit(1)
