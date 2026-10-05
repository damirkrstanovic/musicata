#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Distribution-aware Musicata installer. Python standard library only."""
import argparse
import datetime
import fcntl
import hashlib
import http.client
import json
import os
from pathlib import Path
import platform
import pwd
import re
import shlex
import shutil
import socket
import sqlite3
import stat
import ssl
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
DSP_CONFIG = Path('/etc/musicata-camilladsp.json')
DSP_UNIT = Path('/etc/systemd/system/musicata-camilladsp.service')
DSP_MODULE = Path('/etc/modules-load.d/musicata-dsp.conf')
CURRENT = Path('/opt/musicata/current')
BACKUPS = Path('/var/backups/musicata')
NSS = Path('/etc/nsswitch.conf')
AVAHI = Path('/etc/avahi/services/musicata.service')
CADDY_CONFIG = Path('/etc/musicata/caddy/Caddyfile')
CADDY_UNIT = Path('/etc/systemd/system/musicata-caddy.service')
CADDY_OVERRIDE = Path('/etc/systemd/system/musicata.service.d/caddy.conf')
CADDY_ROOT = Path('/var/lib/musicata-caddy')
CADDY_ENV = Path('/etc/musicata/caddy/cloudflare.env')
CADDY_BINARY = Path('/opt/musicata/caddy')
CADDY_VERSION = 'v2.11.7'
CLOUDFLARE_VERSION = 'v0.2.4'
XCADDY_VERSION = 'v0.4.7'
COMPONENTS = {'caddy', 'dsp', 'mpd', 'snapcast', 'airplay', 'spotify', 'discovery', 'ml'}

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
    p.add_argument('--tls-domain', help='Public DNS hostname for optional Caddy HTTPS')
    p.add_argument('--tls-email', help='Optional ACME account email for Caddy')
    p.add_argument('--tls-dns', choices=['public','cloudflare'], help='ACME validation: public HTTP/TLS or Cloudflare DNS-01 for private LAN access')
    p.add_argument('--tls-dns-token-file', help='Private file containing a scoped Cloudflare API token; never pass the token itself')
    p.add_argument('--upgrade', action='store_true', help='Upgrade an installation created by this installer')
    p.add_argument('--check', action='store_true', help='Report dependencies and blockers without changes')
    p.add_argument('--dry-run', action='store_true', help='Print the installation plan without changes or downloads')
    p.add_argument('--yes', action='store_true', help='Apply the displayed plan without an interactive prompt')
    args = vars(p.parse_args(argv))
    result = dict(mode='docker', version='latest', components=[], user='musicata', library='/srv/music', port=3030, alsa_device='default', tls_domain='', tls_email='', tls_dns='public', tls_dns_token_file='')
    if saved: result.update({k: saved[k] for k in result if k in saved})
    result['tls_dns_token_file'] = ''  # Input path is not an installation setting.
    # An upgrade without a requested version selects the newest stable release.
    if args['upgrade']: result['version'] = 'latest'
    if args['components'] is not None:
        args['components'] = sorted(set(filter(None, args['components'].split(','))))
    result.update({k: v for k, v in args.items() if v is not None})
    if set(result['components']) - COMPONENTS: raise InstallError('Unknown component; use --help.')
    if 'dsp' in result['components'] and (result['mode'] != 'native' or 'mpd' not in result['components']):
        raise InstallError('DSP preparation requires --mode native --with mpd,dsp and an installed CamillaDSP 4.1 executable.')
    if 'dsp' in result['components'] and result['alsa_device'] == 'default':
        raise InstallError('DSP preparation needs an explicit physical DAC --alsa-device, for example plughw:CARD=USB,DEV=0.')
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
    if 'caddy' in result['components']:
        domain = result['tls_domain'].lower()
        labels = domain.split('.')
        if len(domain)>253 or len(labels)<2 or not all(re.fullmatch(r'[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?',label) for label in labels) or not re.fullmatch(r'[a-z][a-z0-9-]+',labels[-1]) or labels[-1] in ('local','localhost','internal','test','invalid','example') or domain.endswith('.home.arpa'):
            raise InstallError('Caddy requires --tls-domain with a public DNS hostname you control, not a URL, IP address or .local name.')
        result['tls_domain'] = domain
        if result['port'] in (80,443): raise InstallError('Caddy reserves ports 80 and 443; choose another Musicata backend port.')
        if result['tls_email'] and not re.fullmatch(r'[A-Za-z0-9_.+%-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}',result['tls_email']):
            raise InstallError('Invalid --tls-email address.')
        if result['tls_dns']=='cloudflare' and not result['tls_dns_token_file'] and not (saved and saved.get('tls_dns')=='cloudflare'):
            raise InstallError('Cloudflare DNS validation requires --tls-dns-token-file on first installation.')
        if result['tls_dns_token_file'] and result['tls_dns']!='cloudflare':
            raise InstallError('--tls-dns-token-file requires --tls-dns cloudflare.')
    elif result['tls_domain'] or result['tls_email'] or result['tls_dns']!='public' or result['tls_dns_token_file']:
        raise InstallError('TLS options require --with caddy.')
    if saved:
        for key in ('mode', 'user', 'library', 'port', 'alsa_device', 'components'):
            if result[key] != saved[key]:
                if key=='components' and 'caddy' not in saved[key] and set(result[key])==set(saved[key])|{'caddy'}: continue
                raise InstallError(f'Upgrade preserves {key}; deployment migrations are not automatic. Keep the saved installation settings.')
        if 'caddy' in saved['components']:
            for key in ('tls_domain','tls_email','tls_dns'):
                if result[key]!=saved.get(key,'public' if key=='tls_dns' else ''): raise InstallError(f'Upgrade preserves {key}; retain the saved TLS settings.')
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
        if 'caddy' in components:
            base += ['caddy']
            if cfg['tls_dns']=='cloudflare': base += [{'debian':'golang-go','arch':'go','fedora':'golang'}[family]]
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
    bind = '127.0.0.1' if 'caddy' in cfg['components'] else '0.0.0.0'
    env = {'MUSICATA_DATABASE':str(DATA/'musicata.db'),'MUSICATA_ADDR':f'{bind}:{cfg["port"]}','MUSICATA_LIBRARY':cfg['library']}
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


