//! Player registry, zones, and the MPD-backed player provider.
//!
//! Players are *registered* with the server (reported in, e.g. from the web UI)
//! and persisted in the database, so they survive restarts and can be renamed and
//! grouped into zones. Musicata owns the command API and live state; the MPD
//! provider translates commands to the MPD protocol and hands MPD absolute
//! Musicata stream URLs (so MPD needs no filesystem access). A per-player
//! background task watches MPD's `idle` channel and broadcasts state to
//! controllers.
//!
//! A zone is a named group of players used as a control target — a command sent
//! to a zone is applied to each player in it. There is no audio synchronization.
//!
//! MPD is the only backend today, so the provider is concrete; the registry and
//! command/state shape are provider-agnostic.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use musicata_core::{
    PlaybackState, PlaybackStatus, Player, PlayerCapabilities, PlayerCommand, QueueItem,
    RepeatMode, Zone,
};
use musicata_storage::{Database, PlayerPlayback, PlayerRecord};
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio::task::JoinHandle;

use crate::mpd::MpdConnection;

/// Stable id of the always-present local browser player.
pub const BROWSER_PLAYER_ID: &str = "browser-local";

/// A registered player's live runtime: the provider instance plus the background
/// task feeding its state broadcast.
struct PlayerEntry {
    handle: PlayerHandle,
    /// Background idle/state task, for backends that have one (MPD). The browser
    /// player is command-driven and has none.
    task: Option<JoinHandle<()>>,
    /// Background task that watches this player's state broadcast and records
    /// listening history. Every player has one.
    recorder: JoinHandle<()>,
}

impl Drop for PlayerEntry {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
        self.recorder.abort();
    }
}

/// A runtime player backend. Modeled as an enum (rather than `dyn`) so the async
/// methods stay object-safe and the set of providers is explicit.
#[derive(Clone)]
pub enum PlayerHandle {
    Mpd(Arc<MpdPlayer>),
    Browser(Arc<BrowserPlayer>),
}

