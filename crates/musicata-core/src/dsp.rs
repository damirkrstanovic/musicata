// SPDX-License-Identifier: AGPL-3.0-or-later
//! Correction profiles and output audio contracts shared by controllers and renderers.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "camelCase")]
pub struct DspBand {
    /// "peaking" | "lowshelf" | "highshelf".
    #[serde(rename = "type")]
    pub band_type: String,
    pub freq: f64,
    pub gain: f64,
    pub q: f64,
}

/// `sampleRate` of a stored room impulse response (the WAV bytes live in a file served by
/// `/api/dsp/profiles/{id}/impulse`; see Phase 4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "camelCase")]
pub struct RoomIr {
    pub sample_rate: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "camelCase")]
pub struct DspProfile {
    pub id: String,
    pub name: String,
    /// Preamp / headroom in dB (applied as a front gain so band boosts don't clip).
    pub preamp_db: f64,
    #[serde(default)]
    pub bands: Vec<DspBand>,
    /// "headphones" | "speakers" — drives the output switcher + which profiles can carry a room IR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_ir: Option<RoomIr>,
}

pub type DspError = &'static str;

impl DspProfile {
    pub fn validate(&self) -> Result<(), DspError> {
        if self.id.trim().is_empty() || self.name.trim().is_empty() {
            return Err("profile id and name are required");
        }
        if !self.preamp_db.is_finite() || self.preamp_db.abs() > 60.0 || self.bands.len() > 128 {
            return Err("invalid preamp or too many filters");
        }
        for band in &self.bands {
            if !matches!(
                band.band_type.as_str(),
                "peaking" | "lowshelf" | "highshelf"
            ) {
                return Err("unsupported filter type");
            }
            if !band.freq.is_finite()
                || !(1.0..=384_000.0).contains(&band.freq)
                || !band.q.is_finite()
                || !(0.01..=100.0).contains(&band.q)
                || !band.gain.is_finite()
                || band.gain.abs() > 60.0
            {
                return Err("invalid filter frequency, Q or gain");
            }
        }
        if self.kind.as_deref() == Some("listening") && self.room_ir.is_some() {
            return Err("listening adjustments cannot include room convolution");
        }
        Ok(())
    }
}

