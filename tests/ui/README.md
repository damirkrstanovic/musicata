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

`scripts/v2-smoke.sh` builds `musicata-server`, starts it on a freshly scanned `testdata`
library, launches headless Chrome with a CDP port, and runs `tests/ui/v2-flows.mjs`, which
drives the app over the **DevTools protocol** (no Playwright — Node's built-in `fetch` +
`WebSocket`). Rebuild the server after changing `web/` — `build.rs` re-runs Vite and the
binary embeds the bundle, so `scripts/ui-smoke.sh` always builds first.

## What it asserts

The **hot-path assertion** is the important one. After starting playback it installs a
`MutationObserver` on the footer's elapsed-time node and the now-title node, then watches ~4 s
of `type:"progress"` ticks. It asserts the elapsed text mutates (the position advances) while
the now-title is **never** mutated — a progress tick does no work beyond the seek readout.
This replaces the old approach of counting calls to the vanilla app's global hot functions
(which the Svelte app doesn't have); in Svelte the property is structural — a tick mutates
only the `elapsed`/`duration` signals, which only the seek row reads.

Plus flow checks: playback starts, queue drawer lists tracks, browse filter narrows the grid,
search renders result sections, a smart playlist opens and lists tracks, the metadata panel
opens, and there are no uncaught exceptions.

Coverage is intentionally narrower than the pre-migration suite for now; broadening it (zone
flows, radio playback, a scale-phase `--no-scan` pass against the real DB) is a follow-up.
