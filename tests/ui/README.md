# UI / lag smoke suite

End-to-end checks that drive the **real web app** in headless Chromium and assert on
both behavior and responsiveness — the layer the Rust tests don't reach. This is where
"typical user flows" and "is it laggy" are verified.

## Run

```sh
scripts/ui-smoke.sh          # builds the server, then runs the suite
# or, against an already-built binary:
node tests/ui/run.mjs
UI_VERBOSE=1 node tests/ui/run.mjs   # print every check, not just failures
```

Exit code is non-zero if any check fails. If no Chromium is found the suite **skips**
(exit 0) with a notice, so it never breaks environments without a browser.

## How it works

`run.mjs` launches headless Chromium and drives it over the **DevTools protocol** (CDP)
— no Playwright or other npm dependency, just Node's built-in `fetch` + `WebSocket`. It
injects the instrumentation in `instrument.mjs` (which wraps the hot client functions to
count how often they run and logs audio-element events, footer mutations, console
errors, and long tasks), then runs the flows in `flows.mjs`. It runs in **two phases**,
each with its own server + page:

1. **behavior** — playback/controller correctness + responsiveness, against the light
   `testdata/` fixture (reliable, fast). Pins the regressions.
2. **scale** — render/scroll/load at realistic scale, against a read-only **copy** of
   the real `~/.../.musicata/musicata.db` (~11k tracks) served with `--no-scan`. Copying
   avoids polluting the real DB; `--no-scan` keeps the copy's data intact (no
   offline-source wipe). Skipped if that DB isn't present (falls back to nothing).

Splitting the phases is deliberate: playback needs only a few streamable tracks, and
driving real `<audio>` streaming on a heavy 11k-row page hits headless HTTP/1.1
connection limits — so playback runs on the light page, and the big DB is used only for
the render/scroll/load measurements (no playback) where scale actually matters. Action
triggers (`playTrack`, clicks, commands) are *fired* without awaiting their fetch —
the test observes the resulting WebSocket state — so an in-flight request never stalls
the run. A per-eval timeout and an overall watchdog guarantee the suite can't hang.

Because the server embeds `static/app.js` via `include_str!` at build time, **rebuild
the server after changing any static asset** or the suite tests the stale embedded
copy (`scripts/ui-smoke.sh` always builds first).

## What it asserts

**Behavior phase (testdata):**

| Flow | Behavior | Lag / waste |
|------|----------|-------------|
| play first / later track | correct track, single broadcast, no audio restart | footer updates ≤ 1.5 s |
| steady playback (3 s) | — | no full-state frames, no track-list re-highlight, no queue/metadata rebuild on position ticks |
| pause / resume | audio pauses then resumes | no `<audio>` src reset |
| next | track advances, single broadcast | — |
| seek | elapsed jumps to target | — |
| queue reorder | order changes, re-renders on change | no audio restart |
| internet radio | station browsed via the provider + plays | no console errors |

**Scale phase (real ~11k-track library):**

| Flow | Checks |
|------|--------|
| initial load at scale | realistic size, full list rendered, app interactive < 20 s, no console errors |
| library scroll | no scroll long task > 400 ms |

Budgets live in `run.mjs` (`CFG.budgets`). Most behavior checks are **count-based**
(deterministic) rather than wall-clock, so they pin the exact regressions — e.g. a
position tick must not trigger a full-state broadcast or a DOM sweep, and playing a
non-first track must produce exactly one broadcast with no `src` reset. The scale
checks are wall-clock outlier detectors (generous budgets, tuned to a dev machine).

## Diagnostics / env

`UI_VERBOSE=1` prints every check (not just failures) and per-flow progress; `UI_EVLOG=1`
logs every CDP eval; `UI_HEARTBEAT=/path` appends a synchronous progress trace.
Other knobs: `MUSICATA_BIN`, `MUSICATA_LIBRARY`, `MUSICATA_REAL_DB`, `CHROMIUM`,
`UI_PORT`, `UI_DEBUG_PORT`.
