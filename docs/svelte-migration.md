# Frontend migration: vanilla JS → Svelte

Plan for rewriting the embedded web app (`crates/musicata-server/static/`) as a
Svelte + TypeScript app, built with Vite and embedded into the server binary.

## Status

**The Svelte app at `/v2` + `/v2/admin` is at functional parity with the old app except the
metadata editor panel**, all CDP-verified against a scanned testdata library. The old vanilla
app still serves `/` + `/admin` (untouched). Done: admin (all 6 panels), player shell +
playback hot path, album/artist/track browsing + nav, queue drawer, search, favorites,
playlists + smart playlists, browse filters, internet radio, zones/output switching.

**Remaining to fully complete the migration:**
1. **Metadata editor panel** — the only missing feature. Brings the full `Track` +
   metadata-observation graph as generated ts-rs types (the component that consumes
   `observed_metadata`): canonical metadata, artwork review, MusicBrainz candidates.
2. **Redesign the lag/flow smoke harness for the Svelte app.** `tests/ui/instrument.mjs`
   wraps the *old app's global functions* (`updateFooterFromState`, `markActiveTrack`,
   `applyProgressTick`, `driveBrowserAudio`) to count hot-path work — none of which exist in
   the Svelte app. The hot-path assertion must be rebuilt around a MutationObserver (assert a
   progress tick mutates only the elapsed/seek nodes, not now-title/queue) or app-exposed test
   counters; `flows.mjs` selectors (`#play-pause`, `#segmented`, `#browse-grid`, `#back-btn`,
   `.link-artist`…) must map to the new DOM. This is the regression gate and must pass before
   cutover.
3. **Cutover (Phase 5).** Flip `/v2`→`/` and `/v2/admin`→`/admin`, remove the `include_str!`
   handlers + `static/`, retire the CLAUDE.md "no build step" convention.
4. **PWA (Phase 4).** `vite-plugin-pwa` service worker (replaces the hand-versioned `sw.js`).

Cutover is deliberately gated on (1) and (2): replacing the live app demands feature parity
*and* the hot-path regression suite, not a rushed flip.

- **Phase 0 (toolchain) — done.** `web/` (Svelte 5 + TS + Vite, two entries) builds via
  `build.rs`, embeds through `rust-embed`, served at `/v2` + `/v2/admin`. Headless-verified
  the Svelte app mounts. Old app at `/` untouched.
