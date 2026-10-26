// SPDX-License-Identifier: AGPL-3.0-or-later
//! Narrow, host-scoped management protocol for remote audio-device selection.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
};
use musicata_storage::{ManagedPlayerDesired, ManagedPlayerState};
use serde::{Deserialize, Serialize};

use crate::{AppError, AppState, auth, db_error};

const PAIRING_TTL_SECONDS: i64 = 10 * 60;
const STALE_AFTER_SECONDS: i64 = 30;
const MAX_CLIENT_ID_BYTES: usize = 128;
const MAX_TOKEN_BYTES: usize = 256;
const MAX_INVENTORY_REVISION_BYTES: usize = 256;
const MAX_DEVICE_ID_BYTES: usize = 256;
const MAX_DEVICE_NAME_BYTES: usize = 256;
const MAX_ERROR_BYTES: usize = 1024;
const MAX_DEVICES: usize = 64;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn valid_client_id(client_id: &str) -> bool {
    !client_id.is_empty()
        && client_id.len() <= MAX_CLIENT_ID_BYTES
        && client_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn valid_text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty() && token.len() <= MAX_TOKEN_BYTES)
}

#[derive(Deserialize)]
pub struct PairingRequest {
    client_id: String,
}

#[derive(Serialize)]
pub struct PairingResponse {
    pairing_token: String,
    expires_at: i64,
}

/// Admin-only: replace any unused pairing token for this client without disrupting its live host.
pub async fn create_pairing(
    State(state): State<AppState>,
    Json(request): Json<PairingRequest>,
) -> Result<Json<PairingResponse>, AppError> {
    if !valid_client_id(&request.client_id) {
        return Err(AppError::bad_request(
            "client_id must be a short host identifier",
        ));
    }
    let pairing_token = auth::generate_token();
    let expires_at = now() + PAIRING_TTL_SECONDS;
    state
        .database
        .create_managed_player_pairing(
            &request.client_id,
            &auth::hash_token(&pairing_token),
            expires_at,
        )
        .await
        .map_err(db_error)?;
    Ok(Json(PairingResponse {
        pairing_token,
        expires_at,
    }))
}

#[derive(Deserialize)]
pub struct EnrollRequest {
    pairing_token: String,
    client_id: String,
}

#[derive(Serialize)]
pub struct EnrollResponse {
    token: String,
    client_id: String,
}

