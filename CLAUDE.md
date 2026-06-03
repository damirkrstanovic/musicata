# Musicata — project guide for Claude

Local-first music server + web controller. Rust workspace, milestone-driven (see
`docs/roadmap.md`).

## Prior art — read before solving a hard problem

**`docs/prior-art.md`** captures how Roon, Jellyfin, Navidrome, beets, Picard, Music
Assistant and Mopidy solve the problems we keep hitting (provider/plugin ecosystem,
incremental library scanning, OpenSubsonic compatibility, SMB access, background-work
UX, metadata sourcing & conflict resolution, and discography completeness) and what
Musicata adopted, with code pointers. When you work in one of those areas, read the
relevant section first rather than re-deriving the design — and append to it when you
learn something new from another project. Reference checkouts live next to this repo
(`../jellyfin`, `../navidrome`).

Other docs: `docs/plugins.md` (Roon research + provider plan), `docs/api.md` (native
+ OpenSubsonic API), `docs/style-guide.md` (web UI conventions), `docs/metadata.md`,
`docs/roadmap.md`.

## Layout

- `crates/musicata-core` — domain types + the scanner. Pure, sync, dependency-light
  (lofty/serde/sha2); **no tokio**. `MusicProvider`, `SourceFs` VFS,
  `scan_source_incremental`, `merge_libraries`.
- `crates/musicata-storage` — SQLite via sqlx. Migrations by `PRAGMA user_version`
  (currently v18). One library cache + separate tables (players, player queues,
  zone queues,
  playlists, favorites, radio, sources, activities).
- `crates/musicata-server` — axum 0.8 (+ws), the providers/registry, players, the
  OpenSubsonic surface, and the embedded web app (`static/`, vanilla HTML/CSS/JS, no
  build step; two pages: `/` player, `/admin`).

## Conventions (hard-won)

- **Enum dispatch, not `dyn`**, for `ProviderHandle` / `PlayerHandle` — async methods
  stay object-safe; a new backend is one variant + match arms, cargo-feature-gated.
- **Capabilities are advertised** (`ProviderCapabilities`) and callers skip what a
  source can't do.
- **Incremental scans**: reuse parsed metadata for files with unchanged size+mtime;
  read tags only for new/changed files; watch the local FS, fall back to a periodic
  pass for network sources. (See prior-art §2.)
- **Network is never on a request's hot path**: bind the web port before scanning;
  connect/scan in the background with timeouts; surface progress/errors via the
  activity log + WebSocket, not blocking calls or polling.
- **Static assets are served `no-cache`** and embedded via `include_str!`; bump the
  `CACHE` version in `static/sw.js` whenever a static asset changes.
- AGPL-3.0; check a new dependency's license before adding it.

## Build / test / run

```
cargo build                                   # default features (incl. SMB source)
cargo build --no-default-features             # minimal: drop the SMB `smb` dep
cargo test                                     # all crates (SMB tests run by default)
cargo run -p musicata-server -- --library <dir> --addr 127.0.0.1:3030
```

- **Before testing against a *running* server, `cargo build`** — `cargo test` only
  builds the test harness, not the `target/debug/musicata-server` binary.
- `cargo fmt` reformats multi-line edits — re-read before editing a region you just
  changed.
- The repo has a real fixture library at `testdata/`.
- Verify UI changes with the headless browser at
  `~/.cache/ms-playwright/chromium-1217/chrome-linux64/chrome` (launch with
  `--headless=new --no-sandbox`). The **`scripts/ui-smoke.sh`** suite (see
  `tests/ui/README.md`) automates this: it drives the real web app over CDP and
  asserts on user flows *and* lag (footer latency, no audio restarts, no full-state
  broadcast or DOM sweep on position ticks). `cargo test` covers the server only — it
  does **not** touch `static/`, so run the smoke suite after changing the web app.
  Rust-level guards for the playback hot path live in `players.rs` tests.

## Git

Branch for feature work; commit/push only when asked. Co-author trailer:
`Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