- **Phase 3 (player) — in progress.** Phase 3a (shell + playback hot path) done: a runes
  `player` store whose `elapsed`/`duration` are their own signals (the hot path — a progress
  tick mutates only those, so the footer time/seek move while now-playing/queue, which derive
  from `playback`, don't), a reconnecting per-player WebSocket routing `progress` ticks vs
  full `PlaybackState`, a `BrowserAudio` service (drive/report/ended), typed `PlayerCommand`
  sender, mediaSession glue, and a `Footer`/`SeekBar`. `PlaybackState`/`QueueItem`/
  `PlaybackStatus`/`RepeatMode` generated via ts-rs. Verified over CDP against a freshly
  scanned testdata library: clicking play starts a track and the elapsed advances
  (0:01→0:04). Phase 3b (browsing views) done: album grid + artists grid (infinite scroll via
  a `use:onVisible` action), album/artist detail heroes, a `TrackList` that plays the list at
  the clicked index, and a History-synced nav stack (tabs + Back). `Track` is a structural
  `TrackRow` (display + playback fields); the full `Track` + metadata graph is generated when
  the metadata panel lands. CDP-verified: grid → detail → play (elapsed advances) → Back.
  Phase 3c (queue + search) done: a queue drawer reading `PlaybackState.queue` (jump/reorder/
  remove/clear via commands, current-track highlight) and a debounced, AbortController-guarded
  search over `/api/search`. CDP-verified: 8-item queue, reorder reflected after the
  round-trip; search "dar" → artists/albums/tracks, 16 cards.
  Next: browse filters, playlists/favorites, metadata panel — then run the smoke suite's lag
  assertions against `/v2`.
- **Phase 2 (admin page) — done.** `/v2/admin` is a full Svelte port: Sources, Artwork &
  Settings, Players & Zones, Merged artists, Identification, Activity (live WS). Reuses the
  promise-based `Modal`, the typed `api` client, and the existing `styles.css`. Server admin
  DTOs (`SourceView`, `AppSettings`, `ArtistAliasGroup`, `Activity`) + `Player`/
  `ProviderCapabilities` are now ts-rs-generated; `i64`/`u64` wire fields overridden to
  `number` (ts-rs defaults them to `bigint`). Headless-verified: all panels render real data,
  remove-button rules correct (local source / browser player excluded). `svelte-check` clean.
- **Phase 1 (shared types) — in progress.** `ts` feature on `musicata-core` (off by default)
  generates TS via ts-rs; `scripts/gen-web-types.sh` regenerates into `web/src/types/`.
  Typed `web/src/lib/api.ts`. The `/v2` shell renders real `/api/library/summary` data
  through the typed client; `svelte-check` is clean.
  - **Done:** the flat library types — `LibrarySummary`, `Artist`, `Album`, `Playlist`,
    `RadioStation`, `Zone`.
  - **Deferred (typed with their components, not speculatively):** `Track` + the metadata
    observation graph, `Player`/`PlayerCapabilities`, `PlaybackState`/`QueueItem`/enums,
    `PlayerCommand`. The generic `Page<T>` envelope is hand-typed in `api.ts` (ts-rs doesn't
    export the wrapper); element types are generated.
  - **`json!` triage:** the ~95 ad-hoc response sites get promoted to structs (then
    ts-rs-derived) or hand-typed as their endpoints are consumed.

**Decisions (locked):**
- **Svelte 5 (runes) + TypeScript**, plain Svelte + **Vite** (no SvelteKit — we serve a
  static SPA from axum, no SSR/Node runtime).
- **Build integration: `build.rs` auto-builds.** `cargo build` invokes the Vite build and
  embeds the output, so assets never go stale. Node+npm become a build-time dependency.
- **Shared types:** generate TypeScript from the Rust API DTOs so client calls are checked
  against the server at compile time.
- **Big-bang cutover:** both pages rewritten on this branch, `static/` deleted in one
  switch. Sequenced internally (admin slightly ahead) to de-risk, but a single merge.

**The non-negotiable:** the playback hot path. `scripts/ui-smoke.sh` (66 checks) asserts
footer latency, no audio restarts, and **no full-state broadcast or DOM sweep on position
ticks**. This is the acceptance gate for every phase touching the player.

---

## Why this shape

- **No SvelteKit.** SvelteKit's value is SSR + filesystem routing + a Node server adapter.
  We have none of those needs: assets are static and embedded, and axum is the server.
  Plain Svelte + Vite gives the compiler and component model without the meta-framework.
- **Two Vite entry points**, mirroring today's two pages: `/` (player) and `/admin`. Each
  emits its own HTML + hashed JS/CSS bundle. No client router needed; the player keeps its
  in-page view + History-API back-stack, now expressed as components + a store.
- **Svelte 5 runes** give fine-grained, signal-based reactivity with no VDOM — structurally
  the same surgical DOM updates the player already does by hand, which is exactly what lets
  us preserve the position-tick optimization (see Hot path below).
- **Keep `styles.css` as-is** (3,080 lines) imported once per entry. CSS is orthogonal to
  the JS rewrite; rewriting it concurrently multiplies risk for no functional gain.
  Componentize/scope CSS later as a separate effort if desired.

---

## Toolchain & embedding

New layout (current `static/` stays until cutover):

```
crates/musicata-server/
  web/                      # the Vite + Svelte project
    package.json
    vite.config.ts          # two inputs (player, admin); vite-plugin-pwa
    tsconfig.json
    index.html              # player entry  -> /
    admin.html              # admin entry   -> /admin
    src/
      lib/                  # api client, ws, stores, shared components, utils
      player/               # player components
      admin/                # admin components
      types/generated.ts    # Rust → TS DTOs (generated; checked in or built)
      styles.css            # moved from static/, imported by each entry
    dist/                   # Vite output (hashed bundles + manifest); git-ignored
  build.rs                  # runs the Vite build, exposes dist/ to the crate
  src/...
```

**`build.rs`:**
1. `cargo:rerun-if-changed=web/src`, `web/package.json`, `web/vite.config.ts`,
   `web/index.html`, `web/admin.html` — rebuild only when the FE changes.
2. Run `npm ci` (when `node_modules` is missing) then `npm run build` in `web/`.
3. Fail the build loudly if Node/npm is absent **unless** `MUSICATA_SKIP_WEB_BUILD=1` is
   set, in which case fall back to a pre-existing `dist/` (escape hatch for offline/CI/
   Node-less environments). Emit a clear warning when falling back.
4. Pass `dist/`'s path to the crate via `cargo:rustc-env` (or rely on a fixed relative
   path) for the embed step.

