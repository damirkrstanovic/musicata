#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Distribution-aware Musicata installer. Python standard library only."""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import pwd
import re
import shlex
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request

REPO = 'damirkrstanovic/musicata'
CONFIG = Path('/etc/musicata/install.json')
DATA = Path('/var/lib/musicata')
ML_DATA = Path('/var/lib/musicata-ml')
UNIT = Path('/etc/systemd/system/musicata.service')
MPD_UNIT = Path('/etc/systemd/system/musicata-mpd.service')
MPD_CONFIG = Path('/etc/musicata-mpd.conf')
CURRENT = Path('/opt/musicata/current')
BACKUPS = Path('/var/backups/musicata')
NSS = Path('/etc/nsswitch.conf')
AVAHI = Path('/etc/avahi/services/musicata.service')
COMPONENTS = {'mpd', 'snapcast', 'airplay', 'spotify', 'discovery', 'ml'}

class InstallError(Exception):
    pass


def parse_os_release(text):
    result = {}
    for line in text.splitlines():
        if '=' in line and not line.startswith('#'):
            key, value = line.split('=', 1)
            words = shlex.split(value)
            result[key] = words[0] if words else ''
    return result


def distro_family(release):
    ids = [release.get('ID', '')] + release.get('ID_LIKE', '').split()
    for name in ids:
        if name in ('debian', 'ubuntu'): return 'debian'
        if name == 'arch': return 'arch'
        if name == 'fedora': return 'fedora'
    raise InstallError('Unsupported distribution: ' + release.get('ID', 'unknown') + '. Supported: Ubuntu, Debian, Arch derivatives, Fedora.')


def options(argv, saved=None):
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--mode', choices=['docker', 'native'])
    p.add_argument('--version', help='Release X.Y.Z (default: latest stable)')
    p.add_argument('--with', dest='components', help='Comma-separated: ' + ','.join(sorted(COMPONENTS)))
    p.add_argument('--user', help='Non-root service user (default: create musicata)')
    p.add_argument('--library', help='Local music directory (default /srv/music; network sources use the UI)')
    p.add_argument('--port', type=int)
    p.add_argument('--alsa-device', help='MPD ALSA device, e.g. plughw:CARD=Pro,DEV=0')
    p.add_argument('--upgrade', action='store_true', help='Upgrade an installation created by this installer')
    p.add_argument('--check', action='store_true', help='Report dependencies and blockers without changes')
    p.add_argument('--dry-run', action='store_true', help='Print the installation plan without changes or downloads')
    p.add_argument('--yes', action='store_true', help='Apply the displayed plan without an interactive prompt')
    args = vars(p.parse_args(argv))
    result = dict(mode='docker', version='latest', components=[], user='musicata', library='/srv/music', port=3030, alsa_device='default')
    if saved: result.update({k: saved[k] for k in result if k in saved})
    # An upgrade without a requested version selects the newest stable release.
    if args['upgrade']: result['version'] = 'latest'
    if args['components'] is not None:
        args['components'] = sorted(set(filter(None, args['components'].split(','))))
    result.update({k: v for k, v in args.items() if v is not None})
    if set(result['components']) - COMPONENTS: raise InstallError('Unknown component; use --help.')
    if result['mode'] == 'native' and 'ml' in result['components']:
        raise InstallError('musicata-ml is available only with --mode docker. A remote ML service can be configured in the web UI.')
    if any(c in result['components'] for c in ('airplay','spotify')):
        result['components'] = sorted(set(result['components']) | {'snapcast', 'discovery'})
    if not re.fullmatch(r'[a-z_][a-z0-9_-]*', result['user']) or result['user'] == 'root':
        raise InstallError('Use a non-root Linux account name for --user.')
    if not 1 <= result['port'] <= 65535: raise InstallError('Port must be between 1 and 65535.')
    if result['version'] != 'latest' and not re.fullmatch(r'v?\d+\.\d+\.\d+', result['version']):
        raise InstallError('Version must be latest or X.Y.Z.')
    result['version'] = result['version'].removeprefix('v')
    library = result['library']
    if not re.fullmatch(r'/[A-Za-z0-9_./ -]+', library) or '..' in Path(library).parts or library == '/':
        raise InstallError('Library must be an absolute directory path without control characters, specifiers, or parent traversal.')
    if not re.fullmatch(r'[A-Za-z0-9_:,=.+-]+', result['alsa_device']): raise InstallError('Invalid ALSA device identifier.')
    if saved:
        for key in ('mode', 'user', 'library', 'port', 'alsa_device', 'components'):
            if result[key] != saved[key]:
                raise InstallError(f'Upgrade preserves {key}; deployment migrations are not automatic. Keep the saved installation settings.')
    return result