def caddy_text(cfg):
    email = '    email '+cfg['tls_email']+'\n' if cfg['tls_email'] else ''
    dns = '    tls {\n        dns cloudflare {env.CLOUDFLARE_API_TOKEN}\n        resolvers 1.1.1.1 1.0.0.1\n    }\n' if cfg['tls_dns']=='cloudflare' else ''
    return f'''{{
    admin off
    acme_ca https://acme-v02.api.letsencrypt.org/directory
{email}}}
{cfg['tls_domain']} {{
{dns}    reverse_proxy 127.0.0.1:{cfg['port']} {{
        header_down Set-Cookie "^(musicata_session=.*)$" "$1; Secure"
    }}
}}
'''


def caddy_unit_text(cfg, uid, gid):
    binary = CADDY_BINARY if cfg['tls_dns']=='cloudflare' else '/usr/bin/caddy'
    credentials = f'EnvironmentFile={CADDY_ENV}\n' if cfg['tls_dns']=='cloudflare' else ''
    return f'''[Unit]
Description=Musicata HTTPS proxy
After=network-online.target musicata.service
Wants=network-online.target
[Service]
User={uid}
Group={gid}
{credentials}ExecStart={binary} run --config {CADDY_CONFIG} --adapter caddyfile
Environment=XDG_DATA_HOME={CADDY_ROOT}/data
Environment=XDG_CONFIG_HOME={CADDY_ROOT}/config
StateDirectory=musicata-caddy
StateDirectoryMode=0750
UMask=0027
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
Restart=on-failure
RestartSec=5
[Install]
WantedBy=multi-user.target
'''


def read_dns_token(path):
    path = Path(path)
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077 or info.st_size > 4096:
        raise InstallError('Cloudflare token must be a regular file with mode 0600 (no symlinks).')
    token = path.read_text().strip()
    if not re.fullmatch(r'[A-Za-z0-9_-]{20,256}',token):
        raise InstallError('Invalid Cloudflare API token file; expected one scoped API token.')
    return token


