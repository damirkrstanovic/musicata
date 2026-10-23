# SPDX-License-Identifier: AGPL-3.0-or-later
"""Shared host playback provisioning; no dependencies outside Python 3.9+."""
import os
import json
from pathlib import Path
import re
import shutil
import tempfile


def proc_outputs(root=Path('/proc/asound')):
    outputs = []
    for card in sorted(root.glob('card[0-9]*')):
        card_id = (card / 'id').read_text().strip()
        checked(card_id, r'[A-Za-z0-9_-]+', 'ALSA card identifier')
        for pcm in sorted(card.glob('pcm*p')):
            match = re.fullmatch(r'pcm(\d+)p', pcm.name)
            if not match:
                continue
            info = dict(line.split(': ', 1) for line in (pcm / 'info').read_text().splitlines() if ': ' in line)
            outputs.append({'device': f'plughw:CARD={card_id},DEV={match[1]}',
                            'name': f"{card_id}: {info.get('name', pcm.name)}"})
    return outputs


def parse_outputs(text):
    outputs = []
    for line in text.splitlines():
        match = re.match(r'card \d+: ([A-Za-z0-9_-]+) \[(.*?)\], device (\d+): (.*)', line)
        if match:
            card, name, device, description = match.groups()
            outputs.append({'device': f'plughw:CARD={card},DEV={device}',
                            'name': f'{name}: {description}'})
    identifiers = [output['device'] for output in outputs]
    if len(identifiers) != len(set(identifiers)):
        raise ValueError('Ambiguous ALSA card identifiers; resolve duplicate device names')
    return outputs


def checked(value, pattern, label):
    if not re.fullmatch(pattern, str(value)):
        raise ValueError('Invalid ' + label)
    return value


def mpd_config(device, root=Path('/var/lib/musicata-mpd'), dsp=False, bind='127.0.0.1', fifo=None):
    checked(device, r'[A-Za-z0-9_:,=.+-]+', 'ALSA output')
    checked(str(root), r'/[A-Za-z0-9_./-]+', 'MPD state directory')
    checked(bind, r'[A-Za-z0-9.:-]+', 'MPD bind address')
    if fifo is not None:
        checked(str(fifo), r'/[A-Za-z0-9_./-]+', 'Snapcast FIFO')
        if dsp:
            raise ValueError('Direct ALSA DSP routing cannot be combined with a Snapcast FIFO')
    output = 'hw:Loopback,0,0' if dsp else device
    audio_format = ' format "48000:32:2"\n' if dsp else ''
    config = f'''music_directory "{root}/music"
playlist_directory "{root}/playlists"
db_file "{root}/database"
state_file "{root}/state"
bind_to_address "{bind}"
port "6600"
restore_paused "yes"
'''
    if fifo is not None:
        return config + f'''audio_output {{
 type "fifo"
 name "Musicata Snapcast"
 path "{fifo}"
 format "48000:16:2"
 mixer_type "software"
}}
'''
    return config + f'''
audio_output {{
 type "alsa"
 name "Musicata ALSA"
 device "{output}"
{audio_format} mixer_type "software"
}}
'''


def audio_unit(uid, gid, description, command, state_directory=None):
    checked(uid, r'[0-9]+', 'service UID')
    checked(gid, r'[0-9]+', 'service GID')
    state = (f'StateDirectory={state_directory}\nStateDirectoryMode=0750\n'
             f'WorkingDirectory=/var/lib/{state_directory}\n') if state_directory else ''
    return f'''[Unit]
Description={description}
After=network-online.target sound.target
Wants=network-online.target
[Service]
User={uid}
Group={gid}
SupplementaryGroups=audio
ExecStart={command}
{state}UMask=0027
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
DevicePolicy=closed
DeviceAllow=char-alsa rw
[Install]
WantedBy=multi-user.target
'''


def mpd_unit(uid, gid, config=Path('/etc/musicata-mpd.conf')):
    checked(str(config), r'/[A-Za-z0-9_./-]+', 'MPD config path')
    return audio_unit(uid, gid, 'Musicata MPD playback',
                      f'/usr/bin/mpd --no-daemon {config}', 'musicata-mpd')


def snapclient_unit(uid, gid, device, host, identity, port=1704, binary='/usr/bin/snapclient'):
    checked(device, r'[A-Za-z0-9_:,=.+-]+', 'ALSA output')
    checked(host, r'[A-Za-z0-9:][A-Za-z0-9.:-]*', 'Snapserver host')
    checked(identity, r'[A-Za-z0-9][A-Za-z0-9:._-]*', 'Snapclient identity')
    checked(binary, r'/[A-Za-z0-9_./-]+', 'Snapclient executable')
    if not isinstance(port, int) or not 1 <= port <= 65535:
        raise ValueError('Invalid Snapserver port')
    return audio_unit(uid, gid, 'Musicata Snapclient playback',
                      f'{binary} --host {host} --port {port} --hostID {identity} --soundcard {device}')


def atomic_write(path, text, mode=0o644):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix='.' + path.name, dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as stream:
            stream.write(text)
            stream.flush()
            os.fchmod(stream.fileno(), mode)
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def apply_files(files, service, backup, run, succeeds, verify, before_restore=None):
    """Apply owned files; retain a private recovery copy and restore service state."""
    backup.mkdir(parents=True, mode=0o700)
    previous = {}
    active = succeeds(['systemctl', 'is-active', '--quiet', service])
    enabled = succeeds(['systemctl', 'is-enabled', '--quiet', service])
    for index, path in enumerate(files):
        if path.is_symlink():
            raise ValueError('Managed file is a symlink: ' + str(path))
        previous[path] = None
        if path.exists():
            previous[path] = backup / str(index)
            shutil.copy2(path, previous[path])
    atomic_write(backup / 'manifest.json', json.dumps({
        'files': {str(path): saved.name if saved else None for path, saved in previous.items()},
        'service': service, 'active': active, 'enabled': enabled,
    }, indent=2) + '\n', 0o600)
    service_touched = False
    try:
        for path, (text, mode) in files.items():
            atomic_write(path, text, mode)
        run(['systemctl', 'daemon-reload'])
        service_touched = True
        run(['systemctl', 'enable', service])
        run(['systemctl', 'restart', service])
        verify()
    except Exception as original:
        errors = []

        def restore(action):
            try:
                action()
            except Exception as error:
                errors.append(str(error))

        if service_touched:
            restore(lambda: run(['systemctl', 'stop', service]))
            if before_restore is not None:
                if errors:
                    raise RuntimeError(f'{original}; cannot stop service; retain binding and recover from {backup}: ' + '; '.join(errors)) from original
                try:
                    before_restore()
                except Exception as error:
                    raise RuntimeError(f'{original}; hardware restore failed ({error}); current binding retained, backup at {backup}') from original
            restore(lambda: run(['systemctl', 'enable' if enabled else 'disable', service]))
        for path, saved in previous.items():
            restore(lambda path=path, saved=saved: shutil.copy2(saved, path)
                    if saved else path.unlink(missing_ok=True))
        restore(lambda: run(['systemctl', 'daemon-reload']))
        if service_touched and active and not errors:
            restore(lambda: run(['systemctl', 'start', service]))
        if errors:
            raise RuntimeError(f'{original}; restore from {backup}: ' + '; '.join(errors)) from original
        raise