**Embedding & serving:** replace the per-file `include_str!` handlers and the manual
`sw.js` cache bump with **`rust-embed`** (MIT) over `web/dist/`:
- Hashed asset bundles (`*.[hash].js/.css`) → served `Cache-Control: immutable, max-age=1y`.
- `index.html` / `admin.html` → served `no-cache` (so a new deploy is picked up; they
  reference the new hashed bundles).
- Content types from the file extension; a single embedded-dir handler replaces the eight
  hand-written asset routes in `main.rs`.
- Debug builds can use rust-embed's "read from disk" mode for fast iteration (no rebuild to
  see a CSS tweak), release builds embed.

**Service worker / PWA:** use **`vite-plugin-pwa`** to generate the SW + precache manifest
with automatic content-hash cache-busting. This *removes* the hand-versioned `CACHE`
constant in `sw.js` — a current maintenance footgun — while preserving offline behavior.

---

## Shared types (Rust → TS)

Audit finding: the API is mixed — ~49 `#[derive(Serialize)]` response structs (e.g.
`LibrarySummary`, `Player`, `PlaybackSessionResponse`, `RescanResponse`, page envelopes)
but ~95 ad-hoc `json!({...})` response sites.

Plan:
- Add **`ts-rs`** (MIT) `#[derive(TS)]` to the response/DTO structs; a `cargo test` exports
  them to `web/src/types/generated.ts`. These cover the typed half for free.
- For the ad-hoc `json!` responses: **prefer promoting the hot ones to real structs**
  (better server hygiene anyway), and hand-write TS types for the long tail. Track which
  endpoints are typed vs hand-modeled.
- A single typed `api()` client in `web/src/lib/api.ts` wraps `fetch` with the generated
  types, so every call site is checked. This is the main payoff justifying the toolchain.

---

## State & the playback hot path (must-preserve)

Today: a global `state` object; the per-player WebSocket sends a lightweight
`type:"progress"` message ~1×/s that `applyProgressTick` (`app.js:3231`) routes to update
*only* the elapsed/seek readout, early-returning before the full-state path so the track
list is never swept (`markActiveTrack`). Manual memoization (`state.lastTrackKey`,
`lastStatus`, `lastShuffle`) guards the per-second cost.

In Svelte 5:
- A **`playerStore`** (`$state` runes) holds what `state` holds today.
- **Position is its own signal** (`elapsed`, `duration`). The footer's seek + elapsed text
  bind to it; Svelte updates exactly those nodes on a tick — same surgical update as today,
  but the memoization band-aids disappear (reactivity skips unchanged work automatically).
- The track-list "active" highlight **derives** from an `activeTrackId` signal, recomputed
  only when it changes — never on a position tick.
- WebSocket handling moves to `web/src/lib/ws.ts` (per-player socket, reconnect/backoff,
  `progress` vs full-state routing) — a direct port of the current logic.
- **Validation:** port the footer + store + WS *first*, then run `scripts/ui-smoke.sh` and
  confirm the lag assertions stay green before building anything else on the player.

---

## Component inventory

**Shared (`web/src/lib/`):** `api.ts` (typed client), `ws.ts`, `stores` (player, library,
ui), `Modal.svelte` (promise-based dialog, ports `admin.js` `openModal`),
`infiniteScroll` (Svelte action wrapping the current IntersectionObserver helper),
`format.ts` (formatTime/escape).

