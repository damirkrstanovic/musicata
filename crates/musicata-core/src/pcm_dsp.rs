// SPDX-License-Identifier: AGPL-3.0-or-later
//! Synchronous PEQ and stereo metering. No audio-device or runtime dependencies.

use std::f64::consts::PI;

use crate::dsp::{DspError, DspProfile, StereoLevels};

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
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
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

fn shelf_alpha(sin: f64, _a: f64, _q: f64) -> f64 {
    // Web Audio shelves use a fixed slope of 1 and ignore Q.
    sin / 2.0 * 2f64.sqrt()
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
    pub fn from_profile(profile: &DspProfile, sample_rate: u32) -> Result<Option<Self>, DspError> {
        profile.validate()?;
        if sample_rate == 0
            || profile
                .bands
                .iter()
                .any(|b| b.freq >= sample_rate as f64 / 2.0)
        {
            return Err("filter frequency must be below the output Nyquist frequency");
        }
        if profile.room_ir.is_some() {
            return Err("room convolution is not supported by this output");
        }
        if profile.bands.is_empty() && profile.preamp_db.abs() < f64::EPSILON {
            return Ok(None);
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
        Ok(Some(Self {
            preamp: 10f64.powf(profile.preamp_db / 20.0),
            left: build(),
            right: build(),
        }))
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

#[derive(Default)]
pub struct StereoMeter {
    squares: [f64; 2],
    peaks: [f64; 2],
    frames: u64,
}

impl StereoMeter {
    pub fn push(&mut self, left: f64, right: f64) {
        if !left.is_finite() || !right.is_finite() {
            return;
        }
        for (i, value) in [left, right].into_iter().enumerate() {
            self.squares[i] += value * value;
            self.peaks[i] = self.peaks[i].max(value.abs());
        }
        self.frames += 1;
    }

    pub fn take(&mut self) -> Option<StereoLevels> {
        if self.frames == 0 {
            return None;
        }
        let levels = StereoLevels {
            rms_l: (self.squares[0] / self.frames as f64).sqrt(),
            rms_r: (self.squares[1] / self.frames as f64).sqrt(),
            peak_l: self.peaks[0],
            peak_r: self.peaks[1],
        };
        self.reset();
        Some(levels)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Single audio producer, any number of readers. Snapshots never block the audio thread.
#[derive(Default)]
pub struct AudioTap {
    sequence: std::sync::atomic::AtomicU64,
    values: [std::sync::atomic::AtomicU64; 4],
    active: std::sync::atomic::AtomicBool,
    pub applied_revision: std::sync::atomic::AtomicU64,
}

impl AudioTap {
    pub fn publish(&self, levels: StereoLevels) {
        use std::sync::atomic::Ordering;
        self.sequence.fetch_add(1, Ordering::AcqRel);
        for (slot, value) in
            self.values
                .iter()
                .zip([levels.rms_l, levels.rms_r, levels.peak_l, levels.peak_r])
        {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        self.active.store(true, Ordering::Relaxed);
        self.sequence.fetch_add(1, Ordering::Release);
    }

    pub fn clear(&self) {
        use std::sync::atomic::Ordering;
        self.sequence.fetch_add(1, Ordering::AcqRel);
        self.active.store(false, Ordering::Relaxed);
        self.sequence.fetch_add(1, Ordering::Release);
    }

    pub fn read(&self) -> Option<(u64, StereoLevels)> {
        use std::sync::atomic::Ordering;
        let sequence = self.sequence.load(Ordering::Acquire);
        if sequence & 1 != 0 || !self.active.load(Ordering::Relaxed) {
            return None;
        }
        let values = self
            .values
            .each_ref()
            .map(|slot| f64::from_bits(slot.load(Ordering::Relaxed)));
        std::sync::atomic::fence(Ordering::Acquire);
        if sequence != self.sequence.load(Ordering::Relaxed) {
            return None;
        }
        Some((
            sequence,
            StereoLevels {
                rms_l: values[0],
                rms_r: values[1],
                peak_l: values[2],
                peak_r: values[3],
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{DspBand, DspProfile, RoomIr};

    fn profile(bands: Vec<DspBand>, preamp_db: f64) -> DspProfile {
        DspProfile {
            id: "test".into(),
            name: "Test".into(),
            preamp_db,
            bands,
            kind: None,
            room_ir: None,
        }
    }

    #[test]
    fn stereo_meter_measures_independent_channels_and_resets() {
        let mut meter = StereoMeter::default();
        assert!(meter.take().is_none());
        for _ in 0..4800 {
            meter.push(0.5, -0.25);
        }
        let levels = meter.take().unwrap();
        assert!((levels.rms_l - 0.5).abs() < 1e-9);
        assert!((levels.rms_r - 0.25).abs() < 1e-9);
        assert_eq!(levels.peak_l, 0.5);
        assert_eq!(levels.peak_r, 0.25);
        assert!(meter.take().is_none());
        meter.push(0.0, 0.0);
        assert_eq!(meter.take().unwrap().rms_l, 0.0);
    }

    #[test]
    fn preamp_and_peaking_response_match_known_signal() {
        let band = DspBand {
            band_type: "peaking".into(),
            freq: 1000.0,
            gain: 6.0,
            q: 1.0,
        };
        let mut eq = StereoEq::from_profile(&profile(vec![band], -6.0), 48_000)
            .unwrap()
            .unwrap();
        let mut meter = StereoMeter::default();
        for i in 0..9600 {
            let x = 0.1 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48000.0).sin();
            let (l, r) = eq.process_frame(x, 0.0);
            if i > 4800 {
                meter.push(l, r);
            }
        }
        let levels = meter.take().unwrap();
        assert!((levels.rms_l - 0.1 / 2f64.sqrt()).abs() < 0.0001);
        assert_eq!(levels.rms_r, 0.0);
        eq.reset();
        assert_eq!(eq.process_frame(0.0, 0.0), (0.0, 0.0));
    }

    #[test]
    fn rejects_nyquist_and_unsupported_convolution() {
        let band = DspBand {
            band_type: "peaking".into(),
            freq: 24_000.0,
            gain: 6.0,
            q: 1.0,
        };
        assert!(StereoEq::from_profile(&profile(vec![band], 0.0), 48_000).is_err());
        assert!(StereoEq::from_profile(&profile(vec![], 0.0), 0).is_err());
        let mut p = profile(vec![], 0.0);
        p.room_ir = Some(RoomIr {
            sample_rate: 48_000,
        });
        assert!(StereoEq::from_profile(&p, 48_000).is_err());
    }

    #[test]
    fn audio_tap_keeps_only_latest_complete_measurement() {
        let tap = AudioTap::default();
        assert!(tap.read().is_none());
        let levels = StereoLevels {
            rms_l: 0.5,
            rms_r: 0.25,
            peak_l: 0.8,
            peak_r: 0.4,
        };
        tap.publish(levels);
        let (first, actual) = tap.read().unwrap();
        assert_eq!(actual, levels);
        tap.publish(StereoLevels::default());
        let (next, _) = tap.read().unwrap();
        assert!(next > first);
        tap.clear();
        assert!(tap.read().is_none());
    }
}
