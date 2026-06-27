# Repository Guidelines

Musicata is a local-first music server + web controller. Rust workspace, milestone-driven
(see `docs/roadmap.md`). **This is the single source of truth for human and AI contributors;
`CLAUDE.md` includes this file.**

## Prior art — read before solving a hard problem

**`docs/prior-art.md`** captures how Roon, Jellyfin, Navidrome, beets, Picard, Music
Assistant and Mopidy solve the problems we keep hitting (provider/plugin ecosystem,
incremental library scanning, OpenSubsonic compatibility, SMB access, background-work UX,
metadata sourcing & conflict resolution, discography completeness, artwork, artist identity)
and what Musicata adopted, with code pointers. When you work in one of those areas, read the
relevant section first rather than re-deriving the design — and append to it when you learn
something new from another project. Reference checkouts live next to this repo
(`../jellyfin`, `../navidrome`, `../snapcast`, `../immich`).

Per-feature designs: `docs/dsp.md` (EQ / room & headphone correction), `docs/loudness.md`
(EBU R128 volume leveling), `docs/recommendations.md` (ListenBrainz similar & radio),
`docs/continuous-play.md` (autoplay), `docs/snapcast.md` (synchronized network transport).
Other docs: `docs/plugins.md`, `docs/api.md` (native + OpenSubsonic), `docs/style-guide.md`
(web UI conventions), `docs/metadata.md`.

## Project Structure & Module Organization

Rust workspace; keep provider-neutral logic out of server-specific code.

- `crates/musicata-core` — domain types + the scanner. Pure, sync, dependency-light
  (lofty/serde/sha2); **no tokio**. `MusicProvider`, `SourceFs` VFS, incremental scan,
  `merge_libraries`.
- `crates/musicata-storage` — SQLite via sqlx. Migrations by `PRAGMA user_version`. One
  library cache + separate per-feature tables (players, player/zone queues, playlists,
  favorites, radio, sources, activities, listens, fingerprints, loudness, …).
- `crates/musicata-server` — axum 0.8 (+ws): the providers/registry, players, the
  OpenSubsonic surface, and the embedded web app. Wire types are generated from the Rust
  structs by ts-rs (`scripts/gen-web-types.sh`).
- `crates/musicata-server/web/` — the web app: **Svelte 5 + TypeScript + Vite**, built by
  `build.rs` and embedded via `rust-embed` from `web/dist/`. Two pages: `/` player, `/admin`.
- `docs/` — research, requirements, roadmap, and per-feature designs. `testdata/` is a real
  fixture library used by scanner tests and the UI smoke suite.

Avoid adding source files at the repository root unless they are standard project entry points.

## Build, Test, and Development Commands

```
cargo build                         # default members; runs the Vite web build
cargo build --no-default-features   # minimal: drop the SMB `smb` dep
cargo test                          # default members (SMB tests run by default)
cargo run -p musicata-server -- --library <dir> --addr 127.0.0.1:3030
cargo build -p musicata-endpoint    # the native audio endpoint — opt-in (needs audio dev libs)
cargo build -p musicata-ml          # the audio-ML service — opt-in (ort/ONNX Runtime; network)
```

`musicata-endpoint` (native playback client, rodio → ALSA) and `musicata-ml` (audio-embedding
service, ort/ONNX Runtime) are workspace members but **not** default members, so plain `cargo
build`/`cargo test` skip them and the server never needs audio or ML libraries. Build/test them
explicitly. See `docs/native-endpoint.md` and `docs/musicata-ml.md`.

- **Node + npm are build dependencies** — `build.rs` runs the Vite build on every `cargo
  build`. Set `MUSICATA_SKIP_WEB_BUILD=1` with a prebuilt `web/dist/` to skip (offline /
  Rust-only iteration).
- Flags/env are **bootstrap-only** (where the DB/library live, the bind address), not feature
  toggles: `--config`, `MUSICATA_LIBRARY`, `MUSICATA_DATABASE`, `MUSICATA_ADDR`,
  `MUSICATA_RESCAN`, `MUSICATA_INCREMENTAL_RESCAN`. `--scan-once` runs the incremental scan and
  exits without binding a port.
- Regenerate the Rust→TS wire types with `scripts/gen-web-types.sh` after changing a
  `#[derive(ts_rs::TS)]` struct.
- **Before testing against a *running* server, `cargo build`** — `cargo test` only builds the
  test harness (not the `target/debug/musicata-server` binary) and does **not** build `web/`.
- In `web/`: `npm run check` (svelte-check) for the frontend typecheck.

## Conventions (hard-won)

- **Configuration lives in the product, not in flags/env/config files.** Musicata is for an
  ordinary user, not an operator editing YAML. User-facing settings (enable artwork fetching,
  an API key, a music source, a player) are **persisted in the DB and edited in the web UI**
  (the `/admin` Settings page) — live, no restart. CLI flags / env vars exist only for
  *bootstrap* and test harnesses. When adding a feature, add a setting + UI, not a `--flag`.
- **Enum dispatch, not `dyn`**, for `ProviderHandle` / `PlayerHandle` — async methods stay
  object-safe; a new backend is one variant + match arms, cargo-feature-gated.
