# Provider & Plugin System — Research and Plan

Background for Milestones 9 (music providers) and 10 (player providers/endpoints).
We studied Roon for the architecture, then chose an open, in-process model matching
how comparable open-source servers do it.

> **Status — this remains a design/plan; no plugin system exists in code.** What shipped is
> the narrower **provider** abstraction: `MusicProvider` (core) + the
> `ProviderHandle` / `ProviderRegistry` / `ProviderCapabilities` trio (server, enum-dispatch).
> The `PlayerProvider` trait below was **planned but NOT built** — players still dispatch via
> the `PlayerHandle` enum. (See the per-item "As built" notes in the Rollout section.)

## How Roon does it (research)

Roon splits into three roles: **Core** (the brain — library, metadata, queue, DSP,
zone coordination), **Control** (thin remotes), and **Output** (endpoints that render
audio). It exposes **two unrelated extensibility surfaces**, plus a closed source
layer:

1. **RAAT / Roon Ready — the audio-endpoint layer.** RAAT ("AirPlay for audiophiles")
   tunnels audio Core→endpoint with the **device owning the audio clock**, ~<1 ms
   multi-room sync, exclusive/bit-perfect output, two-way volume + artwork. Integrated
   via a **closed, partner-only Roon Ready SDK** (free certification, NDA). Devices
   without RAAT use **Roon Tested** — bridged over AirPlay / Chromecast / USB.
2. **Extension SDK (`node-roon-api`) — the control-plane layer.** Out-of-process
   Node.js peers over a WebSocket RPC (MOO), discovered via SOOD. Extensions *consume*
   Transport/Browse/Image/Settings and *provide* Status/Settings/VolumeControl/
   SourceControl. It is explicitly **control only — no hook to add a music source or
   inject audio.**
3. **Music sources are closed/first-party** (TIDAL, Qobuz, KKBOX, nugs, radio). There
   is no public API to add a source — Roon's biggest self-acknowledged limitation.

**Design rules worth taking:** Core stays authoritative; capability negotiation
(providers declare what they support); strict separation of *source layer* vs
*transport layer*; automatic discovery + explicit, persisted pairing/auth; tier
endpoints as *native* (deep, synced) vs *bridged* (just-works). **Where we diverge:**
be open at the source layer — a documented provider API is our differentiator.

## How comparable open-source servers do it

| Project | Lang | Plugin model |
|---|---|---|
| **Music Assistant** | Python | `MusicProvider` + `PlayerProvider` classes; `supported_features: [ProviderFeature]`; two-phase playback `get_stream_details()` → `get_audio_stream()`; built-in providers shipped in the server |
| **Mopidy** | Python | A backend exposes `LibraryProvider` + `PlaybackProvider` + `PlaylistsProvider`; `translate_uri()` is often the only override; in-process |
| Navidrome | Go | WASM (sandboxed) plugins — for untrusted third-party code |
| Jellyfin | C# | Out-of-process assembly DLLs |
| Beets | Python | setuptools entry-point plugins |

Music Assistant is the closest blueprint: two provider kinds, per-provider capability
flags, providers compiled into the server.

## The plan: in-process, compiled-in Rust providers

Per the goal — "simple, open-source, compiled into the server" — we use enum-dispatch
traits gated by cargo features, not out-of-process/WASM (defer that until untrusted
plugins matter). This matches the existing `PlayerHandle` enum pattern (an enum, not
`dyn`, to keep async methods object-safe).

**Two trait families** (formalizing what already exists — `MusicProvider` +
`LocalDiskProvider`, and the `PlayerHandle`/`PlayerCapabilities` pair):

