// SPDX-License-Identifier: AGPL-3.0-or-later
pub use musicata_core::pcm_dsp::StereoEq;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{DspBand, DspProfile};

    fn band(t: &str, freq: f64, gain: f64, q: f64) -> DspBand {
        DspBand {
            band_type: t.into(),
            freq,
            gain,
            q,
        }
    }

    #[test]
    fn no_op_profile_builds_nothing() {
        let p = DspProfile {
            id: "x".into(),
            name: "x".into(),
            preamp_db: 0.0,
            bands: vec![],
            kind: None,
            room_ir: None,
        };
        assert!(StereoEq::from_profile(&p, 48_000).unwrap().is_none());
    }

    #[test]
    fn low_shelf_dc_gain_matches_db() {
        // A low shelf boosts DC (and all low frequencies) by its gain. Feed a constant signal
        // and confirm the steady-state output ≈ input * 10^(gain/20).
        let p = DspProfile {
            id: "ls".into(),
            name: "ls".into(),
            preamp_db: 0.0,
            bands: vec![band("lowshelf", 200.0, 6.0, 0.707)],
            kind: None,
            room_ir: None,
        };
        let mut eq = StereoEq::from_profile(&p, 48_000).unwrap().expect("eq");
        let mut out = 0.0;
        for _ in 0..4000 {
            out = eq.process_frame(1000.0, 1000.0).0; // settle
        }
        let expected = 1000.0 * 10f64.powf(6.0 / 20.0);
        assert!(
            (out - expected).abs() < 1.0,
            "got {out}, expected ~{expected}"
        );
    }

    #[test]
    fn preamp_scales_and_resets_clears_state() {
        let p = DspProfile {
            id: "pre".into(),
            name: "pre".into(),
            preamp_db: -6.0,
            bands: vec![],
            kind: None,
            room_ir: None,
        };
        // preamp only (no bands) is not a no-op.
        let mut eq = StereoEq::from_profile(&p, 48_000).unwrap().expect("eq");
        let (l, _) = eq.process_frame(1000.0, 1000.0);
        assert!((l - 1000.0 * 10f64.powf(-6.0 / 20.0)).abs() < 0.5);
        eq.reset(); // no panic; state cleared
    }
}
