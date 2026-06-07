# Loudness leveling — consistent playback levels (research + plan)

Date: 2026-06-07

## Context

Make every track play at the same *perceived* loudness, so:
- **continuous play** (the killer reason) — an endless auto-queue pulls *unrelated* tracks
  from different masters/eras; without leveling, a −7 LUFS modern master slams in after a
  −16 LUFS jazz cut and you lunge for the volume. Leveling makes the endless stream smooth.
- **multiroom** — a synchronized zone (one server-decoded stream, see `docs/snapcast.md`)
  stays even track-to-track. This is *orthogonal* to per-room **volume** (how loud each room
  is) — leveling normalizes the *content*; room volume trims the *level*.

This is a **DSP stage** — it slots into `docs/dsp.md` (which already lists "Volume Leveling"
as Phase 6 and builds the front-of-chain `preampGain` this must coordinate with). Roon's
"Volume Leveling" is the reference.

## Roon's model (the reference, confirmed from its KB)

- **EBU R128 analysis**, target **−14 LUFS** (the streaming convention, not the −23 broadcast
  target).
- **Constant-gain multiply, NOT compression** — verbatim: "the audio signal is multiplied by a
  constant gain value." Dynamics fully preserved.
- **True-peak clip avoidance by *attenuation*, not limiting** — when a *positive* gain would
  clip, Roon uses the analyzed true-peak to *reduce* that gain. (Rare — most music sits above
  −14 and gets turned *down*.)
- **Modes: Off / Track / Album / Auto.** Auto = **track** gain across tracks from *different*
  albums, **album** gain within the *same* album — best of both.
- **Fixed offset for unknown loudness** (default ~−5 dB) for un-analyzed content / radio, so
  crossing into it doesn't jump.

## The standards (what to measure)

- **Integrated LUFS** (ITU-R BS.1770 K-weighting, gated at −70 LUFS absolute and −10 LU
  relative) — the whole-track loudness.
- **True-peak (dBTP)** — peak of the **4× oversampled** waveform (catches inter-sample peaks a
  raw sample-peak misses). Needed for clip avoidance.
- **LRA** (loudness range) — dynamic-range metadata; nice to store, not needed for the gain.
- **ReplayGain 2.0** is now BS.1770-based too, but references **−18 LUFS** (so a stored
  `REPLAYGAIN_*_GAIN` needs **+4 dB** to hit −14); **Opus `R128_*` tags** reference **−23**
  (+9 dB). RG tags carry only *sample*-peak, not true-peak — so tags are a **bootstrap**, not
  a substitute for our own scan.

## The apply algorithm

```
gain_dB = target_LUFS − track_integrated_LUFS          # constant gain
predicted_peak_dBTP = track_true_peak_dBTP + gain_dB
if predicted_peak_dBTP > −1.0:                          # streaming ceiling
    gain_dB -= (predicted_peak_dBTP − (−1.0))           # ATTENUATE positive gain; never limit
```

- **Track mode:** per-track gain (best for shuffle / radio / **continuous play** — unrelated
  consecutive tracks each hit the same level).
