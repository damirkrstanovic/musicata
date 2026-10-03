# Linux installation and upgrades

Use `scripts/install.sh` from a source checkout, or `install.sh` beside `install.py` in a
release archive. The installer supports systemd hosts running Ubuntu, Debian, Fedora, Arch
Linux and derivatives identified through `/etc/os-release`. It needs Bash and Python 3.9+;
the shell launcher offers to install Python when missing. Other dependencies are installed
through the distribution's package manager. On Arch, this includes `pacman -Syu` because
partial upgrades are unsupported.

## Choose a deployment

Docker is the default. Preview and inspect before applying:

```sh
./scripts/install.sh --dry-run
./scripts/install.sh --check
sudo ./scripts/install.sh
```

For unattended installation, add `--yes`. `--dry-run` reports the plan without downloads or
changes. `--check` also returns a nonzero status when dependencies are missing; both report
CPU, architecture, systemd, account, port and existing-installation blockers. Package
availability is checked by the package manager during preparation; image authentication and
network access are verified when pulling. An unavailable dependency or artifact prevents
activation, with the failing command and its error displayed.

Select optional components with a comma-separated list:

```sh
# Docker server + local ML + LAN discovery, on a CPU supporting x86-64-v3:
sudo ./scripts/install.sh --with ml,discovery

# Native server and USB DAC playback; the existing account must already exist:
sudo ./scripts/install.sh --mode native --user damirk --with mpd,discovery \
  --alsa-device 'plughw:CARD=Pro,DEV=0'

# Pin either deployment to an existing release:
sudo ./scripts/install.sh --version 1.0.9
```

The default service account is a dedicated non-login `musicata` user. Processes run as that
user, including inside Docker. Native services are enabled at boot. Docker is enabled at
boot and containers use `restart=unless-stopped`. Neither requires a user login.

Open `http://<host>:3030/` to create the first account. Configure music sources, players,
API keys, multi-room settings and analysis schedules in **/admin**. The installer sets only
bootstrap paths/addresses and seeds the optional local MPD/ML addresses.

`--library /path/to/music` supplies a local library directory (default `/srv/music`); the
service account needs read and traversal access. Docker mounts it read-only. Network sources
such as SMB are added in the UI and need no host mount. `--port` changes the HTTP port.
For ALSA names, use `aplay -L` after installing `alsa-utils`; `--alsa-device` defaults to
ALSA's `default` device. MPD uses software volume and listens on loopback port 6600.

## Components and distribution dependencies

| Component | Installed/configured |
|---|---|
| Server, always | Native static release + systemd, or the released Docker image |
| `ml` | Separate CPU-only Docker container; persistent model cache; localhost port 3091 |
| `mpd` | Host MPD + ALSA utilities; dedicated non-root `musicata-mpd.service` with audio-device access |
| `discovery` | Host Avahi, NSS mDNS resolution, `_http._tcp` advertisement with the Musicata port |
| `snapcast` | Managed snapserver runtime; native packages also supply snapclient for room setup |
| `airplay` | Shairport Sync cast-in; also selects Snapcast and discovery |
| `spotify` | Librespot Spotify Connect cast-in; also selects Snapcast and discovery |

Apple support means AirPlay cast-in, not a native Apple Music catalogue provider. Configure
cast-in and rooms in Musicata's Multi-room settings; account/subscription requirements of
upstream services still apply. Snapclient runs on each playback endpoint; installing a
server does not create rooms or select a DAC automatically.

Native optional packages come from the host repositories. Docker's optional Snapcast and
cast-in executables are installed into a derived server image, because host packages alone
are not visible inside a container. Docker uses the host network for LAN playback and
discovery; AirPlay also shares the host system D-Bus socket to reach Avahi. ML has no GPU
access and binds only `127.0.0.1`. Docker bind mounts use SELinux labels (`Z` for private
state, `z` for the shared read-only music directory), including on Fedora.

Verified official-repository availability during implementation:

| Host / image | Docker engine | Snapcast | AirPlay | Librespot |
|---|---|---|---|---|
| Ubuntu 24.04 / 26.04 | `docker.io` | `snapserver`, `snapclient` | `shairport-sync` | unavailable |
| Debian 13 (trixie) | `docker.io` | `snapserver`, `snapclient` | `shairport-sync` | unavailable |
| Fedora 43 / 44 | `moby-engine` | unavailable | `shairport-sync` | unavailable |
| Arch rolling / derivatives | `docker` | unavailable in current official repositories | `shairport-sync` | `librespot` |
| Docker server's Debian bookworm base | host engine | `snapserver` | `shairport-sync` | unavailable |

**Unavailable selections stop preparation with a package error.** No AUR helper, COPR or
third-party repository is enabled automatically. In particular, `--with spotify` cannot
currently complete with the stock Docker image's repositories. Native Spotify also needs
Snapcast; its availability must be resolved on Arch even though Librespot itself is packaged.
Install a supported upstream dependency/package or configure an existing external playback
service instead. These are reported limitations, not silently skipped components.

Packaged Snapcast versions differ: bookworm 0.26, Ubuntu 24.04 0.27, Debian 13 0.31, Ubuntu
26.04 0.34. The project's detailed multi-room validation used 0.35; older distro packages
need their actual room/cast-in behavior checked. See [Snapcast](snapcast.md).

