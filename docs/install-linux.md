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

# Native server and recommended local playback; the existing account must already exist:
sudo ./scripts/install.sh --mode native --user damirk --with snapclient,discovery \
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
ALSA's `default` device. Interactive player setup requires selecting a stable card identifier.
Snapclient is the recommended player. MPD remains an explicitly selected compatibility option
(`--with mpd`), uses software volume, and listens on loopback port 6600. Existing MPD
configuration and service files are preserved on upgrade.

Choose **MPD through Snapcast** in interactive setup, or use `--with mpd,snapclient`, for
local MPD with USB standby. The installer creates a shared FIFO, configures MPD's FIFO output,
registers local MPD, enables its Snapcast input, and assigns the local Snapclient to it.
Snapclient alone opens the DAC. Native and Docker servers use the same host MPD/Snapclient
services; Docker receives a read-only mount of the FIFO directory, with matching service UID.
An older Musicata release without this input is rejected by the post-install stream check.
Custom direct-MPD routing is preserved and must be migrated explicitly before selecting both
backends. Existing shared Snapcast groups are not automatically rerouted.

To add playback to an installer-managed server-only host, use `--upgrade` with the desired
components and `--alsa-device`. Existing playback device selections remain protected against
accidental changes. The MPD compatibility input can subsequently be enabled/disabled in
Settings → Outputs; pause MPD before changing that input or the Snapcast buffer. These changes
briefly reconnect rooms and preserve the previous configuration if activation fails.

### Standalone player and optional USB standby

On a separate Linux player, use the player entry point from the same checkout/release:

```sh
sudo ./scripts/install-player.sh --server musicata.local
```

Snapclient is the recommended default. It selects an ALSA output, creates a stable player identity, installs only the Snapclient
dependencies, and verifies the connection to Snapserver. It does not install another Musicata
server, Snapserver, or MPD. On Debian/Ubuntu, Fedora and Arch it installs a non-root systemd
service with boot activation and retry. Generic/non-systemd Linux receives a manual startup
command and needs an existing Snapclient binary. `--check` checks installation prerequisites,
not runtime player health;
`--output 'plughw:CARD=Pro,DEV=0' --yes` supports unattended provisioning.

For a standalone MPD compatibility player, select MPD interactively or pass `--backend mpd`.
Use a private, root-owned file containing a Musicata administrator API token (mode `0600`):

```sh
sudo ./scripts/install-player.sh --backend mpd \
  --output 'plughw:CARD=Pro,DEV=0' \
  --mpd-bind 192.168.1.50 --confirm-trusted-network \
  --musicata-url https://musicata.example.org --token-file /root/musicata-player.token
```

Replace the bind address with this player's private IPv4 address reachable from Musicata.
MPD defaults to `127.0.0.1:6600` for a server on the same host. The current Musicata MPD
client has no password authentication; remote binding therefore requires explicit trusted-network
confirmation. Restrict port 6600 to the Musicata host using your existing firewall/network policy;
do not expose it to untrusted networks. The installer neither opens firewall ports nor binds all
interfaces. Musicata's advertised/public URL must also be reachable from the player for audio fetches.

This installs only MPD and audio prerequisites, creates `musicata-mpd.service` and
`/etc/musicata-mpd.conf`, checks MPD locally, registers its address in Musicata and waits for
Musicata to report it online. No audible playback is started. The API token is used for the
registration operation and is not saved. Reruns reuse the registered address and preserve state;
provide the token file again. Modified managed files are rejected rather than overwritten.
Changing backend, bind address or Musicata server requires explicit migration; this avoids
leaving a duplicate/stale registration. Local activation failures restore previous managed files;
registration failure retains the working local service and reports how to retry (an offline
registration may remain). Generic/non-systemd Linux prints a configuration and launch command
for an existing MPD binary; setup and registration remain manual in that fallback.

Standalone direct MPD rejects USB auto-disconnect. For MPD with USB standby, use the combined
server's MPD → Snapcast route and a Snapclient player instead.

USB standby is optional and disabled by default. First independently verify that switching
the DAC's port affects only that device; the presence of a kernel `disable` attribute does
not establish electrical isolation. With a private root-owned API token file, bind the DAC:

```sh
sudo ./scripts/install-player.sh --server musicata.local \
  --output 'plughw:CARD=Pro,DEV=0' --usb-device /sys/bus/usb/devices/1-1 \
  --musicata-url https://musicata.example.org --token-file /root/musicata-player.token \
  --confirm-isolated-usb
```

The USB path above is an example to replace with the selected device. Discovery resolves the
kernel port relationship; it does not assume PCI, a particular DAC model, or a USB bus number.
The selected ALSA output must belong to that device. Hubs, ambiguous mappings, and devices
without a serial are rejected. The token is copied into a root-private file; a listener API
token suffices. Enable standby and set the idle delay in **Settings → Outputs → USB DAC
standby**. This shared policy applies to every explicitly bound player and refreshes within
15 seconds when Musicata is reachable. Status errors reset the idle timer and preserve the
current USB state; playback on the player's assigned Snapcast stream restores the port.

