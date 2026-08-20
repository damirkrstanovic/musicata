# Musicata

**Your music, your server, every room.** Musicata is an open-source, self-hosted music
platform for personal and shared libraries — a Roon-like experience you run yourself. Point it
at your music, open it in any browser, and play to your speakers, anywhere in the house.

It runs as a single Rust binary with the web app built in. No cloud account, no subscription,
no phoning home — your library and listening history stay on your machine.

## Why Musicata?

- **It's yours.** Local-first and self-hosted. Your files, your metadata, your data — on
  hardware you control, on your own network.
- **Bring your music from anywhere.** Local folders, SMB network shares, or another
  OpenSubsonic/Navidrome server — Musicata pulls them all into one library. New sources are
  added the same way, so streaming catalogs can slot in later.
- **It makes your library look great.** Automatic artwork, MusicBrainz enrichment, and
  acoustic fingerprinting clean up and complete your collection in the background while you
  keep listening.
- **Play it everywhere, in sync.** Stream to your browser, drive an MPD player, or play the
  same track *perfectly in sync across rooms* with Snapcast.
- **Sounds the way you want.** Per-output EQ and room/headphone correction, plus loudness
  leveling so albums and playlists play at an even volume.
- **No config files.** Every user-facing setting lives in the app — add a music source, a
  player, or an API key right in the Settings page. Live, no restart.
- **Works with what you already use.** Musicata speaks the OpenSubsonic API, so existing
  Subsonic apps can browse and stream your library too.

## Features

- A central server with a SQLite-backed library that rescans incrementally as your files
  change.
- **Music sources:** local disk, SMB network shares, and upstream OpenSubsonic/Navidrome
  servers.
- **Rich metadata:** tag extraction, artwork fetching (iTunes / Deezer / Cover Art Archive /
  fanart.tv), MusicBrainz enrichment, and AcoustID fingerprinting — each running quietly on its
  own background worker.
- **Browse & search:** full-text search and browse by artist, album, track, genre, year,
  composer, and folder.
- **Playback everywhere:** playback queues and multi-player zones; browser playback plus MPD
  and Snapcast backends.
- **Synchronized multi-room:** play the same music sample-accurately across rooms via Snapcast,
  with optional AirPlay / Spotify Connect cast-in.
- **Great sound:** per-output DSP (parametric EQ, room & headphone correction) and EBU R128
  loudness leveling.
- **Discovery:** listening history with ListenBrainz similar-track radio and continuous play.
- **Multi-user:** accounts with cookie sessions and per-user API tokens.
- **Open APIs:** native HTTP + WebSocket APIs, an OpenSubsonic surface (Musicata both serves
  and consumes it), and an installable Svelte PWA controller.
- **Backup & migrate:** library export/import.

## Getting started

Download the binary for your machine from the
[latest release](https://github.com/damirkrstanovic/musicata/releases/latest), then:

```sh
tar -xzf musicata-x86_64-linux.tar.gz
cd musicata-x86_64-linux
./musicata-server --library /path/to/music --addr 0.0.0.0:3030
```

Three builds are published: `musicata-x86_64-linux` runs on any 64-bit Intel/AMD machine
including low-power NAS CPUs, `musicata-x86_64-v3-linux` is faster on post-2015 CPUs (AVX2 —
it will crash on older ones), and `musicata-aarch64-linux` is for ARM (Raspberry Pi 4/5, Apple
silicon VMs). When in doubt take the first.

Open `http://<host>:3030`, create your admin account, and add your music sources and players
from the **Settings** page. That's it.

Building from source, controlling MPD, running as a systemd service, and remote access are all
covered in **[Running & deploying Musicata](docs/deployment.md)**.

> **Heads up — Musicata is LAN-first.** Keep it on a trusted home network and reach it from
> outside over a VPN or SSH tunnel, not the raw internet. See
> [Remote access](docs/deployment.md#remote-access).

## Documentation

- [Running & deploying](docs/deployment.md) — build from source, MPD, systemd, remote access
- [Native + OpenSubsonic API](docs/api.md)
- [Roadmap](docs/roadmap.md)
- [Prior Art](docs/prior-art.md)
- [Metadata Update Strategy](docs/metadata.md)
- [DSP — EQ, room & headphone correction](docs/dsp.md)
- [Loudness (EBU R128)](docs/loudness.md)
- [Recommendations & Radio](docs/recommendations.md)
- [Continuous Play](docs/continuous-play.md)
- [Snapcast Transport](docs/snapcast.md)
- [Native Playback Endpoint](docs/native-endpoint.md)
- [Audio ML Service (embeddings & tags)](docs/musicata-ml.md)
- [Plugins](docs/plugins.md)
- [Web UI Style Guide](docs/style-guide.md)
- [Initial Requirements](docs/requirements.md)

Contributors: see **[AGENTS.md](AGENTS.md)** for architecture, conventions, and the build/test
workflow.

## License

Musicata is free software licensed under the **GNU Affero General Public License, version 3
or later** — see [COPYING](COPYING) for the full text. If you run a modified Musicata as a
network service, AGPL section 13 requires you to offer its source to your users — and Musicata
does that for you. A **Source code** link, with the running version beside it, appears on the
sign-in screen and in the player's account menu. Point it at your fork under **Settings → About
& source**: it's a setting, so complying with the license never means rebuilding.

Third-party material stored in this repository (the AudioSet class labels) is attributed in
[NOTICE](NOTICE). The dependencies compiled *into* each release — the Rust crates and the
Svelte/Workbox code inside the embedded web app — are attributed in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md), which ships in the release tarball and in
both container images.

Security reports: see [SECURITY.md](SECURITY.md) — please don't file them as public issues.