impl PlayerHandle {
    pub fn is_online(&self) -> bool {
        match self {
            PlayerHandle::Mpd(player) => player.is_online(),
            PlayerHandle::Browser(player) => player.is_online(),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PlaybackState> {
        match self {
            PlayerHandle::Mpd(player) => player.subscribe(),
            PlayerHandle::Browser(player) => player.subscribe(),
        }
    }

    pub async fn state(&self, database: &Database) -> Result<PlaybackState> {
        match self {
            PlayerHandle::Mpd(player) => player.state(database).await,
            PlayerHandle::Browser(player) => Ok(player.snapshot().await),
        }
    }

    pub async fn execute(
        &self,
        command: PlayerCommand,
        database: &Database,
        base_url: &str,
    ) -> Result<()> {
        match self {
            PlayerHandle::Mpd(player) => player.execute(command, database, base_url).await,
            PlayerHandle::Browser(player) => player.execute(command, database).await,
        }
    }
}

/// Wall-clock seconds since the Unix epoch. Saturates to 0 before 1970.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Watches one player's state stream and decides when a track counts as played. A
/// play is recorded the moment a (different) track becomes the current track while the
/// player is playing — i.e. as soon as you start it. Progress ticks, pauses, resumes,
/// and seeks on the same track don't re-record; switching to another track does.
#[derive(Default)]
struct PlayTracker {
    last_played: Option<String>,
}

impl PlayTracker {
    /// Returns the track id to record as a play, or `None` if this state doesn't start
    /// a new track.
    fn observe(&mut self, state: &PlaybackState) -> Option<String> {
        if state.status != PlaybackStatus::Playing {
            return None;
        }
        let current = state
            .now_playing
            .as_ref()
            .and_then(|n| n.track_id.clone())?;
        if self.last_played.as_deref() == Some(current.as_str()) {
            return None;
        }
        self.last_played = Some(current.clone());
        Some(current)
    }
}

/// Spawns the per-player task that consumes its state broadcast and records plays.
fn spawn_listen_recorder(
    player_id: String,
    mut states: broadcast::Receiver<PlaybackState>,
    database: Database,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tracker = PlayTracker::default();
        loop {
            match states.recv().await {
                Ok(state) => {
                    if let Some(track_id) = tracker.observe(&state)
                        && let Err(error) = database
                            .record_listen(&track_id, &player_id, now_unix())
                            .await
                    {
                        tracing::warn!(%player_id, %track_id, %error, "failed to record play");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// A zone's live runtime: its server-owned queue plus the background task that
/// records listening history from the zone's state broadcast.
struct ZoneEntry {
    player: Arc<ZonePlayer>,
    recorder: JoinHandle<()>,
}

impl Drop for ZoneEntry {
    fn drop(&mut self) {
        self.recorder.abort();
    }
}

/// Registry of registered players and zones, backed by the database.
pub struct PlayerManager {
    database: Database,
    public_base_url: String,
    players: RwLock<BTreeMap<String, PlayerEntry>>,
    zones: RwLock<BTreeMap<String, ZoneEntry>>,
}

impl PlayerManager {
    /// Build the manager and bring up a runtime entry for every persisted player.
    pub async fn load(database: Database, public_base_url: String) -> Result<Arc<Self>> {
        let manager = Arc::new(Self {
            database,
            public_base_url,
            players: RwLock::new(BTreeMap::new()),
            zones: RwLock::new(BTreeMap::new()),
        });
        // The local browser player always exists.
        if manager
            .database
            .player_record(BROWSER_PLAYER_ID)
            .await?
            .is_none()
        {
            manager
                .database
                .upsert_player(&PlayerRecord {
                    id: BROWSER_PLAYER_ID.to_string(),
                    kind: "browser".to_string(),
                    address: "local".to_string(),
                    name: "This Browser".to_string(),
                    zone_id: None,
                })
                .await?;
        }
        for record in manager.database.list_players().await? {
            manager.bring_up(&record).await;
        }
        for zone in manager.database.list_zones().await? {
            manager.bring_up_zone(&zone).await;
        }
        Ok(manager)
    }

    /// Bring up the runtime entry for a persisted zone: its server-owned queue
    /// (restored from the database) plus a listen recorder keyed by the zone id.
    async fn bring_up_zone(&self, zone: &Zone) {
        let player = Arc::new(ZonePlayer::new(self.database.clone(), zone.id.clone()));
        player.restore().await;
        let recorder =
            spawn_listen_recorder(zone.id.clone(), player.subscribe(), self.database.clone());
        self.zones
            .write()
            .await
            .insert(zone.id.clone(), ZoneEntry { player, recorder });
    }

    pub async fn get_zone(&self, id: &str) -> Option<Arc<ZonePlayer>> {
        self.zones
            .read()
            .await
            .get(id)
            .map(|entry| entry.player.clone())
    }

    pub fn public_base_url(&self) -> &str {
        &self.public_base_url
    }

    /// Bring up the runtime entry for a persisted player record.
    async fn bring_up(&self, record: &PlayerRecord) {
        let (handle, task) = match record.kind.as_str() {
            "browser" => {
                let player = Arc::new(BrowserPlayer::new(self.database.clone(), record.id.clone()));
                // Reload any queue persisted before the last shutdown so it survives a
                // server restart, not just a page refresh.
                player.restore().await;
                (PlayerHandle::Browser(player), None)
            }
            _ => {
                let player = Arc::new(MpdPlayer::new(record.id.clone(), record.address.clone()));
                let task = player.clone().spawn_state_task(self.database.clone());
                (PlayerHandle::Mpd(player), Some(task))
            }
        };
        let recorder =
            spawn_listen_recorder(record.id.clone(), handle.subscribe(), self.database.clone());
        self.players.write().await.insert(
            record.id.clone(),
            PlayerEntry {
                handle,
                task,
                recorder,
            },
        );
    }

    /// Register a player (idempotent by kind+address); persists it and brings it
    /// up. Re-registering the same address just updates the display name.
    pub async fn register(&self, kind: &str, address: &str, name: &str) -> Result<Player> {
        if kind != "mpd" {
            return Err(anyhow!("unsupported player kind: {kind}"));
        }
        let id = player_id(kind, address);
        let record = PlayerRecord {
            id: id.clone(),
            kind: kind.to_string(),
            address: address.to_string(),
            name: name.to_string(),
            zone_id: self
                .database
                .player_record(&id)
                .await?
                .and_then(|existing| existing.zone_id),
        };
        self.database.upsert_player(&record).await?;
        if !self.players.read().await.contains_key(&id) {
            self.bring_up(&record).await;
        }
        self.descriptor(&record).await
    }

    pub async fn rename(&self, id: &str, name: &str) -> Result<Player> {
        self.require_record(id).await?;
        self.database.update_player_name(id, name).await?;
        self.descriptor(&self.require_record(id).await?).await
    }

    pub async fn set_zone(&self, id: &str, zone_id: Option<&str>) -> Result<Player> {
        self.require_record(id).await?;
        self.database.update_player_zone(id, zone_id).await?;
        self.descriptor(&self.require_record(id).await?).await
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        self.require_record(id).await?;
        self.players.write().await.remove(id); // Drop aborts the task.
        self.database.delete_player(id).await?;
        Ok(())
    }

    pub async fn descriptors(&self) -> Result<Vec<Player>> {
        let mut players = Vec::new();
        for record in self.database.list_players().await? {
            players.push(self.descriptor(&record).await?);
        }
        Ok(players)
    }

    pub async fn get(&self, id: &str) -> Option<PlayerHandle> {
        self.players
            .read()
            .await
            .get(id)
            .map(|entry| entry.handle.clone())
    }

    async fn require_record(&self, id: &str) -> Result<PlayerRecord> {
        self.database
            .player_record(id)
            .await?
            .ok_or_else(|| anyhow!("unknown player: {id}"))
    }

    async fn descriptor(&self, record: &PlayerRecord) -> Result<Player> {
        let online = match self.players.read().await.get(&record.id) {
            Some(entry) => entry.handle.is_online(),
            None => false,
        };
        Ok(Player {
            id: record.id.clone(),
            name: record.name.clone(),
            kind: record.kind.clone(),
            address: record.address.clone(),
            zone_id: record.zone_id.clone(),
            online,
            capabilities: PlayerCapabilities {
                seek: true,
                volume: true,
                repeat: true,
                shuffle: true,
                queue: true,
            },
        })
    }

    // ---- Zones ---------------------------------------------------------------

    pub async fn zones(&self) -> Result<Vec<Zone>> {
        self.database.list_zones().await
    }

    pub async fn create_zone(&self, name: &str) -> Result<Zone> {
        let id = zone_id(name);
        self.database.insert_zone(&id, name).await?;
        let zone = Zone {
            id,
            name: name.to_string(),
        };
        self.bring_up_zone(&zone).await;
        Ok(zone)
    }

    pub async fn rename_zone(&self, id: &str, name: &str) -> Result<()> {
        self.database.update_zone_name(id, name).await
    }

    pub async fn delete_zone(&self, id: &str) -> Result<()> {
        self.zones.write().await.remove(id); // Drop aborts the recorder.
        self.database.delete_zone(id).await
    }

    /// Apply a command to a zone's canonical queue, which then drives its members.
    pub async fn command_zone(&self, zone_id: &str, command: PlayerCommand) -> Result<()> {
        let zone = self
            .get_zone(zone_id)
            .await
            .ok_or_else(|| anyhow!("unknown zone: {zone_id}"))?;
        let mut members = Vec::new();
        for record in self.database.players_in_zone(zone_id).await? {
            if let Some(handle) = self.get(&record.id).await {
                members.push(handle);
            }
        }
        zone.execute(command, &self.database, &self.public_base_url, &members)
            .await
    }

    /// The current playback state of a zone's canonical queue.
    pub async fn zone_state(&self, zone_id: &str) -> Result<PlaybackState> {
        let zone = self
            .get_zone(zone_id)
            .await
            .ok_or_else(|| anyhow!("unknown zone: {zone_id}"))?;
        Ok(zone.snapshot().await)
    }
}

/// Deterministic, stable id from a player's kind and address.
fn player_id(kind: &str, address: &str) -> String {
    let slug: String = address
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    format!("{kind}-{slug}")
}

fn zone_id(name: &str) -> String {
    let slug: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    format!("zone-{}", slug.trim_matches('-'))
}

pub struct MpdPlayer {
    id: String,
    addr: String,
    /// Command connection, reconnected on demand. A separate connection is used
    /// for the blocking idle loop.
    connection: Mutex<Option<MpdConnection>>,
    online: AtomicBool,
    state_tx: broadcast::Sender<PlaybackState>,
}

impl MpdPlayer {
    fn new(id: String, addr: String) -> Self {
        let (state_tx, _) = broadcast::channel(16);
        Self {
            id,
            addr,
            connection: Mutex::new(None),
            online: AtomicBool::new(false),
            state_tx,
        }
    }

    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PlaybackState> {
        self.state_tx.subscribe()
    }

    /// Fetch and enrich the current playback state via the command connection.
    pub async fn state(&self, database: &Database) -> Result<PlaybackState> {
        let mut guard = self.connection.lock().await;
        let result = async {
            let connection = ensure_connected(&mut guard, &self.addr).await?;
            connection.playback_state().await
        }
        .await;
        match result {
            Ok(mut state) => {
                self.online.store(true, Ordering::Relaxed);
                enrich_state(&mut state, database).await;
                Ok(state)
            }
            Err(error) => {
                self.online.store(false, Ordering::Relaxed);
                *guard = None;
                Err(error)
            }
        }
    }

    /// Apply a command, resolving any referenced track ids to stream URLs, then
    /// broadcast the resulting state to subscribers.
    pub async fn execute(
        &self,
        command: PlayerCommand,
        database: &Database,
        public_base_url: &str,
    ) -> Result<()> {
        let result = {
            let mut guard = self.connection.lock().await;
            async {
                let connection = ensure_connected(&mut guard, &self.addr).await?;
                apply_command(connection, &command, database, public_base_url).await
            }
            .await
        };
        if let Err(error) = &result {
            self.online.store(false, Ordering::Relaxed);
            *self.connection.lock().await = None;
            return Err(anyhow!(error.to_string()));
        }
        if let Ok(state) = self.state(database).await {
            let _ = self.state_tx.send(state);
        }
        Ok(())
    }

    /// Background task: keep a dedicated idle connection open and broadcast a
    /// fresh state snapshot whenever MPD reports a change. Reconnects with backoff.
    fn spawn_state_task(self: Arc<Self>, database: Database) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                if let Err(error) = self.run_idle_loop(&database).await {
                    self.online.store(false, Ordering::Relaxed);
                    tracing::debug!(player = %self.id, %error, "mpd idle loop ended");
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    }

    async fn run_idle_loop(&self, database: &Database) -> Result<()> {
        let mut idle = MpdConnection::connect(&self.addr).await?;
        if let Ok(state) = self.state(database).await {
            let _ = self.state_tx.send(state);
        }
        loop {
            idle.idle().await?;
            if let Ok(state) = self.state(database).await {
                let _ = self.state_tx.send(state);
            }
        }
    }
}

async fn ensure_connected<'a>(
    guard: &'a mut Option<MpdConnection>,
    addr: &str,
) -> Result<&'a mut MpdConnection> {
    if guard.is_none() {
        *guard = Some(MpdConnection::connect(addr).await?);
    }
    Ok(guard.as_mut().expect("connection just set"))
}

async fn apply_command(
    connection: &mut MpdConnection,
    command: &PlayerCommand,
    database: &Database,
    public_base_url: &str,
) -> Result<()> {
    match command {
        PlayerCommand::Play => connection.play().await,
        PlayerCommand::Pause => connection.pause().await,
        PlayerCommand::Stop => connection.stop().await,
        PlayerCommand::Next => connection.next().await,
        PlayerCommand::Previous => connection.previous().await,
        PlayerCommand::Seek { position_seconds } => connection.seek(*position_seconds).await,
        PlayerCommand::SetVolume { volume } => connection.set_volume(*volume).await,
        PlayerCommand::SetRepeat { mode } => connection.set_repeat(*mode).await,
        PlayerCommand::SetShuffle { enabled } => connection.set_shuffle(*enabled).await,
        PlayerCommand::Clear => connection.clear().await,
        PlayerCommand::PlayQueueIndex { index } => connection.play_index(*index).await,
        PlayerCommand::PlayTracks {
            track_ids,
            start_index,
        } => {
            let urls = resolve_urls(database, public_base_url, track_ids).await?;
            connection.replace_queue(&urls).await?;
            // Start at the requested queue position (default 0) in the same command,
            // so the controller needn't follow up with a separate play_queue_index.
            if *start_index > 0 && (*start_index) < urls.len() {
                connection.play_index(*start_index).await?;
            }
            Ok(())
        }
        PlayerCommand::Enqueue { track_ids } => {
            let urls = resolve_urls(database, public_base_url, track_ids).await?;
            for url in &urls {
                connection.add(url).await?;
            }
            Ok(())
        }
        PlayerCommand::RemoveQueueItem { index } => connection.delete_index(*index).await,
        PlayerCommand::MoveQueueItem { from, to } => connection.move_item(*from, *to).await,
        // Radio/external streams: MPD plays the URL directly and reads its own metadata.
        PlayerCommand::PlayStream { url, .. } => connection.replace_queue(&[url.clone()]).await,
    }
}

/// Resolve library track ids to absolute stream URLs MPD can fetch over HTTP.
async fn resolve_urls(
    database: &Database,
    public_base_url: &str,
    track_ids: &[String],
) -> Result<Vec<String>> {
    let mut urls = Vec::with_capacity(track_ids.len());
    for id in track_ids {
        if let Some(track) = database.track(id).await? {
            urls.push(format!("{public_base_url}{}", track.stream_url));
        }
    }
    Ok(urls)
}

/// Fill in title/artist/album/artwork for queue items that link to a known
/// library track but came back from MPD without tags (common when streaming
/// over HTTP).
async fn enrich_state(state: &mut PlaybackState, database: &Database) {
    for item in state.queue.iter_mut().chain(state.now_playing.iter_mut()) {
        let Some(track_id) = item.track_id.clone() else {
            continue;
        };
        if item.title.is_empty() {
            if let Ok(Some(track)) = database.track(&track_id).await {
                item.title = track.title;
                item.artist = track.artist_name;
                item.album = track.album_title;
                if item.artwork_url.is_none() {
                    item.artwork_url = database
                        .album_artwork_url(&track.album_id)
                        .await
                        .ok()
                        .flatten();
                }
            }
        } else if item.artwork_url.is_none() {
            if let Ok(Some(track)) = database.track(&track_id).await {
                item.artwork_url = database
                    .album_artwork_url(&track.album_id)
                    .await
                    .ok()
                    .flatten();
            }
        }
    }
}

// ---- Browser player -------------------------------------------------------

/// A server-owned player whose audio is rendered by a browser tab. The queue and
/// playback intent live here (so they survive a page refresh and stay in sync
/// across controllers); a tab acting as output drives its `<audio>` from this
/// state over the WebSocket and reports progress / track-ended back.
pub struct BrowserPlayer {
    /// This player's stable id, used as the persistence key.
    player_id: String,
    /// Where the server-owned queue is persisted, so it survives a restart.
    database: Database,
    state: Mutex<QueueState>,
    state_tx: broadcast::Sender<PlaybackState>,
    /// Lightweight position ticks. The output tab reports progress ~1×/second; rather
    /// than re-broadcast the whole `PlaybackState` (queue and all) on every tick, those
    /// go out on this channel as a tiny frame. Full state is reserved for real changes
    /// (play/pause, track change, queue edits). Kept separate from `state_tx` so the
    /// listen recorder and MPD's `PlayerHandle::subscribe` path are unaffected.
    progress_tx: broadcast::Sender<ProgressTick>,
    /// Unix second of the last throttled progress persist. Progress ticks arrive
    /// ~1×/second; we persist the elapsed position at most once per
    /// [`PROGRESS_PERSIST_SECS`] rather than writing SQLite every tick.
    last_progress_persist: AtomicI64,
}

/// Throttle window for persisting in-track elapsed position from progress ticks.
const PROGRESS_PERSIST_SECS: i64 = 10;

/// Which slice of the persisted queue a change needs written back.
enum QueuePersist {
    /// Only the lightweight playback row changed (play/pause/seek/volume/…).
    Playback(PlayerPlayback),
    /// The queue items changed too (enqueue/clear/remove/move/play-tracks).
    Queue(PlayerPlayback, Vec<QueueItem>),
}

/// A position-only update for controllers: the current elapsed time and (once known)
/// the track duration. Serialized as `{ "type": "progress", … }` on the WebSocket.
#[derive(Clone, Copy, Debug)]
pub struct ProgressTick {
    pub elapsed_seconds: f64,
    pub duration_seconds: Option<f64>,
}

#[derive(Default)]
struct QueueState {
    status: PlaybackStatus,
    queue: Vec<QueueItem>,
    position: Option<usize>,
    elapsed_seconds: Option<f64>,
    duration_seconds: Option<f64>,
    volume: Option<u8>,
    repeat: RepeatMode,
    shuffle: bool,
}

impl QueueState {
    /// The lightweight, persistable playback row (everything but the queue items).
    fn playback(&self) -> PlayerPlayback {
        PlayerPlayback {
            status: self.status,
            position: self.position,
            elapsed_seconds: self.elapsed_seconds,
            volume: self.volume,
            repeat: self.repeat,
            shuffle: self.shuffle,
        }
    }
}

impl BrowserPlayer {
    fn new(database: Database, player_id: String) -> Self {
        let (state_tx, _) = broadcast::channel(32);
        let (progress_tx, _) = broadcast::channel(32);
        Self {
            player_id,
            database,
            state: Mutex::new(QueueState::default()),
            state_tx,
            progress_tx,
            last_progress_persist: AtomicI64::new(0),
        }
    }

    /// Reload the queue persisted before the last shutdown. A queue that was
    /// `Playing` is restored as `Paused` at its saved position — no output tab is
    /// rendering audio at startup, so we never silently auto-resume; the user
    /// presses play and continues where they left off.
    async fn restore(&self) {
        let snapshot = match self.database.load_player_queue(&self.player_id).await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(player = %self.player_id, %error, "failed to load persisted queue");
                return;
            }
        };
        let mut state = self.state.lock().await;
        let playback = snapshot.playback;
        state.queue = snapshot.items;
        // Guard against a position that no longer indexes the queue.
        state.position = playback.position.filter(|&index| index < state.queue.len());
        state.status = match playback.status {
            PlaybackStatus::Playing => PlaybackStatus::Paused,
            other if state.position.is_none() => match other {
                // A queue we can't point into can't be paused/playing.
                PlaybackStatus::Paused => PlaybackStatus::Stopped,
                other => other,
            },
            other => other,
        };
        state.elapsed_seconds = playback.elapsed_seconds;
        state.duration_seconds = None;
        state.volume = playback.volume;
        state.repeat = playback.repeat;
        state.shuffle = playback.shuffle;
    }

    /// Write a queue change back to the database. Failures are logged, never
    /// propagated — persistence must not break live playback control.
    async fn persist(&self, persist: QueuePersist) {
        let result = match &persist {
            QueuePersist::Playback(playback) => {
                self.database
                    .save_player_playback(&self.player_id, playback)
                    .await
            }
            QueuePersist::Queue(playback, items) => {
                self.database
                    .save_player_queue(&self.player_id, playback, items)
                    .await
            }
        };
        if let Err(error) = result {
            tracing::warn!(player = %self.player_id, %error, "failed to persist player queue");
        }
    }

    /// Always available — it is the local browser.
    pub fn is_online(&self) -> bool {
        true
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PlaybackState> {
        self.state_tx.subscribe()
    }

    /// Subscribe to lightweight position ticks (separate from full-state updates).
    pub fn subscribe_progress(&self) -> broadcast::Receiver<ProgressTick> {
        self.progress_tx.subscribe()
    }

    pub async fn snapshot(&self) -> PlaybackState {
        let state = self.state.lock().await;
        PlaybackState {
            status: state.status,
            now_playing: state
                .position
                .and_then(|index| state.queue.get(index).cloned()),
            elapsed_seconds: state.elapsed_seconds,
            duration_seconds: state.duration_seconds,
            volume: state.volume,
            repeat: state.repeat,
            shuffle: state.shuffle,
            queue: state.queue.clone(),
            queue_position: state.position,
        }
    }

    async fn broadcast(&self) {
        let _ = self.state_tx.send(self.snapshot().await);
    }

    pub async fn execute(&self, command: PlayerCommand, database: &Database) -> Result<()> {
        // Commands that change the queue itself need the item list rewritten;
        // the rest only touch the lightweight playback row.
        let mutates_queue = command_mutates_queue(&command);
        let persist = {
            let mut state = self.state.lock().await;
            apply_to_queue_state(&mut state, command, database).await?;
            let playback = state.playback();
            if mutates_queue {
                QueuePersist::Queue(playback, state.queue.clone())
            } else {
                QueuePersist::Playback(playback)
            }
        };
        self.broadcast().await;
        self.persist(persist).await;
        Ok(())
    }

    /// The output tab finished the current track: advance (honoring repeat).
    pub async fn track_ended(&self) {
        let playback = {
            let mut state = self.state.lock().await;
            if state.repeat == RepeatMode::One {
                state.elapsed_seconds = Some(0.0);
            } else {
                advance(&mut state, true);
            }
            state.playback()
        };
        self.broadcast().await;
        // Advancing only moves the position within the same queue, so the item
        // list is unchanged — persist just the playback row.
        self.persist(QueuePersist::Playback(playback)).await;
    }

    /// The output tab reports its real playback position and track duration. This
    /// fires ~1×/second, so it emits only a lightweight position tick — not a full
    /// `PlaybackState` (which would re-send the whole queue every second). The stored
    /// state is still updated so the next full snapshot/refresh reflects the position.
    pub async fn report_progress(&self, elapsed_seconds: f64, duration_seconds: Option<f64>) {
        let (duration, playback) = {
            let mut state = self.state.lock().await;
            state.elapsed_seconds = Some(elapsed_seconds);
            if let Some(duration) = duration_seconds {
                state.duration_seconds = Some(duration);
            }
            (state.duration_seconds, state.playback())
        };
        let _ = self.progress_tx.send(ProgressTick {
            elapsed_seconds,
            duration_seconds: duration,
        });
        // Persist the elapsed position so a restart resumes mid-track — but at most
        // once per PROGRESS_PERSIST_SECS, not on every ~1 Hz tick.
        let now = now_unix();
        let last = self.last_progress_persist.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= PROGRESS_PERSIST_SECS
            && self
                .last_progress_persist
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.persist(QueuePersist::Playback(playback)).await;
        }
    }
}

/// A zone's server-owned, canonical playback queue. Modeled exactly like
/// [`BrowserPlayer`] — the queue, position, and playback intent live here and
/// persist to SQLite (`zone_queue` tables) — but a zone drives *member players* as
/// outputs rather than a single browser tab. There is no audio sample-sync (see the
/// roadmap): the zone's [`QueueState`] is the single source of truth for the queue,
/// position, now-playing, repeat, and shuffle; `elapsed_seconds` is owned by the
/// browser output that renders the zone. MPD members are best-effort mirrors driven
/// by forwarding the same command, and the zone does not poll them for position —
/// so an MPD-only zone (no browser output reporting `ended`) can drift in position
/// until the user issues next/previous. Mapping MPD state back to the zone is a
/// future improvement.
pub struct ZonePlayer {
    /// This zone's stable id, used as the persistence key.
    zone_id: String,
    /// Where the canonical zone queue is persisted, so it survives a restart.
    database: Database,
    state: Mutex<QueueState>,
    state_tx: broadcast::Sender<PlaybackState>,
    progress_tx: broadcast::Sender<ProgressTick>,
    last_progress_persist: AtomicI64,
}

impl ZonePlayer {
    fn new(database: Database, zone_id: String) -> Self {
        let (state_tx, _) = broadcast::channel(32);
        let (progress_tx, _) = broadcast::channel(32);
        Self {
            zone_id,
            database,
            state: Mutex::new(QueueState::default()),
            state_tx,
            progress_tx,
            last_progress_persist: AtomicI64::new(0),
        }
    }

    /// Reload the queue persisted before the last shutdown. As with the browser
    /// player, a queue that was `Playing` is restored as `Paused` at its saved
    /// position — no output is rendering at startup, so we never auto-resume.
    async fn restore(&self) {
        let snapshot = match self.database.load_zone_queue(&self.zone_id).await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(zone = %self.zone_id, %error, "failed to load persisted zone queue");
                return;
            }
        };
        let mut state = self.state.lock().await;
        let playback = snapshot.playback;
        state.queue = snapshot.items;
        state.position = playback.position.filter(|&index| index < state.queue.len());
        state.status = match playback.status {
            PlaybackStatus::Playing => PlaybackStatus::Paused,
            other if state.position.is_none() => match other {
                PlaybackStatus::Paused => PlaybackStatus::Stopped,
                other => other,
            },
            other => other,
        };
        state.elapsed_seconds = playback.elapsed_seconds;
        state.duration_seconds = None;
        state.volume = playback.volume;
        state.repeat = playback.repeat;
        state.shuffle = playback.shuffle;
    }

    async fn persist(&self, persist: QueuePersist) {
        let result = match &persist {
            QueuePersist::Playback(playback) => {
                self.database
                    .save_zone_playback(&self.zone_id, playback)
                    .await
            }
            QueuePersist::Queue(playback, items) => {
                self.database
                    .save_zone_queue(&self.zone_id, playback, items)
                    .await
            }
        };
        if let Err(error) = result {
            tracing::warn!(zone = %self.zone_id, %error, "failed to persist zone queue");
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PlaybackState> {
        self.state_tx.subscribe()
    }

    pub fn subscribe_progress(&self) -> broadcast::Receiver<ProgressTick> {
        self.progress_tx.subscribe()
    }

    pub async fn snapshot(&self) -> PlaybackState {
        let state = self.state.lock().await;
        PlaybackState {
            status: state.status,
            now_playing: state
                .position
                .and_then(|index| state.queue.get(index).cloned()),
            elapsed_seconds: state.elapsed_seconds,
            duration_seconds: state.duration_seconds,
            volume: state.volume,
            repeat: state.repeat,
            shuffle: state.shuffle,
            queue: state.queue.clone(),
            queue_position: state.position,
        }
    }

    async fn broadcast(&self) {
        let _ = self.state_tx.send(self.snapshot().await);
    }

    /// Apply a command to the canonical zone queue, broadcast/persist it, then drive
    /// member outputs. `members` are the live handles of the zone's players, passed
    /// in by the manager (avoids a `ZonePlayer`→`PlayerManager` reference cycle).
    pub async fn execute(
        &self,
        command: PlayerCommand,
        database: &Database,
        public_base_url: &str,
        members: &[PlayerHandle],
    ) -> Result<()> {
        let mutates_queue = command_mutates_queue(&command);
        // Phase 1: update the canonical queue exactly like the browser player.
        let persist = {
            let mut state = self.state.lock().await;
            apply_to_queue_state(&mut state, command.clone(), database).await?;
            let playback = state.playback();
            if mutates_queue {
                QueuePersist::Queue(playback, state.queue.clone())
            } else {
                QueuePersist::Playback(playback)
            }
        };
        self.broadcast().await;
        self.persist(persist).await;
        // Phase 2: drive members. Browser members render the zone's now-playing
        // straight off the broadcast above (no extra work). MPD members are driven
        // by forwarding the command — queue ops map 1:1 to MPD and indices stay
        // aligned because MPD's queue mirrors the zone's.
        self.drive_members(&command, members, database, public_base_url)
            .await;
        Ok(())
    }

    async fn drive_members(
        &self,
        command: &PlayerCommand,
        members: &[PlayerHandle],
        database: &Database,
        public_base_url: &str,
    ) {
        for member in members {
            // Browser members are driven by the zone broadcast; only MPD members
            // need an out-of-band command. (Forwarding to the single browser player
            // would give it a second, competing queue.)
            if !matches!(member, PlayerHandle::Mpd(_)) {
                continue;
            }
            if let Err(error) = member
                .execute(command.clone(), database, public_base_url)
                .await
            {
                tracing::debug!(zone = %self.zone_id, %error, "zone member command failed");
            }
        }
    }

    /// The browser output rendering this zone finished the current track: advance
    /// (honoring repeat). MPD members advance on their own — see the type docs.
    pub async fn track_ended(&self) {
        let playback = {
            let mut state = self.state.lock().await;
            if state.repeat == RepeatMode::One {
                state.elapsed_seconds = Some(0.0);
            } else {
                advance(&mut state, true);
            }
            state.playback()
        };
        self.broadcast().await;
        self.persist(QueuePersist::Playback(playback)).await;
    }

    /// The browser output reports its real position/duration for this zone. Emits a
    /// lightweight tick (not full state) and persists the elapsed position, throttled.
    pub async fn report_progress(&self, elapsed_seconds: f64, duration_seconds: Option<f64>) {
        let (duration, playback) = {
            let mut state = self.state.lock().await;
            state.elapsed_seconds = Some(elapsed_seconds);
            if let Some(duration) = duration_seconds {
                state.duration_seconds = Some(duration);
            }
            (state.duration_seconds, state.playback())
        };
        let _ = self.progress_tx.send(ProgressTick {
            elapsed_seconds,
            duration_seconds: duration,
        });
        let now = now_unix();
        let last = self.last_progress_persist.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= PROGRESS_PERSIST_SECS
            && self
                .last_progress_persist
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.persist(QueuePersist::Playback(playback)).await;
        }
    }
}

/// Whether a command changes the queue items (vs. only the lightweight playback
/// row). Queue-mutating changes need the item list rewritten on persist.
fn command_mutates_queue(command: &PlayerCommand) -> bool {
    matches!(
        command,
        PlayerCommand::PlayTracks { .. }
            | PlayerCommand::Enqueue { .. }
            | PlayerCommand::Clear
            | PlayerCommand::RemoveQueueItem { .. }
            | PlayerCommand::MoveQueueItem { .. }
            | PlayerCommand::PlayStream { .. }
    )
}

/// Apply a command to a server-owned queue, mutating its in-memory state. Shared
/// by the browser player and the zone player so their queue semantics are
/// identical — only how the resulting state is *rendered* (a browser tab vs. zone
/// member outputs) differs.
async fn apply_to_queue_state(
    state: &mut QueueState,
    command: PlayerCommand,
    database: &Database,
) -> Result<()> {
    match command {
        PlayerCommand::Play => state.status = PlaybackStatus::Playing,
        PlayerCommand::Pause => state.status = PlaybackStatus::Paused,
        PlayerCommand::Stop => {
            state.status = PlaybackStatus::Stopped;
            state.elapsed_seconds = Some(0.0);
        }
        PlayerCommand::Next => advance(state, false),
        PlayerCommand::Previous => {
            state.position = match state.position {
                Some(index) if index > 0 => Some(index - 1),
                other => other,
            };
            state.elapsed_seconds = Some(0.0);
            state.duration_seconds = None;
        }
        PlayerCommand::Seek { position_seconds } => {
            state.elapsed_seconds = Some(position_seconds);
        }
        PlayerCommand::SetVolume { volume } => state.volume = Some(volume.min(100)),
        PlayerCommand::SetRepeat { mode } => state.repeat = mode,
        PlayerCommand::SetShuffle { enabled } => state.shuffle = enabled,
        PlayerCommand::Clear => {
            state.queue.clear();
            state.position = None;
            state.status = PlaybackStatus::Stopped;
            state.elapsed_seconds = Some(0.0);
        }
        PlayerCommand::PlayQueueIndex { index } => {
            if index < state.queue.len() {
                state.position = Some(index);
                state.status = PlaybackStatus::Playing;
                state.elapsed_seconds = Some(0.0);
                state.duration_seconds = None;
            }
        }
        PlayerCommand::PlayTracks {
            track_ids,
            start_index,
        } => {
            state.queue = resolve_queue_items(database, &track_ids).await?;
            state.position = (!state.queue.is_empty())
                .then(|| start_index.min(state.queue.len().saturating_sub(1)));
            state.status = if state.queue.is_empty() {
                PlaybackStatus::Stopped
            } else {
                PlaybackStatus::Playing
            };
            state.duration_seconds = None;
            state.elapsed_seconds = Some(0.0);
        }
        PlayerCommand::Enqueue { track_ids } => {
            state
                .queue
                .extend(resolve_queue_items(database, &track_ids).await?);
        }
        PlayerCommand::RemoveQueueItem { index } => remove_queue_item(state, index),
        PlayerCommand::MoveQueueItem { from, to } => move_queue_item(state, from, to),
        PlayerCommand::PlayStream { url, title } => {
            state.queue = vec![QueueItem {
                track_id: None,
                title,
                artist: String::new(),
                album: String::new(),
                stream_url: url,
                artwork_url: None,
            }];
            state.position = Some(0);
            state.status = PlaybackStatus::Playing;
            state.duration_seconds = None;
            state.elapsed_seconds = Some(0.0);
        }
    }
    Ok(())
}

/// Advance to the next queue item. When `stop_at_end` is set (a track finished),
/// stop after the last item unless repeat-all is on; otherwise (an explicit Next)
/// clamp at the last item.
fn advance(state: &mut QueueState, stop_at_end: bool) {
    let Some(index) = state.position else {
        return;
    };
    state.elapsed_seconds = Some(0.0);
    state.duration_seconds = None;
    if index + 1 < state.queue.len() {
        state.position = Some(index + 1);
    } else if state.repeat == RepeatMode::All && !state.queue.is_empty() {
        state.position = Some(0);
    } else if stop_at_end {
        state.status = PlaybackStatus::Stopped;
    }
}

/// Resolve library track ids to queue items with relative stream URLs the browser
/// fetches directly. Unknown ids are skipped.
async fn resolve_queue_items(database: &Database, track_ids: &[String]) -> Result<Vec<QueueItem>> {
    let mut items = Vec::with_capacity(track_ids.len());
    for id in track_ids {
        if let Some(track) = database.track(id).await? {
            let artwork_url = database.album_artwork_url(&track.album_id).await?;
            items.push(QueueItem {
                track_id: Some(track.id),
                title: track.title,
                artist: track.artist_name,
                album: track.album_title,
                stream_url: track.stream_url,
                artwork_url,
            });
        }
    }
    Ok(items)
}

/// Remove a queue item, keeping `position` pointing at the same playing track
/// (or stopping if the playing track itself was removed).
fn remove_queue_item(state: &mut QueueState, index: usize) {
    if index >= state.queue.len() {
        return;
    }
    state.queue.remove(index);
    match state.position {
        Some(pos) if pos == index => {
            if state.queue.is_empty() {
                state.position = None;
                state.status = PlaybackStatus::Stopped;
            } else {
                state.position = Some(pos.min(state.queue.len() - 1));
                state.elapsed_seconds = Some(0.0);
            }
        }
        Some(pos) if pos > index => state.position = Some(pos - 1),
        _ => {}
    }
}

/// Move a queue item, keeping `position` pointing at the same playing track.
fn move_queue_item(state: &mut QueueState, from: usize, to: usize) {
    if from >= state.queue.len() || to >= state.queue.len() || from == to {
        return;
    }
    let item = state.queue.remove(from);
    state.queue.insert(to, item);
    if let Some(pos) = state.position {
        state.position = Some(reindex_after_move(pos, from, to));
    }
}

/// New index of the element previously at `pos` after moving `from` -> `to`.
fn reindex_after_move(pos: usize, from: usize, to: usize) -> usize {
    if pos == from {
        to
    } else if from < pos && pos <= to {
        pos - 1
    } else if to <= pos && pos < from {
        pos + 1
    } else {
        pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use musicata_core::{Album, Artist, Library, ProviderMapping, Track};
    use std::path::PathBuf;

    fn playing(track_id: &str) -> PlaybackState {
        PlaybackState {
            status: PlaybackStatus::Playing,
            now_playing: Some(QueueItem {
                track_id: Some(track_id.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn paused(track_id: &str) -> PlaybackState {
        PlaybackState {
            status: PlaybackStatus::Paused,
            ..playing(track_id)
        }
    }

    fn stopped() -> PlaybackState {
        PlaybackState {
            status: PlaybackStatus::Stopped,
            ..Default::default()
        }
    }

    #[test]
    fn records_a_play_when_a_new_track_starts() {
        let mut tracker = PlayTracker::default();
        assert_eq!(tracker.observe(&playing("a")), Some("a".to_string()));
    }

    #[test]
    fn does_not_rerecord_the_same_track_on_progress_pause_or_resume() {
        let mut tracker = PlayTracker::default();
        assert_eq!(tracker.observe(&playing("a")), Some("a".to_string()));
        assert_eq!(tracker.observe(&playing("a")), None); // progress tick
        assert_eq!(tracker.observe(&paused("a")), None); // pause
        assert_eq!(tracker.observe(&playing("a")), None); // resume same track
    }

    #[test]
    fn records_again_after_switching_to_another_track() {
        let mut tracker = PlayTracker::default();
        assert_eq!(tracker.observe(&playing("a")), Some("a".to_string()));
        assert_eq!(tracker.observe(&playing("b")), Some("b".to_string()));
        // Returning to a previously played track is a new play.
        assert_eq!(tracker.observe(&playing("a")), Some("a".to_string()));
    }

    #[test]
    fn ignores_stopped_and_idle_states() {
        let mut tracker = PlayTracker::default();
        assert_eq!(tracker.observe(&stopped()), None);
        assert_eq!(tracker.observe(&paused("a")), None);
    }

    fn temp_db(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("musicata-{name}-{nanos}.db"))
    }

    fn library_with_tracks(count: usize) -> Library {
        let artist = Artist {
            id: "artist_1".to_string(),
            name: "Artist".to_string(),
            album_count: 1,
            track_count: count,
        };
        let album = Album {
            id: "album_1".to_string(),
            title: "Album".to_string(),
            artist_id: "artist_1".to_string(),
            artist_name: "Artist".to_string(),
            year: Some(2026),
            track_count: count,
            artwork_url: None,
            artwork_path: None,
        };
        let tracks = (1..=count)
            .map(|i| Track {
                id: format!("track_{i}"),
                provider: ProviderMapping {
                    provider_id: "local-disk".to_string(),
                    item_id: format!("album/{i}.mp3"),
                },
                observed_metadata: Vec::new(),
                title: format!("Song {i}"),
                artist_id: "artist_1".to_string(),
                artist_name: "Artist".to_string(),
                album_id: "album_1".to_string(),
                album_title: "Album".to_string(),
                year: Some(2026),
                track_number: Some(i as u16),
                disc_number: None,
                extension: "mp3".to_string(),
                file_size_bytes: Some(1),
                duration_seconds: Some(180.0),
                modified_at_unix_seconds: Some(1),
                content_hash: Some(format!("h{i}")),
                relative_path: format!("album/{i}.mp3"),
                stream_url: format!("/api/tracks/track_{i}/stream"),
                added_at_unix_seconds: None,
                path: PathBuf::from(format!("/music/album/{i}.mp3")),
            })
            .collect();
        Library {
            provider_id: "local-disk".to_string(),
            source_root: "/music".to_string(),
            artists: vec![artist],
            albums: vec![album],
            tracks,
            scan_errors: Vec::new(),
        }
    }

    async fn play(handle: &PlayerHandle, database: &Database, track_id: &str) {
        handle
            .execute(
                PlayerCommand::PlayTracks {
                    track_ids: vec![track_id.to_string()],
                    start_index: 0,
                },
                database,
                "http://localhost",
            )
            .await
            .expect("play command");
    }

    /// The recorder runs in a background task, so poll the history until it has at
    /// least `expected` rows (or fail after a generous timeout). Returns track ids,
    /// most-recent first.
    async fn wait_for_recent(database: &Database, expected: usize) -> Vec<String> {
        for _ in 0..200 {
            let recent = database.recently_played(50).await.expect("recent");
            if recent.len() >= expected {
                return recent.into_iter().map(|(track, _)| track.id).collect();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let recent = database.recently_played(50).await.expect("recent");
        panic!(
            "recently_played reached only {} of {expected} rows",
            recent.len()
        );
    }

    // End-to-end through the real player + recorder wiring: play one track, then four
    // more, and confirm each played track lands in Recently played.
    #[tokio::test]
    async fn recently_played_reflects_each_played_track() {
        let db_path = temp_db("recent-plays");
        let database = Database::connect(&db_path).await.expect("connect");
        let mut library = library_with_tracks(5);
        database.save_library(&mut library).await.expect("save");

        let manager = PlayerManager::load(database.clone(), "http://localhost".to_string())
            .await
            .expect("manager");
        let browser = manager
            .get(BROWSER_PLAYER_ID)
            .await
            .expect("browser player present");

        // Play one track -> exactly one entry, and it's that track.
        play(&browser, &database, "track_1").await;
        let recent = wait_for_recent(&database, 1).await;
        assert_eq!(recent, vec!["track_1".to_string()]);

        // Play four more distinct tracks -> all five present.
        for id in ["track_2", "track_3", "track_4", "track_5"] {
            play(&browser, &database, id).await;
        }
        let recent = wait_for_recent(&database, 5).await;
        let found: std::collections::BTreeSet<&str> = recent.iter().map(String::as_str).collect();
        for id in ["track_1", "track_2", "track_3", "track_4", "track_5"] {
            assert!(found.contains(id), "recent missing {id}; got {recent:?}");
        }
        assert_eq!(recent.len(), 5, "expected exactly five distinct tracks");

        let _ = std::fs::remove_file(db_path);
    }

    // A position tick (the ~1×/second report from the output tab) must go out as a
    // lightweight progress frame, NOT a full PlaybackState — otherwise a playing track
    // re-sends the whole queue every second (the bug that made controls sluggish).
    #[tokio::test]
    async fn report_progress_emits_lightweight_tick_not_full_state() {
        let db_path = temp_db("report-progress");
        let database = Database::connect(&db_path).await.expect("connect");
        let player = BrowserPlayer::new(database, BROWSER_PLAYER_ID.to_string());
        let mut states = player.subscribe();
        let mut ticks = player.subscribe_progress();

        player.report_progress(12.5, Some(200.0)).await;

        let tick = ticks.try_recv().expect("a progress tick was broadcast");
        assert_eq!(tick.elapsed_seconds, 12.5);
        assert_eq!(tick.duration_seconds, Some(200.0));
        assert!(
            matches!(
                states.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "a progress tick must not broadcast a full PlaybackState"
        );

        let _ = std::fs::remove_file(db_path);
    }

    // Playing a non-first track must set the queue and start at that position in a
    // single broadcast — not play_tracks (position 0) then play_queue_index, which
    // briefly announced the wrong now-playing track and restarted browser audio.
    #[tokio::test]
    async fn play_tracks_with_start_index_starts_there_in_one_broadcast() {
        let db_path = temp_db("start-index");
        let database = Database::connect(&db_path).await.expect("connect");
        let mut library = library_with_tracks(5);
        database.save_library(&mut library).await.expect("save");

        let player = BrowserPlayer::new(database.clone(), BROWSER_PLAYER_ID.to_string());
        let mut states = player.subscribe();
        player
            .execute(
                PlayerCommand::PlayTracks {
                    track_ids: vec![
                        "track_1".to_string(),
                        "track_2".to_string(),
                        "track_3".to_string(),
                    ],
                    start_index: 2,
                },
                &database,
            )
            .await
            .expect("play");

        let state = states.try_recv().expect("one state frame");
        assert_eq!(state.queue_position, Some(2));
        assert_eq!(
            state.now_playing.and_then(|n| n.track_id),
            Some("track_3".to_string()),
            "now-playing is the requested track, not queue index 0"
        );
        assert!(
            matches!(
                states.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "play_tracks with start_index must be a single broadcast"
        );

        let _ = std::fs::remove_file(db_path);
    }

    // start_index past the end clamps to the last track rather than panicking or
    // leaving nothing playing.
    #[tokio::test]
    async fn play_tracks_start_index_clamps_to_last() {
        let db_path = temp_db("start-index-clamp");
        let database = Database::connect(&db_path).await.expect("connect");
        let mut library = library_with_tracks(5);
        database.save_library(&mut library).await.expect("save");

        let player = BrowserPlayer::new(database.clone(), BROWSER_PLAYER_ID.to_string());
        player
            .execute(
                PlayerCommand::PlayTracks {
                    track_ids: vec!["track_1".to_string(), "track_2".to_string()],
                    start_index: 99,
                },
                &database,
            )
            .await
            .expect("play");

        let snapshot = player.snapshot().await;
        assert_eq!(snapshot.queue_position, Some(1));

        let _ = std::fs::remove_file(db_path);
    }

    // The browser player's server-owned queue must survive a server restart, not just
    // a page refresh: play a queue at a non-zero position, drop the manager, reload it
    // from the same database, and confirm the queue + position come back — paused (no
    // output tab renders audio at startup), never auto-resumed as playing.
    #[tokio::test]
    async fn browser_queue_survives_a_restart_restored_paused() {
        let db_path = temp_db("queue-restart");
        let database = Database::connect(&db_path).await.expect("connect");
        let mut library = library_with_tracks(5);
        database.save_library(&mut library).await.expect("save");

        {
            let manager = PlayerManager::load(database.clone(), "http://localhost".to_string())
                .await
                .expect("manager");
            let browser = manager.get(BROWSER_PLAYER_ID).await.expect("browser");
            browser
                .execute(
                    PlayerCommand::PlayTracks {
                        track_ids: vec![
                            "track_1".to_string(),
                            "track_2".to_string(),
                            "track_3".to_string(),
                        ],
                        start_index: 1,
                    },
                    &database,
                    "http://localhost",
                )
                .await
                .expect("play");
            browser
                .execute(
                    PlayerCommand::SetRepeat {
                        mode: RepeatMode::All,
                    },
                    &database,
                    "http://localhost",
                )
                .await
                .expect("repeat");
            // Status is Playing here; the restart should restore it as Paused.
            assert_eq!(
                browser.state(&database).await.expect("state").status,
                PlaybackStatus::Playing
            );
        }

        // Fresh manager over the same database = a server restart.
        let manager = PlayerManager::load(database.clone(), "http://localhost".to_string())
            .await
            .expect("reload manager");
        let browser = manager.get(BROWSER_PLAYER_ID).await.expect("browser");
        let state = browser.state(&database).await.expect("state");

        assert_eq!(state.queue.len(), 3, "queue restored");
        assert_eq!(state.queue_position, Some(1), "position restored");
        assert_eq!(
            state.now_playing.and_then(|n| n.track_id),
            Some("track_2".to_string())
        );
        assert_eq!(state.repeat, RepeatMode::All, "repeat mode restored");
        assert_eq!(
            state.status,
            PlaybackStatus::Paused,
            "a restored queue is paused, never auto-resumed as playing"
        );

        let _ = std::fs::remove_file(db_path);
    }

    // A zone owns a canonical queue just like the browser player: playing a non-first
    // track sets the queue and starts at that position in a single broadcast. An empty
    // member list exercises the canonical-queue path with no MPD/browser I/O.
    #[tokio::test]
    async fn zone_play_tracks_with_start_index_starts_there_in_one_broadcast() {
        let db_path = temp_db("zone-start-index");
        let database = Database::connect(&db_path).await.expect("connect");
        let mut library = library_with_tracks(5);
        database.save_library(&mut library).await.expect("save");

        let zone = ZonePlayer::new(database.clone(), "zone-living-room".to_string());
        let mut states = zone.subscribe();
        zone.execute(
            PlayerCommand::PlayTracks {
                track_ids: vec![
                    "track_1".to_string(),
                    "track_2".to_string(),
                    "track_3".to_string(),
                ],
                start_index: 2,
            },
            &database,
            "http://localhost",
            &[],
        )
        .await
        .expect("play");

        let state = states.try_recv().expect("one state frame");
        assert_eq!(state.queue_position, Some(2));
        assert_eq!(
            state.now_playing.and_then(|n| n.track_id),
            Some("track_3".to_string()),
            "now-playing is the requested track, not queue index 0"
        );
        assert!(
            matches!(
                states.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "play_tracks with start_index must be a single broadcast"
        );

        let _ = std::fs::remove_file(db_path);
    }

    // A zone progress report emits a lightweight tick, not a full PlaybackState.
    #[tokio::test]
    async fn zone_report_progress_emits_lightweight_tick_not_full_state() {
        let db_path = temp_db("zone-report-progress");
        let database = Database::connect(&db_path).await.expect("connect");
        let zone = ZonePlayer::new(database, "zone-x".to_string());
        let mut states = zone.subscribe();
        let mut ticks = zone.subscribe_progress();

        zone.report_progress(8.0, Some(180.0)).await;

        let tick = ticks.try_recv().expect("a progress tick was broadcast");
        assert_eq!(tick.elapsed_seconds, 8.0);
        assert_eq!(tick.duration_seconds, Some(180.0));
        assert!(
            matches!(
                states.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "a progress tick must not broadcast a full PlaybackState"
        );

        let _ = std::fs::remove_file(db_path);
    }

    // A zone's canonical queue survives a server restart, restored paused — same
    // contract as the browser player, driven through the manager lifecycle.
    #[tokio::test]
    async fn zone_queue_survives_a_restart_restored_paused() {
        let db_path = temp_db("zone-queue-restart");
        let database = Database::connect(&db_path).await.expect("connect");
        let mut library = library_with_tracks(5);
        database.save_library(&mut library).await.expect("save");

        let zone_id = {
            let manager = PlayerManager::load(database.clone(), "http://localhost".to_string())
                .await
                .expect("manager");
            let zone = manager.create_zone("Living Room").await.expect("zone");
            manager
                .command_zone(
                    &zone.id,
                    PlayerCommand::PlayTracks {
                        track_ids: vec![
                            "track_1".to_string(),
                            "track_2".to_string(),
                            "track_3".to_string(),
                        ],
                        start_index: 1,
                    },
                )
                .await
                .expect("play");
            // Playing here; the restart should restore it as Paused.
            assert_eq!(
                manager.zone_state(&zone.id).await.expect("state").status,
                PlaybackStatus::Playing
            );
            zone.id
        };

        // Fresh manager over the same database = a server restart.
        let manager = PlayerManager::load(database.clone(), "http://localhost".to_string())
            .await
            .expect("reload manager");
        let state = manager.zone_state(&zone_id).await.expect("state");

        assert_eq!(state.queue.len(), 3, "queue restored");
        assert_eq!(state.queue_position, Some(1), "position restored");
        assert_eq!(
            state.now_playing.and_then(|n| n.track_id),
            Some("track_2".to_string())
        );
        assert_eq!(
            state.status,
            PlaybackStatus::Paused,
            "a restored zone queue is paused, never auto-resumed as playing"
        );

        let _ = std::fs::remove_file(db_path);
    }
}