- **Capabilities are advertised** (`ProviderCapabilities`) and callers skip what a source
  can't do.
- **Incremental scans**: reuse parsed metadata for files with unchanged size+mtime; read tags
  only for new/changed files; watch the local FS, fall back to a periodic pass for network
  sources. (See prior-art §2.)
- **Network is never on a request's hot path**: bind the web port before scanning;
  connect/scan in the background with timeouts; surface progress/errors via the activity log +
  WebSocket, not blocking calls or polling.
- **The web app is built by `build.rs`** (Vite) and embedded via `rust-embed` from `web/dist/`.
  Hashed `/assets/*` bundles are served immutable; the HTML entries `no-cache`. Edit components
  in `web/src/`; run `npm run check`.
- **Don't couple fundamentally different operations.** Each long-running background job (source
  discovery/scan, fingerprint identification, MusicBrainz enrichment, artwork fetch, loudness
  analysis) is its **own task draining its own DB-backed queue at its own pace** — they
  coordinate *only* through the database, never in lockstep in one loop. A slow step (an SMB
  rescan) must never stall an unrelated one (identification). Each worker loops: do available
  work → if it did work, loop again (drain the backlog); else sleep a short idle poll. Group
  jobs into one task only when they share an external rate limit (e.g. the two MusicBrainz
  passes). See `*_loop` fns in `main.rs`. The trade-off — eventual consistency instead of
  strict ordering — is the point; don't re-entangle them for ordering.
- AGPL-3.0; check a new dependency's license before adding it.

## Coding Style & Naming

Rust 2024 edition and standard `rustfmt` output. Keep modules small, explicit, and aligned
with the architecture: provider-neutral logic in core, persistence in storage, transport
concerns in server. Use `snake_case` for functions/modules, `PascalCase` for types and traits,
and `SCREAMING_SNAKE_CASE` only for true constants. `cargo fmt` reformats multi-line edits —
re-read a region you just edited before editing it again.

Frontend (Svelte 5 + TypeScript in `web/src/`): `PascalCase.svelte` for components,
`kebab-case` for other assets; follow `docs/style-guide.md` for UI conventions.

## Testing Guidelines

Prefer unit tests beside the Rust code they cover. Scanner tests use `testdata/`, so keep that
sample library stable enough for deterministic counts and searches. Do not require private
credentials or network access in default tests. Each feature should cover expected behavior
and at least one failure or edge case. Run `cargo test` before handing off changes.

The **UI smoke suite** — `scripts/ui-smoke.sh` (→ `scripts/v2-smoke.sh` + `tests/ui/v2-flows.mjs`)
— drives the Svelte app over CDP (headless Chromium at
`~/.cache/ms-playwright/chromium-1217/chrome-linux64/chrome`) and asserts user flows *and* the
playback hot path: via a MutationObserver, a progress tick must move only the elapsed/seek
text, never the now-title. `cargo test` covers the server only and does **not** build `web/`,
so run the smoke suite after any UI change. Rust-level hot-path guards live in `players.rs`
tests.

## Commit & Pull Request Guidelines

Branch for feature work; commit/push **only when asked**. Use short, imperative commit
subjects, e.g. `Add playback queue model`; Conventional-Commit prefixes (`feat:`, `fix:`,
`docs:`) are acceptable if used consistently. End commit messages with the co-author trailer:

```
Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
```

Pull requests should include a concise summary, verification commands, linked issues when
relevant, and screenshots or recordings for UI changes. The `pre-commit` hook runs `cargo
test` + the UI smoke suite — keep them green.

## Agent-Specific Instructions

Keep generated changes scoped to the requested task. Do not collapse provider-neutral domain
logic into local-disk or browser-specific code. Preserve the Rust-first direction and keep
dependency additions tied to roadmap milestones.

### Coding Principles

**1. Think Before Coding** — Don't assume. Don't hide confusion. Surface tradeoffs. State
assumptions explicitly; if uncertain, ask. If multiple interpretations exist, present them —
don't pick silently. If a simpler approach exists, say so. If something is unclear, stop, name
what's confusing, and ask.

**2. Simplicity First** — Minimum code that solves the problem; nothing speculative. No
features beyond what was asked, no abstractions for single-use code, no unrequested
"flexibility," no error handling for impossible scenarios. If you write 200 lines and it could
be 50, rewrite it. Ask: "Would a senior engineer say this is overcomplicated?"

**3. Surgical Changes** — Touch only what you must. Don't "improve" adjacent code, comments, or
formatting; don't refactor what isn't broken; match existing style even if you'd do it
differently. If you notice unrelated dead code, mention it — don't delete it. Remove
imports/variables/functions that *your* changes made unused; don't remove pre-existing dead
code unless asked. Every changed line should trace directly to the request.

**4. Goal-Driven Execution** — Define success criteria; loop until verified. Turn tasks into
verifiable goals ("Add validation" → "Write tests for invalid inputs, then make them pass").
For multi-step tasks, state a brief plan with a verify check per step. Strong success criteria
let you loop independently.

These principles are working if: fewer unnecessary changes in diffs, fewer rewrites due to
overcomplication, and clarifying questions come before implementation rather than after
mistakes.