- **Album mode:** one album-wide gain (the album's integrated LUFS), attenuated by the album's
  **worst** track true-peak — preserves intended intra-album dynamics (best for front-to-back
  / classical / concept albums).
- **Auto:** track gain across album boundaries, album gain within one (Roon's model).
- **Unknown loudness:** apply the fixed offset (~−5 dB) until analyzed.

**Target: −14 LUFS default, user-configurable** (DB-backed setting, not a flag): offer −14
(streaming consensus, matches Roon) / −16 (Apple-style, more headroom) / −18 (RG-native).

## ⚠️ The one thing most likely to ship broken: double-clipping with the EQ preamp

The DSP graph is `source → preampGain → [biquads] → … → destination`. The EQ **preamp** is a
headroom attenuation (often negative); the **leveling** gain is per-track (often negative, but
*can be positive* for quiet tracks). They are independent GainNodes, but for **clipping they
SUM**. A leveling boost that's "safe" alone and a preamp that's "safe" alone can together push
true-peak over 0 dBFS.

**So the clip check must run on the COMBINED gain, not each separately:**
```
total_dB = leveling_gain_dB + eq_preamp_dB
# attenuate `total_dB` against the −1 dBTP ceiling using the track's true-peak
```
Implement as one combined gain, or two nodes whose product is validated together. (The
agent's "they're independent, no interaction" framing is wrong for the clipping case — this is
the load-bearing detail.)

## Measurement — the `ebur128` crate

[`ebur128`](https://crates.io/crates/ebur128) — **pure-Rust**, EBU-conformant, **MIT** (clean
under AGPL-3.0). `EbuR128::new(channels, rate, Mode::I | Mode::LRA | Mode::TRUE_PEAK)`, feed
interleaved PCM with `add_frames_f32(&[f32])`, then read `loudness_global()` (LUFS),
`true_peak(ch)` (linear → `20·log10` → dBTP), `loudness_range()` (LRA). **Cost = one full
decode per track** (the dominant cost) → analyze once at **scan time**, cache in the DB, never
on a playback hot path. Reuses Musicata's existing **symphonia** decode
(`fingerprint.rs:decode_samples`).

## How it fits Musicata (anchors)

- **Scan-time analysis** — a **new `loudness_loop`** background worker draining its own
  DB-backed queue, exactly like `fingerprint_loop` (`main.rs`), gated by a setting. Decode
  with symphonia (own `loudness.rs`, mirroring `fingerprint.rs`) → ebur128 → store. (Could
  share fingerprinting's decode later, but keep it a separate worker per the decoupling
  convention; revisit only if double-decode cost matters.)
- **Storage — migration v26** (current max v25): add `tracks.integrated_loudness_lufs` +
  `tracks.true_peak_dbtp` (+ `lra` optional). Album LUFS aggregated from member tracks
  (on-the-fly `AVG`, or a small cache table). Mirror the `duration_seconds` precedent
  (`MIGRATION_012`).
- **Carry to the player** — add the two fields to `QueueItem` (musicata-core) +
  ts-rs regen, filled in `resolve_queue_items` (`players.rs`). They ride to the browser in
  `PlaybackState`.
- **Apply (browser, Tier 1):** one **leveling `GainNode`** in `web/src/lib/audio.ts` (after
  the EQ chain, before destination), set per track from the now-playing item's LUFS via the
  combined-gain clip check above. Changing it is a `gain.value` set (not a graph rebuild), so
  the ui-smoke hot-path guard stays green.
- **Apply (server, Tier 2/3):** the **same combined gain once in the Snapcast decode→FIFO
  loop** (`docs/snapcast.md`) — multiply PCM before the FIFO, coordinated with the CamillaDSP
  preamp if that tier is active. One gain for all synchronized rooms.
- **Tags as bootstrap:** read `REPLAYGAIN_*` / `R128_*` via lofty (not done today) and
  populate provisionally with reference correction (+4 / +9 dB) before a full scan completes;
  prefer the own scan once available (only it gives true-peak).
- **Settings:** `loudness_mode` (off/track/album/auto) + `loudness_target_lufs` in
  `AppSettings`, edited in `/admin`.

## Build order

1. **Storage + `loudness_loop` + ebur128** — measure & cache LUFS + true-peak (+ album
   aggregate). Nothing audible yet; the data exists. (Migration v26, new worker.)
2. **Browser Track-mode leveling** — the leveling GainNode with the **combined-gain clip
   check**, the −14 setting, Off/Track. *This is the killer feature for continuous play* — ship
   it as soon as data exists.
3. **Album mode + Auto.**
4. **Tag bootstrap** (RG/R128 with reference correction) — fills in before scans complete.
5. **Server-side apply in the Snapcast decode loop** — when that tier lands; identical math.

## Verification

Unit tests: ebur128 over a `testdata/` track (known LUFS within tolerance); the gain +
combined-clip-check math (a loud master attenuates, a quiet one boosts but never exceeds
−1 dBTP *after* adding the EQ preamp). ui-smoke: enable Track leveling, confirm the leveling
GainNode is set per track and the now-title/hot-path is undisturbed. Manual: a continuous-play
session sounds level track-to-track.

## Sources

[Roon Volume Leveling KB](https://help.roonlabs.com/portal/en/kb/articles/volume-leveling) ·
[Roon FAQ](https://help.roonlabs.com/portal/en/kb/articles/faq-what-s-volume-leveling) ·
[ITU-R BS.1770-5] · [EBU R128] · [ReplayGain 2.0 spec](https://wiki.hydrogenaudio.org/index.php?title=ReplayGain_2.0_specification) ·
[`ebur128` crate](https://crates.io/crates/ebur128) · loudgain / rsgain. Musicata anchors:
`fingerprint.rs` (symphonia decode), `main.rs` (`*_loop` pattern), `storage` (migrations,
track columns), `musicata-core` (Track/QueueItem), `web/src/lib/audio.ts` (the EQ graph +
preamp this coordinates with). Related: `docs/dsp.md`, `docs/continuous-play.md`,
`docs/snapcast.md`.
