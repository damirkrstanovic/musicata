//! Server-stored DSP profiles (EQ + optional room correction), so a user's correction follows
//! them across browsers/devices instead of living in one browser's `localStorage`.
//!
//! The profile library is a JSON array kept in the `dsp_profiles` app setting (reusing
//! `get_setting`/`set_setting` — no migration). The wire shape mirrors the web client's
//! hand-written `EqProfile`/`EqBand` (`web/src/lib/dsp.ts`): `{ id, name, preampDb,
//! bands:[{type,freq,gain,q}], kind?, roomIr? }`. The output binding (which sink + remembered
//! volume) stays client-side — see `web/src/lib/audioDevices.svelte.ts`.

use std::path::PathBuf;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::{AppError, AppState, db_error};

const DSP_PROFILES_KEY: &str = "dsp_profiles";

#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomIr {
    pub sample_rate: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

async fn load_profiles(state: &AppState) -> Vec<DspProfile> {
    state
        .database
        .get_setting(DSP_PROFILES_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str::<Vec<DspProfile>>(&json).ok())
        .unwrap_or_default()
}

async fn store_profiles(state: &AppState, profiles: &[DspProfile]) -> Result<(), AppError> {
    let json = serde_json::to_string(profiles).map_err(|e| AppError::internal(e.to_string()))?;
    state
        .database
        .set_setting(DSP_PROFILES_KEY, &json)
        .await
        .map_err(db_error)
}

/// The profile library, in stored order. Authenticated but not admin-only — EQ is a playback
/// preference, so any signed-in user can read it (and edit it via PUT/DELETE).
pub async fn list_profiles(State(state): State<AppState>) -> Json<Vec<DspProfile>> {
    Json(load_profiles(&state).await)
}

/// Upsert a profile by id (the id in the path wins over the body's). Returns the saved profile.
pub async fn upsert_profile(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(mut profile): Json<DspProfile>,
) -> Result<Json<DspProfile>, AppError> {
    profile.id = id;
    if profile.name.trim().is_empty() {
        return Err(AppError::bad_request("profile name is required"));
    }
    let mut profiles = load_profiles(&state).await;
    profiles.retain(|p| p.id != profile.id);
    profiles.push(profile.clone());
    store_profiles(&state, &profiles).await?;
    Ok(Json(profile))
}

pub async fn delete_profile(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let mut profiles = load_profiles(&state).await;
    let before = profiles.len();
    profiles.retain(|p| p.id != id);
    if profiles.len() != before {
        store_profiles(&state, &profiles).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- Room-correction impulse responses (Phase 4) ----
//
// The WAV bytes are too big for the JSON profile, so they live as a file under `<db-dir>/dsp/`
// and the profile just records `roomIr { sampleRate }`. The browser ConvolverNode fetches the
// WAV from the GET endpoint.

fn dsp_dir(state: &AppState) -> PathBuf {
    state
        .db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("dsp")
}

fn impulse_path(state: &AppState, id: &str) -> PathBuf {
    // `id` is a slug we generated; keep only filename-safe chars to avoid path traversal.
    let safe: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    dsp_dir(state).join(format!("{safe}.wav"))
}

/// Sample rate from a WAV header (fmt chunk, offset 24, little-endian u32), or 0 if not parseable.
fn wav_sample_rate(bytes: &[u8]) -> u32 {
    if bytes.len() >= 28 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]])
    } else {
        0
    }
}

/// Upload a WAV impulse response for a profile (raw body). Stores the file + records `roomIr`.
pub async fn upload_impulse(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, AppError> {
    let mut profiles = load_profiles(&state).await;
    let profile = profiles
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| AppError::not_found("unknown profile"))?;
    if wav_sample_rate(&body) == 0 {
        return Err(AppError::bad_request("expected a WAV impulse response"));
    }
    let dir = dsp_dir(&state);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
    tokio::fs::write(impulse_path(&state, &id), &body)
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
    profile.room_ir = Some(RoomIr {
        sample_rate: wav_sample_rate(&body),
    });
    store_profiles(&state, &profiles).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Serve a profile's stored impulse response (for the browser ConvolverNode).
pub async fn get_impulse(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let bytes = tokio::fs::read(impulse_path(&state, &id))
        .await
        .map_err(|_| AppError::not_found("no impulse response for this profile"))?;
    Ok(([(CONTENT_TYPE, "audio/wav")], bytes).into_response())
}

/// Remove a profile's impulse response.
pub async fn delete_impulse(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let _ = tokio::fs::remove_file(impulse_path(&state, &id)).await;
    let mut profiles = load_profiles(&state).await;
    if let Some(profile) = profiles.iter_mut().find(|p| p.id == id) {
        profile.room_ir = None;
        store_profiles(&state, &profiles).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_json_matches_the_web_client_shape() {
        // The exact shape web/src/lib/dsp.ts emits (camelCase preampDb, band `type`).
        let json = r#"{"id":"hd600","name":"HD 600","preampDb":-6.8,
            "bands":[{"type":"peaking","freq":21,"gain":4.7,"q":0.7}],"kind":"headphones"}"#;
        let profile: DspProfile = serde_json::from_str(json).expect("parse");
        assert_eq!(profile.id, "hd600");
        assert_eq!(profile.preamp_db, -6.8);
        assert_eq!(profile.bands.len(), 1);
        assert_eq!(profile.bands[0].band_type, "peaking");
        assert_eq!(profile.kind.as_deref(), Some("headphones"));
        // Round-trips back to the same camelCase keys the client expects.
        let out = serde_json::to_string(&profile).unwrap();
        assert!(out.contains("\"preampDb\":-6.8"));
        assert!(out.contains("\"type\":\"peaking\""));
        assert!(!out.contains("roomIr")); // None is skipped
    }

    #[test]
    fn wav_sample_rate_reads_the_fmt_chunk() {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&[0; 4]); // chunk size (ignored)
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes()); // fmt size
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // channels
        wav.extend_from_slice(&48_000u32.to_le_bytes()); // sample rate @ offset 24
        assert_eq!(wav_sample_rate(&wav), 48_000);
        assert_eq!(wav_sample_rate(b"not a wav"), 0);
    }
}