For a combined server/player host, `scripts/install.sh` accepts the same USB binding options
with the selected `snapclient` role (or `mpd,snapclient`). Configure an API token after initial
account setup, then pass the binding options on `--upgrade`. Both entry points use the same
USB controller and recovery lifecycle. USB setup follows the verified playback installation
as a separate phase: if USB setup fails, the working server/player remains installed and the
error identifies the USB recovery step. Upgrades refresh an existing controller while
preserving its bound port and token. The main entry point also accepts `--remove-usb`.

Remove the controller with `sudo ./scripts/install-player.sh --remove-usb`. It stops the
controller and restores the port before removing its binding; Snapclient remains installed.
If restoration fails, the binding is retained for recovery. Configuration/service backups
are under `/var/backups/musicata-player`; check `journalctl -u musicata-usb-autowake` and
`journalctl -u musicata-snapclient` when diagnosing failures.

ARM coverage and Raspberry Pi support are tracked separately in
[#66](https://github.com/damirkrstanovic/musicata/issues/66), including package/runtime coverage,
Pi/hub/kernel combinations, physical wake/standby and reboot. No Raspberry Pi hardware
compatibility is claimed here. This controller supports Snapcast playback. Direct
MPD ALSA playback is unsuitable: real MPD testing showed it loses playback intent when its
device is disconnected. MPD → Snapserver FIFO → Snapclient is provisioned by the combined
installer and has passed a real-process integration probe with ALSA null output. This covers
Musicata-driven play/pause, stream assignment, guarded configuration changes and process
restart; it does not establish physical audio, Docker audio permissions or system boot.

## HTTPS with Caddy

Add `caddy` to `--with` for either native or Docker installation. Musicata then listens on
`127.0.0.1:<port>` and Caddy serves HTTPS on port 443, redirects HTTP on port 80, and proxies
media and WebSockets. Browser session cookies receive `Secure`. Plain HTTP remains the
default when Caddy is omitted.

For a **private LAN installation with a Cloudflare-hosted domain**, use a hostname under a
registered domain you control, such as `music.example.org`. Configure your router's DNS to
resolve it to the server's private IP. The certificate challenge uses public Cloudflare DNS;
no public A/AAAA record or router port forwarding is required. `.local` names cannot receive
a Let's Encrypt certificate. Allow outbound HTTPS and DNS from the server and LAN access
to ports 80/443.

Create a Cloudflare API token scoped to the relevant zone with **Zone / DNS / Edit** and
**Zone / Zone / Read**. Save only the token in a file on the installation host, with mode
`0600`. Keep it out of shell arguments and command history. Then select either mode:

```sh
sudo ./scripts/install.sh --mode native --with caddy \
  --tls-domain music.example.org --tls-dns cloudflare \
  --tls-dns-token-file /root/cloudflare-token --dry-run

sudo ./scripts/install.sh --mode docker --with caddy \
  --tls-domain music.example.org --tls-dns cloudflare \
  --tls-dns-token-file /root/cloudflare-token
```

Remove `--dry-run` to apply the native example. Add other desired components to the same
comma-separated `--with` list. `--tls-email admin@example.org` is optional.

Docker installations pull the prebuilt `ghcr.io/damirkrstanovic/musicata-caddy:1.3.1` image,
built in release CI with Caddy 2.11.7 and Cloudflare 0.2.4. No compilation runs on the host.
Native mode builds a pinned Caddy using Go and xcaddy; this requires internet access and
may take several minutes. Native distributions with an older Go package receive
a checksum-verified temporary toolchain. Go's checksum database verifies downloaded modules.
The DNS challenge uses public resolvers to avoid a private router's DNS view hiding the
challenge record. See the [Cloudflare module](https://github.com/caddy-dns/cloudflare) and
[Caddy DNS validation](https://caddyserver.com/docs/automatic-https#dns-challenge).

For a server reachable from the internet, omit `--tls-dns` and `--tls-dns-token-file`:

```sh
sudo ./scripts/install.sh --mode docker --with caddy --tls-domain music.example.org
```

That default uses HTTP/TLS validation and requires public DNS pointing to the server and
inbound public ports 80/443. The installer leaves firewall and router configuration to you.

Native Caddy runs under the separate distribution `caddy` account in
`musicata-caddy.service`; Docker runs it as the service UID inside the separate
`musicata-caddy` container.
Configuration is in `/etc/musicata/caddy/Caddyfile`; certificates and ACME state persist in
`/var/lib/musicata-caddy`. The Cloudflare token is copied to root-only
`/etc/musicata/caddy/cloudflare.env`, supplied at runtime, and excluded from the installation
manifest and build context. Ordinary upgrades reuse it without needing the original token
file. Supply a new `--tls-dns-token-file` to rotate the token. Root-only installer backups
intentionally include the token so rollback can restore it. Never publish the credential
file or installer backups.

An upgrade can add Caddy while preserving the other components: use `--upgrade --with`
with the existing component list plus `caddy`, and the TLS options above. Subsequent upgrades
preserve the hostname, validation method, custom Caddy configuration and certificate state.
Enabling HTTPS invalidates old browser sessions; sign in again at the HTTPS URL. Custom
native units that pass `--addr` must remove that argument first so the loopback environment
setting takes effect. Unmanaged Caddy resources and occupied ports stop installation.
Activation failures restore the old proxy, credentials, binary/container and Musicata state.
HTTPS activation succeeds only after a trusted certificate and the proxied health endpoint
are available.

The native **audio endpoint** currently supports only HTTP/WebSocket connections; it cannot
connect remotely to an HTTPS-only installation until HTTPS/WSS support is added. Native
server installation and local MPD playback work with Caddy. With discovery selected, host
mDNS and cast-in discovery remain enabled, but the loopback HTTP backend is no longer
advertised; access the web app using its configured TLS hostname.

```sh
sudo journalctl -u musicata-caddy -n 100 --no-pager  # native proxy
sudo docker logs --tail 100 musicata-caddy          # Docker proxy
```

## Components and distribution dependencies

| Component | Installed/configured |
|---|---|
| Server, always | Native static release + systemd, or the released Docker image |
| `caddy` | HTTPS proxy; optional Cloudflare DNS-01 for private LAN access; persistent certificates |
| `ml` | Separate CPU-only Docker container; persistent model cache; localhost port 3091 |
| `mpd` | Host MPD + ALSA utilities; dedicated non-root `musicata-mpd.service` with audio-device access |
| `snapclient` | Recommended local player; dedicated non-root service, stable identity and explicit output |
| `discovery` | Host Avahi, NSS mDNS resolution, `_http._tcp` advertisement with the Musicata port |
| `snapcast` | Managed snapserver runtime; native packages also supply snapclient for room setup |
| `airplay` | Shairport Sync cast-in; also selects Snapcast and discovery |
| `spotify` | Librespot Spotify Connect cast-in; also selects Snapcast and discovery |

Apple support means AirPlay cast-in, not a native Apple Music catalogue provider. Configure
cast-in and rooms in Musicata's Multi-room settings; account/subscription requirements of
upstream services still apply. Snapclient runs on each playback endpoint; installing a
server does not create rooms or select a DAC automatically.

Both server installation modes install host playback utilities: ALSA tools,
Snapserver/Snapclient, Shairport Sync, Librespot, and Avahi/mDNS. MPD is installed only
when explicitly selected; upgrades preserve existing MPD installations. Storage and
system troubleshooting tools are also included: `smartctl`, `nvme`, `sensors`, `iostat`,
and `lspci`. Installing these tools does not select a DAC, create rooms, enable cast-in,
or start a new standalone audio service. Configure playback in the web UI; `--with mpd`
explicitly configures the dedicated MPD service. Existing audio services are preserved.

Debian/Ubuntu use official distribution packages for Snapcast. On Fedora and Arch,
missing Snapcast executables are built from pinned upstream 0.35.0 source (build tools
are installed as needed). Librespot 0.8.0 is bundled with the checksum-verified native
release archives, including its license notices; an existing host executable is preserved.
No AUR helper, COPR, or third-party package repository is enabled.

The published server image includes Snapserver, Shairport Sync, and Librespot with pipe
output support. The installer uses this image directly, without building a derived image.
It uses host networking and shares `/run/dbus` read-only for host Avahi discovery. Host
MPD, endpoint Snapclient, and hardware diagnostic tools remain host utilities; no audio
device access or privileged container is needed for managed server streaming. ML has no
GPU access and binds only `127.0.0.1`. Docker bind mounts use SELinux labels (`Z` for
private state, `z` for the shared read-only music directory), including on Fedora.

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
python3 tests/installer/caddy-smoke.py --caddy /path/to/caddy --cloudflare
bash tests/installer/caddy-container-smoke.sh --cloudflare
bash tests/installer/native-smoke.sh
```

The Caddy smoke tests validate the real Cloudflare module with a dummy token, then use
an isolated local CA to check HTTPS trust, HTTP redirects, session cookies, media streams,
WebSocket tunnels and certificate persistence after restart. The container harness runs
the same release-pinned prebuilt Cloudflare image as the installer, without a build step.
Production Cloudflare DNS changes and public ACME issuance
require a separate check with the deployment's actual domain and token.

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

### Existing Snapserver on a Docker host

Before installing or upgrading Docker, the installer checks for standalone host
Snapserver services/processes and listeners on TCP 1704, 1705 and 1780. The same
checks run with `--check` and `--dry-run`. It refuses conflicts before changing
packages, services or containers and leaves the existing deployment intact.

Back up and remove the standalone server deployment, or resolve the reported port
conflict, before retrying. Stopping an independently configured service alone is
not sufficient: an installed standalone service can start again later. Keep
Snapclient on the playback host; only the conflicting Snapserver is involved.
Use sudo when process ownership is unavailable in a read-only check.

An unchanged, disabled utility installed by Musicata is permitted, as is Snapserver
inside the existing installer-managed Musicata container. New installations record
unit/config fingerprints. For legacy 1.6.1 upgrades, only disabled, unmodified
Debian package files without local service overrides qualify as inert utilities;
custom configurations need explicit conflict resolution.
