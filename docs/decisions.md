# Decision log

Short, dated records of non-obvious choices made while building Musicata — the "why" behind
work that isn't self-evident from the code. Newest first. Referenced from `roadmap.md`.

## 2026-06-27 — musicata-ml Phase 1 (audio embeddings, the experiment)

Building the optional audio-ML service (`crates/musicata-ml`). Design + run guide in
`musicata-ml.md`; the choices:

- **Rust + ONNX (`ort`), not Python/Essentia** — per the brief ("keep the model in Rust"). `ort`
  with `download-binaries` fetches ONNX Runtime, so there's no system install and no Python at
  runtime. Validated `ort` builds + runs here before writing the service.
- **Picked a raw-waveform model (PANNs CNN14 16 kHz) on purpose.** Its ONNX graph contains the
  mel-spectrogram, so the input is the raw audio samples. This **eliminates the biggest risk** —
  hand-writing a model-exact mel-spectrogram in Rust — leaving only decode → mono → resample to
  16 kHz (symphonia + rubato, already in the tree). It outputs both a 2048-d embedding *and* 527
  AudioSet tags from one pass, so similarity + tagging come together. Verified against a real
  track (a dub track scored Music/Drum/Bass/Percussion — correct).
- **Model is fetched + cached at runtime, never committed** (~327 MB). The 527 AudioSet display
  names *are* bundled (8 KB) so tags are human-readable without a download.
- **Excluded from the default workspace build** (like the endpoint) — `ort`/ONNX Runtime is heavy
  and network-fetched; the server's build/test/release path must not depend on it. Build with
  `cargo build -p musicata-ml`.
- **HTTP boundary, POST raw bytes to `/analyze`.** Keeps the ML service stateless and fully
  decoupled — the server fetches a track's bytes and posts them; the ML service knows nothing
  about the library. The model runs inside `spawn_blocking` behind a mutex (one inference at a
  time; ONNX Runtime parallelizes internally) — fine for a background batch service.
- **Deferred to later phases:** sqlite-vec storage + the **scheduled** worker (default 02:00
  local), recommendation integration, and packaging. Phase 1 is just the verified service.

### Phase 2b — scheduled worker, settings

- **The ML worker is scheduled, not always-draining** (unlike the other workers): off by default,
  it runs at a user-set daily **local** time (default 02:00), or on demand via `POST
  /api/ml/analyze`. Local time is computed with `libc::localtime_r` (no datetime dep — the project
  avoids chrono); `seconds_until` is pure + unit-tested. Settings (`ml_enabled`/`ml_service_url`/
  `ml_schedule`) live in the DB + `/api/settings`, per the in-product-config convention. `libc`
  became a non-optional server dep (was snapcast-only).
- **The service analyzes a centered ~15 s excerpt, not the whole track.** PANNs is trained on
  ~10 s AudioSet clips, so a short centered window is both faster and more model-appropriate.
- **CPU only — GPU was investigated and dropped.** A release build does ~1 s/track on CPU; the
  cost is the audio decode, not inference, so GPU offers no speedup. I trialled the ONNX Runtime
  GPU execution providers (CUDA/ROCm/MIGraphX/WebGPU) on the dev box — WebGPU/Vulkan and MIGraphX
  did engage the AMD GPU, but neither beat release CPU for this model, so they weren't worth the
  build/deploy complexity. The lesson that mattered: **always measure release** — debug-build
  timings were decode-dominated and made GPU look necessary when it wasn't.

### Phase 2a — sqlite-vec as standard storage (not ML-gated)

- **sqlite-vec is loaded on every Musicata connection, unconditionally** (per the directive:
  "make it standard for musicata without ml also"). `register_sqlite_vec()` registers
  `sqlite3_vec_init` as a global SQLite **auto-extension** once per process, before the sqlx pool
  opens — so the shared bundled SQLite (sqlx + a direct `libsqlite3-sys` dep at the same version)
  has `vec0` available on every connection. Cheap, and it removes the conditional/wiring risk:
  vector search is just ordinary storage now.
- **The embedding index is a `vec0` virtual table** (`track_embedding`, `float[2048]`,
  `distance_metric=cosine` — cosine is the right metric for these embeddings), with a companion
  `track_features` table for model provenance + AudioSet tags JSON. vec0 has no UPSERT, so a
  re-analysis deletes+reinserts the vector row. KNN similarity (`similar_by_embedding`) fetches
  the seed vector and `ORDER BY distance LIMIT k+1`, dropping the seed. Validated end-to-end (a
  vec0 KNN test + a 2048-d store/similarity test).

## 2026-06-27 — Native endpoint prototype (M10)

A native playback endpoint (`crates/musicata-endpoint`). Design + usage in `native-endpoint.md`;
the key calls:

- **Reuse the browser player server-side.** A native endpoint and a browser tab are the same
  server-side thing — a queue-output driven over the bidirectional WS. So `bring_up` maps the
  `native` kind to a `BrowserPlayer`; no new player variant, no duplicated queue logic. The only
  difference is identity (registered + tokened vs. the singleton browser). This kept the server
  change tiny and the endpoint automatically inherits zones, leveling, and the command model.