def build_caddy(scratch):
    # Go's public checksum database verifies the pinned modules and their dependencies.
    environment = {**os.environ,'GOBIN':str(scratch),'GOTOOLCHAIN':'auto',
                   'GOSUMDB':'sum.golang.org','GOPROXY':'https://proxy.golang.org',
                   'GOPRIVATE':'','GONOSUMDB':'','GONOPROXY':''}
    version = re.search(r'go1\.(\d+)',run(['go','version']))
    if not version or int(version[1])<21:
        release = fetch_json('https://go.dev/dl/?mode=json')[0]
        arch = 'arm64' if platform.machine() in ('aarch64','arm64') else 'amd64'
        package = next(f for f in release['files'] if f['os']=='linux' and f['arch']==arch and f['kind']=='archive')
        archive = scratch/'go.tar.gz'
        urllib.request.urlretrieve('https://go.dev/dl/'+package['filename'],archive)
        verify_sha256(archive,package['sha256'])
        with tarfile.open(archive) as tf: tf.extractall(scratch)
        environment['PATH'] = str(scratch/'go/bin')+os.pathsep+environment.get('PATH','')
    print('Building Caddy with the Cloudflare DNS module…',flush=True)
    run(['go','install','github.com/caddyserver/xcaddy/cmd/xcaddy@'+XCADDY_VERSION],env=environment)
    binary = scratch/'caddy'
    run([scratch/'xcaddy','build',CADDY_VERSION,'--with',
         'github.com/caddy-dns/cloudflare@'+CLOUDFLARE_VERSION,'--output',binary],env=environment,cwd=scratch)
    return binary


def cloudflare_dockerfile():
    return f'''FROM caddy:2-builder AS builder
RUN GOSUMDB=sum.golang.org GOPROXY=https://proxy.golang.org xcaddy build {CADDY_VERSION} --with github.com/caddy-dns/cloudflare@{CLOUDFLARE_VERSION}
FROM caddy:2-alpine
COPY --from=builder /usr/bin/caddy /usr/bin/caddy
RUN setcap cap_net_bind_service=+ep /usr/bin/caddy
'''


def prepare_caddy(cfg, scratch):
    candidate = CADDY_CONFIG
    if not candidate.exists():
        candidate = scratch/'Caddyfile'
        candidate.write_text(caddy_text(cfg))
    credentials = []
    environment = os.environ.copy()
    binary = '/usr/bin/caddy'
    if cfg['tls_dns']=='cloudflare':
        if cfg['tls_dns_token_file']:
            token = read_dns_token(cfg['tls_dns_token_file'])
        else:
            token = CADDY_ENV.read_text().removeprefix('CLOUDFLARE_API_TOKEN=').strip()
            if not re.fullmatch(r'[A-Za-z0-9_-]{20,256}',token): raise InstallError('Saved Cloudflare credentials are invalid.')
        cfg['_dns_env'] = scratch/'cloudflare.env'
        atomic_write(cfg['_dns_env'],'CLOUDFLARE_API_TOKEN='+token+'\n',0o600)
        environment['CLOUDFLARE_API_TOKEN'] = token
        credentials = ['--env-file',cfg['_dns_env']]
        if cfg['mode']=='native':
            binary = build_caddy(scratch)
            cfg['_caddy_binary'] = binary
    if cfg['mode']=='native':
        run([binary,'validate','--config',candidate,'--adapter','caddyfile'],env=environment)
    else:
        run(['docker','run','--rm','--network','none',*credentials,'-v',f'{candidate}:/etc/caddy/Caddyfile:ro,z',
             cfg['images']['caddy'],'caddy','validate','--config','/etc/caddy/Caddyfile','--adapter','caddyfile'])


def expire_http_sessions():
    database = DATA/'musicata.db'
    if database.exists():
        connection = sqlite3.connect(database)
        try:
            if connection.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='sessions'").fetchone():
                connection.execute('DELETE FROM sessions')
                connection.commit()
        finally: connection.close()


