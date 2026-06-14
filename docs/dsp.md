# DSP — EQ, room & headphone correction (research + plan)

Date: 2026-06-07

## Context

Milestone 11 reserves a **per-zone DSP pipeline** and names CamillaDSP as the engine.
This doc is the research behind that milestone and the concrete plan for shipping it. The
goal is product-level: let an ordinary user **improve how their music sounds** —
headphone correction with zero effort, room correction if they're willing to measure —
without becoming an audio operator. It distils a prior-art pass over Roon, Dirac, the
open-source correction stack (REW / DRC-FIR / AutoEq), the phone-app landscape
(JamesDSP / Wavelet), and a first-hand read of the CamillaDSP source (`../camilladsp`).

**The headline decisions:**

- **Three tiers, browser-first.** Most users get correction in the **browser player via the
  Web Audio API** (works everywhere incl. iOS, no server processing). The hi-fi/DAC path
  gets **CamillaDSP as a managed subprocess**. Phone apps are an *export* target, not
  something we drive.
- **We apply filters; we don't measure.** Like Roon: ingest industry-standard filters
  (WAV impulse responses for rooms, ParametricEQ/GraphicEQ text for headphones); leave
  measurement to REW/DRC + a calibrated mic.
- **One shared DSP profile** (PEQ bands + optional room IR), stored in the DB, compiles to
  *either* Web Audio nodes *or* CamillaDSP YAML — so a user's correction follows them
  across endpoints. Fits "configuration lives in the product."
- **AutoEq headphone correction is the first, highest-value feature**: pick your headphone
  model → instant correction, no microphone, no measurement.

---

## Prior art

### Roon — the architecture lesson

- **DSP runs on the server (Core), per-zone, in 64-bit float**, then ships *finished*
  uncompressed PCM to dumb endpoints over RAAT. The renderer never processes. This is the
  one load-bearing lesson: **do the math centrally, hand endpoints finished samples.**
- Stages: Headroom Management (fixed first; a single attenuation dB + clip detector) →
  Sample-Rate Conversion → Parametric EQ → Procedural EQ (matrix/channel mixing,
  crossovers) → Crossfeed → Convolution → Speaker Setup (delay/level/phase). Plus **Volume
  Leveling** (EBU R128, target −14 LUFS, a *constant-gain multiply* — not compression —
  true-peak-limited).
- **Roon never measures your room.** It only *applies* filters generated elsewhere, imported
  as **WAV impulse responses** (also `.flac/.aiff/ALAC/.pcm/.dbl`, 16–64-bit), with a
  "zip-of-IRs, one per sample rate" convention; mono IR → all channels, multichannel IR →
  mapped per channel. Convolution forces Headroom Management on (it can raise gain).
- **Signal-path display** (purple = lossless … blue = enhanced … yellow = lossy) grades the
  worst link in the chain — a *trust* feature. Cheap to copy, disproportionate payoff.

### Dirac Live — why it's a dead end for us

- Commercial, tiered (~$99–$799). Uses **mixed-phase FIR+IIR** correction (fixes magnitude
  *and* time/impulse response — genuinely better than pure EQ), measured with a **calibrated
  USB mic** (not a phone), spatially averaged over many positions.
- **The filter is locked to Dirac's own processor** (AVR firmware or the Dirac Live
  Processor plugin/virtual-soundcard). It is **not exportable** to a generic convolution
  engine. We can neither ingest nor apply Dirac filters — don't try. (Acourate/Audiolense
  are also commercial but at least export plain WAV IRs a user could feed us.)

### Open-source / free correction stack (build on these)