/// Compose the active correction and listening layers for every renderer.
pub fn compose_profiles(
    correction: Option<&DspProfile>,
    listening: Option<&DspProfile>,
) -> Result<Option<DspProfile>, DspError> {
    if let Some(profile) = correction {
        profile.validate()?;
    }
    if let Some(profile) = listening {
        profile.validate()?;
        if profile.room_ir.is_some() {
            return Err("listening adjustments cannot include room convolution");
        }
    }
    let Some(mut effective) = correction.cloned().or_else(|| listening.cloned()) else {
        return Ok(None);
    };
    if correction.is_none() {
        effective.room_ir = None;
    }
    if let (Some(_), Some(listening)) = (correction, listening) {
        effective.preamp_db += listening.preamp_db;
        effective.bands.extend(listening.bands.clone());
    }
    effective.validate()?;
    Ok(Some(effective))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct OutputDspSelection {
    pub profile_id: Option<String>,
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub correction_enabled: bool,
    #[serde(default)]
    pub listening_profile_id: Option<String>,
    #[serde(default)]
    pub listening_enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for OutputDspSelection {
    fn default() -> Self {
        Self {
            profile_id: None,
            enabled: false,
            correction_enabled: true,
            listening_profile_id: None,
            listening_enabled: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct StereoLevels {
    pub rms_l: f64,
    pub rms_r: f64,
    pub peak_l: f64,
    pub peak_r: f64,
}

impl StereoLevels {
    pub fn is_valid(&self) -> bool {
        [self.rms_l, self.rms_r, self.peak_l, self.peak_r]
            .into_iter()
            .all(|v| v.is_finite() && (0.0..=16.0).contains(&v))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum DspStatus {
    Bypassed,
    Pending,
    Applied,
    Unavailable,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum MeasurementPoint {
    BrowserOutput,
    NativeOutput,
    SnapcastStream,
    CamilladspPlayback,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AudioCapabilities {
    pub peq: bool,
    pub room_ir: bool,
    pub meter: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct OutputDspState {
    pub output_id: String,
    pub configured: bool,
    pub session_id: String,
    pub selection: OutputDspSelection,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub desired_revision: u64,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub applied_revision: Option<u64>,
    pub status: DspStatus,
    pub error: Option<String>,
    pub capabilities: AudioCapabilities,
    pub measurement_point: MeasurementPoint,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> DspProfile {
        DspProfile {
            id: "test".into(),
            name: "Test".into(),
            preamp_db: -6.0,
            bands: vec![DspBand {
                band_type: "peaking".into(),
                freq: 1000.0,
                gain: 6.0,
                q: 1.0,
            }],
            kind: None,
            room_ir: None,
        }
    }

    #[test]
    fn rejects_invalid_and_unsupported_profile_values() {
        let mut p = profile();
        assert!(p.validate().is_ok());
        p.preamp_db = f64::NAN;
        assert!(p.validate().is_err());
        p.preamp_db = -6.0;
        p.bands[0].q = 0.0;
        assert!(p.validate().is_err());
        p.bands[0].q = 1.0;
        p.bands[0].freq = -1.0;
        assert!(p.validate().is_err());
        p.bands[0].freq = 1000.0;
        p.bands[0].band_type = "unsupported".into();
        assert!(p.validate().is_err());
    }

    #[test]
    fn rejects_extreme_gains_and_limits_filter_work() {
        let mut p = profile();
        p.preamp_db = 1000.0;
        assert!(p.validate().is_err());
        p.preamp_db = 0.0;
        p.bands[0].gain = f64::INFINITY;
        assert!(p.validate().is_err());
        p.bands[0].gain = 6.0;
        p.bands = vec![p.bands[0].clone(); 129];
        assert!(p.validate().is_err());
    }

    #[test]
    fn composes_correction_and_listening_with_correction_impulse_identity() {
        // A regression here would send the room IR request to the listening profile, or drop
        // either layer's headroom/filter contribution before a renderer sees it.
        let correction = DspProfile {
            id: "room-speakers".into(),
            name: "Room correction".into(),
            preamp_db: -4.0,
            bands: vec![DspBand {
                band_type: "lowshelf".into(),
                freq: 90.0,
                gain: -3.0,
                q: 0.7,
            }],
            kind: Some("speakers".into()),
            room_ir: Some(RoomIr {
                sample_rate: 48_000,
            }),
        };
        let listening = DspProfile {
            id: "late-night".into(),
            name: "Late night".into(),
            preamp_db: -2.0,
            bands: vec![DspBand {
                band_type: "peaking".into(),
                freq: 2_500.0,
                gain: 1.5,
                q: 1.2,
            }],
            kind: Some("headphones".into()),
            room_ir: None,
        };

        let effective = compose_profiles(Some(&correction), Some(&listening))
            .expect("valid layers compose")
            .expect("at least one enabled layer produces a renderer profile");

        assert_eq!(effective.id, "room-speakers");
        assert_eq!(effective.preamp_db, -6.0);
        assert_eq!(
            effective.room_ir,
            Some(RoomIr {
                sample_rate: 48_000
            })
        );
        assert_eq!(effective.bands.len(), 2);
        assert_eq!(effective.bands[0].freq, 90.0);
        assert_eq!(effective.bands[1].freq, 2_500.0);
    }

    #[test]
    fn composition_rejects_two_layers_that_exceed_the_shared_filter_limit() {
        // Each profile is individually valid; accepting the pair would let renderers receive
        // more than the documented 128-filter maximum.
        let mut correction = profile();
        correction.bands = vec![correction.bands[0].clone(); 64];
        let mut listening = profile();
        listening.id = "listening".into();
        listening.bands = vec![listening.bands[0].clone(); 65];

        assert!(correction.validate().is_ok());
        assert!(listening.validate().is_ok());
        assert!(matches!(
            compose_profiles(Some(&correction), Some(&listening)),
            Err("invalid preamp or too many filters")
        ));
    }

    #[test]
    fn default_selection_enables_correction_and_disables_listening() {
        let selection = OutputDspSelection::default();

        assert!(selection.correction_enabled);
        assert_eq!(selection.listening_profile_id, None);
        assert!(!selection.listening_enabled);
    }
}