def configure_caddy(cfg, uid, gid):
    if cfg['mode']=='native':
        account = pwd.getpwnam('caddy')  # Created by the distribution's Caddy package.
        uid,gid = account.pw_uid,account.pw_gid
    for path in (CADDY_ROOT,CADDY_ROOT/'data',CADDY_ROOT/'config'):
        path.mkdir(parents=True,exist_ok=True);os.chown(path,uid,gid);path.chmod(0o750)
    CADDY_CONFIG.parent.mkdir(parents=True,exist_ok=True)
    os.chown(CADDY_CONFIG.parent,0,gid);CADDY_CONFIG.parent.chmod(0o750)
    if not CADDY_CONFIG.exists():
        atomic_write(CADDY_CONFIG,caddy_text(cfg),0o640);os.chown(CADDY_CONFIG,0,gid)
    if cfg['tls_dns']=='cloudflare':
        shutil.copy2(cfg['_dns_env'],CADDY_ENV);os.chown(CADDY_ENV,0,0);CADDY_ENV.chmod(0o600)
        if cfg['mode']=='native':
            CADDY_BINARY.parent.mkdir(parents=True,exist_ok=True)
            shutil.copy2(cfg['_caddy_binary'],CADDY_BINARY);os.chown(CADDY_BINARY,0,0);CADDY_BINARY.chmod(0o755)
    if cfg['mode']=='native':
        if not CADDY_UNIT.exists(): atomic_write(CADDY_UNIT,caddy_unit_text(cfg,uid,gid))
        atomic_write(CADDY_OVERRIDE,f'[Service]\nEnvironment="MUSICATA_ADDR=127.0.0.1:{cfg["port"]}"\n')


def docker_components(cfg):
    return ['server'] + (['ml'] if 'ml' in cfg['components'] else []) + (['caddy'] if 'caddy' in cfg['components'] else [])


def processor_config(cfg):
    return {'devices': {'samplerate':48000, 'chunksize':1024, 'queuelimit':4,
        'silence_threshold':-100, 'silence_timeout':0,
        'capture':{'type':'Alsa','channels':2,'device':'hw:Loopback,1,0','format':'S32_LE'},
        'playback':{'type':'Alsa','channels':2,'device':cfg['alsa_device'],'format':'S32_LE'}},
        'filters':{}, 'pipeline':[]}


def configure_dsp(cfg, uid, gid, runtime_changes=None):
    executable = shutil.which('camilladsp')
    if not executable: raise InstallError('Install CamillaDSP 4.1 first; no unverified executable is downloaded.')
    if not DSP_CONFIG.exists(): atomic_write(DSP_CONFIG,json.dumps(processor_config(cfg),indent=2)+'\n')
    run([executable,'--check',str(DSP_CONFIG)])
    was_loaded = Path('/sys/module/snd_aloop').exists()
    run(['modprobe','snd-aloop'])
    if runtime_changes is not None and not was_loaded: runtime_changes['loopback_loaded'] = True
    atomic_write(DSP_MODULE,'snd-aloop\n')
    atomic_write(DSP_UNIT,f'''[Unit]
Description=Musicata output correction
After=sound.target
Before=musicata-mpd.service
[Service]
User={uid}
Group={gid}
SupplementaryGroups=audio
ExecStart={executable} -w -a 127.0.0.1 -p 1234 {DSP_CONFIG}
Restart=on-failure
NoNewPrivileges=true
[Install]
WantedBy=multi-user.target
''')


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
 device "{'hw:Loopback,0,0' if 'dsp' in cfg['components'] else cfg['alsa_device']}"
{(' format "48000:32:2"' if 'dsp' in cfg['components'] else '')}
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
    if 'caddy' in cfg['components']:
        image = 'caddy:2-alpine'
        if cfg['tls_dns']=='cloudflare':
            build_dir=scratch/'caddy-image';build_dir.mkdir()
            (build_dir/'Dockerfile').write_text(cloudflare_dockerfile())
            image='musicata-caddy-cloudflare:'+CADDY_VERSION[1:]+'-'+CLOUDFLARE_VERSION[1:]
            run(['docker','build','--pull','-t',image,build_dir])
            images['caddy']=run(['docker','image','inspect','--format','{{.Id}}',image])
            return images
        print('Pulling',image,flush=True)
        run(['docker','pull',image])
        images['caddy'] = run(['docker','image','inspect','--format','{{.Id}}',image])
    return images