def cpu_issues(machine, flags, mode, ml):
    if machine in ('aarch64','arm64'):
        return ['Published Docker images are amd64-only; use --mode native on ARM64.'] if mode == 'docker' else []
    if machine not in ('x86_64','amd64'): return ['Unsupported CPU architecture: ' + machine]
    required = {'lm','sse','sse2','ssse3','sse4_1','sse4_2','popcnt','cx16','lahf_lm'}
    missing = sorted(required - flags)
    if not flags.intersection({'pni','sse3'}): missing.append('sse3')
    issues = ['CPU lacks x86-64-v2 instructions: ' + ', '.join(missing)] if missing else []
    if ml:
        # ort's bundled native runtime targets v3 independently of Rust's v2 baseline.
        missing_ml = sorted({'avx','avx2','bmi1','bmi2','f16c','fma','movbe','xsave'} - flags)
        if not flags.intersection({'abm','lzcnt'}): missing_ml.append('lzcnt')
        if missing_ml: issues.append('ML requires x86-64-v3; missing: '+', '.join(missing_ml)+'. Omit ml and configure a remote ML service in /admin.')
    return issues


def packages(family, cfg):
    base = {'debian':['ca-certificates','curl','python3','tar'], 'arch':['ca-certificates','curl','python','tar'], 'fedora':['ca-certificates','curl','python3','tar']}[family].copy()
    base += ['systemd','util-linux',{'debian':'passwd','arch':'shadow','fedora':'shadow-utils'}[family]]
    if cfg['mode'] == 'docker': base += [{'debian':'docker.io','arch':'docker','fedora':'moby-engine'}[family]]
    components = cfg['components']
    if 'mpd' in components: base += ['mpd','alsa-utils']
    if 'discovery' in components:
        base += {'debian':['avahi-daemon','avahi-utils','libnss-mdns'], 'arch':['avahi','nss-mdns'], 'fedora':['avahi','avahi-tools','nss-mdns']}[family]
    # Managed Snapcast/cast-in binaries must live beside the Musicata process.
    if cfg['mode'] == 'native':
        if 'snapcast' in components: base += ['snapcast'] if family == 'arch' else ['snapserver','snapclient']
        if 'airplay' in components: base += ['shairport-sync']
        if 'spotify' in components: base += ['librespot']
    return sorted(set(base))


def run(argv, **kwargs):
    result = subprocess.run([str(a) for a in argv], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs)
    if result.returncode:
        raise InstallError(shlex.join(map(str, argv)) + '\n' + (result.stderr or result.stdout)[-4000:])
    return result.stdout.strip()


