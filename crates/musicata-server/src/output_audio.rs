// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-output correction and latest-value telemetry, separate from playback state.

use crate::{AppError, AppState, db_error};
use axum::{
    Json,
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::{IntoResponse, Response},
};
use musicata_core::dsp::{
    AudioCapabilities, DspProfile, DspStatus, MeasurementPoint, OutputDspSelection, OutputDspState,
    StereoLevels,
};
use musicata_storage::Database;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, watch};

/// Set only by scoped endpoint authentication, never by a WebSocket payload.
#[derive(Clone)]
pub struct EndpointIdentity(pub String);

pub struct OutputRuntime {
    pub state: OutputDspState,
    owner: Option<u64>,
    sequence: u64,
    pub last_levels: Option<(Instant, StereoLevels)>,
}

impl OutputRuntime {
    fn new(
        id: String,
        selection: OutputDspSelection,
        point: MeasurementPoint,
        capabilities: AudioCapabilities,
    ) -> Self {
        let session_id = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let status = if selection.enabled {
            DspStatus::Pending
        } else {
            DspStatus::Bypassed
        };
        Self {
            state: OutputDspState {
                output_id: id,
                configured: false,
                session_id,
                selection,
                desired_revision: 1,
                applied_revision: None,
                status,
                error: None,
                capabilities,
                measurement_point: point,
            },
            owner: None,
            sequence: 0,
            last_levels: None,
        }
    }

    fn select(&mut self, selection: OutputDspSelection) {
        self.state.configured = true;
        self.state.selection = selection;
        self.state.desired_revision += 1;
        self.state.applied_revision = None;
        self.state.error = None;
        self.state.status = DspStatus::Pending;
        self.last_levels = None;
        self.sequence = 0;
    }