def docker_command(cfg, uid, gid, component, image):
    if component == 'caddy':
        return ['docker','run','-d','--name','musicata-caddy','--restart','unless-stopped','--network','host',
            '--user',f'{uid}:{gid}','--cap-drop','ALL','--cap-add','NET_BIND_SERVICE',
            '--log-opt','max-size=10m','--log-opt','max-file=3',
            '-v',f'{CADDY_CONFIG}:/etc/caddy/Caddyfile:ro,z',
            '-v',f'{CADDY_ROOT}/data:/data:z','-v',f'{CADDY_ROOT}/config:/config:z',
            *(['--env-file',str(CADDY_ENV)] if cfg['tls_dns']=='cloudflare' else []),
            image,'caddy','run','--config','/etc/caddy/Caddyfile','--adapter','caddyfile']
    ml = component == 'ml'
    argv = ['docker','run','-d','--name','musicata-ml' if ml else 'musicata','--restart','unless-stopped','--network','host',
            '--user',f'{uid}:{gid}','--log-opt','max-size=10m','--log-opt','max-file=3',
            '-v',f'{ML_DATA if ml else DATA}:/data:Z']
    if ml: argv += ['-e','MUSICATA_ML_ADDR=127.0.0.1:3091']
    else:
        bind = '127.0.0.1' if 'caddy' in cfg['components'] else '0.0.0.0'
        argv += ['-v',cfg['library']+':/music:ro,z','-e',f'MUSICATA_ADDR={bind}:{cfg["port"]}']
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


def verify_loopback_backend(port):
    # Check the effective listener, including custom systemd environment/drop-ins.
    found = False
    for name,loopback in (('tcp','0100007F'),('tcp6','00000000000000000000000001000000')):
        table = Path('/proc/net')/name
        if not table.exists(): continue
        for line in table.read_text().splitlines()[1:]:
            columns = line.split()
            address,number = columns[1].split(':')
            if columns[3]=='0A' and int(number,16)==port:
                if address!=loopback:
                    raise InstallError('Musicata backend is exposed beyond loopback; check custom service bind settings.')
                found = True
    if not found: raise InstallError('No loopback Musicata backend listener found.')


