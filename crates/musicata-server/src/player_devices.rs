// SPDX-License-Identifier: AGPL-3.0-or-later
//! Administrative device configuration uses separate MPD connections, never the playback lock.
use crate::{
    AppError, AppState, db_error,
    mpd::{MpdConnection, MpdOutput},
};
use axum::{
    Json,
    extract::{Path, State},
};
use musicata_core::Player;
use musicata_storage::PlayerRecord;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct ConnectionRequest {
    host: String,
    port: u16,
    password: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct OutputIdentity {
    name: String,
    plugin: String,
}

#[derive(Serialize)]
pub struct OutputView {
    outputs: Vec<MpdOutput>,
    selected: Option<OutputIdentity>,
    selection_supported: bool,
    reason: Option<String>,
}

async fn record(state: &AppState, id: &str) -> Result<PlayerRecord, AppError> {
    let record = state
        .database
        .player_record(id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::not_found("Player not found"))?;
    if record.kind != "mpd" {
        return Err(AppError::bad_request(
            "This operation is available for MPD players only",
        ));
    }
    Ok(record)
}

// Only administrative mutations take these locks. Separate players remain independent; playback
// controls and websocket updates never wait for device enumeration, network probes or SQLite.
pub async fn configuration_lock(
    state: &AppState,
    id: &str,
) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    let mut locks = state.player_device_changes.lock().await;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(id).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(id.to_owned(), std::sync::Arc::downgrade(&lock));
    lock
}

pub async fn connection(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ConnectionRequest>,
) -> Result<Json<Player>, AppError> {
    record(&state, &id).await?;
    let address = crate::mpd_address(&request.host, request.port).map_err(AppError::bad_request)?;
    if request
        .password
        .as_deref()
        .is_some_and(|p| p.contains(['\r', '\n']))
    {
        return Err(AppError::bad_request(
            "MPD passwords cannot contain line breaks",
        ));
    }
    // Keep persisted and live connection settings in step even if the HTTP caller leaves.
    tokio::spawn(async move {
        let lock = configuration_lock(&state, &id).await;
        let _guard = lock.lock().await;
        let player = state
            .players
            .rebind_mpd(&id, &address, request.password.as_deref())
            .await
            .map_err(|error| AppError::bad_request(error.to_string()))?;
        Ok(Json(player))
    })
    .await
    .map_err(|_| AppError::internal("Connection update failed; refresh the player"))?
}

async fn selected(state: &AppState, id: &str) -> Result<Option<OutputIdentity>, AppError> {
    Ok(state
        .database
        .player_output_selection(id)
        .await
        .map_err(db_error)?
        .map(|(name, plugin)| OutputIdentity { name, plugin }))
}

async fn protection(state: &AppState, id: &str) -> Result<Option<String>, AppError> {
    Ok(state.database.player_dsp_processor(id).await.map_err(db_error)?.map(|_| {
        "This player uses an audio processor. Change its playback device through the managed processor setup to preserve correction.".to_owned()
    }))
}

async fn connect(state: &AppState, player: &PlayerRecord) -> Result<MpdConnection, AppError> {
    let password = state
        .database
        .player_mpd_password(&player.id)
        .await
        .map_err(db_error)?;
    MpdConnection::connect_with_password(&player.address, password.as_deref())
        .await
        .map_err(|_| {
            AppError::probe_failed("Could not connect to MPD. Check the connection and password.")
        })
}

pub async fn outputs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<OutputView>, AppError> {
    let player = record(&state, &id).await?;
    let selected = selected(&state, &id).await?;
    if let Some(reason) = protection(&state, &id).await? {
        return Ok(Json(OutputView {
            outputs: vec![],
            selected,
            selection_supported: false,
            reason: Some(reason),
        }));
    }
    let outputs = connect(&state, &player)
        .await?
        .outputs()
        .await
        .map_err(|_| {
            AppError::probe_failed(
                "Could not read MPD outputs. Check connectivity and MPD output permissions.",
            )
        })?;
    Ok(Json(OutputView {
        outputs,
        selected,
        selection_supported: true,
        reason: None,
    }))
}

pub async fn select(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<OutputIdentity>,
) -> Result<Json<OutputView>, AppError> {
    record(&state, &id).await?;
    if request.name.is_empty() || request.name.len() > 1024 || request.plugin.len() > 128 {
        return Err(AppError::bad_request("Choose a configured MPD output"));
    }
    // A disconnected browser must not abandon a partially applied physical-device change.
    // The task owns both the configuration lock and persistence through verification/rollback.
    tokio::spawn(async move { select_owned(state, id, request).await })
        .await
        .map_err(|_| AppError::internal("Audio device operation failed; refresh its state"))?
}

async fn select_owned(
    state: AppState,
    id: String,
    request: OutputIdentity,
) -> Result<Json<OutputView>, AppError> {
    let lock = configuration_lock(&state, &id).await;
    let _guard = lock.lock().await;
    let player = record(&state, &id).await?;
    if let Some(reason) = protection(&state, &id).await? {
        return Err(AppError::bad_request(reason));
    }
    let outputs = connect(&state, &player)
        .await?
        .select_output(&request.name, &request.plugin)
        .await
        .map_err(|error| AppError::bad_request(error.to_string()))?;
    state.database.set_player_output_selection(&id, &request.name, &request.plugin).await
        .map_err(|_| AppError::internal("MPD changed its output, but the selection could not be saved. Refresh before trying again."))?;
    Ok(Json(OutputView {
        outputs,
        selected: Some(request),
        selection_supported: true,
        reason: None,
    }))
}
