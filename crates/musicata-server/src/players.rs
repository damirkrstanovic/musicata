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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use musicata_core::{
    PlaybackState, PlaybackStatus, Player, PlayerCapabilities, PlayerCommand, QueueItem,
    RepeatMode, Zone,
};
use musicata_storage::{Database, PlayerRecord};
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
}

impl Drop for PlayerEntry {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
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

/// Registry of registered players and zones, backed by the database.
pub struct PlayerManager {
    database: Database,
    public_base_url: String,
    players: RwLock<BTreeMap<String, PlayerEntry>>,
}

impl PlayerManager {
    /// Build the manager and bring up a runtime entry for every persisted player.
    pub async fn load(database: Database, public_base_url: String) -> Result<Arc<Self>> {
        let manager = Arc::new(Self {
            database,
            public_base_url,
            players: RwLock::new(BTreeMap::new()),
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
        Ok(manager)
    }

    pub fn public_base_url(&self) -> &str {
        &self.public_base_url
    }

    /// Bring up the runtime entry for a persisted player record.
    async fn bring_up(&self, record: &PlayerRecord) {
        let (handle, task) = match record.kind.as_str() {
            "browser" => (PlayerHandle::Browser(Arc::new(BrowserPlayer::new())), None),
            _ => {
                let player = Arc::new(MpdPlayer::new(record.id.clone(), record.address.clone()));
                let task = player.clone().spawn_state_task(self.database.clone());
                (PlayerHandle::Mpd(player), Some(task))
            }
        };
        self.players
            .write()
            .await
            .insert(record.id.clone(), PlayerEntry { handle, task });
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
        Ok(Zone {
            id,
            name: name.to_string(),
        })
    }

    pub async fn rename_zone(&self, id: &str, name: &str) -> Result<()> {
        self.database.update_zone_name(id, name).await
    }

    pub async fn delete_zone(&self, id: &str) -> Result<()> {
        self.database.delete_zone(id).await
    }

    /// Apply a command to every player in a zone.
    pub async fn command_zone(&self, zone_id: &str, command: PlayerCommand) -> Result<()> {
        for record in self.database.players_in_zone(zone_id).await? {
            if let Some(player) = self.get(&record.id).await {
                player
                    .execute(command.clone(), &self.database, &self.public_base_url)
                    .await?;
            }
        }
        Ok(())
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
        PlayerCommand::PlayTracks { track_ids } => {
            let urls = resolve_urls(database, public_base_url, track_ids).await?;
            connection.replace_queue(&urls).await
        }
        PlayerCommand::Enqueue { track_ids } => {
            let urls = resolve_urls(database, public_base_url, track_ids).await?;
            for url in &urls {
                connection.add(url).await?;
            }
            Ok(())
        }
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

/// Fill in title/artist/album for queue items that link to a known library track
/// but came back from MPD without tags (common when streaming over HTTP).
async fn enrich_state(state: &mut PlaybackState, database: &Database) {
    for item in state.queue.iter_mut().chain(state.now_playing.iter_mut()) {
        if !item.title.is_empty() {
            continue;
        }
        if let Some(track_id) = &item.track_id {
            if let Ok(Some(track)) = database.track(track_id).await {
                item.title = track.title;
                item.artist = track.artist_name;
                item.album = track.album_title;
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
    state: Mutex<BrowserState>,
    state_tx: broadcast::Sender<PlaybackState>,
}

#[derive(Default)]
struct BrowserState {
    status: PlaybackStatus,
    queue: Vec<QueueItem>,
    position: Option<usize>,
    elapsed_seconds: Option<f64>,
    volume: Option<u8>,
    repeat: RepeatMode,
    shuffle: bool,
}

impl BrowserPlayer {
    fn new() -> Self {
        let (state_tx, _) = broadcast::channel(32);
        Self {
            state: Mutex::new(BrowserState::default()),
            state_tx,
        }
    }

    /// Always available — it is the local browser.
    pub fn is_online(&self) -> bool {
        true
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PlaybackState> {
        self.state_tx.subscribe()
    }

    pub async fn snapshot(&self) -> PlaybackState {
        let state = self.state.lock().await;
        PlaybackState {
            status: state.status,
            now_playing: state
                .position
                .and_then(|index| state.queue.get(index).cloned()),
            elapsed_seconds: state.elapsed_seconds,
            duration_seconds: None,
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
        {
            let mut state = self.state.lock().await;
            match command {
                PlayerCommand::Play => state.status = PlaybackStatus::Playing,
                PlayerCommand::Pause => state.status = PlaybackStatus::Paused,
                PlayerCommand::Stop => {
                    state.status = PlaybackStatus::Stopped;
                    state.elapsed_seconds = Some(0.0);
                }
                PlayerCommand::Next => advance(&mut state, false),
                PlayerCommand::Previous => {
                    state.position = match state.position {
                        Some(index) if index > 0 => Some(index - 1),
                        other => other,
                    };
                    state.elapsed_seconds = Some(0.0);
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
                    }
                }
                PlayerCommand::PlayTracks { track_ids } => {
                    state.queue = resolve_queue_items(database, &track_ids).await?;
                    state.position = (!state.queue.is_empty()).then_some(0);
                    state.status = if state.queue.is_empty() {
                        PlaybackStatus::Stopped
                    } else {
                        PlaybackStatus::Playing
                    };
                    state.elapsed_seconds = Some(0.0);
                }
                PlayerCommand::Enqueue { track_ids } => {
                    state
                        .queue
                        .extend(resolve_queue_items(database, &track_ids).await?);
                }
            }
        }
        self.broadcast().await;
        Ok(())
    }

    /// The output tab finished the current track: advance (honoring repeat).
    pub async fn track_ended(&self) {
        {
            let mut state = self.state.lock().await;
            if state.repeat == RepeatMode::One {
                state.elapsed_seconds = Some(0.0);
            } else {
                advance(&mut state, true);
            }
        }
        self.broadcast().await;
    }

    /// The output tab reports its real playback position (for controllers).
    pub async fn report_progress(&self, elapsed_seconds: f64) {
        self.state.lock().await.elapsed_seconds = Some(elapsed_seconds);
        self.broadcast().await;
    }
}

/// Advance to the next queue item. When `stop_at_end` is set (a track finished),
/// stop after the last item unless repeat-all is on; otherwise (an explicit Next)
/// clamp at the last item.
fn advance(state: &mut BrowserState, stop_at_end: bool) {
    let Some(index) = state.position else {
        return;
    };
    state.elapsed_seconds = Some(0.0);
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
            items.push(QueueItem {
                track_id: Some(track.id),
                title: track.title,
                artist: track.artist_name,
                album: track.album_title,
                stream_url: track.stream_url,
            });
        }
    }
    Ok(items)
}