    pub(crate) fn claim(&mut self, connection: u64) -> bool {
        if self.owner.is_some_and(|owner| owner != connection) {
            return false;
        }
        if self.owner != Some(connection) {
            self.state.session_id = format!(
                "{}-{}-{connection}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            );
            self.state.applied_revision = None;
            self.sequence = 0;
            self.last_levels = None;
        }
        self.owner = Some(connection);
        true
    }

    pub(crate) fn acknowledge(&mut self, owner: u64, revision: u64, error: Option<String>) -> bool {
        if self.owner != Some(owner) || revision != self.state.desired_revision {
            return false;
        }
        self.state.error = error;
        self.state.status = if self.state.error.is_some() {
            DspStatus::Error
        } else if self.state.selection.enabled {
            DspStatus::Applied
        } else {
            DspStatus::Bypassed
        };
        if self.state.error.is_none() {
            self.state.applied_revision = Some(revision);
        } else {
            self.state.applied_revision = None;
            self.last_levels = None;
        }
        true
    }

    pub(crate) fn accept_levels(
        &mut self,
        owner: u64,
        revision: u64,
        sequence: u64,
        levels: StereoLevels,
    ) -> bool {
        if self.owner != Some(owner)
            || revision != self.state.desired_revision
            || self.state.applied_revision != Some(revision)
            || sequence <= self.sequence
            || !levels.is_valid()
        {
            return false;
        }
        self.sequence = sequence;
        self.last_levels = Some((Instant::now(), levels));
        true
    }

    fn release(&mut self, owner: u64) {
        if self.owner != Some(owner) {
            return;
        }
        self.owner = None;
        self.last_levels = None;
        self.state.applied_revision = None;
        self.state.status = DspStatus::Unavailable;
    }
}

#[derive(Clone, Serialize)]
pub struct AudioConfig {
    pub state: OutputDspState,
    pub profile: Option<DspProfile>,
    pub meter_subscribed: bool,
}

pub struct AudioOutput {
    pub runtime: Mutex<OutputRuntime>,
    pub config: watch::Sender<AudioConfig>,
    persist: watch::Sender<Option<OutputDspSelection>>,
    pub kind: String,
    pub processor: watch::Sender<Option<(String, u16)>>,
    pub closed: AtomicBool,
    pub subscribers: AtomicUsize,
}

impl AudioOutput {
    pub async fn publish_state(&self) {
        let runtime = self.runtime.lock().await;
        self.config
            .send_modify(|config| config.state = runtime.state.clone());
    }
}

#[derive(Default)]
pub struct OutputAudio {
    // Serialize profile edits and selections, never playback, checkpoints or PCM work.
    pub profile_changes: Arc<Mutex<()>>,
    entries: Mutex<BTreeMap<String, Arc<AudioOutput>>>,
    connections: AtomicU64,
}

impl OutputAudio {
    pub async fn entry(
        &self,
        database: &Database,
        players: &Arc<crate::players::PlayerManager>,
        id: &str,
    ) -> Result<Arc<AudioOutput>, AppError> {
        // Check the persisted identity even for cached entries, so removed outputs cannot revive.
        let record = database
            .player_record(id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| AppError::not_found("unknown output"))?;
        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get(id) {
            return Ok(entry.clone());
        }
        let saved = database.player_dsp(id).await.map_err(db_error)?;
        let mut configured = saved.is_some();
        let mut selection = saved.unwrap_or_default();
        if record.kind == "snapcast" && database.player_dsp(id).await.map_err(db_error)?.is_none() {
            if let Some(profile_id) = database
                .get_setting("snapcast.dsp_profile_id")
                .await
                .map_err(db_error)?
                .filter(|id| !id.is_empty())
            {
                selection = OutputDspSelection {
                    profile_id: Some(profile_id),
                    enabled: true,
                };
                configured = true;
            }
        }
        let point = match record.kind.as_str() {
            "native" => MeasurementPoint::NativeOutput,
            "mpd" => MeasurementPoint::CamilladspPlayback,
            "snapcast" => MeasurementPoint::SnapcastStream,
            _ => MeasurementPoint::BrowserOutput,
        };
        let capabilities = AudioCapabilities {
            peq: record.kind != "mpd",
            room_ir: record.kind == "browser",
            meter: record.kind != "mpd",
        };
        let profile = match selection.profile_id.as_deref() {
            Some(id) => crate::dsp::profile_by_id(database, id).await,
            None => None,
        };
        // A checkpoint may survive a profile deletion if the process crashes before its
        // coalesced bypass write. Never restore correction for a profile that no longer exists.
        let missing_profile = selection.profile_id.is_some() && profile.is_none();
        if missing_profile {
            selection = OutputDspSelection::default();
        }
        let mut runtime = OutputRuntime::new(id.into(), selection.clone(), point, capabilities);
        runtime.state.configured = configured;
        let (config, _) = watch::channel(AudioConfig {
            state: runtime.state.clone(),
            profile,
            meter_subscribed: false,
        });
        let (persist, mut writes) = watch::channel::<Option<OutputDspSelection>>(None);
        let db = database.clone();
        let output_id = id.to_owned();
        tokio::spawn(async move {
            while writes.changed().await.is_ok() {
                loop {
                    let selection = writes.borrow_and_update().clone();
                    let Some(selection) = selection else { break };
                    match db.set_player_dsp(&output_id, &selection).await {
                        Ok(()) => break,
                        Err(error) => {
                            tracing::warn!(output = %output_id, %error, "output correction checkpoint failed");
                            if db.player_record(&output_id).await.ok().flatten().is_none() {
                                return;
                            }
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            }
        });
        let (processor, _) =
            watch::channel(database.player_dsp_processor(id).await.map_err(db_error)?);
        let entry = Arc::new(AudioOutput {
            runtime: Mutex::new(runtime),
            config,
            persist,
            kind: record.kind,
            processor,
            closed: AtomicBool::new(false),
            subscribers: AtomicUsize::new(0),
        });
        if missing_profile
            || (configured && database.player_dsp(id).await.map_err(db_error)?.is_none())
        {
            entry.persist.send_replace(Some(selection));
        }
        entries.insert(id.into(), entry.clone());
        drop(entries);
        #[cfg(feature = "snapcast")]
        if entry.kind == "snapcast" {
            if let Some(crate::players::PlayerHandle::Snapcast(player)) = players.get(id).await {
                start_snapcast(entry.clone(), player);
            }
        }
        if entry.kind == "mpd" {
            crate::mpd_dsp::start(entry.clone());
        }
        #[cfg(not(feature = "snapcast"))]
        let _ = players;
        Ok(entry)
    }

    pub async fn remove(&self, id: &str) {
        if let Some(output) = self.entries.lock().await.remove(id) {
            output.closed.store(true, Ordering::Release);
        }
    }

    pub async fn refresh_profile(&self, id: &str, profile: Option<DspProfile>) {
        let entries: Vec<_> = self.entries.lock().await.values().cloned().collect();
        for entry in entries {
            let mut runtime = entry.runtime.lock().await;
            if runtime.state.selection.profile_id.as_deref() != Some(id) {
                continue;
            }
            let mut selection = runtime.state.selection.clone();
            if profile.is_none() {
                selection = OutputDspSelection::default();
            }
            runtime.select(selection.clone());
            entry.config.send_replace(AudioConfig {
                state: runtime.state.clone(),
                profile: profile.clone(),
                meter_subscribed: entry.subscribers.load(Ordering::Acquire) > 0,
            });
            entry.persist.send_replace(Some(selection));
        }
    }
}

pub async fn get_selection(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<OutputDspState>, AppError> {
    let _profiles = state.output_audio.profile_changes.lock().await;
    let output = state
        .output_audio
        .entry(&state.database, &state.players, &id)
        .await?;
    Ok(Json(output.runtime.lock().await.state.clone()))
}

#[derive(Default, Deserialize)]
pub struct SelectionQuery {
    #[serde(default)]
    migrate: bool,
}

pub async fn set_selection(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<SelectionQuery>,
    Json(selection): Json<OutputDspSelection>,
) -> Result<Json<OutputDspState>, AppError> {
    let _profiles = state.output_audio.profile_changes.lock().await;
    let output = state
        .output_audio
        .entry(&state.database, &state.players, &id)
        .await?;
    let profile = if let Some(id) = selection.profile_id.as_deref() {
        let profile = crate::dsp::profile_by_id(&state.database, id)
            .await
            .ok_or_else(|| AppError::bad_request("unknown correction profile"))?;
        profile.validate().map_err(AppError::bad_request)?;
        if selection.enabled
            && profile.room_ir.is_some()
            && !output.runtime.lock().await.state.capabilities.room_ir
        {
            return Err(AppError::bad_request(
                "this output does not support room convolution",
            ));
        }
        Some(profile)
    } else {
        if selection.enabled {
            return Err(AppError::bad_request(
                "select a correction profile before enabling EQ",
            ));
        }
        None
    };
    let mut runtime = output.runtime.lock().await;
    if query.migrate && runtime.state.configured {
        return Ok(Json(runtime.state.clone()));
    }
    runtime.select(selection.clone());
    let response = runtime.state.clone();
    output.config.send_replace(AudioConfig {
        state: response.clone(),
        profile,
        meter_subscribed: output.subscribers.load(Ordering::Acquire) > 0,
    });
    output.persist.send_replace(Some(selection));
    Ok(Json(response))
}

pub async fn audio_ws(
    State(state): State<AppState>,
    Path(id): Path<String>,
    identity: Option<axum::Extension<EndpointIdentity>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let _profiles = state.output_audio.profile_changes.lock().await;
    match state
        .output_audio
        .entry(&state.database, &state.players, &id)
        .await
    {
        Ok(output) => {
            drop(_profiles);
            let connection = state
                .output_audio
                .connections
                .fetch_add(1, Ordering::Relaxed)
                + 1;
            let can_render = output.kind == "browser"
                || (output.kind == "native" && identity.is_some_and(|identity| identity.0.0 == id));
            upgrade.on_upgrade(move |socket| {
                socket_loop(socket, state, output, connection, can_render)
            })
        }
        Err(error) => error.into_response(),
    }
}

#[cfg(feature = "snapcast")]
fn start_snapcast(output: Arc<AudioOutput>, player: Arc<crate::players::SnapcastPlayer>) {
    let mut configs = output.config.subscribe();
    let tap = player.audio_tap();
    let weak = Arc::downgrade(&output);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(50));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut revision = 0;
        let mut sequence = 0;
        loop {
            interval.tick().await;
            let Some(output) = weak.upgrade() else {
                break;
            };
            if output.closed.load(Ordering::Acquire) {
                break;
            }
            if !player.writer_alive() {
                let mut runtime = output.runtime.lock().await;
                if runtime.state.status != DspStatus::Unavailable {
                    runtime.claim(0);
                    runtime.release(0);
                    drop(runtime);
                    output.publish_state().await;
                }
                continue;
            }
            let config = configs.borrow_and_update().clone();
            if config.state.desired_revision != revision {
                let result = if config.state.selection.enabled {
                    match &config.profile {
                        Some(profile) => musicata_core::pcm_dsp::StereoEq::from_profile(
                            profile,
                            player.sample_rate(),
                        ),
                        None => Err("the selected correction profile is unavailable"),
                    }
                } else {
                    Ok(None)
                };
                let mut runtime = output.runtime.lock().await;
                runtime.claim(0);
                revision = config.state.desired_revision;
                match result {
                    Ok(eq) => player.set_output_dsp(eq, revision),
                    Err(error) => {
                        runtime.acknowledge(0, revision, Some(error.into()));
                    }
                }
                drop(runtime);
                output.publish_state().await;
            }
            let applied = tap.applied_revision.load(Ordering::Acquire);
            let mut runtime = output.runtime.lock().await;
            if applied == 0 && runtime.state.applied_revision.is_some() {
                runtime.release(0);
                drop(runtime);
                output.publish_state().await;
                continue;
            }
            if applied == revision && runtime.state.applied_revision != Some(applied) {
                runtime.claim(0);
                runtime.acknowledge(0, applied, None);
                drop(runtime);
                output.publish_state().await;
                runtime = output.runtime.lock().await;
            }
            if let Some((next, levels)) = tap.read() {
                if next != sequence && applied == revision {
                    sequence = next;
                    runtime.accept_levels(0, applied, next, levels);
                }
            } else {
                runtime.last_levels = None;
            }
        }
    });
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Inbound {
    Renderer,
    DspApplied {
        session_id: String,
        revision: u64,
        #[serde(default)]
        error: Option<String>,
    },
    MeterSubscription {
        enabled: bool,
    },
    Levels {
        session_id: String,
        revision: u64,
        sequence: u64,
        levels: StereoLevels,
    },
}

async fn send(socket: &mut WebSocket, value: serde_json::Value) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            socket.send(Message::Text(value.to_string().into()))
        )
        .await,
        Ok(Ok(()))
    )
}