def healthy_tls(domain, timeout=180):
    # Connect locally, but validate the public certificate and send its hostname/SNI.
    # This works even when the router cannot loop back through its public address.
    context = ssl.create_default_context()
    end = time.monotonic()+timeout
    while time.monotonic()<end:
        try:
            with socket.create_connection(('127.0.0.1',443),timeout=3) as raw:
                with context.wrap_socket(raw,server_hostname=domain) as tls:
                    connection = http.client.HTTPSConnection(domain,timeout=3)
                    connection.sock = tls
                    connection.request('GET','/api/health')
                    if connection.getresponse().status==200: return
        except (OSError,http.client.HTTPException): pass
        time.sleep(1)
    raise InstallError(f'Caddy HTTPS health/certificate check failed for {domain}. Check Caddy logs, DNS validation credentials/permissions, outbound DNS/HTTPS, or public ports 80/443 when using public validation.')


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
    if 'caddy' in cfg['components']:
        if cfg['tls_dns_token_file']:
            try: read_dns_token(cfg['tls_dns_token_file'])
            except (OSError,InstallError) as error: issues.append(str(error))
        if cfg['mode']=='native' and UNIT.exists():
            effective = UNIT.read_text()+'\n'+run(['systemctl','cat','musicata.service'])
            if re.search(r'--addr(?:[=\s]|$)',effective):
                issues.append('Custom Musicata service uses --addr, which overrides the loopback environment. Remove the CLI --addr from the unit/drop-ins before enabling Caddy.')
        managed = bool(saved and 'caddy' in saved['components'])
        if not managed:
            if any(path.exists() for path in (CADDY_CONFIG,CADDY_UNIT,CADDY_OVERRIDE,CADDY_ENV,CADDY_BINARY)) or (CADDY_ROOT.exists() and any(p.is_file() for p in CADDY_ROOT.rglob('*'))):
                issues.append('Existing Caddy configuration/state is unmanaged; refusing to overwrite it.')
            if shutil.which('docker') and succeeds(['docker','inspect','musicata-caddy']):
                issues.append('Unmanaged container already exists: musicata-caddy')
            for port in (80,443):
                for family,address in ((socket.AF_INET,'0.0.0.0'),(socket.AF_INET6,'::')):
                    try:
                        with socket.socket(family) as sock:
                            if family==socket.AF_INET6: sock.setsockopt(socket.IPPROTO_IPV6,socket.IPV6_V6ONLY,1)
                            sock.bind((address,port))
                    except OSError as error:
                        if family==socket.AF_INET6 and error.errno in (97,99): continue  # IPv6 disabled.
                        issues.append(f'Caddy port {port} ({address}) is already in use or cannot be bound.')
        services = ['caddy.service','caddy-api.service']+([] if managed else ['musicata-caddy.service'])
        for service in services:
            if succeeds(['systemctl','is-active','--quiet',service]) or succeeds(['systemctl','is-enabled','--quiet',service]):
                issues.append(f'{service} is active/enabled outside this installation. Resolve the Caddy service conflict first.')
    try:
        if pwd.getpwnam(cfg['user']).pw_uid == 0: issues.append('Service account must not have UID 0.')
    except KeyError:
        if cfg['user'] != 'musicata': issues.append('Requested service user does not exist: '+cfg['user'])
    for component, services in [('mpd',['mpd.service','mpd.socket']),('snapcast',['snapserver.service','snapcast.service']),('airplay',['shairport-sync.service'])]:
        if component in cfg['components']:
            for service in services:
                if succeeds(['systemctl','is-active','--quiet',service]): issues.append(f'{service} already runs outside Musicata. Resolve the service/port conflict before installing this component.')
    if 'dsp' in cfg['components']:
        if not shutil.which('camilladsp'): issues.append('Install CamillaDSP 4.1 before selecting dsp; no third-party packages are downloaded.')
        elif not re.search(r'\b4\.1\.',run(['camilladsp','--version'])): issues.append('DSP preparation supports CamillaDSP 4.1.x.')
        if not succeeds(['modprobe','--dry-run','snd-aloop']): issues.append('Kernel ALSA loopback module snd-aloop is unavailable.')
        if not saved and (DSP_UNIT.exists() or DSP_CONFIG.exists()): issues.append('Existing processor configuration is unmanaged; refusing to overwrite it.')
        if MPD_CONFIG.exists() and not saved: issues.append('DSP preparation only configures new installer-managed MPD routing; existing routing requires manual backed-up migration.')
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
    if 'caddy' in cfg['components']:
        # The loopback HTTP backend is not discoverable; HTTPS uses the public DNS name.
        AVAHI.unlink(missing_ok=True)
    else:
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
    if 'caddy' in missing and cfg['mode']=='native':
        # A newly installed distro package may auto-start its stock example service.
        if succeeds(['systemctl','cat','caddy.service']): run(['systemctl','disable','--now','caddy.service'])
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
        if 'caddy' in cfg['components']: prepare_caddy(cfg,scratch)
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
        managed_files = [CONFIG,UNIT,MPD_UNIT,MPD_CONFIG,NSS,AVAHI] + ([DSP_CONFIG,DSP_UNIT,DSP_MODULE] if "dsp" in cfg["components"] else []) + ([CADDY_CONFIG,CADDY_UNIT,CADDY_OVERRIDE,CADDY_ENV,CADDY_BINARY] if 'caddy' in cfg['components'] else [])
        caddy_managed = bool(saved and 'caddy' in saved['components'])
        caddy_was_active = bool(caddy_managed and cfg['mode']=='native' and succeeds(['systemctl','is-active','--quiet','musicata-caddy.service']))
        caddy_was_enabled = bool(caddy_managed and cfg['mode']=='native' and succeeds(['systemctl','is-enabled','--quiet','musicata-caddy.service']))
        dsp_was_active = bool(saved and "dsp" in cfg["components"] and succeeds(["systemctl","is-active","--quiet","musicata-camilladsp.service"]))
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
        caddy_backup_complete = False
        runtime_changes = {}
        try:
            # A consistent SQLite backup requires the server to be stopped (including WAL).
            if saved:
                if cfg['mode'] == 'native':
                    if caddy_managed: run(['systemctl','stop','musicata-caddy.service'])
                    run(['systemctl','stop','musicata.service'])
                else:
                    previous = ['musicata']+(['musicata-ml'] if 'ml' in cfg['components'] else [])+(['musicata-caddy'] if caddy_managed else [])
                    for name in previous:
                        run(['docker','stop',name]); stopped_containers.append(name)
                        run(['docker','rename',name,name+'-previous-'+stamp]);old_containers.append(name)
                stopped = True
            if shutil.disk_usage(DATA).free < sum(p.stat().st_size for p in DATA.rglob('*') if p.is_file()) + 100*1024*1024:
                raise InstallError('Insufficient free space for a state backup.')
            with tarfile.open(backup/'state.tar.gz','w:gz') as tf: tf.add(DATA,arcname=DATA.name)
            backup_complete = True
            if 'caddy' in cfg['components']:
                with tarfile.open(backup/'caddy-state.tar.gz','w:gz') as tf:
                    if CADDY_ROOT.exists(): tf.add(CADDY_ROOT,arcname=CADDY_ROOT.name)
                caddy_backup_complete = True
                if saved and not caddy_managed: expire_http_sessions()
                configure_caddy(cfg,uid,gid)
            if 'dsp' in cfg['components']: configure_dsp(cfg,uid,gid,runtime_changes)
            if 'mpd' in cfg['components']: configure_mpd(cfg,uid,gid)
            if cfg['mode'] == 'native':
                destination = CURRENT.parent/(version+'-'+stamp)
                CURRENT.parent.mkdir(parents=True,exist_ok=True)
                shutil.copytree(artifact,destination)
                temp_link = CURRENT.with_name('current.new');temp_link.symlink_to(destination);os.replace(temp_link,CURRENT)
                if not saved: atomic_write(UNIT,unit_text(cfg,uid,gid))
            if 'discovery' in cfg['components']: configure_discovery(cfg)
            run(['systemctl','daemon-reload'])
            if 'dsp' in cfg['components']:
                run(['systemd-analyze','verify',DSP_UNIT]);run(['systemctl','enable','--now','musicata-camilladsp.service'])
            if 'mpd' in cfg['components']:
                run(['systemd-analyze','verify',MPD_UNIT]);run(['systemctl','enable','--now','musicata-mpd.service'])
            if cfg['mode'] == 'native':
                run(['systemd-analyze','verify',UNIT]);run(['systemctl','enable','musicata.service']);run(['systemctl','restart','musicata.service'])
            else:
                for component in docker_components(cfg):
                    name = 'musicata' if component == 'server' else 'musicata-'+component
                    new_containers.append(name)
                    run(docker_command(cfg,uid,gid,component,cfg['images'][component]))
            healthy(f'http://127.0.0.1:{cfg["port"]}/')
            if 'ml' in cfg['components']:
                print('Waiting for ML model download/startup (up to 5 minutes)…',flush=True)
                healthy('http://127.0.0.1:3091/health',300)
            if 'caddy' in cfg['components']:
                verify_loopback_backend(cfg['port'])
                if cfg['mode']=='native':
                    run(['systemd-analyze','verify',CADDY_UNIT])
                    run(['systemctl','enable','musicata-caddy.service'])
                    run(['systemctl','restart','musicata-caddy.service'])
                print('Waiting for Caddy HTTPS and a trusted certificate (up to 3 minutes)…',flush=True)
                healthy_tls(cfg['tls_domain'])
            atomic_write(CONFIG,json.dumps({k:v for k,v in cfg.items() if k not in ('check','dry_run','yes','upgrade','tls_dns_token_file') and not k.startswith('_')},indent=2)+'\n',0o600)
        except Exception as original_error:
            print('Activation failed. Recovering Musicata files/state; dependency packages are retained. Backup:',backup,file=sys.stderr,flush=True)
            recovery_errors = []
            def recover(label, action):
                try: action()
                except Exception as error: recovery_errors.append(label+': '+str(error))
            for name in new_containers:
                succeeds(['docker','rm','-f',name])
            if cfg['mode'] == 'native': succeeds(['systemctl','stop','musicata.service'])
            if 'caddy' in cfg['components'] and cfg['mode']=='native':
                succeeds(['systemctl','stop','musicata-caddy.service'])
                if not caddy_managed or not caddy_was_enabled: succeeds(['systemctl','disable','musicata-caddy.service'])
            if 'dsp' in cfg['components']:
                succeeds(['systemctl','stop','musicata-camilladsp.service'])
                if not saved: succeeds(['systemctl','disable','musicata-camilladsp.service'])
            if not saved:
                succeeds(['systemctl','disable','musicata.service'])
                if 'mpd' in cfg['components']: succeeds(['systemctl','disable','--now','musicata-mpd.service'])
            if runtime_changes.get('loopback_loaded'):
                # The kernel refuses removal if another process still uses this module.
                recover('unload new ALSA loopback module',lambda:run(['modprobe','-r','snd-aloop']))
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
            def restore_caddy_state():
                if caddy_backup_complete:
                    if CADDY_ROOT.exists(): CADDY_ROOT.rename(CADDY_ROOT.with_name(CADDY_ROOT.name+'-failed-'+stamp))
                    with tarfile.open(backup/'caddy-state.tar.gz') as tf:
                        kwargs = {'filter':'fully_trusted'} if hasattr(tarfile,'fully_trusted_filter') else {}
                        tf.extractall(CADDY_ROOT.parent,**kwargs)
            recover('Caddy certificates/state',restore_caddy_state)
            recover('systemd reload',lambda:run(['systemctl','daemon-reload']))
            for name in old_containers:
                recover('container '+name,lambda name=name:run(['docker','rename',name+'-previous-'+stamp,name]))
            # Never launch an old binary against partially restored or migrated state.
            if not recovery_errors:
                if dsp_was_active: recover("start output processor",lambda:run(["systemctl","start","musicata-camilladsp.service"]))
                for name in stopped_containers:
                    recover('start '+name,lambda name=name:run(['docker','start',name]))
                if saved and stopped and cfg['mode']=='native':
                    recover('start service',lambda:run(['systemctl','start','musicata.service']))
                if caddy_was_active: recover('start HTTPS proxy',lambda:run(['systemctl','start','musicata-caddy.service']))
            if recovery_errors:
                raise InstallError(str(original_error)+'; manual recovery required from '+str(backup)+'; '+'; '.join(recovery_errors)) from original_error
            raise
        for name in old_containers:
            if not succeeds(['docker','rm',name+'-previous-'+stamp]):
                print('Old stopped container retained:',name+'-previous-'+stamp)
        hostname = socket.gethostname().removesuffix('.local') + '.local' if 'discovery' in cfg['components'] else '<server-ip>'
        url = f'https://{cfg["tls_domain"]}/' if 'caddy' in cfg['components'] else f'http://{hostname}:{cfg["port"]}/'
        print(f'Installed Musicata {version}. Open {url}')
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
    if 'caddy' in cfg['components']:
        print(f'HTTPS: https://{cfg["tls_domain"]}/ ; Caddy manages Let\'s Encrypt certificates. Backend: 127.0.0.1:{cfg["port"]}')
        if cfg['tls_dns']=='cloudflare':
            print('Cloudflare DNS-01: hostname resolves privately through your router; no public inbound ports required. Token needs Zone DNS Edit and Zone Read for this zone. Caddy is built with the Cloudflare module.')
        else:
            print('Public validation requires public DNS pointing to this host and internet access to ports 80/443.')
        if saved and 'caddy' not in saved['components']: print('Enabling HTTPS signs out existing browser sessions; sign in again at the HTTPS address.')
        print('Native audio endpoints currently lack HTTPS/WSS support; the loopback backend is not reachable directly from other devices.')
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