- **The endpoint holds only its scoped player token, never a user account.** That token already
  authenticated the WS channel (shipped earlier); this round it was **extended to authorize the
  audio streams** the endpoint fetches (`GET /api/tracks/{id}/stream`, via `player_token_exists`).
  Without that, a tokenless-but-userless device couldn't fetch what it plays. Bootstrap
  registration still needs a one-time **user** API token (registration is user-gated — the player
  token doesn't exist yet at that point); after that the device uses only its player token.
- **Blocking client (tungstenite + rodio), not async.** rodio is sync and the control loop is
  simple; a single-threaded blocking design (WS read with a short socket timeout as a ~2 Hz tick
  for progress/ended) is far less code than an async runtime. `ws://`-only (LAN); whole-track
  buffering; no seek-follow/gapless — all acceptable prototype limits, documented.
- **Excluded from the default workspace build.** rodio pulls cpal → ALSA, which the server build
  must not require. `default-members` lists the three server crates only, so the pre-commit's
  plain `cargo test` and the `-p musicata-server` release/Docker builds never touch the endpoint;
  it builds on demand (`cargo build -p musicata-endpoint`).
- **Control logic is a pure `decide()` function** separate from audio/IO, so the state machine is
  unit-tested without a sound device; the audio path itself is a manual run (like `live_mpd`).

## 2026-06-27 — Roadmap sweep #3: UI surfacing (M7 stats view, M11 leveling selector)

Surface two already-shipped backends in the web player.

- **M11 leveling selector.** The boolean leveling toggle became an explicit Off/Track/Album
  selector (`dsp.levelingMode`). The legacy persisted boolean migrates to `album` (the Auto
  behavior shipped last round), so existing users keep album-aware leveling. `audio.ts` applies
  the mode; the smoke test now drives the `<select>` and checks track-mode boost vs album-mode
  using-the-album-LUFS.
- **M7 stats view.** A footer "Listening stats" panel (`StatsPanel.svelte` + a tiny
  `statsPanel` store) fetches `GET /api/history/stats` on open and renders the figures. The
  `HistoryStats` type is **hand-typed in `api.ts`** rather than ts-rs-generated, because the
  server serializes that response without a ts-rs derive (it's a plain `Serialize` struct);
  this matches the existing convention for the few non-ts-rs endpoints. The panel reuses the
  shared `.eq-drawer`/`.eq-head` rail-overlay classes. Smoke-tested (panel opens, renders rows).

## 2026-06-27 — Roadmap sweep #2: M9 (Internet Archive), M11 (album leveling), M10 (player auth)

### M9 — Internet Archive provider

- **Built an Internet Archive source, modelled on the podcast provider.** Browse-only
  (`STREAM_ONLY`), no API key, no new deps (the JSON metadata + download endpoints are public).
  Feature `provider-archive`, default-on.
- **A source is one IA *item*, not a collection/search.** The flat `/api/sources/{id}/browse`
  shape (no path param) can't drill collection → item → files, so a source maps to a single
  item identifier and `browse()` lists that item's audio files — clean and useful for the
  live-music/netlabel case. Multi-item collection/search browse is a documented follow-up.
- **One file per track.** IA carries the same track in several formats (FLAC + MP3 + Ogg);
  `parse_item` groups by file stem and keeps the best-ranked format, so the track list isn't
  triplicated. The stream URL is the canonical `archive.org/download/<id>/<file>` (range-capable),
  with the path percent-encoded in-house (no url-encoding dep).

### M11 — Album volume leveling

- **"Album mode" is delivered as smarter behaviour of the existing leveling toggle, not a new
  UI control.** When leveling is on, the browser now prefers the track's **album** integrated
  loudness when it's available, falling back to per-track — i.e. the design's *Auto* mode. This
  keeps an album's internal dynamics intact (quiet interludes stay quiet) without adding a
  Track-vs-Album selector that would also churn the smoke suite. An explicit selector is a
  documented follow-up.
- **Album loudness is an energy-weighted, duration-weighted mean of the per-track measurements**,
  not a fresh R128 pass over concatenated audio. True album-integrated R128 can't be derived
  exactly from per-track values (the gating is non-linear), but the duration-weighted linear mean
  is the standard, well-defined approximation (what ReplayGain-style tools use) and needs no
  re-decode. Stored in a new `album_loudness` table (migration), recomputed from `track_loudness`
  after each loudness pass that did work.

### M10 — Player endpoint auth

- **Implemented the per-player token mechanism now (not deferred).** Last round it was designed
  but deferred "until something presents a token." This round, the user asked for M10, so the
  mechanism shipped: token issuance (`issue_token` at registration), SHA-256 storage
  (`players.auth_token_hash`, migration v30), and `require_auth` accepting a player token on that
  player's `/state`/`/commands`/`/ws` channels in place of a user session.
- **It is additive and opt-in, so it changes nothing for existing players.** The token is
  consulted only when user auth fails, only for those three channel paths, and only for a player
  that has a token. Server-initiated backends (browser/MPD/Snapcast) carry no token and stay
  user-gated — the web app keeps working unchanged. This is why it was safe to enforce now even
  though the self-registering native-endpoint *kind* (which would actually present the token)
  doesn't exist yet; that endpoint program + its player variant remain the open M10 piece.
- Tested end to end over HTTP (tokened channel reachable token-only; wrong/absent token 401;
  token scoped to its own player) plus unit tests. See `player-auth.md`.

## 2026-06-27 — Roadmap sweep: M7, M9, M10, M12

A batch of milestone work was done autonomously; the judgement calls are recorded here.

### M7 — Listening stats (session/streak + favorites stats)

- **Scope:** shipped a read-only **stats API** (`GET /api/history/stats`) and the storage query
  behind it, covering the two open M7 stats items — *session/streak views* and *favorites
  stats* — plus library-wide play/skip totals. Left as a follow-up: a web view for it and the
  remaining "richer playback events" (loved/disliked/rated as discrete events; favorites already
  cover "loved"). Rationale: the data + API is the load-bearing, testable part; the UI is a thin
  read-only follow-up that would also need the smoke suite extended, so it's deferred rather than
  rushed.
- **A "session" is a run of plays with < 30 min between consecutive listens.** No `session_id`
  column was added — sessions are derived in Rust from ordered played-listen timestamps, so the
  definition can change without a migration and historical data participates immediately.
- **A "streak" is consecutive UTC calendar days with ≥ 1 played listen.** UTC (not local) is
  deliberate: the server has no reliable user timezone, and UTC keeps the query and the tests
  deterministic. Current streak counts back from today (or yesterday, so a streak isn't "broken"
  until a full day is missed).

### M9 — Podcasts provider + plugin isolation

- **Built a Podcasts (RSS) source, not Jamendo, as the next provider.** Both were open M9 items;
  podcasts won because RSS needs no API key or account (Jamendo needs a `client_id`), maps
  cleanly onto the existing **STREAM_ONLY browse/resolve** provider shape (episodes are streams
  with known enclosure URLs, exactly like radio stations), and is fully unit-testable offline
  from a fixture feed. It is **feature-gated `provider-podcast`** (default-on, like
  `provider-opensubsonic`) and added through the same `/api/sources` path. New dep: `quick-xml`
  (MIT) for RSS parsing.
- **A podcast source is browse-only (`STREAM_ONLY`), not scanned into the library.** Episodes are
  fetched and parsed on demand in `browse()` (the feed URL lives in the source's `host` column,
  reusing the OpenSubsonic field convention) — never on the scan hot path. This mirrors the
  internet-radio provider and keeps podcast feeds (which change often and can be huge) out of the
  canonical track tables.
- **No `/admin` UI for adding a podcast yet** — it's reachable via `POST /api/sources` with
  `{"kind":"podcast","host":"<feed-url>"}`. UI is a follow-up; the provider and its tests are the
  substance.
- **Plugin isolation (the standing "evaluate isolation" item) is now decided** — see
  `plugins.md`. Short version: first-party providers stay **in-process enum-dispatch**;
  untrusted third-party plugins, if ever, target the **WASM component model** with an
  out-of-process subprocess as the fallback. No third-party plugin loading ships now.

### M10 — Player providers & endpoint capabilities

- **Shipped `PlayerCapabilities`** (advertised per player, like `ProviderCapabilities` for
  sources) and surfaced it on the player descriptor / `GET /api/players`. This completes the
  "define PlayerProvider and endpoint capabilities" item: a controller can now ask what a player
  supports (seek, volume, queue editing, streams) instead of probing.
- **Per-player endpoint auth token: designed and documented, enforcement deferred.** See
  `player-auth.md`. The token column + issuance are *not* added yet because the only thing that
  would present it — a native non-browser endpoint that registers *itself* — does not exist; the
  browser, MPD and Snapcast players are all **server-initiated** and already covered by the
  user-session `require_auth` middleware. Adding an unenforced token now would be security
  theatre. The design is committed so it lands with the native endpoint (the M10 "native endpoint
  prototype" task).

### M12 — Packaging, security & operations

- **Release builds, the systemd unit, and the Docker image are done** (commit `557b2f3`:
  `.github/workflows/release.yml`, `packaging/musicata.service`, `Dockerfile`) and the roadmap is
  updated to reflect that. `docs/deployment.md` documents running them.
- **Backup/restore is documented** (`deployment.md`) on top of the existing library
  export/import; the database + artwork cache live under one state dir (`/var/lib/musicata`), so
  backup is "copy that directory while stopped, or snapshot it."
- **Source secrets (SMB/MPD/OpenSubsonic passwords) stay plaintext at rest — deliberately, for
  now.** Encrypting them only helps if the key lives somewhere the DB-reader can't reach; without
  an OS keyring or hardware-backed key the key would sit next to the database and the encryption
  would be decorative. Given Musicata's **LAN-first** posture, the honest mitigation is
  filesystem permissions (the systemd unit already runs as a locked-down system user with state
  in `0750` `/var/lib/musicata`) plus clear documentation. Real encryption is revisited if/when
  an OS-keyring integration lands. Documented in `deployment.md` and the roadmap.