/// Host-only: a one-time pairing token becomes a durable credential scoped to one client id.
pub async fn enroll(
    State(state): State<AppState>,
    Json(request): Json<EnrollRequest>,
) -> Result<Json<EnrollResponse>, AppError> {
    if !valid_text(&request.pairing_token, MAX_TOKEN_BYTES) || !valid_client_id(&request.client_id)
    {
        return Err(AppError::unauthorized(
            "pairing token is invalid or expired",
        ));
    }
    let token = auth::generate_token();
    let client_id = state
        .database
        .enroll_managed_player(
            &request.client_id,
            &auth::hash_token(&request.pairing_token),
            now(),
            &auth::hash_token(&token),
        )
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::unauthorized("pairing token is invalid or expired"))?;
    Ok(Json(EnrollResponse { token, client_id }))
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ManagedDevice {
    id: String,
    name: String,
    available: bool,
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    Idle,
    Applied,
    Failed,
    Unsupported,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Report {
    inventory_revision: String,
    devices: Vec<ManagedDevice>,
    selected_device_id: Option<String>,
    applied_revision: u64,
    attempted_revision: u64,
    status: ReportStatus,
    error: Option<String>,
    selection_supported: bool,
}

impl Report {
    fn validate(&self, desired_revision: u64) -> Result<(), AppError> {
        if !valid_text(&self.inventory_revision, MAX_INVENTORY_REVISION_BYTES)
            || self.devices.len() > MAX_DEVICES
            || self.devices.iter().any(|device| {
                !valid_text(&device.id, MAX_DEVICE_ID_BYTES)
                    || !valid_text(&device.name, MAX_DEVICE_NAME_BYTES)
            })
            || self
                .selected_device_id
                .as_deref()
                .is_some_and(|id| !valid_text(id, MAX_DEVICE_ID_BYTES))
            || self
                .error
                .as_deref()
                .is_some_and(|error| error.len() > MAX_ERROR_BYTES)
        {
            return Err(AppError::bad_request("report contains invalid device data"));
        }
        let mut ids = std::collections::HashSet::new();
        if self.devices.iter().any(|device| !ids.insert(&device.id)) {
            return Err(AppError::bad_request("device identifiers must be unique"));
        }
        if self.applied_revision > self.attempted_revision
            || self.attempted_revision > desired_revision
        {
            return Err(AppError::bad_request(
                "report acknowledges a future device revision",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
pub struct Desired {
    revision: u64,
    device_id: Option<String>,
    inventory_revision: Option<String>,
}

impl From<Option<ManagedPlayerDesired>> for Desired {
    fn from(value: Option<ManagedPlayerDesired>) -> Self {
        match value {
            Some(value) => Self {
                revision: value.revision,
                device_id: value.device_id,
                inventory_revision: value.inventory_revision,
            },
            None => Self {
                revision: 0,
                device_id: None,
                inventory_revision: None,
            },
        }
    }
}

async fn authenticated_host(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(String, String), AppError> {
    let token = bearer_token(headers)
        .ok_or_else(|| AppError::unauthorized("managed host token required"))?;
    let token_hash = auth::hash_token(token);
    state
        .database
        .managed_player_for_host_token(&token_hash)
        .await
        .map_err(db_error)?
        .map(|client_id| (client_id, token_hash))
        .ok_or_else(|| AppError::unauthorized("managed host token is invalid"))
}

/// Host-only. This authenticates exclusively against managed-host credentials; a user, player,
/// or session token cannot report state on this endpoint.
pub async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(report): Json<Report>,
) -> Result<Json<Desired>, AppError> {
    let (client_id, token_hash) = authenticated_host(&state, &headers).await?;
    let state_before = state
        .database
        .managed_player_state(&client_id)
        .await
        .map_err(db_error)?;
    let expected_desired_revision = state_before
        .desired
        .as_ref()
        .map_or(0, |desired| desired.revision);
    report.validate(expected_desired_revision)?;
    let report_json = serde_json::to_string(&report)
        .map_err(|error| AppError::internal(format!("serialize managed report: {error}")))?;
    let saved = state
        .database
        .save_managed_player_report(
            &client_id,
            &token_hash,
            &report.inventory_revision,
            &report_json,
            now(),
            report.applied_revision,
            report.attempted_revision,
        )
        .await
        .map_err(db_error)?;
    if !saved {
        return Err(AppError::conflict(
            "report is older than the latest host state",
        ));
    }
    let desired = state
        .database
        .managed_player_state(&client_id)
        .await
        .map_err(db_error)?
        .desired
        .into();
    Ok(Json(desired))
}

#[derive(Serialize)]
pub struct ManagedPlayerView {
    paired: bool,
    stale: bool,
    last_seen: Option<i64>,
    report: Option<Report>,
    desired: Desired,
}

fn view_from_state(state: ManagedPlayerState) -> Result<ManagedPlayerView, AppError> {
    let report = state
        .report_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| AppError::internal(format!("read managed report: {error}")))?;
    let stale = state.paired
        && state
            .last_seen_unix_seconds
            .is_none_or(|last_seen| last_seen < now() - STALE_AFTER_SECONDS);
    Ok(ManagedPlayerView {
        paired: state.paired,
        stale,
        last_seen: state.last_seen_unix_seconds,
        report,
        desired: state.desired.into(),
    })
}

pub async fn get(
    State(state): State<AppState>,
    Path(client_id): Path<String>,
) -> Result<Json<ManagedPlayerView>, AppError> {
    if !valid_client_id(&client_id) {
        return Err(AppError::bad_request(
            "client_id must be a short host identifier",
        ));
    }
    Ok(Json(view_from_state(
        state
            .database
            .managed_player_state(&client_id)
            .await
            .map_err(db_error)?,
    )?))
}

#[derive(Deserialize)]
pub struct SetDeviceRequest {
    device_id: String,
    inventory_revision: String,
}

/// Admin-only. The selected id must belong to the current fresh inventory and be available.
pub async fn set_device(
    State(state): State<AppState>,
    Path(client_id): Path<String>,
    Json(request): Json<SetDeviceRequest>,
) -> Result<Json<ManagedPlayerView>, AppError> {
    if !valid_client_id(&client_id)
        || !valid_text(&request.device_id, MAX_DEVICE_ID_BYTES)
        || !valid_text(&request.inventory_revision, MAX_INVENTORY_REVISION_BYTES)
    {
        return Err(AppError::bad_request("invalid device selection"));
    }
    let current_state = state
        .database
        .managed_player_state(&client_id)
        .await
        .map_err(db_error)?;
    let expected_report_json = current_state.report_json.clone();
    let current = view_from_state(current_state)?;
    let report = current
        .report
        .ok_or_else(|| AppError::bad_request("host has not reported its devices"))?;
    if !report.selection_supported {
        return Err(AppError::bad_request(
            "this host does not support device selection",
        ));
    }
    if current.stale || report.inventory_revision != request.inventory_revision {
        return Err(AppError::conflict(
            "device inventory changed; refresh and choose again",
        ));
    }
    let matching = report
        .devices
        .iter()
        .filter(|device| device.id == request.device_id && device.available)
        .count();
    if matching != 1 {
        return Err(AppError::bad_request(
            "choose one available device from the current inventory",
        ));
    }
    state
        .database
        .set_managed_player_device(
            &client_id,
            Some(&request.device_id),
            Some(&request.inventory_revision),
            expected_report_json.as_deref(),
        )
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::conflict("device inventory changed; refresh and choose again"))?;
    Ok(Json(view_from_state(
        state
            .database
            .managed_player_state(&client_id)
            .await
            .map_err(db_error)?,
    )?))
}

pub async fn revoke(
    State(state): State<AppState>,
    Path(client_id): Path<String>,
) -> Result<StatusCode, AppError> {
    if !valid_client_id(&client_id) {
        return Err(AppError::bad_request(
            "client_id must be a short host identifier",
        ));
    }
    state
        .database
        .revoke_managed_player(&client_id)
        .await
        .map_err(db_error)?;
    Ok(StatusCode::NO_CONTENT)
}
