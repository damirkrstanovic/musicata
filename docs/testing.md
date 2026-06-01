# Testing roadmap

How Musicata is tested today, and what's planned. Two layers:

- **Rust tests** (`cargo test`) — correctness of the scanner, storage/SQL, HTTP and
  OpenSubsonic routes, the MPD protocol, SMB adapter, config, and the playback hot
  path. Fast, deterministic, no browser. Run on every commit (see the pre-commit hook).
- **UI / lag smoke suite** (`scripts/ui-smoke.sh`, see [`tests/ui/README.md`](../tests/ui/README.md))
  — drives the real web app in headless Chromium over CDP and asserts on user flows
  *and* responsiveness. `cargo test` never touches `static/`, so this is the only thing
  that exercises the actual UI. Runs against a read-only copy of the real library DB
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
- [ ] **Zones** — a command sent to a zone applies to every player in it; assert each
  member player receives it. No audio sync, just command fan-out.
- [ ] **Browser-output handoff** — one tab "owns" audio at a time. Two browser
  contexts: claiming output in tab B must release tab A (the `localStorage`
  coordination). Assert only one tab plays.
- [ ] **Heartbeat / disconnect** — browser playback stops when the playback-session
  heartbeat is lost; the player socket reconnects and controls recover after a server
  restart.
- [ ] **Server↔player auth** (blocked on Milestone 10) — once endpoints authenticate,
  test that an unauthenticated device can't register or control a player.

### Multi-controller & realtime

- [ ] **Two controllers stay in sync** — two browser tabs as controllers; a command in
  one reflects in the other over the WebSocket within a budget (the M5 "done when").
- [ ] **WebSocket reconnect** — drop and restore the socket; state resubscribes and the
  UI recovers without a manual refresh.

### Library scale & lag (extend the smoke suite)

- [ ] **Search / browse latency** on the large DB — type-to-search and facet filters
  return within a budget at ~11k tracks.
- [ ] **Artist/album navigation** render budgets on large discographies.
- [ ] Quantify and track the full-library render cost (currently asserted only as "no
  long task > budget"); record a baseline and alert on regressions.

### Other UI surfaces

- [ ] Metadata review/apply flow (approve/reject fields, artwork selection).
- [ ] Radio directory browse (Radio Browser proxy) and add/remove.
- [ ] Playlists & favorites CRUD from the UI.
- [ ] Error/offline states (source offline, stream 404, server down) surface correctly.
- [ ] PWA: service-worker caching + offline shell; cache-version bump behavior.
- [ ] Mobile layout (narrow viewport: drawer, bottom now-playing sheet) and basic a11y.

### Infrastructure

- [ ] Decide on CI wiring for the smoke suite (headless Chromium in CI, or keep it a
  local pre-commit gate). Today it's a local pre-commit gate only.
- [ ] A synthetic large-library fixture so scale tests don't depend on a developer's
  personal `.musicata/musicata.db`.
