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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        PlayerCommand::RemoveQueueItem { index } => connection.delete_index(*index).await,
        PlayerCommand::MoveQueueItem { from, to } => connection.move_item(*from, *to).await,
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
    state: Mutex<BrowserState>,
    state_tx: broadcast::Sender<PlaybackState>,
}

#[derive(Default)]
struct BrowserState {
    status: PlaybackStatus,
    queue: Vec<QueueItem>,
    position: Option<usize>,
    elapsed_seconds: Option<f64>,
    duration_seconds: Option<f64>,
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
                PlayerCommand::PlayTracks { track_ids } => {
                    state.queue = resolve_queue_items(database, &track_ids).await?;
                    state.position = (!state.queue.is_empty()).then_some(0);
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
                PlayerCommand::RemoveQueueItem { index } => remove_queue_item(&mut state, index),
                PlayerCommand::MoveQueueItem { from, to } => move_queue_item(&mut state, from, to),
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

    /// The output tab reports its real playback position and track duration.
    pub async fn report_progress(&self, elapsed_seconds: f64, duration_seconds: Option<f64>) {
        {
            let mut state = self.state.lock().await;
            state.elapsed_seconds = Some(elapsed_seconds);
            if let Some(duration) = duration_seconds {
                state.duration_seconds = Some(duration);
            }
        }
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
fn remove_queue_item(state: &mut BrowserState, index: usize) {
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
fn move_queue_item(state: &mut BrowserState, from: usize, to: usize) {
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
}
