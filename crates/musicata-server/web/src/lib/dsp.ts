// SPDX-License-Identifier: AGPL-3.0-or-later
// Headphone/EQ correction model + an AutoEq / REW "ParametricEQ.txt" parser.
// A profile is a preamp (dB, usually negative for headroom) plus a cascade of biquad bands.
// It compiles to Web Audio `BiquadFilterNode`s in `BrowserAudio` (see lib/audio.ts). Sourced
// either from a built-in demo preset or by importing a headphone preset from autoeq.app /
// the AutoEq project. Browser-output only; the CamillaDSP/DAC path reuses the same model.

export type EqBandType = "peaking" | "lowshelf" | "highshelf";

export interface EqBand {
  type: EqBandType;
  /** Centre / corner frequency in Hz. */
  freq: number;
  /** Gain in dB (ignored by Web Audio for shelves but kept for display + CamillaDSP). */
  gain: number;
  q: number;
}

export interface EqProfile {
  id: string;
  name: string;
  /** Preamp/headroom in dB — applied as a front gain so band boosts don't clip. */
  preampDb: number;
  bands: EqBand[];
  /** "headphones" | "speakers" — drives the output switcher; room IR only on speakers. */
  kind?: "headphones" | "speakers";
  /** Set when a room-correction WAV impulse response is stored for this profile (speakers). */
  roomIr?: { sampleRate: number };
}

// AutoEq/REW filter-type tokens → biquad kind. PK = peaking, LSC/LS = low shelf, HSC/HS = high
// shelf. Other tokens (notch/allpass) are rare for headphone correction and skipped.
const TYPE_MAP: Record<string, EqBandType> = {
  PK: "peaking",
  PEQ: "peaking",
  LS: "lowshelf",
  LSC: "lowshelf",
  LSQ: "lowshelf",
  HS: "highshelf",
  HSC: "highshelf",
  HSQ: "highshelf",
};

function slugify(name: string): string {
  const base = name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
  return `eq-${base || "preset"}-${name.length}`;
}

/**
 * Parse an AutoEq / REW "ParametricEQ.txt" body into an EqProfile. Recognises:
 *   Preamp: -6.8 dB
 *   Filter 1: ON PK Fc 21 Hz Gain 4.7 dB Q 0.7
 * Disabled (`OFF`) and unsupported filter types are skipped. Returns a profile with an empty
 * `bands` array if nothing parsed (callers treat that as a failed import).
 */
export function parseParametricEq(text: string, name = "Imported preset"): EqProfile {
  let preampDb = 0;
  const bands: EqBand[] = [];
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) continue;
    const pre = line.match(/^Preamp:\s*(-?[\d.]+)\s*dB/i);
    if (pre) {
      preampDb = Number.parseFloat(pre[1]);
      continue;
    }
    const m = line.match(
      /^Filter\s+\d+:\s*(ON|OFF)\s+(\w+)\s+Fc\s+([\d.]+)\s*Hz(?:\s+Gain\s+(-?[\d.]+)\s*dB)?(?:\s+Q\s+([\d.]+))?/i,
    );
    if (!m) continue;
    if (m[1].toUpperCase() === "OFF") continue;
    const type = TYPE_MAP[m[2].toUpperCase()];
    if (!type) continue;
    bands.push({
      type,
      freq: Number.parseFloat(m[3]),
      gain: m[4] != null ? Number.parseFloat(m[4]) : 0,
      q: m[5] != null ? Number.parseFloat(m[5]) : 0.707,
    });
  }
  return { id: slugify(name), name, preampDb, bands };
}

// Generic demo presets so the engine is immediately audible without an import. These are NOT
// model-specific corrections — real headphone correction comes from importing an AutoEq preset.
export const BUILT_IN_PROFILES: EqProfile[] = [
  {
    id: "demo-bass",
    name: "Bass boost (demo)",
    preampDb: -4,
    bands: [{ type: "lowshelf", freq: 100, gain: 6, q: 0.7 }],
  },
  {
    id: "demo-vshape",
    name: "V-shape (demo)",
    preampDb: -5,
    bands: [
      { type: "lowshelf", freq: 90, gain: 5, q: 0.7 },
      { type: "peaking", freq: 900, gain: -3, q: 1.0 },
      { type: "highshelf", freq: 6000, gain: 4, q: 0.7 },
    ],
  },
  {
    id: "demo-warm",
    name: "Warm / treble cut (demo)",
    preampDb: 0,
    bands: [{ type: "highshelf", freq: 5000, gain: -4, q: 0.7 }],
  },
];