## CPU eligibility and ML

The portable x86-64 server requires **x86-64-v2**, including SSE4.2, POPCNT, CMPXCHG16B and
LAHF/SAHF. The installer selects the portable native archive, never the AVX2-only `-v3`
archive. ARM64 supports native installation; published Docker images are currently amd64
only. Other architectures are rejected.

Optional ML additionally requires **x86-64-v3** (AVX, AVX2, BMI1/2, F16C, FMA, LZCNT,
MOVBE and XSAVE). Its prebuilt ONNX Runtime has a stronger baseline than the Rust executable's
build flags. We tested the exact 1.0.9 ML executable on the Mele Celeron N4500: it terminated
with an illegal instruction before opening its health endpoint. The server remains supported
on that CPU. Use a remote ML service such as the working CPU deployment on lizard10.

ML is opt-in (`--with ml`) and rejected with `--mode native` or an incompatible CPU. Its model
is about 327 MB and is downloaded on first start; allow time and disk space for this. Enable
analysis in **/admin** and use `http://127.0.0.1:3091` for the installer-managed local ML
container, or supply the URL of your remote ML host.

## Upgrade and recovery

```sh
sudo ./scripts/install.sh --upgrade              # newest stable release
sudo ./scripts/install.sh --upgrade --version 1.0.9 --yes
```

Settings are saved in root-readable `/etc/musicata/install.json`. Upgrades retain deployment
mode, service user, library path, HTTP port, audio device and component selection. Mode changes
and migrations from older hand-written install scripts are deliberately rejected with a
message; the existing Mele installation is not automatically adopted or overwritten.

Release metadata is fetched before dependency installation. Native archives must match the
SHA-256 supplied by GitHub's release asset metadata, or the published `.sha256` sidecar.
Archive traversal and links are rejected. Docker verifies pulled image layers and the
installer records/runs their immutable image IDs. Downloads/images are prepared before the
Musicata service is stopped.

During activation, the server is stopped and its complete state is backed up, including
SQLite WAL files, under `/var/backups/musicata/<timestamp>/state.tar.gz`. Relevant service
files/configuration are also saved. Native versions remain in `/opt/musicata/`; `current`
selects the active version. Docker containers retain `/var/lib/musicata` across replacement.
The ML model cache is separate in `/var/lib/musicata-ml`.

If activation or health checks fail, the installer restores the previous binary/container,
configuration and state. Failed state is retained in `/var/lib/musicata-failed-<timestamp>`.
If restoration itself fails, it reports **manual recovery required** and keeps the service
stopped rather than opening a partially restored database. Recovery does not uninstall
system packages, remove the created service account, or undo a full Arch system upgrade.
Backups can contain credentials; their directory is root-only. They are retained until the
administrator removes them.

For manual recovery: stop Musicata; retain the failed state; restore `state.tar.gz` under
`/var/lib` with ownership preserved; restore the backed-up files to their original paths;
select the previous native version or saved Docker image and restart. Do not run an old
binary against a database already migrated by a newer release without restoring its backup.

## Diagnostics

```sh
sudo journalctl -u musicata -u musicata-mpd -n 100 --no-pager  # native
sudo docker logs --tail 100 musicata                         # Docker
sudo docker logs --tail 100 musicata-ml                      # optional ML
```

If GHCR returns `unauthorized`, the packages need public visibility or an authenticated
`sudo docker login ghcr.io` using a token with `read:packages`. The installer reports this
before replacing an existing Musicata service. It does not request or store registry tokens.

For LAN reachability, allow the selected HTTP port through your existing firewall. Discovery
uses UDP 5353; Snapcast room streaming uses TCP 1704 and Snapweb TCP 1780. Expose services only
on the intended LAN. Diagnose `.local` resolution with `getent hosts <host>.local` and inspect
Avahi with `systemctl status avahi-daemon`.

References: [ONNX Runtime Rust release notes](https://github.com/pykeio/ort/releases),
[Shairport Avahi backend](https://github.com/mikebrady/shairport-sync/blob/master/mdns_avahi.c),
[Debian Snapserver](https://packages.debian.org/trixie/snapserver),
[Ubuntu Snapserver](https://packages.ubuntu.com/noble/snapserver),
[Fedora Shairport](https://packages.fedoraproject.org/pkgs/shairport-sync/shairport-sync/).

## Installer verification

The installer tests cover planning, CPU checks, verified archive extraction, state
preservation and failure recovery. Run them with:

```sh
python3 -m unittest discover -s tests/installer
bash tests/installer/native-smoke.sh
```

The native integration harness uses disposable Kubernetes pods for Debian 13, Ubuntu
26.04, Fedora 44 and Arch. It installs the real released server, runs it under the service
account and checks HTTP health and preserved state after an upgrade. A test-only service
manager substitutes for systemd inside the pods; host boot and physical audio devices
require separate validation. The harness deletes only its own namespace.

`bash tests/installer/docker-smoke.sh` exercises installation, upgrade and rollback on
a Docker host using isolated names and temporary paths. Set `MUSICATA_RELEASE_ARCHIVE`
to a downloaded portable 1.0.9 archive if it is not at the harness's default path.
It builds a local image from that release binary and substitutes only registry lookups,
so it tests real container execution and persistent data, but not GHCR authentication.
This test passed on lizard10 without replacing its existing ML container.
