# UI / hot-path smoke suite

End-to-end checks that drive the **real Svelte web app** in headless Chromium and assert on
both behavior and the playback hot path — the layer the Rust tests don't reach.

## Run

```sh
scripts/ui-smoke.sh          # builds the server, scans testdata, runs the suite against /
scripts/v2-smoke.sh /v2      # same suite against a chosen base path
```

Exit code is non-zero if any check fails. If no Chromium is found the suite **skips**
(exit 0), so it never breaks environments without a browser.

## How it works

`scripts/v2-smoke.sh` builds `musicata-server`, launches headless Chrome with a CDP port, and
runs `tests/ui/v2-flows.mjs` (which drives the app over the **DevTools protocol** — no
Playwright, just Node's built-in `fetch` + `WebSocket`) in **two phases**, each with its own
server:

1. **behavior** — playback + flows against the light `testdata` fixture (scanned). Pins the
   hot path and the user flows.
2. **scale** — render/scroll/search against a read-only **copy** of the real
   `.musicata/musicata.db` (~11k tracks) served with `--no-scan`. Copying avoids touching the
   real DB; skipped if it isn't present (set `MUSICATA_REAL_DB` to point elsewhere).

Rebuild the server after changing `web/` — `build.rs` re-runs Vite and the binary embeds the
bundle, so the script always builds first.

## What it asserts

**Behavior phase.** The **hot-path assertion** is the important one: after starting playback it
installs a `MutationObserver` on the footer's elapsed-time node and the now-title node, then
watches ~4 s of `type:"progress"` ticks and asserts the elapsed text mutates while the
now-title is **never** mutated — a tick does no work beyond the seek readout. (This replaces
the old approach of counting calls to the vanilla app's global hot functions, which the Svelte
app doesn't have; in Svelte the property is structural — a tick mutates only the
`elapsed`/`duration` signals, which only the seek row reads.) Plus flows: queue
drawer, browse filter, search (shows on a segment and persists across a segment switch), smart
playlist, metadata panel, a radio station (listed + play sets now-playing), a **zone** (switch
the output to a zone holding the browser player and confirm playback advances), and no
uncaught exceptions.

**Scale phase.** Pins the incremental-loading work against the big library: the Tracks landing
must render a **window** (≤ 200 rows, not all ~11k), infinite scroll appends the next page when
the sentinel scrolls into view, and search stays a bounded page. A real viewport is set over
CDP (`Emulation.setDeviceMetricsOverride`) so the scroll-driven paging behaves as in a browser.
