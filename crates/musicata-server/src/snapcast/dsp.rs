//! In-process EQ for the server-side Snapcast PCM stream.
//!
//! The same correction model as the browser Web Audio EQ — a preamp + a cascade of RBJ-cookbook
//! biquads built from a [`DspProfile`] — but applied **server-side**, in the decode→FIFO writer,
//! so Snapcast multi-room gets correction too **without a CamillaDSP subprocess or ALSA
//! loopback** (Musicata already owns the PCM here; we just filter it in place). The biquad math
//! is the standard Audio EQ Cookbook (the same EqualizerAPO/AutoEq presets target), computed
//! ourselves rather than vendoring CamillaDSP's GPL filter modules. FIR room convolution is a
//! future addition (the browser tier already does it).

use std::f64::consts::PI;

use crate::dsp::DspProfile;

/// A direct-form-I biquad with per-channel state (f64).
#[derive(Clone, Default)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    /// Build from already-normalised (a0 = 1) coefficients.
    fn new(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            ..Default::default()
        }
    }

    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

// --- RBJ Audio EQ Cookbook coefficient builders ---

fn peaking(fs: f64, f0: f64, q: f64, gain_db: f64) -> Biquad {
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * f0 / fs;
    let (sin, cos) = (w0.sin(), w0.cos());
    let alpha = sin / (2.0 * q);
    Biquad::new(
        1.0 + alpha * a,
        -2.0 * cos,
        1.0 - alpha * a,
        1.0 + alpha / a,
        -2.0 * cos,
        1.0 - alpha / a,
    )
}

fn shelf_alpha(sin: f64, a: f64, q: f64) -> f64 {
    sin / 2.0 * ((a + 1.0 / a) * (1.0 / q - 1.0) + 2.0).max(0.0).sqrt()
}

fn low_shelf(fs: f64, f0: f64, q: f64, gain_db: f64) -> Biquad {
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * f0 / fs;
    let (sin, cos) = (w0.sin(), w0.cos());
    let alpha = shelf_alpha(sin, a, q);
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
    Biquad::new(
        a * ((a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha),
        2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
        a * ((a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha),
        (a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha,
        -2.0 * ((a - 1.0) + (a + 1.0) * cos),
        (a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha,
    )
}

fn high_shelf(fs: f64, f0: f64, q: f64, gain_db: f64) -> Biquad {
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * f0 / fs;
    let (sin, cos) = (w0.sin(), w0.cos());
    let alpha = shelf_alpha(sin, a, q);
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
    Biquad::new(
        a * ((a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha),
        -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
        a * ((a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha),
        (a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha,
        2.0 * ((a - 1.0) - (a + 1.0) * cos),
        (a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha,
    )
}

/// A preamp + a per-channel biquad cascade, applied to interleaved stereo i16 in place.
#[derive(Clone)]
pub struct StereoEq {
    preamp: f64,
    left: Vec<Biquad>,
    right: Vec<Biquad>,
}

impl StereoEq {
    /// Build from a profile at `sample_rate`. Returns `None` for a no-op profile (no bands and a
    /// 0 dB preamp) so the writer keeps its fast copy path.
    pub fn from_profile(profile: &DspProfile, sample_rate: u32) -> Option<Self> {
        if profile.bands.is_empty() && profile.preamp_db.abs() < f64::EPSILON {
            return None;
        }
        let fs = sample_rate as f64;
        let build = || -> Vec<Biquad> {
            profile
                .bands
                .iter()
                .filter_map(|b| match b.band_type.as_str() {
                    "peaking" => Some(peaking(fs, b.freq, b.q.max(0.01), b.gain)),
                    "lowshelf" => Some(low_shelf(fs, b.freq, b.q.max(0.01), b.gain)),
                    "highshelf" => Some(high_shelf(fs, b.freq, b.q.max(0.01), b.gain)),
                    _ => None, // unsupported (notch/etc.) — skipped, matching the browser parser
                })
                .collect()
        };
        Some(Self {
            preamp: 10f64.powf(profile.preamp_db / 20.0),
            left: build(),
            right: build(),
        })
    }

    /// Filter one interleaved stereo chunk (L,R,L,R…) in place. Returns the (preamp + EQ) output
    /// as f64 via the closure so the caller can fold in volume/leveling before requantising —
    /// avoids double-clamping. (Used directly: see `process_frame`.)
    pub fn process_frame(&mut self, left: f64, right: f64) -> (f64, f64) {
        let mut l = left * self.preamp;
        for bq in &mut self.left {
            l = bq.process(l);
        }
        let mut r = right * self.preamp;
        for bq in &mut self.right {
            r = bq.process(r);
        }
        (l, r)
    }

    /// Clear filter state (call on track load/seek so the previous track doesn't bleed in).
    pub fn reset(&mut self) {
        for bq in self.left.iter_mut().chain(self.right.iter_mut()) {
            bq.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{DspBand, DspProfile};

    fn band(t: &str, freq: f64, gain: f64, q: f64) -> DspBand {
        DspBand { band_type: t.into(), freq, gain, q }
    }

    #[test]
    fn no_op_profile_builds_nothing() {
        let p = DspProfile {
            id: "x".into(), name: "x".into(), preamp_db: 0.0, bands: vec![], kind: None, room_ir: None,
        };
        assert!(StereoEq::from_profile(&p, 48_000).is_none());
    }

    #[test]
    fn low_shelf_dc_gain_matches_db() {
        // A low shelf boosts DC (and all low frequencies) by its gain. Feed a constant signal
        // and confirm the steady-state output ≈ input * 10^(gain/20).
        let p = DspProfile {
            id: "ls".into(), name: "ls".into(), preamp_db: 0.0,
            bands: vec![band("lowshelf", 200.0, 6.0, 0.707)], kind: None, room_ir: None,
        };
        let mut eq = StereoEq::from_profile(&p, 48_000).expect("eq");
        let mut out = 0.0;
        for _ in 0..4000 {
            out = eq.process_frame(1000.0, 1000.0).0; // settle
        }
        let expected = 1000.0 * 10f64.powf(6.0 / 20.0);
        assert!((out - expected).abs() < 1.0, "got {out}, expected ~{expected}");
    }

    #[test]
    fn preamp_scales_and_resets_clears_state() {
        let p = DspProfile {
            id: "pre".into(), name: "pre".into(), preamp_db: -6.0, bands: vec![], kind: None, room_ir: None,
        };
        // preamp only (no bands) is not a no-op.
        let mut eq = StereoEq::from_profile(&p, 48_000).expect("eq");
        let (l, _) = eq.process_frame(1000.0, 1000.0);
        assert!((l - 1000.0 * 10f64.powf(-6.0 / 20.0)).abs() < 0.5);
        eq.reset(); // no panic; state cleared
    }
}
