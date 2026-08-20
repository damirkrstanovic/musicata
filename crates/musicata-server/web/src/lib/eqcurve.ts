// SPDX-License-Identifier: AGPL-3.0-or-later
// Analytic magnitude response of an EqProfile's biquad cascade, for the EQ frequency-curve
// visualization. Uses the RBJ cookbook formulas (the same ones Web Audio's BiquadFilterNode
// is based on), so the drawn curve matches what's applied. Pure math — no AudioContext needed,
// so the curve renders even before playback starts.
import type { EqBand, EqProfile } from "./dsp";

type Coeffs = [number, number, number, number, number]; // b0, b1, b2, a1, a2 (a0 normalized to 1)

function coeffs(band: EqBand, fs: number): Coeffs {
  const w0 = (2 * Math.PI * band.freq) / fs;
  const cw = Math.cos(w0);
  const sw = Math.sin(w0);
  const q = band.q || 0.707;
  const alpha = sw / (2 * q);
  const A = 10 ** (band.gain / 40);
  let b0: number, b1: number, b2: number, a0: number, a1: number, a2: number;
  if (band.type === "peaking") {
    b0 = 1 + alpha * A;
    b1 = -2 * cw;
    b2 = 1 - alpha * A;
    a0 = 1 + alpha / A;
    a1 = -2 * cw;
    a2 = 1 - alpha / A;
  } else if (band.type === "lowshelf") {
    const s = 2 * Math.sqrt(A) * alpha;
    b0 = A * (A + 1 - (A - 1) * cw + s);
    b1 = 2 * A * (A - 1 - (A + 1) * cw);
    b2 = A * (A + 1 - (A - 1) * cw - s);
    a0 = A + 1 + (A - 1) * cw + s;
    a1 = -2 * (A - 1 + (A + 1) * cw);
    a2 = A + 1 + (A - 1) * cw - s;
  } else {
    // highshelf
    const s = 2 * Math.sqrt(A) * alpha;
    b0 = A * (A + 1 + (A - 1) * cw + s);
    b1 = -2 * A * (A - 1 + (A + 1) * cw);
    b2 = A * (A + 1 + (A - 1) * cw - s);
    a0 = A + 1 - (A - 1) * cw + s;
    a1 = 2 * (A - 1 - (A + 1) * cw);
    a2 = A + 1 - (A - 1) * cw - s;
  }
  return [b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0];
}

function magDb(c: Coeffs, w: number): number {
  const [b0, b1, b2, a1, a2] = c;
  const cw = Math.cos(w);
  const sw = Math.sin(w);
  const c2 = Math.cos(2 * w);
  const s2 = Math.sin(2 * w);
  const numRe = b0 + b1 * cw + b2 * c2;
  const numIm = -(b1 * sw + b2 * s2);
  const denRe = 1 + a1 * cw + a2 * c2;
  const denIm = -(a1 * sw + a2 * s2);
  return 20 * Math.log10(Math.hypot(numRe, numIm) / Math.hypot(denRe, denIm));
}

/** Combined magnitude response in dB at each frequency (preamp excluded — it's a flat offset). */
export function responseCurveDb(profile: EqProfile, freqs: number[], fs = 48000): number[] {
  const cs = profile.bands.map((b) => coeffs(b, fs));
  return freqs.map((f) => {
    const w = (2 * Math.PI * f) / fs;
    let db = 0;
    for (const c of cs) db += magDb(c, w);
    return db;
  });
}

/** Log-spaced frequencies from 20 Hz to 20 kHz (n points) for plotting. */
export function logFreqs(n = 96): number[] {
  return Array.from({ length: n }, (_, i) => 20 * 1000 ** (i / (n - 1)));
}
