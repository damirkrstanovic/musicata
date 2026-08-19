# Gapless playback for the native endpoint

**Date:** 2026-06-30
**Status:** Shipped (merged to `main`). The endpoint holds one persistent rodio `Sink` and
prefetches the next track into it — see `crates/musicata-endpoint/src/audio.rs`. Kept as the
design record for why it works this way, not as open work.
**Scope:** `musicata-core`, `musicata-server`, `musicata-endpoint`, docs

## Goal

The native endpoint (`crates/musicata-endpoint`, rodio → ALSA) plays the server-owned
queue but **reloads a fresh `Sink` per track**, producing an audible gap at every track
boundary. Make playback **gapless** — *no audible gap* between consecutive library tracks
(not bit-perfect codec-padding removal).

## Decisions (locked)

- **Gapless = "no audible gap."** We do not strip encoder/decoder padding; we just keep the
  output stream flowing across the boundary.
- **Add a tiny additive `next_up` hint to the broadcast state** so the endpoint can prefetch
  the correct next track under shuffle/repeat without re-deriving the server's play order.
- **Audio mechanism: Approach 1** — one persistent `Sink`, decode-ahead buffer, append the
  next source in the final window of the current track. rodio plays appended sources
  back-to-back natively → gapless.

## Section 1 — Server `next_up` hint

`crates/musicata-core/src/lib.rs` — add to `PlaybackState`:

```rust
#[serde(default)]
pub next_up: Option<QueueItem>,
```

Optional and defaulted, so the web app, the MPD bridge, and the current endpoint all keep
working unchanged (backward-compatible, ts-rs regenerated).

`crates/musicata-server/src/players.rs` — a pure, **non-mutating** peek mirroring the
"decide next index" block of `advance()` (lines ~1901–1919):

```rust
fn peek_next_index(state: &QueueState) -> Option<usize>
```

Rules:
- **repeat-one** → current index (the next playback is the same track).
- **shuffle, mid-order** → `shuffle_order[cursor + 1]`.
- **shuffle, last in order + repeat-all** → `None`. `advance()` *reshuffles* on wrap, so any
  prediction here is wrong ~half the time; we decline and let the endpoint reload once per
  full shuffle cycle.
- **linear** → `index + 1`; under repeat-all the wrap is `0`; else `None`.

`snapshot()` (line ~832) populates:
```rust
next_up: peek_next_index(&state).and_then(|i| state.queue.get(i).cloned()),
```

## Section 2 — Endpoint protocol (`protocol.rs`)

- `PlaybackState` gains `next_up: Option<QueueItem>` (`#[serde(default)]`).
- New actions:
  - `Action::Prefetch { stream_url, track_id }` — fetch+decode the next library track into a
    held buffer (no append yet).
  - `Action::Advance` — the server cursor moved to the track we already prefetched; promote it
    without a reload.
- `EndpointView` gains `prefetched: Option<String>` — the track id currently buffered/appended.
- `decide()` reworked: when the new `now_playing` equals `view.prefetched`, return
  `Action::Advance` (no fetch). Prefetch is emitted **only** for library tracks
  (`track_id.is_some()` and a relative `stream_url`) — never radio/external, preserving the
  BUG-04 token rule (`resolve_request`).

## Section 3 — Endpoint audio (`audio.rs`) — Approach 1

- **One persistent `Sink`** for the player's lifetime (replaces per-track `Sink::try_new`).
- `prefetch(stream_url)`: fetch + decode into `Option<Decoder>`; do not append.
- Control loop appends the buffered decoder when the current track is within the **last ~12 s**
  *and* `next_up` is stable *and* it's a library track. rodio plays it back-to-back → gapless.
- **Boundary detection:** with two sources queued the sink is not empty at the boundary, so we
  track the source count (`sink.len()`); a decrement means track N finished — reset the
  elapsed clock, report `{"ended"}` to advance the server cursor while audio keeps flowing.
- **Invalidation:** a queue edit/skip *before* the append window drops the buffer cleanly.
  *After* append we can't un-append → corrective reload = one small gap (rare, documented).

## Section 4 — Edge cases

- **Shuffle-cycle end:** `next_up` is `None`; one reload per cycle.
- **Repeat-one:** `next_up` = current item → still gapless (re-appends the same source).
- **Radio/external:** never prefetched; token never leaves the library host.
- **Queue edit during append window:** corrective reload.
- **Pause/seek during append window:** buffer retained; append logic re-evaluates on resume.

## Section 5 — Testing

- `protocol.rs` unit tests: Prefetch only for library `next_up`; `Advance` (not `Load`) on
  arriving at the prefetched id; no prefetch for radio `next_up`; parse with `next_up` present.
- Server unit tests: `peek_next_index` — linear, repeat-all wrap, repeat-one, shuffle mid,
  shuffle-end → `None`.
- Manual: real USB-DAC listening test for the absence of an audible gap.

## Section 6 — Docs

- `docs/requirements.md` (Playback And Players): add the gapless requirement.
- `docs/native-endpoint.md`: flip "No gapless / crossfade" → gapless-via-prefetch + limits.
- `docs/roadmap.md` M10: note the gapless follow-up.
