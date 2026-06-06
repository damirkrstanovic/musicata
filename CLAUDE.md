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
  OpenSubsonic surface, and the embedded web app (`web/` — Svelte 5 + TypeScript + Vite,
  built by `build.rs` and embedded via `rust-embed`; two pages: `/` player, `/admin`).
  Wire types are generated from the Rust structs by ts-rs (`scripts/gen-web-types.sh`).

## Conventions (hard-won)

- **Configuration lives in the product, not in flags/env/config files.** Musicata is
  for an ordinary user, not an operator editing YAML. User-facing settings (enable
  artwork fetching, an API key, a music source, a player) are **persisted in the DB and
  edited in the web UI** (the `/admin` Settings page) — live, no restart. CLI flags /
  env vars exist only for *bootstrap* (where the DB and library live, the bind address)
  and test harnesses, not for features a user would toggle. When adding a feature, add a
  setting + UI, not a `--flag`.
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
- **The web app is built by `build.rs`** (Vite) and embedded via `rust-embed` from
  `web/dist/`. Hashed `/assets/*` bundles are served immutable; the HTML entries
  `no-cache`. `cargo build` therefore needs Node+npm (or `MUSICATA_SKIP_WEB_BUILD=1` with a
  prebuilt `web/dist/`). Edit components in `web/src/`; run `npm run check` (svelte-check).
- AGPL-3.0; check a new dependency's license before adding it.

## Build / test / run

```
cargo build                                   # default features (incl. SMB source)
cargo build --no-default-features             # minimal: drop the SMB `smb` dep
cargo test                                     # all crates (SMB tests run by default)
cargo run -p musicata-server -- --library <dir> --addr 127.0.0.1:3030
```

- **Frontend** is Svelte 5 + TS + Vite in `crates/musicata-server/web/`; `build.rs` runs
  the Vite build on every `cargo build`, so **Node + npm are build dependencies** (set
  `MUSICATA_SKIP_WEB_BUILD=1` with a prebuilt `web/dist/` to skip offline). Regenerate the
  Rust→TS wire types with `scripts/gen-web-types.sh` after changing a `#[derive(ts_rs::TS)]`
  struct. (See `docs/svelte-migration.md` for the migration history.)
- **Before testing against a *running* server, `cargo build`** — `cargo test` only
  builds the test harness, not the `target/debug/musicata-server` binary.
- `cargo fmt` reformats multi-line edits — re-read before editing a region you just
  changed.
- The repo has a real fixture library at `testdata/`.
- Verify UI changes with the headless browser at
  `~/.cache/ms-playwright/chromium-1217/chrome-linux64/chrome`. The **`scripts/ui-smoke.sh`**
  suite (→ `scripts/v2-smoke.sh` + `tests/ui/v2-flows.mjs`) drives the Svelte app over CDP
  and asserts user flows *and* the playback hot path — via a MutationObserver, a progress
  tick must move only the elapsed/seek text, never the now-title. `cargo test` covers the
  server only and does **not** build `web/`, so run the smoke suite after changing the UI.
  Run `npm run check` in `web/` for typecheck.
  Rust-level guards for the playback hot path live in `players.rs` tests.

## Git

Branch for feature work; commit/push only when asked. Co-author trailer:
`Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
