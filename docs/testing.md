# Testing roadmap

How Musicata is tested today, and what's planned. Two layers:

- **Rust tests** (`cargo test`) — correctness of the scanner, storage/SQL, HTTP and
  OpenSubsonic routes, the MPD protocol, SMB adapter, config, and the playback hot
  path. Fast, deterministic, no browser. Run on every commit (see the pre-commit hook).
- **UI / lag smoke suite** (`scripts/ui-smoke.sh`, see [`tests/ui/README.md`](../tests/ui/README.md))
  — drives the real web app in headless Chromium over CDP and asserts on user flows
  *and* responsiveness. `cargo test` never builds `web/` (the web app builds to
  `crates/musicata-server/web/dist/`), so this is the only thing that exercises the actual UI. Runs against a read-only copy of the real library DB
  (~11k tracks) when present, for realistic scale; falls back to the testdata fixture.
  Also runs on every commit (skips cleanly if no Chromium).

## Current coverage

| Area | Rust | UI smoke |
|------|------|----------|
| Scan / metadata / incremental | ✅ broad | — |
| Storage, search, browse, pagination | ✅ | — |
| Native HTTP routes + error envelopes | ✅ | initial load |
| OpenSubsonic surface | ✅ | — |
| Players: registry, zones, recording | ✅ | play / pause / next / seek |
| Playback hot path (progress frame, start_index) | ✅ regression tests | play-first / play-later, steady-playback no-waste |
| Provider system (radio browse/resolve, sources) | ✅ | internet radio |
| Queue | partial | reorder, no-restart |
| Lag / responsiveness | — | footer latency, no per-tick DOM work, render+scroll long tasks |

## Planned

### Player testing (priority)

The smoke suite only covers the **browser** player today; the player layer is otherwise
Rust-only. Add:

- [ ] **Player switch** end-to-end — currently auto-skipped because only the browser
  player is registered. Needs a second registered player in the harness (a fake/stub
  player provider, or a headless MPD) so the switch flow actually exercises footer
  re-init and per-player state.
- [ ] **MPD player** end-to-end — drive a real or embedded MPD (or a scripted TCP
  fake) through play/pause/seek/next/enqueue and assert state syncs over the WebSocket.
  (Unit-level MPD protocol parsing is already covered.)
- [ ] **Zones** — switching output to a zone and playing through it are covered
  (`switched output to a zone`, `zone plays (elapsed advances)`); the remaining gap is
  **fan-out to multiple members** — assert each member player receives the command.
- [ ] **Browser-output handoff** — one tab "owns" audio at a time. Two browser
  contexts: claiming output in tab B must release tab A (the `localStorage`
  coordination). Assert only one tab plays.
- [ ] **Heartbeat / disconnect** — browser playback stops when the playback-session
  heartbeat is lost; the player socket reconnects and controls recover after a server
  restart.
- [ ] **Server↔player auth** — core multi-user auth has shipped (`auth.rs`, with
  `auth_setup_then_gates_protected_routes` covering the gate); the remaining item is
  per-*player* endpoint auth: test that an unauthenticated device can't register or
  control a player.

### Multi-controller & realtime

- [ ] **Two controllers stay in sync** — two browser tabs as controllers; a command in
  one reflects in the other over the WebSocket within a budget (the M5 "done when").
- [ ] **WebSocket reconnect** — drop and restore the socket; state resubscribes and the
  UI recovers without a manual refresh.

### Library scale & lag (extend the smoke suite)

- [x] **Search / browse latency** on the large DB — covered by `latency: search renders
  within budget` (behavior) and `search at scale returns a bounded page` +
  `browse filter changes the grid` (scale).
- [ ] **Artist/album navigation** render budgets on large discographies.
- [ ] Quantify and track the full-library render cost. Windowing is now asserted
  (`initial render is windowed`, `infinite scroll appends a page`); what's missing is a
  recorded **baseline** to alert on regressions.

### Other UI surfaces

- [ ] Metadata review/apply flow — field approval is covered (`metadata panel opens`,
  `metadata field approval persists`); **artwork selection** is not.
- [ ] Radio — browse and play are covered (`radio station listed`, `radio play sets
  now-playing`, `radio enqueues a station after the seed`); **add/remove** of a station is not.
- [ ] Playlists & favorites — the favorite toggle and smart-playlist read are covered
  (`favorite toggles a track`, `smart playlist opens + lists tracks`); **playlist
  create/rename/delete** is not.
- [ ] Error/offline states (source offline, stream 404, server down) surface correctly.
- [ ] PWA: service-worker caching + offline shell; cache-version bump behavior.
- [ ] Mobile layout (narrow viewport: drawer, bottom now-playing sheet) and basic a11y.

### Infrastructure

- [ ] Decide on CI wiring for the smoke suite (headless Chromium in CI, or keep it a
  local pre-commit gate). Today it's a local pre-commit gate only.
- [ ] A synthetic large-library fixture so scale tests don't depend on a developer's
  personal `.musicata/musicata.db`. **Now the blocking gap for outside contributors:**
  `testdata/` is git-ignored (it holds real, copyrighted music), so a fresh clone can run
  neither phase — `behavior` has no fixture library and `scale` skips for want of a DB.