| Tool | License | Role | Output | Mic? |
|---|---|---|---|---|
| **REW** (Room EQ Wizard) | free, **closed** | measure + design | WAV IR, biquad/PEQ text, CamillaDSP/APO exports | yes (calibrated) |
| **DRC-FIR** (Sbragion) | **GPLv2, open** | measure → FIR | WAV impulse response (mixed/linear phase) | yes (calibrated) |
| **rePhase** | free, closed | FIR designer | FIR IR | no (designer) |
| **AutoEq** | **MIT** | headphone DB → filter | ParametricEQ.txt, GraphicEQ.txt, convolution WAV | **no — none** |
| **CamillaDSP** | GPL-3.0 / MPL-2.0 | the engine (Rust) | — | no (applies) |
| **JamesDSP / RootlessJamesDSP** | **GPLv2** | Android apply | loads IR WAV + graphic EQ | no |
| **Wavelet** | (Android) | headphone apply | built-in AutoEq profiles | no |

- **DRC-FIR is the only fully-open end-to-end room-correction generator** — the closest
  free analog to Dirac's time+frequency FIR correction.
- **REW is free but not open source** — fine to *use its outputs*, not to bundle.

### Phone apps — apply, don't measure (and don't over-rely)

- Phone/laptop/webcam mics are uncalibrated, AGC-ridden, voice-tuned → fine for *spotting*
  a big bass mode, useless for trustworthy filters. **Measure on desktop.**
- **RootlessJamesDSP** (Android, GPLv2) applies an IR + EQ system-wide via audio-capture
  (Shizuku, no root) — **but it can't capture Chrome/Spotify**, so it won't catch
  Musicata's *browser* player. **Wavelet** ships the whole AutoEq DB for headphones.
- **iOS has no system-wide DSP** (sandboxing) — correction only per-app or via a USB DAC.
  This is the clinching argument for **browser Web Audio** as the cross-platform backstop:
  it's the only software path that works on iOS at all.

---

## Measurement — how to cheaply measure a room

- **Built-in laptop / webcam mics (incl. Logitech Brio): no.** Active noise-cancelling
  *fights* a measurement sweep (it suppresses sustained tones), plus AGC, unknown
  voice-tuned response, and no calibration. Usable only to learn REW / see a gross bass
  peak — never to generate a filter.
- **The cheap, trustworthy entry: miniDSP UMIK-1 (~$80) + REW (free).** USB
  plug-and-play (no audio interface, no phantom power) and — the key part — ships with an
  **individual calibration file** per serial that cancels the mic's own coloration. The
  laptop just runs REW; the UMIK is the sensor.
- **Workflow:** load the UMIK cal file into REW → play REW's sweep through the system, mic
  at the listening position (average a few nearby points) → set a target curve → export a
  **WAV impulse response** (room) or **ParametricEQ.txt** (bands). DRC-FIR (GPLv2) is the
  fully-open alternative generator.