async fn socket_loop(
    mut socket: WebSocket,
    state: AppState,
    output: Arc<AudioOutput>,
    connection: u64,
    can_render: bool,
) {
    let id = output.config.borrow().state.output_id.clone();
    let Some(handle) = state.players.get(&id).await else {
        return;
    };
    let mut playback = handle.subscribe();
    let mut playing = handle
        .state(&state.database)
        .await
        .ok()
        .is_some_and(|s| s.status == musicata_core::PlaybackStatus::Playing);
    let mut configs = output.config.subscribe();
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let initial = configs.borrow_and_update().clone();
    if !send(
        &mut socket,
        serde_json::json!({"type":"dsp_state", "config": initial}),
    )
    .await
    {
        return;
    }
    let mut subscribed = false;
    let mut rendering = false;
    let mut sequence = 0;
    loop {
        tokio::select! {
            update = playback.recv() => {
                match update {
                    Ok(update) => {
                        playing = update.status == musicata_core::PlaybackStatus::Playing;
                        if !playing { output.runtime.lock().await.last_levels = None; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        playing = handle.state(&state.database).await.ok().is_some_and(|s| s.status == musicata_core::PlaybackStatus::Playing);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            changed = configs.changed() => {
                if changed.is_err() { break; }
                let config = configs.borrow_and_update().clone();
                if !send(&mut socket, serde_json::json!({"type": if rendering { "dsp_config" } else { "dsp_state" }, "config": config})).await { break; }
            }
            _ = interval.tick() => {
                if output.closed.load(Ordering::Acquire) {break;}
                if !subscribed { continue; }
                let mut runtime = output.runtime.lock().await;
                let levels = runtime.last_levels.filter(|(time, _)| playing && time.elapsed() < Duration::from_secs(1)).map(|(_, levels)| levels);
                if levels.is_none() { runtime.last_levels = None; }
                sequence += 1;
                let frame = serde_json::json!({"type":"levels", "output_id":runtime.state.output_id, "session_id":runtime.state.session_id,
                    "revision": runtime.state.desired_revision, "sequence":sequence, "measurement_point":runtime.state.measurement_point, "levels":levels});
                drop(runtime);
                if !send(&mut socket, frame).await { break; }
            }
            incoming = socket.recv() => {
                let text = match incoming { Some(Ok(Message::Text(text))) => text, Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue, _ => break };
                let Ok(frame) = serde_json::from_str::<Inbound>(&text) else { continue; };
                match frame {
                    Inbound::Renderer => {
                        if can_render && output.runtime.lock().await.claim(connection) {
                            rendering = true;
                            output.publish_state().await;
                            let config = output.config.borrow().clone();
                            if !send(&mut socket, serde_json::json!({"type":"renderer_granted", "config":config})).await { break; }

                        } else if !send(&mut socket, serde_json::json!({"type":"renderer_denied"})).await { break; }
                    }
                    Inbound::DspApplied { session_id, revision, error } => {
                        let mut runtime = output.runtime.lock().await;
                        if runtime.state.session_id == session_id && runtime.acknowledge(connection, revision, error.map(|e| e.chars().take(512).collect())) {
                            drop(runtime);
                            output.publish_state().await;
                        }
                    }
                    Inbound::MeterSubscription { enabled } => {
                        if enabled != subscribed {
                            if enabled {output.subscribers.fetch_add(1,Ordering::AcqRel);} else {output.subscribers.fetch_sub(1,Ordering::AcqRel);}
                            subscribed = enabled;
                            let active=output.subscribers.load(Ordering::Acquire)>0;
                            output.config.send_modify(|config| config.meter_subscribed=active);
                        }
                    },
                    Inbound::Levels { session_id, revision, sequence, levels } => {
                        let mut runtime = output.runtime.lock().await;
                        if runtime.state.session_id == session_id { runtime.accept_levels(connection, revision, sequence, levels); }
                    }
                }
            }
        }
    }
    if subscribed {
        output.subscribers.fetch_sub(1, Ordering::AcqRel);
        let active = output.subscribers.load(Ordering::Acquire) > 0;
        output
            .config
            .send_modify(|config| config.meter_subscribed = active);
    }
    output.runtime.lock().await.release(connection);
    output.publish_state().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> OutputRuntime {
        OutputRuntime::new(
            "browser".into(),
            OutputDspSelection::default(),
            MeasurementPoint::BrowserOutput,
            AudioCapabilities {
                peq: true,
                room_ir: true,
                meter: true,
            },
        )
    }

    #[test]
    fn renderer_lease_prevents_other_tabs_from_acknowledging_or_publishing() {
        let mut output = runtime();
        assert!(output.claim(1));
        assert!(!output.claim(2));
        let revision = output.state.desired_revision;
        assert!(!output.acknowledge(2, revision, None));
        assert!(output.acknowledge(1, revision, None));
        output.release(1);
        assert!(output.claim(2));
    }

    #[test]
    fn old_profile_ack_cannot_overwrite_latest_selection() {
        let mut output = runtime();
        output.claim(1);
        let old = output.state.desired_revision;
        output.select(OutputDspSelection {
            profile_id: Some("new".into()),
            enabled: true,
        });
        assert!(!output.acknowledge(1, old, None));
        assert_eq!(output.state.status, DspStatus::Pending);
        assert!(output.acknowledge(1, output.state.desired_revision, None));
        assert_eq!(output.state.status, DspStatus::Applied);
    }

    #[test]
    fn measurements_require_current_applied_revision() {
        let mut output = runtime();
        output.claim(1);
        let levels = StereoLevels {
            rms_l: 0.1,
            rms_r: 0.2,
            peak_l: 0.2,
            peak_r: 0.3,
        };
        let revision = output.state.desired_revision;
        assert!(!output.accept_levels(1, revision, 1, levels));
        output.acknowledge(1, revision, None);
        assert!(output.accept_levels(1, revision, 1, levels));
        output.acknowledge(1, revision, Some("failed".into()));
        assert!(!output.accept_levels(1, revision, 2, levels));
    }

    #[test]
    fn invalid_stale_and_unowned_levels_are_rejected() {
        let mut output = runtime();
        output.claim(1);
        let valid = StereoLevels {
            rms_l: 0.5,
            rms_r: 0.25,
            peak_l: 0.5,
            peak_r: 0.25,
        };
        let revision = output.state.desired_revision;
        output.acknowledge(1, revision, None);
        assert!(output.accept_levels(1, revision, 1, valid));
        assert!(!output.accept_levels(1, revision, 1, valid));
        assert!(!output.accept_levels(2, revision, 2, valid));
        assert!(!output.accept_levels(1, revision + 1, 2, valid));
        let invalid = StereoLevels {
            rms_l: f64::NAN,
            ..valid
        };
        assert!(!output.accept_levels(1, revision, 2, invalid));
        output.release(1);
        assert!(output.last_levels.is_none());
    }
}

#[derive(Serialize, Deserialize)]
pub struct ProcessorBinding {
    pub host: String,
    pub port: u16,
}
pub async fn get_processor(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Option<ProcessorBinding>>, AppError> {
    let output = state
        .output_audio
        .entry(&state.database, &state.players, &id)
        .await?;
    if output.kind != "mpd" {
        return Err(AppError::bad_request(
            "processor binding is only available for MPD outputs",
        ));
    }
    let binding = output
        .processor
        .borrow()
        .clone()
        .map(|(host, port)| ProcessorBinding { host, port });
    Ok(Json(binding))
}
pub async fn set_processor(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(binding): Json<Option<ProcessorBinding>>,
) -> Result<Json<Option<ProcessorBinding>>, AppError> {
    let output = state
        .output_audio
        .entry(&state.database, &state.players, &id)
        .await?;
    if output.kind != "mpd" {
        return Err(AppError::bad_request(
            "processor binding is only available for MPD outputs",
        ));
    }
    if let Some(binding) = &binding {
        if binding.host.is_empty()
            || binding.host.len() > 253
            || binding.port == 0
            || !binding
                .host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-:_".contains(&c))
        {
            return Err(AppError::bad_request(
                "enter a processor hostname or IP address and a valid port",
            ));
        }
    }
    state
        .database
        .set_player_dsp_processor(
            &id,
            binding.as_ref().map(|b| b.host.as_str()),
            binding.as_ref().map(|b| b.port),
        )
        .await
        .map_err(db_error)?;
    output
        .processor
        .send_replace(binding.as_ref().map(|b| (b.host.clone(), b.port)));
    Ok(Json(binding))
}