**Player (`web/src/player/`):** `App.svelte` (shell + nav stack), `NavTabs`, `Footer`
(transport), `SeekBar`, `LibraryGrid` + `AlbumCard`/`ArtistCard`, `AlbumDetail`,
`ArtistDetail`, `TrackList` + `TrackRow`, `QueueDrawer` + `QueueRow`, `SearchBar`,
`BrowseFilters`, `PlaylistView`/`SmartPlaylistView`, `AddToPlaylistPopover`,
`MetadataPanel` (+ `CanonicalMetadata`, `ArtworkReview`, `MusicBrainzCandidates`).

**Admin (`web/src/admin/`):** `App.svelte`, `SettingsSection`, `SourcesPanel`,
`PlayersPanel`, `ZonesPanel`, `ActivityFeed` (WS), `MergeArtistsModal`.

This also fixes today's duplication (`buildAlbumCard`/`buildArtistCard`, the near-identical
`openAlbum`/`openArtist`/`openSmartPlaylistView`) by sharing real components.

---

## Phases (single branch, single cutover)

**Phase 0 — Toolchain scaffold.** Create `web/` (Vite + Svelte 5 + TS), two entries
rendering placeholders, `build.rs` + `rust-embed` wiring, axum serving the embedded `dist/`
at `/` and `/admin`. Gate: `cargo build` produces a binary that serves the Svelte shell;
smoke suite *loads* (even if flows fail). Proves the pipeline before any logic is ported.

**Phase 1 — Shared types + api client.** `ts-rs` export test; typed `api()`; audit/triage
the `json!` endpoints. Gate: generated types compile; a couple of real calls typecheck.

**Phase 2 — Admin page.** Smaller, no playback hot path — shakes out Modal/store/api/WS
patterns. Gate: admin smoke flows (sources, players, zones, activity feed) green.

**Phase 3 — Player.** Order: shell + `playerStore` + `ws.ts` + Footer/SeekBar **first**
(run smoke, confirm lag assertions), then library/album/artist/track views, then
queue/search/browse, then playlists, then metadata panel. Gate: full player smoke green at
each sub-step; never let the lag checks regress.

**Phase 4 — PWA + cleanup.** `vite-plugin-pwa` service worker; delete `static/`, the
per-file asset handlers, and the manual `sw.js` cache bump.

**Phase 5 — Cutover & docs.** Full `scripts/ui-smoke.sh` + manual QA on a real library.
Rewrite the **CLAUDE.md** frontend conventions (the "vanilla HTML/CSS/JS, **no build
step**" and "bump `CACHE` in `sw.js`" lines are now false), update `docs/style-guide.md`,
and document the `web/` build in "Build / test / run".

---

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| Position-tick latency regresses | Port Footer/store/WS first; run smoke each step; keep position as its own signal. |
| Node becomes a hard build dep | `MUSICATA_SKIP_WEB_BUILD=1` + prebuilt `dist/` fallback for CI/offline. |
| `json!` endpoints aren't typed | Promote hot ones to structs; hand-write TS for the tail; track coverage. |
| Bundle bloat | Measure gzipped output vs today's ~140 KB JS; Svelte output should be ≤ that. Fail loud if not. |
| Smoke suite drives real assets | Unchanged — it runs the built binary, which now embeds the Vite bundle. |
| CSS rewrite scope creep | Keep `styles.css` global and as-is for the migration; componentize later. |

**Licenses (AGPL-compatible, all permissive):** Svelte (MIT), Vite (MIT), `rust-embed`
(MIT), `ts-rs` (MIT), `vite-plugin-pwa` (MIT). Verify each at add time per project policy.

---

## Convention changes this lands

- `static/` (vanilla, no build step) → `web/` (Svelte + Vite, built by `build.rs`).
- `include_str!` per file → `rust-embed` over `dist/`.
- Manual `sw.js` `CACHE` bump → `vite-plugin-pwa`-generated, content-hashed.
- CLAUDE.md "no build step" convention is retired; `cargo build` now needs Node+npm
  (or `MUSICATA_SKIP_WEB_BUILD=1` with a prebuilt `dist/`).