- **Scope:** measurement-based correction is only valid in the **bass/low-mids** (below the
  room's transition frequency, ~300 Hz). Above that, use light EQ or — for headphones —
  AutoEq, which needs no mic at all.

---

## AutoEq — zero-effort headphone correction (the first feature)

Every headphone colours the sound; labs measure each model's deviation from neutral.
[AutoEq](https://github.com/jaakkopasanen/AutoEq) (MIT) aggregates those measurements for
**thousands of models** and precomputes the EQ that corrects each toward a preferred target
(Harman). **The user picks their headphone model and gets a ready-made preset — no mic, no
measurement, no room**, because headphones of one model measure consistently (unlike rooms,
which are all unique). Outputs are exactly our interchange formats: `ParametricEQ.txt`,
`GraphicEQ.txt`, and convolution WAV. Caveats to surface in UI: it's a *preference* target
(let users nudge bass), treble >10 kHz is approximate, in-ears vary with fit. This is the
single biggest "sounds better for free" win and the natural Tier-1 launch feature.

---

## CamillaDSP — first-hand source notes (`../camilladsp`, v4.x)

### Embeddable *and* a binary

`Cargo.toml` exposes both a library (`[lib] name = "camillalib"`) and the `camilladsp`
binary. So there are three integration shapes:

- **A. Subprocess + WebSocket** (run the binary, control over JSON/WS) — lowest effort,
  cleanest licensing; the planned path for the DAC tier.
- **B. Vendor the DSP core** — the **filter math is cleanly reusable**
  (`src/filters/*.rs`, `mixer.rs`, ~7.7k LOC; deps `realfft`, `num-complex`,
  `parking_lot`). But the *engine* (`processing.rs`) is welded to crossbeam channels + a
  real-time thread barrier — it is **not** a tidy `process(chunk)->chunk`. "Embed" thus
  means lifting filter modules, not the pipeline.
- **C. Reimplement a subset** — the biquad cookbook math is small and spelled out in
  `src/filters/biquad.rs:88-410`; FIR convolution (`src/filters/fftconv.rs`, FFT
  overlap-add via `realfft`) is the only real work. Tier 1 doesn't need Rust at all (Web
  Audio does it).

### License — fine for AGPL either way

`GPL-3.0-only OR MPL-2.0` (ASIO build would force GPL-only; we never build ASIO).
**Subprocess** = separate program, no entanglement. **Embed/vendor** = combined work → take
the **GPL-3.0** arm, which is cross-compatible with Musicata's AGPL-3.0. Prefer "require it
installed / optional dep," cargo-feature-gated like the SMB source.

### Control protocol (the bits that matter)

- **`PatchConfig`** (RFC-merge JSON) is the live-EQ primitive: send only
  `{"filters":{"bass":{"parameters":{"gain":6.0}}}}` → that filter recomputes **without
  interrupting audio**. Preferred over re-sending full `SetConfig`/`SetConfigJson`.
- **Live-apply caveat:** filter/mixer/pipeline edits apply live; **changing
  device/samplerate/channels forces a restart** (audio drops). Our generated config must
  keep `devices` stable and only patch filters.
- **Headless lifecycle:** start with `-w` (wait for config over WS) + `-s <statefile>`
  (persists volume + config path across restarts). `SIGHUP` = reload. Musicata owns this.
- **Monitoring for a signal-path/health UI:** `GetState`, `GetProcessingLoad`,
  `GetClippedSamples`, `GetCaptureSignalRms/Peak`, `GetBufferLevel`.
- Built-in `translate_rew_xml.py` converts REW exports → CamillaDSP biquad YAML.

### Audio routing

- **Production path = ALSA loopback** (`snd-aloop`): player → `hw:Loopback,1,0`; CamillaDSP
  captures `hw:Loopback,0,0` → real DAC. Robust; survives player start/stop; the
  `alsa_cdsp` plugin handles dynamic sample-rate switching (else "first opener fixes the
  rate"). macOS uses BlackHole equivalently.
- **stdin/pipe capture** works but is fragile (fixed rate at launch, no backpressure,
  finicky EOF) — only viable under tight co-managed lifecycle. **Loopback wins** for MPD.

---

## The shared DSP profile (canonical model)

Two objects, split by *where they live*, because correction is **per physical output**, not
per library or per player.

**`DspProfile` — server-stored (the correction content; syncs across devices).** A named
correction the user can pick or edit. Lives in the DB (extend `AppSettings`, or a
`dsp_profiles` table) and is editable in `/admin`.

```
DspProfile {
  id, name,                         // "HD600 (AutoEq)", "Desk speakers"
  kind: Headphones | Speakers,
  preamp_db: f32,                   // headroom / AutoEq preamp (front of chain)
  bands: Vec<Band>,                 // parametric EQ — from AutoEq or user
  room_ir: Option<ConvSpec>,        // per-channel WAV impulse response (Slice 2)
}
Band { kind: Peaking|LowShelf|HighShelf|LowPass|HighPass|Notch, freq_hz, q, gain_db }
ConvSpec { left_wav, right_wav, sample_rate }
```

**`OutputPreset` — client-stored (the hardware binding; per browser, in `localStorage`).**
Binds a physical sink + a profile + a remembered volume. Lives client-side because output
**device IDs are browser/machine-scoped** and the binding ("*this* machine's headphone jack")
isn't portable.

```
OutputPreset {
  label,                            // "Speakers" | "Headphones"
  sink_device_id: Option<String>,   // AudioContext.setSinkId target (enhancement)
  dsp_profile_id: Option<String>,   // which server-side DspProfile
  volume: number,                   // remembered PER output (safety: see plan)
}
// plus: activeOutput index, persisted in localStorage
```

- **Headphone correction** = a profile whose `bands` come from an AutoEq model (or
  `GraphicEQ.txt`); **room correction** = a profile with a measured `room_ir`.
- A profile compiles to **Web Audio** (`BiquadFilterNode` cascade + `ConvolverNode`) *or*
  **CamillaDSP YAML** (`Biquad` + `Conv` + front `Gain`).
- **Home-office example (the canonical case):** two presets in the one `browser-local`
  player — *"Desk speakers"* → USB DAC sink + a room-IR profile + vol 40%; *"Headphones"* →
  headphone-amp sink + an AutoEq profile + vol 15%. A footer toggle swaps profile + sink +
  volume in one tap. They are the **same Musicata player**, two OS sinks — not two players.

### Filter capability matrix (the spec, from CamillaDSP)

Internal precision is **f64**. We target a subset first (bold = Tier 1):

| Filter | Subtypes / params | Tier |
|---|---|---|
| **Biquad** | **Peaking, Low/Highshelf, Low/Highpass, Notch** (freq/Q/gain); + FO variants, Allpass, Bandpass, LinkwitzTransform, Free(raw coeffs) | **1** |
| **Gain** | gain dB/linear, invert, mute — used for **headroom/preamp** | **1** |
| **Conv (FIR)** | IR from **Wav**/Raw/Values/Dummy; FFT overlap-add (`realfft`) — room correction | 2 |
| BiquadCombo | Butterworth / Linkwitz-Riley crossovers, Tilt, FivePointPeq, **GraphicEqualizer** (`gains: Vec<f32>` ← AutoEq GraphicEQ.txt) | 2 |
| Volume | ramped, 4 aux faders, limit | 2 |
| Loudness | psychoacoustic low/high boost vs reference level | 3 |
| Delay | ms/us/mm/samples, subsample allpass — speaker time-align | 3 |
| Dither | 21 noise-shapers — final requantize | 3 |
| Limiter / Compressor / NoiseGate | dynamics | 3 |
| RACE | built-in speaker crosstalk cancellation | 3 |
| Mixer | per-output source list (gain/invert/mute) — up/downmix, crossovers, multichannel | 3 |

### Filter interchange formats

- **`ParametricEQ.txt`** (REW/AutoEq "APO" format: `Preamp` + `Filter N: ON PK Fc <hz>
  Gain <db> Q <q>`) — canonical for **headphone/PEQ**; compiles 1:1 to Web Audio biquads
  *and* CamillaDSP biquads.
- **`GraphicEQ.txt`** (`GraphicEQ: <hz> <db>; …`) — export for JamesDSP/Wavelet on phones;
  maps to CamillaDSP `GraphicEqualizer`.
- **WAV impulse response** — canonical for **room correction**; consumed by Web Audio
  `ConvolverNode`, CamillaDSP `Conv`, JamesDSP, EasyEffects, BruteFIR.

---

## Architecture — three tiers

| Tier | Path | Reach | Effort |
|---|---|---|---|
| **1. Browser Web Audio** | `MediaElementSource → BiquadFilterNode cascade → ConvolverNode → destination` in the web player | **every OS incl. iOS Safari**; only the Musicata tab; stereo conv only | low |
| **2. Server-side CamillaDSP** | managed subprocess on the MPD→DAC machine; loopback in, DAC out; live `PatchConfig` over WS | the DAC-attached host (Linux/Mac/Win); multichannel | medium |
| **3. Phone-app export** | emit ParametricEQ.txt / GraphicEQ.txt / IR WAV for JamesDSP / Wavelet | Android; documented, not driven | trivial |

Web Audio specifics to get right: tap the existing `<audio>` once with
`createMediaElementSource` (only one source node per element); `ConvolverNode.normalize =
false` **before** assigning `.buffer` (correction IRs must not be loudness-normalised);
convolution is mono/stereo/4-ch only; same-origin audio avoids the CORS-silence trap.

---

## Implementation plan — per-output DSP, browser-first

Grounded in the current code: one `<audio>` in `web/src/player/App.svelte` driven by the
**`BrowserAudio`** class in `web/src/lib/audio.ts` (the single Web Audio insertion point);
per-player volume already in `PlaybackState.volume`; the proven
`AppSettings`→ts-rs→`/api/settings`→`admin/SettingsPanel.svelte` settings pattern; **no**
`setSinkId`/device enumeration yet. The home-office two-output case is the driving example.

### Phase 0 — Server: profile model + storage + API

- **`musicata-core`**: add the `DspProfile` / `Band` / `ConvSpec` types
  (`crates/musicata-core/src/lib.rs`), `#[derive(ts_rs::TS)]`.
  > **As built:** the types live in `crates/musicata-server/src/dsp.rs` (not
  > `musicata-core`) and are named `DspProfile` / `DspBand` / `RoomIr` (not `Band` /
  > `ConvSpec`).
- **Storage**: persist the profile library. Start simplest — a `dsp_profiles` JSON value via
  the existing `get_setting`/`set_setting` (`crates/musicata-storage/src/lib.rs`); promote to
  a dedicated table only if it grows. No migration needed for the JSON-in-settings route.
- **Server API** (`crates/musicata-server/src/main.rs`): `GET/PUT /api/dsp/profiles`
  (list + upsert/delete), mirroring the `get_settings`/`update_settings` handlers. Run
  `scripts/gen-web-types.sh`; add client methods to `web/src/lib/api.ts`.
- **`/admin` panel**: `admin/DSPProfilesPanel.svelte` (create/edit/delete a profile: name,
  kind, preamp, a small band editor + a **paste-a-`ParametricEQ.txt`** import box), wired into
  `admin/App.svelte`. Mirrors `SettingsPanel.svelte`.
  > **As built:** the DSP/EQ UI is the player-side `web/src/player/EqPanel.svelte` (an AutoEq
  > model picker + an inline ParametricEQ paste box), not an `/admin` panel.

### Phase 1 — Browser DSP core (prove it: one global profile, audible, hot-path-safe)

- In **`web/src/lib/audio.ts`**, build the Web Audio graph inside `BrowserAudio`:
  `createMediaElementSource(el)` (once) → `preampGain` → `[BiquadFilterNode …bands]` →
  `masterGain` → `audioContext.destination`. Lazily create + `resume()` the `AudioContext`
  on the existing user-gesture play path (`primePlay`).
- `applyProfile(profile)` rebuilds the biquad chain; `setBypass(bool)` swaps to a
  source→destination passthrough for A/B. Keep the element's `.volume` as master (pre-tap).
- **Hot-path guard:** the graph touches no DOM and rebuilding bands must **not** reload the
  `<audio>` element — the ui-smoke MutationObserver (a progress tick must never disturb the
  now-title) must still pass.
- Wire one active profile end-to-end first to confirm audible change + green smoke before
  adding the switcher.

### Phase 2 — Output presets + speakers/headphones switcher (the home-office feature)

- New client store **`web/src/lib/audioDevices.svelte.ts`**: the `OutputPreset[]` +
  `activeOutput`, persisted to `localStorage`; `enumerateDevices()` to list sinks; a
  `devicechange` listener.
- **Footer toggle** (`web/src/player/Footer.svelte`, next to the volume slider): a
  🔊 Speakers / 🎧 Headphones segmented control. One tap → `setActiveOutput(i)`, which
  (a) `audio.applyProfile(profile)`, (b) `audio.setSink(sinkId)`, (c) restores that preset's
  **volume** (reuse the existing `set_volume` command path). Remembered across sessions.
- **Per-output volume is a safety feature**, not a nicety: headphones are far more sensitive
  than active speakers; never carry speaker volume into headphones. Each preset owns its level.
- **Routing caveat (document in UI):** once audio is routed through Web Audio to
  `destination`, the output device is chosen via **`AudioContext.setSinkId`** (the element's
  own `setSinkId` no longer applies). Support: Chromium yes; Safari/Firefox limited. Fallback
  when unsupported → DSP still applies, device follows the **OS default**, and the toggle
  still swaps profile + volume (which covers the common "one shared output, unplug-to-switch"
  setup). Optional convenience: auto-select a preset on `devicechange`, off by default.

### Phase 3 — AutoEq headphone profiles (zero-effort correction)

- Bundle a **curated set** of AutoEq `ParametricEQ.txt` presets (MIT) as an embedded asset +
  a searchable model picker (`GET /api/dsp/headphones?q=`), which creates a `DspProfile`.
  Keep the Phase-0 paste-`ParametricEQ.txt` box for any model not bundled. (Shipping the full
  ~thousands-of-models DB is a size decision — defer; curated + import covers the MVP.)
- A small `ParametricEQ.txt` parser (`Preamp` + `Filter N: ON PK Fc … Gain … Q …`) → bands;
  unit-tested. This is the shared importer for both the picker and the paste box.
  > **As built:** there is no `GET /api/dsp/headphones?q=` endpoint and no Rust parser. The
  > AutoEq presets ship as a **bundled client asset** `web/src/lib/autoeq-presets.json`, and the
  > `ParametricEQ.txt` parser is **TypeScript** in `web/src/lib/dsp.ts` — the picker and paste
  > box both run client-side against that asset.

### Phase 4 — Room correction (convolution) in the browser

`ConvolverNode` loading a user-uploaded **WAV impulse response** into the same graph for a
speakers profile; `normalize = false` **before** assigning `.buffer`; upload/select UI.
Stereo only — multichannel needs the CamillaDSP tier.

### Phase 5 — server-side DSP (revised: in-process, NOT a subprocess by default) — DONE for Snapcast

**Re-verified against CamillaDSP 4.1.3 (`../camilladsp`): the DSP is a clean library, so a
subprocess is *not* needed for audio Musicata itself produces.** The engine is built on
`pub trait Filter { fn process_waveform(&mut self, &mut [PrcFmt]); }` (`src/filters/mod.rs:39`)
with public, engine-free constructors (`BiquadCoefficients::new`, `Biquad::new`/`from_config`,
`FftConv::new`). Neither `camilladsp` nor `camillalib` is published on crates.io, and a git-dep
drags the whole crate (ALSA/CoreAudio/WASAPI backends + websocket, edition 2024) — so **vendor the
math, don't depend on the crate**.

**What's built (in-process, for Snapcast):** `crate::snapcast::dsp` — an RBJ-cookbook biquad
cascade + preamp (`StereoEq`, our own ~150 lines, the same EqualizerAPO/AutoEq target; no GPL
vendoring needed for biquads) applied **in the decode→FIFO writer** before the FIFO
(`writer.rs`, `WriterMsg::SetDsp`), built from the **same server `DspProfile`** as the browser
tier. A `snapcast.dsp_profile_id` setting (an `/admin` Multi-room selector) chooses the profile;
it's pushed to the writer live (`PlayerManager::set_snapcast_dsp`). **No subprocess, no ALSA
loopback** — Musicata already owns the PCM here. (Server-side **FIR room convolution** for
Snapcast is the natural next step: vendor `fftconv.rs` (realfft overlap-add) and add a `Conv`
stage in the writer; the browser tier already does room conv.)

**The subprocess stays a niche, deferred option** only for correcting audio Musicata does *not*
produce — the **MPD → external-DAC** path, where CamillaDSP must intercept the OS audio path
(snd-aloop). There a managed subprocess (Option A: `-w` + statefile, live `PatchConfig` over WS,
the same `DspProfile` → CamillaDSP YAML) is the only option; cargo-feature-gated + "require
installed." Not the primary plan.

### Phase 6 — polish

**Volume Leveling** (EBU R128, constant-gain Track/Album) — full design in
**`docs/loudness.md`**. The killer feature for continuous play + even multiroom; analyze LUFS
+ true-peak once at scan time (`ebur128`), apply a per-track gain at playback. **Critical
coordination:** the leveling gain and this EQ chain's **preamp** sum for clipping — run the
−1 dBTP clip check on the *combined* `leveling_dB + preamp_dB`, or they double-clip. Plus a
Roon-style **signal-path badge** over the WebSocket, and phone-app filter **export**
(GraphicEQ.txt / IR WAV).

### Files touched (Phases 0–2, the home-office MVP)

- **New:** `web/src/lib/audioDevices.svelte.ts`, `web/src/admin/DSPProfilesPanel.svelte`.
- **Modify:** `crates/musicata-core/src/lib.rs` (+`DspProfile`), `crates/musicata-storage/src/lib.rs`
  (profile get/set), `crates/musicata-server/src/main.rs` (`/api/dsp/*`),
  `web/src/lib/audio.ts` (Web Audio graph + `applyProfile`/`setSink`/`setBypass`),
  `web/src/player/Footer.svelte` (toggle + per-output volume), `web/src/player/App.svelte`
  (wire the store), `web/src/lib/api.ts`, `web/src/admin/App.svelte`. Run `scripts/gen-web-types.sh`.

### Verification

- **Rust:** `ParametricEQ.txt` parse + profile round-trip in storage tests; `cargo test` + `cargo build`.
- **Frontend:** `npm run check`; extend `scripts/ui-smoke.sh` — "apply headphone profile →
  biquad graph present", "toggle 🔊/🎧 swaps profile + volume", "bypass restores", and the
  existing **hot-path assertion still green** (progress tick never disturbs the now-title).
- **Manual:** on the dev box, define Speakers + Headphones presets, confirm a one-tap swap
  changes tonal balance + volume, and that switching never reloads/restarts the current track.

---

## What we deliberately do NOT do

- **No measurement suite** — no sweep player, no RTA, no mic capture. Users measure with
  REW/DRC; we apply. (Roon's boundary.)
- **No Dirac ingestion** — its filters are locked to its processor; impossible and pointless
  to try.
- **No vendoring the CamillaDSP engine** initially — subprocess for the DAC tier; lift only
  the biquad math if we ever need in-process server DSP for the browser stream.

---

## Sources

Roon MUSE / DSP Engine / Convolution / Volume Leveling / Signal Path KB
(help.roonlabs.com), RAAT/Roon Ready KB; Roon Labs blog on DIY room correction.
Dirac Live (helpdesk.dirac.com, minidsp.com Dirac-vs-REW, manuals.denon.com).
REW (roomeqwizard.com), DRC-FIR (sourceforge.net/projects/drc-fir), rePhase,
AutoEq (github.com/jaakkopasanen/AutoEq, MIT), miniDSP UMIK-1.
CamillaDSP source `../camilladsp` (Cargo.toml, src/filters/*, src/config/mod.rs,
src/socketserver.rs, backend_alsa.md, websocket.md, translate_rew_xml.py),
JamesDSP/RootlessJamesDSP (GPLv2), Wavelet, EasyEffects; MDN Web Audio
(BiquadFilterNode, ConvolverNode).