def succeeds(argv):
    return subprocess.run(list(map(str,argv)), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0


def package_installed(family, package):
    if family == 'debian':
        try: return run(['dpkg-query','-W','-f=${db:Status-Status}',package]) == 'installed'
        except InstallError: return False
    return succeeds(['pacman','-Q',package] if family == 'arch' else ['rpm','-q',package])


def install_packages(family, names):
    if not names: return
    print('Installing dependencies:', ', '.join(names), flush=True)
    if family == 'debian':
        run(['apt-get','update'])
        for name in names:
            if not succeeds(['apt-cache','show',name]):
                raise InstallError(f'Package {name} is unavailable in configured repositories. Enable the appropriate official distro repository or install the component first; no third-party repositories were added.')
        run(['apt-get','install','-y','--no-install-recommends',*names], env={**os.environ,'DEBIAN_FRONTEND':'noninteractive'})
    elif family == 'arch':
        # Arch does not support partial upgrades. Always refresh and upgrade together.
        run(['pacman','-Syu','--needed','--noconfirm',*names])
    else:
        run(['dnf','install','-y',*names])


def fetch_json(url):
    request = urllib.request.Request(url, headers={'User-Agent':'Musicata-installer','Accept':'application/vnd.github+json'})
    with urllib.request.urlopen(request, timeout=30) as response: return json.load(response)


def release_metadata(version):
    suffix = 'latest' if version == 'latest' else 'tags/v' + version
    release = fetch_json(f'https://api.github.com/repos/{REPO}/releases/{suffix}')
    version = release['tag_name'].removeprefix('v')
    if not re.fullmatch(r'\d+\.\d+\.\d+',version) or release.get('prerelease') or release.get('draft'):
        raise InstallError('Expected a published stable semantic-version release.')
    return version, release


def verify_sha256(path, expected):
    if not re.fullmatch('[0-9a-fA-F]{64}', expected or ''): raise InstallError('Release has no valid SHA-256 checksum; refusing an unverified native download.')
    h = hashlib.sha256()
    with open(path,'rb') as source:
        for chunk in iter(lambda:source.read(1024*1024),b''): h.update(chunk)
    if h.hexdigest() != expected.lower(): raise InstallError('Release SHA-256 checksum mismatch; nothing installed.')


def unpack_release(archive, dest, name):
    dest.mkdir(parents=True,exist_ok=True)
    with tarfile.open(archive) as tf:
        members = tf.getmembers()
        for member in members:
            parts = Path(member.name).parts
            if not parts or parts[0] != name or '..' in parts or not (member.isfile() or member.isdir()):
                raise InstallError('Unsafe path or link in release archive: ' + member.name)
        kwargs = {'filter':'data'} if hasattr(tarfile,'data_filter') else {}
        tf.extractall(dest, members=members, **kwargs)
    binary = dest/name/'musicata-server'
    if not binary.is_file(): raise InstallError('Release archive is missing musicata-server.')
    return binary.parent


def native_artifact(version, release, scratch):
    arch = 'aarch64' if platform.machine() in ('aarch64','arm64') else 'x86_64'
    name = 'musicata-' + arch + '-linux'
    asset = next((a for a in release['assets'] if a['name'] == name+'.tar.gz'), None)
    if not asset: raise InstallError('This release has no native archive for ' + arch)
    url = asset['browser_download_url']
    if not url.startswith(f'https://github.com/{REPO}/releases/download/v{version}/'):
        raise InstallError('Unexpected release download URL.')
    archive = scratch/'release.tar.gz'
    with urllib.request.urlopen(url,timeout=60) as source, archive.open('wb') as out: shutil.copyfileobj(source,out)
    digest = asset.get('digest') or ''
    if not digest.startswith('sha256:'):
        checksum = next((a for a in release['assets'] if a['name'] == name+'.tar.gz.sha256'),None)
        if checksum:
            checksum_url = checksum['browser_download_url']
            if not checksum_url.startswith(f'https://github.com/{REPO}/releases/download/v{version}/'): raise InstallError('Unexpected checksum URL.')
            with urllib.request.urlopen(checksum_url,timeout=30) as response: digest = 'sha256:'+response.read(4096).decode().split()[0]
    verify_sha256(archive,digest.removeprefix('sha256:'))
    extracted = unpack_release(archive,scratch/'unpacked',name)
    run([extracted/'musicata-server','--version'])  # Actual execution catches incompatible artifacts.
    return extracted


def atomic_write(path, text, mode=0o644):
    path.parent.mkdir(parents=True,exist_ok=True)
    temporary = path.with_name(path.name+'.new')
    temporary.write_text(text)
    temporary.chmod(mode)
    os.replace(temporary,path)


def unit_text(cfg, uid, gid):
    env = {'MUSICATA_DATABASE':str(DATA/'musicata.db'),'MUSICATA_ADDR':f'0.0.0.0:{cfg["port"]}','MUSICATA_LIBRARY':cfg['library']}
    if 'mpd' in cfg['components']:
        env.update(MUSICATA_MPD='127.0.0.1:6600',MUSICATA_PUBLIC_URL=f'http://127.0.0.1:{cfg["port"]}')
    lines = '\n'.join('Environment='+json.dumps(k+'='+v) for k,v in env.items())
    return f'''[Unit]
Description=Musicata music server
After=network-online.target
Wants=network-online.target
[Service]
User={uid}
Group={gid}
ExecStart={CURRENT}/musicata-server
WorkingDirectory={DATA}
StateDirectory=musicata
StateDirectoryMode=0750
UMask=0027
{lines}
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
[Install]
WantedBy=multi-user.target
'''


def configure_mpd(cfg, uid, gid):
    root = Path('/var/lib/musicata-mpd')
    for directory in (root,root/'music',root/'playlists'):
        directory.mkdir(exist_ok=True,parents=True);os.chown(directory,uid,gid);directory.chmod(0o750)
    # Preserve customized configuration on upgrades.
    if not MPD_CONFIG.exists():
        atomic_write(MPD_CONFIG,f'''music_directory "{root}/music"
playlist_directory "{root}/playlists"
db_file "{root}/database"
state_file "{root}/state"
bind_to_address "127.0.0.1"
port "6600"
restore_paused "yes"
audio_output {{
 type "alsa"
 name "Musicata ALSA"
 device "{cfg['alsa_device']}"
 mixer_type "software"
}}
''')
    atomic_write(MPD_UNIT,f'''[Unit]
Description=Musicata MPD playback
After=network.target sound.target
[Service]
User={uid}
Group={gid}
SupplementaryGroups=audio
ExecStart=/usr/bin/mpd --no-daemon {MPD_CONFIG}
StateDirectory=musicata-mpd
StateDirectoryMode=0750
WorkingDirectory={root}
UMask=0027
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
DevicePolicy=closed
DeviceAllow=char-alsa rw
[Install]
WantedBy=multi-user.target
''')


def docker_images(cfg, scratch):
    images = {}
    for component in ['server'] + (['ml'] if 'ml' in cfg['components'] else []):
        image = f'ghcr.io/{REPO}-{component}:{cfg["version"]}'
        print('Pulling', image, flush=True)
        try: run(['docker','pull',image])
        except InstallError as error:
            raise InstallError(f'Cannot pull {image}. Check release availability/network access. If GHCR reports unauthorized, make the package public or run sudo docker login ghcr.io with a token granting read:packages. Existing Musicata services have not been stopped.\n{error}') from error
        images[component] = run(['docker','image','inspect','--format','{{.Id}}',image])
    extra = []
    if 'snapcast' in cfg['components']: extra += ['snapserver']
    if 'airplay' in cfg['components']: extra += ['shairport-sync']
    if 'spotify' in cfg['components']: extra += ['librespot']
    if extra:
        # The base image is Debian even when the host is Fedora or Arch.
        dockerfile = scratch/'Dockerfile'
        dockerfile.write_text('FROM '+images['server']+'\nUSER root\nRUN apt-get update && apt-get install -y --no-install-recommends '+ ' '.join(extra)+' && rm -rf /var/lib/apt/lists/*\nUSER musicata\n')
        try: run(['docker','build','-t','musicata-installer:'+cfg['version'],scratch])
        except InstallError as error:
            raise InstallError('Optional audio packages could not be installed in the Debian server image. No running Musicata service was replaced. In particular librespot may be absent from configured repositories.\n'+str(error)) from error
        images['server'] = run(['docker','image','inspect','--format','{{.Id}}','musicata-installer:'+cfg['version']])
    return images


def docker_command(cfg, uid, gid, component, image):
    ml = component == 'ml'
    argv = ['docker','run','-d','--name','musicata-ml' if ml else 'musicata','--restart','unless-stopped','--network','host',
            '--user',f'{uid}:{gid}','--log-opt','max-size=10m','--log-opt','max-file=3',
            '-v',f'{ML_DATA if ml else DATA}:/data:Z']
    if ml: argv += ['-e','MUSICATA_ML_ADDR=127.0.0.1:3091']
    else:
        argv += ['-v',cfg['library']+':/music:ro,z','-e',f'MUSICATA_ADDR=0.0.0.0:{cfg["port"]}']
        if 'ml' in cfg['components']: argv += ['-e','MUSICATA_ML_SERVICE_URL=http://127.0.0.1:3091']
        if 'mpd' in cfg['components']: argv += ['-e','MUSICATA_MPD=127.0.0.1:6600','-e',f'MUSICATA_PUBLIC_URL=http://127.0.0.1:{cfg["port"]}']
        if 'airplay' in cfg['components']: argv += ['-v','/run/dbus:/run/dbus:ro']
    return argv + [image]


def healthy(url, timeout=60):
    end = time.monotonic()+timeout
    while time.monotonic()<end:
        try:
            with urllib.request.urlopen(url,timeout=3) as response:
                if response.status == 200: return
        except (OSError,ValueError): pass
        time.sleep(1)
    raise InstallError('Service did not become healthy: '+url)


def preflight(cfg, saved, family):
    issues = []
    flags = set()
    for line in Path('/proc/cpuinfo').read_text().splitlines():
        if line.startswith('flags'):
            this = set(line.split(':',1)[1].split())
            flags = this if not flags else flags & this
    issues += cpu_issues(platform.machine(),flags,cfg['mode'],'ml' in cfg['components'])
    if not Path('/run/systemd/system').is_dir(): issues.append('A running systemd host is required (containers/chroots are not installation targets).')
    if cfg['upgrade'] and not saved: issues.append('No installer-managed installation found. Existing manual installs require migration; they will not be overwritten.')
    if saved and not cfg['upgrade']: issues.append('Already installed. Use --upgrade to preserve the saved deployment settings.')
    if not saved:
        if UNIT.exists() or succeeds(['systemctl','is-active','--quiet','musicata.service']): issues.append('An unmanaged native Musicata service exists. Refusing to overwrite it.')
        if DATA.exists() and any(DATA.iterdir()): issues.append('Existing /var/lib/musicata data has no installer manifest. Refusing to adopt it silently.')
        if MPD_UNIT.exists() and 'mpd' in cfg['components']: issues.append('An unmanaged musicata-mpd.service exists.')
        if shutil.which('docker'):
            for name in ['musicata']+(['musicata-ml'] if 'ml' in cfg['components'] else []):
                if succeeds(['docker','inspect',name]): issues.append('Unmanaged container already exists: '+name)
        for port in [cfg['port']]+([3091] if 'ml' in cfg['components'] else [])+([6600] if 'mpd' in cfg['components'] else []):
            with socket.socket() as sock:
                try: sock.bind(('0.0.0.0',port))
                except OSError: issues.append(f'Port {port} is already in use or cannot be bound.')
    try:
        if pwd.getpwnam(cfg['user']).pw_uid == 0: issues.append('Service account must not have UID 0.')
    except KeyError:
        if cfg['user'] != 'musicata': issues.append('Requested service user does not exist: '+cfg['user'])
    for component, services in [('mpd',['mpd.service','mpd.socket']),('snapcast',['snapserver.service','snapcast.service']),('airplay',['shairport-sync.service'])]:
        if component in cfg['components']:
            for service in services:
                if succeeds(['systemctl','is-active','--quiet',service]): issues.append(f'{service} already runs outside Musicata. Resolve the service/port conflict before installing this component.')
    return issues


def configure_discovery(cfg):
    nss = NSS
    lines = nss.read_text().splitlines()
    for i,line in enumerate(lines):
        if line.startswith('hosts:') and not any('mdns' in token for token in line.split()):
            # Insert after files, before ordinary DNS; preserves resolved/other NSS modules.
            tokens = line.split(); at = tokens.index('files')+1 if 'files' in tokens else 1
            tokens[at:at] = ['mdns4_minimal','[NOTFOUND=return]']
            lines[i] = ' '.join(tokens)
    atomic_write(nss,'\n'.join(lines)+'\n')
    atomic_write(AVAHI,f'''<?xml version="1.0" standalone="no"?>
<!DOCTYPE service-group SYSTEM "avahi-service.dtd">
<service-group><name replace-wildcards="yes">Musicata on %h</name>
<service><type>_http._tcp</type><port>{cfg['port']}</port><txt-record>path=/</txt-record></service>
</service-group>
''')
    run(['systemctl','enable','--now','avahi-daemon.service'])


def deploy(cfg, saved, family, missing):
    # Reject unavailable releases before making package-manager changes.
    version, release = release_metadata(cfg['version'])
    cfg['version'] = version
    install_packages(family,missing)
    # Stop only package-provided audio daemons verified inactive in preflight.
    for component,services in [('mpd',['mpd.service','mpd.socket']),('snapcast',['snapserver.service','snapcast.service']),('airplay',['shairport-sync.service'])]:
        if component in cfg['components']:
            for service in services:
                if succeeds(['systemctl','cat',service]): run(['systemctl','disable','--now',service])
    with tempfile.TemporaryDirectory(prefix='musicata-install-') as work:
        scratch = Path(work)
        artifact = None
        if cfg['mode'] == 'docker':
            run(['systemctl','enable','--now','docker.service'])
            cfg['images'] = docker_images(cfg,scratch)
        else: artifact = native_artifact(version,release,scratch)
        try: account = pwd.getpwnam(cfg['user'])
        except KeyError:
            run(['useradd','--system','--user-group','--no-create-home','--shell','/usr/sbin/nologin','musicata'])
            account = pwd.getpwnam(cfg['user'])
        uid,gid = account.pw_uid,account.pw_gid
        for path in [DATA,Path(cfg['library'])]+([ML_DATA] if 'ml' in cfg['components'] else []):
            if not path.exists(): path.mkdir(parents=True);os.chown(path,uid,gid);path.chmod(0o750)
        if not succeeds(['runuser','-u',cfg['user'],'--','test','-r',cfg['library']]):
            raise InstallError('Service user cannot read the library directory. Grant access and rerun.')
        stamp = datetime.datetime.now().strftime('%Y%m%d-%H%M%S-%f')
        backup = BACKUPS/stamp
        backup.mkdir(parents=True,mode=0o700)
        backup.chmod(0o700)
        managed_files = [CONFIG,UNIT,MPD_UNIT,MPD_CONFIG,NSS,AVAHI]
        existing_files = {path for path in managed_files if path.exists()}
        for path in managed_files:
            if path.exists():
                dest = backup/'files'/path.relative_to('/');dest.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(path,dest)
        old_link = os.readlink(CURRENT) if CURRENT.is_symlink() else None
        old_containers = []
        stopped_containers = []
        new_containers = []
        stopped = False
        backup_complete = False
        try:
            # A consistent SQLite backup requires the server to be stopped (including WAL).
            if saved:
                if cfg['mode'] == 'native': run(['systemctl','stop','musicata.service'])
                else:
                    for name in ['musicata']+(['musicata-ml'] if 'ml' in cfg['components'] else []):
                        run(['docker','stop',name]); stopped_containers.append(name)
                        run(['docker','rename',name,name+'-previous-'+stamp]);old_containers.append(name)
                stopped = True
            if shutil.disk_usage(DATA).free < sum(p.stat().st_size for p in DATA.rglob('*') if p.is_file()) + 100*1024*1024:
                raise InstallError('Insufficient free space for a state backup.')
            with tarfile.open(backup/'state.tar.gz','w:gz') as tf: tf.add(DATA,arcname=DATA.name)
            backup_complete = True
            if 'mpd' in cfg['components']: configure_mpd(cfg,uid,gid)
            if cfg['mode'] == 'native':
                destination = CURRENT.parent/(version+'-'+stamp)
                CURRENT.parent.mkdir(parents=True,exist_ok=True)
                shutil.copytree(artifact,destination)
                temp_link = CURRENT.with_name('current.new');temp_link.symlink_to(destination);os.replace(temp_link,CURRENT)
                if not saved: atomic_write(UNIT,unit_text(cfg,uid,gid))
            if 'discovery' in cfg['components']: configure_discovery(cfg)
            run(['systemctl','daemon-reload'])
            if 'mpd' in cfg['components']:
                run(['systemd-analyze','verify',MPD_UNIT]);run(['systemctl','enable','--now','musicata-mpd.service'])
            if cfg['mode'] == 'native':
                run(['systemd-analyze','verify',UNIT]);run(['systemctl','enable','musicata.service']);run(['systemctl','restart','musicata.service'])
            else:
                for component in ['server']+(['ml'] if 'ml' in cfg['components'] else []):
                    name = 'musicata' if component == 'server' else 'musicata-ml'
                    new_containers.append(name)
                    run(docker_command(cfg,uid,gid,component,cfg['images'][component]))
            healthy(f'http://127.0.0.1:{cfg["port"]}/')
            if 'ml' in cfg['components']:
                print('Waiting for ML model download/startup (up to 5 minutes)…',flush=True)
                healthy('http://127.0.0.1:3091/health',300)
            atomic_write(CONFIG,json.dumps({k:v for k,v in cfg.items() if k not in ('check','dry_run','yes','upgrade')},indent=2)+'\n',0o600)
        except Exception as original_error:
            print('Activation failed. Recovering Musicata files/state; dependency packages are retained. Backup:',backup,file=sys.stderr,flush=True)
            recovery_errors = []
            def recover(label, action):
                try: action()
                except Exception as error: recovery_errors.append(label+': '+str(error))
            for name in new_containers:
                succeeds(['docker','rm','-f',name])
            if cfg['mode'] == 'native': succeeds(['systemctl','stop','musicata.service'])
            if not saved:
                succeeds(['systemctl','disable','musicata.service'])
                if 'mpd' in cfg['components']: succeeds(['systemctl','disable','--now','musicata-mpd.service'])
            def restore_link():
                if old_link or not saved:
                    CURRENT.unlink(missing_ok=True)
                    if old_link: CURRENT.symlink_to(old_link)
            recover('binary link',restore_link)
            for path in managed_files:
                if path not in existing_files: recover(str(path),lambda path=path:path.unlink(missing_ok=True))
            for file in (backup/'files').rglob('*'):
                if file.is_file():
                    dest = Path('/')/file.relative_to(backup/'files')
                    recover(str(dest),lambda file=file,dest=dest:shutil.copy2(file,dest))
            def restore_state():
                if backup_complete:
                    DATA.rename(DATA.with_name('musicata-failed-'+stamp))
                    with tarfile.open(backup/'state.tar.gz') as tf:
                        # Our root-private backup must preserve ownership on Python 3.14 too.
                        kwargs = {'filter':'fully_trusted'} if hasattr(tarfile,'fully_trusted_filter') else {}
                        tf.extractall(DATA.parent,**kwargs)
            recover('database/state',restore_state)
            recover('systemd reload',lambda:run(['systemctl','daemon-reload']))
            for name in old_containers:
                recover('container '+name,lambda name=name:run(['docker','rename',name+'-previous-'+stamp,name]))
            # Never launch an old binary against partially restored or migrated state.
            if not recovery_errors:
                for name in stopped_containers:
                    recover('start '+name,lambda name=name:run(['docker','start',name]))
                if saved and stopped and cfg['mode']=='native':
                    recover('start service',lambda:run(['systemctl','start','musicata.service']))
            if recovery_errors:
                raise InstallError(str(original_error)+'; manual recovery required from '+str(backup)+'; '+'; '.join(recovery_errors)) from original_error
            raise
        for name in old_containers:
            if not succeeds(['docker','rm',name+'-previous-'+stamp]):
                print('Old stopped container retained:',name+'-previous-'+stamp)
        hostname = socket.gethostname().removesuffix('.local') + '.local' if 'discovery' in cfg['components'] else '<server-ip>'
        print(f'Installed Musicata {version}. Open http://{hostname}:{cfg["port"]}/')
        print('Backup:',backup)
        print('Configure sources, players, Snapcast/cast-in, and optional ML in /admin. ML URL (if selected): http://127.0.0.1:3091')


def main(argv=None):
    saved = json.loads(CONFIG.read_text()) if CONFIG.exists() else None
    cfg = options(sys.argv[1:] if argv is None else argv,saved)
    family = distro_family(parse_os_release(Path('/etc/os-release').read_text()))
    names = packages(family,cfg)
    missing = [p for p in names if not package_installed(family,p)]
    issues = preflight(cfg,saved,family)
    print(f'Distribution: {family}; installation: {cfg["mode"]}; release: {cfg["version"]}')
    print('Components:', ', '.join(['server']+cfg['components']))
    print('Service user:',cfg['user'],'; library:',cfg['library'],'; port:',cfg['port'])
    print('Dependencies to install:',', '.join(missing) or 'none')
    if family == 'arch' and missing: print('Arch package installation includes a full system upgrade (pacman -Syu); partial upgrades are unsupported.')
    if cfg['mode']=='docker': print('Docker uses host networking for LAN audio/discovery. ML, if selected, binds localhost only.')
    for issue in issues: print('BLOCKER:',issue)
    if cfg['check'] or cfg['dry_run']: return 1 if issues or (cfg['check'] and missing) else 0
    if issues: raise InstallError('Resolve the blockers above before installing.')
    if os.geteuid()!=0: raise InstallError('Run with sudo to install dependencies and system services. --check and --dry-run do not require root.')
    if not cfg['yes']:
        if not sys.stdin.isatty(): raise InstallError('Non-interactive installation requires --yes. Review --dry-run first.')
        if input('Apply this installation plan? [y/N] ').lower() not in ('y','yes'): return 0
    with open('/run/lock/musicata-install.lock','w') as lock:
        try: fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        except BlockingIOError: raise InstallError('Another Musicata installer is already running.')
        current_saved = json.loads(CONFIG.read_text()) if CONFIG.exists() else None
        if current_saved != saved: raise InstallError('Installation changed while reviewing the plan; rerun the installer.')
        deploy(cfg,saved,family,missing)
    return 0


if __name__=='__main__':
    try: sys.exit(main())
    except (InstallError,OSError,ValueError,KeyError) as error:
        print('Installation error:',error,file=sys.stderr);sys.exit(1)
