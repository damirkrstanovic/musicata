//! Player registry and the MPD-backed player provider.
//!
//! A player is a controllable playback endpoint. Musicata owns the command API
//! and the live state; the MPD provider translates commands to the MPD protocol
//! and mirrors MPD's state back. Tracks are handed to MPD as absolute Musicata
//! stream URLs, so MPD needs no filesystem access. A per-player background task
//! watches MPD's `idle` channel and broadcasts state to connected controllers.
//!
//! Only MPD exists today, so the provider is concrete; the shape (registry +
//! command API + state broadcast) is provider-agnostic and a `PlayerProvider`
//! trait can be extracted when a second backend (e.g. the browser) lands.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use musicata_core::{PlaybackState, Player, PlayerCapabilities, PlayerCommand};
use musicata_storage::Database;
use tokio::sync::{Mutex, broadcast};

use crate::mpd::MpdConnection;

/// Configuration for one MPD-backed player.
#[derive(Clone, Debug)]
pub struct MpdPlayerConfig {
    pub id: String,
    pub name: String,
    pub addr: String,
}

/// Registry of configured players. Cheap to clone-share via `Arc`.
pub struct PlayerManager {
    players: BTreeMap<String, Arc<MpdPlayer>>,
    order: Vec<String>,
    public_base_url: String,
}

impl PlayerManager {
    pub fn new(configs: Vec<MpdPlayerConfig>, public_base_url: String) -> Self {
        let mut players = BTreeMap::new();
        let mut order = Vec::new();
        for config in configs {
            order.push(config.id.clone());
            players.insert(config.id.clone(), Arc::new(MpdPlayer::new(config)));
        }
        Self {
            players,
            order,
            public_base_url,
        }
    }

    /// Start each player's idle/state-broadcast task.
    pub fn start(&self, database: Database) {
        for player in self.players.values() {
            player
                .clone()
                .spawn_state_task(database.clone(), self.public_base_url.clone());
        }
    }

    pub fn descriptors(&self) -> Vec<Player> {
        self.order
            .iter()
            .filter_map(|id| self.players.get(id))
            .map(|player| player.descriptor())
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<MpdPlayer>> {
        self.players.get(id).cloned()
    }

    pub fn public_base_url(&self) -> &str {
        &self.public_base_url
    }
}

pub struct MpdPlayer {
    config: MpdPlayerConfig,
    /// Command connection, reconnected on demand. A separate connection is used
    /// for the blocking idle loop.
    connection: Mutex<Option<MpdConnection>>,
    online: AtomicBool,
    state_tx: broadcast::Sender<PlaybackState>,
}

impl MpdPlayer {
    fn new(config: MpdPlayerConfig) -> Self {
        let (state_tx, _) = broadcast::channel(16);
        Self {
            config,
            connection: Mutex::new(None),
            online: AtomicBool::new(false),
            state_tx,
        }
    }

    pub fn descriptor(&self) -> Player {
        Player {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            kind: "mpd".to_string(),
            online: self.online.load(Ordering::Relaxed),
            capabilities: PlayerCapabilities {
                seek: true,
                volume: true,
                repeat: true,
                shuffle: true,
                queue: true,
            },
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PlaybackState> {
        self.state_tx.subscribe()
    }

    /// Fetch and enrich the current playback state via the command connection.
    pub async fn state(&self, database: &Database) -> Result<PlaybackState> {
        let mut guard = self.connection.lock().await;
        let result = async {
            let connection = ensure_connected(&mut guard, &self.config.addr).await?;
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
                let connection = ensure_connected(&mut guard, &self.config.addr).await?;
                apply_command(connection, &command, database, public_base_url).await
            }
            .await
        };
        if let Err(error) = &result {
            self.online.store(false, Ordering::Relaxed);
            *self.connection.lock().await = None;
            return Err(anyhow::anyhow!(error.to_string()));
        }
        // Reflect the new state to controllers immediately.
        if let Ok(state) = self.state(database).await {
            let _ = self.state_tx.send(state);
        }
        Ok(())
    }

    /// Background task: keep a dedicated idle connection open and broadcast a
    /// fresh state snapshot whenever MPD reports a change. Reconnects with backoff.
    fn spawn_state_task(self: Arc<Self>, database: Database, public_base_url: String) {
        tokio::spawn(async move {
            loop {
                if let Err(error) = self.run_idle_loop(&database).await {
                    self.online.store(false, Ordering::Relaxed);
                    tracing::debug!(player = %self.config.id, %error, "mpd idle loop ended");
                }
                let _ = &public_base_url;
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    async fn run_idle_loop(&self, database: &Database) -> Result<()> {
        let mut idle = MpdConnection::connect(&self.config.addr).await?;
        // Push an initial snapshot so newly connected controllers are current.
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
/// Unknown ids are skipped.
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