- `MusicProvider` (a *source*): `id` · `capabilities()` · `configure/start/stop` ·
  `health()` (Roon's Status analog) · `scan() -> Library` (library sources) ·
  `browse(path)` (non-scannable sources) · `resolve(id) -> StreamSpec` (MA's
  stream-details/stream split). **In practice the split lands across two layers:** the
  core `MusicProvider` trait stays sync and tokio-free (`id`/`capabilities`/`scan`),
  while the async, possibly DB- or network-backed methods (`validate`, `scan_with_progress`,
  `browse`, `resolve`) live on the server-side `ProviderHandle` enum where async
  dispatch already is. Radio (whose catalogue is a SQLite table) is the first source
  to implement `browse`/`resolve` there.
- `PlayerProvider` (an *endpoint*): `id` · `capabilities()` · `discover()` ·
  `execute(PlayerCommand)` · `subscribe() -> PlaybackState`.

**Mechanics:**
- Dispatch by **enum**, consistent with `PlayerHandle`; adding a provider = one variant
  + match arms.
- **Cargo features** (`provider-smb`, `provider-opensubsonic`, `snapcast`, …) compile
  providers in/out; default = local disk + browser + MPD.
  > **As built:** the actual features are `provider-smb`, `provider-opensubsonic`, `snapcast`,
  > and `ts`. The `provider-radio` / `player-airplay` names in this doc do **not** exist —
  > radio is always-present (not feature-gated), and AirPlay/Spotify cast-in is via Snapcast,
  > not a cargo feature.
- A **`ProviderRegistry`** built at startup from an explicit list (one entry per
  feature). Explicit list beats macro auto-registration for a handful of providers;
  adopt `inventory`/`linkme` only if that list becomes friction.
- **Runtime config** decides which available providers are *active* (like `--mpd` and
  the persisted players table today).
- Keep the **trait as the stable boundary** so a future WASM/subprocess host can wrap
  it without rewriting providers.

**Roon-derived rules baked in:** Core authoritative; capability negotiation; source ≠
transport; explicit auth/pairing for networked endpoints (the M10 server↔player auth
task); endpoint tiering native vs bridged.

## Rollout

- **M9 (sources):** ① **Internet radio** — the first non-library provider (no scan;
  user-managed stations; `resolve()` returns the stream URL). Realized first as a
  working feature (storage + native API + Subsonic internet-radio endpoints + web UI +
  a `PlayStream` player command), then **promoted to a real `ProviderHandle::Radio`**
  (a built-in, always-present source like local-disk, backed by the `radio_stations`
  table). It advertises `STREAM_ONLY` capabilities (`can_browse` + `can_stream`, no
  scan/search) and implements the two non-scannable methods the design called for:
  `browse() -> Vec<BrowseEntry>` (the saved stations, each carrying its stream URL) and
  `resolve(item_id) -> StreamSpec` (a station id → its playable stream). Exposed
  generically at `GET /api/sources/{id}/browse` and `/resolve?item=…`; the web Radio
  sidebar and Subsonic `getInternetRadioStations` both read through this path (station
  *management* — create/update/delete — stays on `/api/radio`). This is the first
  exercise of the source-vs-transport split for a non-library source and the template
  for future streaming providers (Spotify/Tidal/Qobuz), whose `resolve()` will mint a
  short-lived URL on demand rather than carrying it inline. ✅
  ② Formalize the provider abstraction. ✅ Landed as `ProviderCapabilities` + a
  `SourceFs` VFS (one scanner over any backend) + a `ProviderHandle` enum/
  `ProviderRegistry` (enum-dispatch, like `PlayerHandle`) + `merge_libraries` (N sources
  → one library, per-track `provider_id` kept). Sources persist (`sources` table, v15)
  and are managed via `/api/sources`. Local disk is the reference impl.
  ③ **SMB share** — the first remote-disk source. ✅ Pure-Rust `smb` crate (no mount/FFI),
  feature `provider-smb`: scanning drives the shared scanner over an SMB `SourceFs`
  (`Read+Seek`-over-`read_at` with a read-ahead cache for lofty); streaming reads only
  the requested byte range. Credentials stored plaintext for now (HARDEN IN M12).
  ④ **OpenSubsonic-as-source also shipped** — `ProviderHandle::OpenSubsonic` behind the
  `provider-opensubsonic` feature consumes a remote Subsonic/Navidrome/Funkwhale server as a
  music source. ✅
- **M10 (players):** ③ re-express Browser + MPD behind `PlayerProvider`; ④ one bridged
  endpoint (Snapcast or AirPlay/Chromecast) as the "Roon Tested" tier.
  > **As built:** item ③ did **not** happen — there is no `PlayerProvider` trait; Browser and
  > MPD still dispatch via the `PlayerHandle` enum. Item ④ **did** ship: the Snapcast bridged
  > endpoint (`PlayerHandle::Snapcast`, `snapcast` feature). See `docs/snapcast.md`.

References: Roon KB (RAAT, partner programs), `node-roon-api`, Music Assistant
developer docs, Mopidy backend API, the `inventory`/`linkme` crates.
