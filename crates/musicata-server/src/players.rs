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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use musicata_core::{
    PlaybackState, PlaybackStatus, Player, PlayerCapabilities, PlayerCommand, QueueItem,
    RepeatMode, Zone,
};
use musicata_storage::{Database, ListenKind, PlayerPlayback, PlayerRecord};
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio::task::JoinHandle;

use crate::mpd::{MpdConnection, MpdStatus};
use crate::providers::ProviderRegistry;
#[cfg(feature = "snapcast")]
use crate::snapcast::{DecodedTrack, SnapcastManager, WriterEvent, WriterMsg};

/// Stable id of the always-present local browser player.
pub const BROWSER_PLAYER_ID: &str = "browser-local";

/// A registered player's live runtime: the provider instance plus the background
/// task feeding its state broadcast.
struct PlayerEntry {
    handle: PlayerHandle,
    /// Background idle/state task, for backends that have one (MPD). The browser
    /// player is command-driven and has none.
    task: Option<JoinHandle<()>>,
    /// Background task that periodically polls MPD's position so elapsed advances on the
    /// state broadcast (MPD's idle only fires on events). MPD-only.
    poll: Option<JoinHandle<()>>,
    /// Background task that watches this player's state broadcast and records
    /// listening history. Every player has one.
    recorder: JoinHandle<()>,
}

impl Drop for PlayerEntry {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
        if let Some(poll) = &self.poll {
            poll.abort();
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
    #[cfg(feature = "snapcast")]
    Snapcast(Arc<SnapcastPlayer>),
}

impl PlayerHandle {
    pub fn is_online(&self) -> bool {
        match self {
            PlayerHandle::Mpd(player) => player.is_online(),
            PlayerHandle::Browser(player) => player.is_online(),
            #[cfg(feature = "snapcast")]
            PlayerHandle::Snapcast(player) => player.is_online(),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PlaybackState> {
        match self {
            PlayerHandle::Mpd(player) => player.subscribe(),
            PlayerHandle::Browser(player) => player.subscribe(),
            #[cfg(feature = "snapcast")]
            PlayerHandle::Snapcast(player) => player.subscribe(),
        }
    }

    /// The per-second position tick stream, if the backend emits one. Only the browser
    /// player does (MPD's elapsed rides its periodic state broadcast instead); the
    /// listen recorder uses this to see elapsed advance without the hot-path cost of
    /// re-broadcasting the full state every second.
    pub fn subscribe_progress(&self) -> Option<broadcast::Receiver<ProgressTick>> {
        match self {
            PlayerHandle::Mpd(_) => None,
            PlayerHandle::Browser(player) => Some(player.subscribe_progress()),
            #[cfg(feature = "snapcast")]
            PlayerHandle::Snapcast(player) => Some(player.subscribe_progress()),
        }
    }

    pub async fn state(&self, database: &Database) -> Result<PlaybackState> {
        match self {
            PlayerHandle::Mpd(player) => player.state(database).await,
            PlayerHandle::Browser(player) => Ok(player.snapshot().await),
            #[cfg(feature = "snapcast")]
            PlayerHandle::Snapcast(player) => Ok(player.snapshot().await),
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
            #[cfg(feature = "snapcast")]
            PlayerHandle::Snapcast(player) => player.execute(command, database, base_url).await,
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

/// ListenBrainz completion rule: a track counts as a real listen once it has played
/// past `min(duration/2, 240s)`. A track replaced before that — but after a small noise
/// floor — is a skip. Below the noise floor it's neither (just channel-flipping noise).
const LISTEN_CAP_SECONDS: f64 = 240.0;
/// Progress below this is too little to be either a listen or a skip.
const NOISE_FLOOR_SECONDS: f64 = 4.0;
/// An elapsed value this far below the running max means the track restarted (a replay
/// / repeat-one), as opposed to a small seek-back within the same play.
const BACKWARD_JUMP_SECONDS: f64 = 4.0;
/// Durations at or below this (including 0/NaN/negative) are treated as unknown, so we
/// fall back to the 4-minute cap rather than computing a near-zero half-duration.
const MIN_DURATION_SECONDS: f64 = 1.0;

/// The play time after which a track is a confirmed listen.
fn listen_threshold(duration: Option<f64>) -> f64 {
    match duration {
        Some(d) if d.is_finite() && d >= MIN_DURATION_SECONDS => (d / 2.0).min(LISTEN_CAP_SECONDS),
        _ => LISTEN_CAP_SECONDS,
    }
}

/// What a single observed tick implies for listening history.
#[derive(Debug, PartialEq, Eq)]
enum ListenAction {
    /// Nothing to record this tick.
    None,
    /// This track crossed the listen threshold — a confirmed listen.
    RecordListen(String),
    /// This (outgoing) track was abandoned past the noise floor — a skip.
    RecordSkip(String),
    /// A coarse tick both finalized the outgoing track as a skip *and* found the
    /// incoming track already past its threshold (a confirmed listen).
    RecordSkipThenListen(String, String),
}

/// The in-flight play the tracker is following.
struct TrackPlay {
    track_id: String,
    /// Running max of observed elapsed — robust to MPD's irregular sampling and to
    /// small seek-back jitter (a real listen is "we were ever past the threshold").
    max_elapsed: f64,
    duration: Option<f64>,
    /// True once a `RecordListen` has been emitted for this play (never re-fire).
    listen_recorded: bool,
}

/// Decides when a track counts as a listen vs a skip from a stream of playback ticks.
/// Pure (no clock, no DB) so it is exhaustively unit-tested; the recorder stamps the
/// event time when it writes.
#[derive(Default)]
struct ListenTracker {
    current: Option<TrackPlay>,
}

impl ListenTracker {
    /// Finalize the in-flight play: returns its id if it should be recorded as a skip
    /// (past the floor and not already a listen), clearing it either way.
    fn finalize(&mut self) -> Option<String> {
        let prev = self.current.take()?;
        (!prev.listen_recorded && prev.max_elapsed >= NOISE_FLOOR_SECONDS).then_some(prev.track_id)
    }

    /// Fold one tick (status + the active library track's id/elapsed/duration) into the
    /// state machine and report what to record.
    fn observe(
        &mut self,
        status: PlaybackStatus,
        track_id: Option<&str>,
        elapsed: Option<f64>,
        duration: Option<f64>,
    ) -> ListenAction {
        // Pause holds the in-flight play untouched (resume continues it).
        if status == PlaybackStatus::Paused {
            return ListenAction::None;
        }
        // Stopped, or playing something with no library track id (e.g. radio): the
        // in-flight play is over — finalize it.
        let Some(id) = track_id.filter(|_| status == PlaybackStatus::Playing) else {
            return match self.finalize() {
                Some(skip) => ListenAction::RecordSkip(skip),
                None => ListenAction::None,
            };
        };

        let el = elapsed.unwrap_or(0.0).max(0.0);
        let restart = match &self.current {
            Some(p) if p.track_id == id => {
                // Same track: a large backward jump means a replay (restarted near zero,
                // or restarted after a completed listen) — a *new* play. A smaller / mid
                // seek-back stays the same play (don't lower max_elapsed).
                el < p.max_elapsed - BACKWARD_JUMP_SECONDS
                    && (el < BACKWARD_JUMP_SECONDS || p.listen_recorded)
            }
            _ => true, // different track, or nothing in flight
        };

        let mut skip_out = None;
        if restart {
            skip_out = self.finalize();
            self.current = Some(TrackPlay {
                track_id: id.to_string(),
                max_elapsed: el,
                duration,
                listen_recorded: false,
            });
        } else if let Some(p) = self.current.as_mut() {
            p.max_elapsed = p.max_elapsed.max(el);
            if duration.is_some() {
                p.duration = duration;
            }
        }

        let listen_now = {
            let p = self.current.as_mut().expect("current set above");
            if !p.listen_recorded && p.max_elapsed >= listen_threshold(p.duration) {
                p.listen_recorded = true;
                true
            } else {
                false
            }
        };

        match (skip_out, listen_now) {
            (None, false) => ListenAction::None,
            (None, true) => ListenAction::RecordListen(id.to_string()),
            (Some(skip), false) => ListenAction::RecordSkip(skip),
            (Some(skip), true) => ListenAction::RecordSkipThenListen(skip, id.to_string()),
        }
    }
}

/// Persist whatever a tick decided. The event time is stamped here (`now_unix`), so a
/// listen confirmed mid-track is timestamped when it crossed the threshold.
async fn record_action(database: &Database, player_id: &str, action: ListenAction) {
    let writes: Vec<(&str, ListenKind)> = match &action {
        ListenAction::None => return,
        ListenAction::RecordListen(id) => vec![(id.as_str(), ListenKind::Played)],
        ListenAction::RecordSkip(id) => vec![(id.as_str(), ListenKind::Skipped)],
        ListenAction::RecordSkipThenListen(skip, listen) => vec![
            (skip.as_str(), ListenKind::Skipped),
            (listen.as_str(), ListenKind::Played),
        ],
    };
    let now = now_unix();
    for (track_id, kind) in writes {
        if let Err(error) = database.record_listen(track_id, player_id, now, kind).await {
            tracing::warn!(%player_id, %track_id, ?kind, %error, "failed to record listen");
        }
    }
}

/// Await the next progress tick, or never resolve when there is no progress channel
/// (MPD/zone) — so the recorder's `select!` simply falls through to state frames.
async fn next_progress(progress: &mut Option<broadcast::Receiver<ProgressTick>>) -> ProgressTick {
    loop {
        match progress {
            Some(rx) => match rx.recv().await {
                Ok(tick) => return tick,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => *progress = None,
            },
            None => std::future::pending::<()>().await,
        }
    }
}

/// Spawns the per-player task that records listening history under the ListenBrainz
/// completion rule. It folds two sources into the tracker: full `PlaybackState` frames
/// (track changes, status, and — for MPD — elapsed) and, when present, the browser's
/// lightweight per-second `ProgressTick`s (which carry the advancing elapsed the full
/// state deliberately doesn't re-broadcast every second).
fn spawn_listen_recorder(
    player_id: String,
    mut states: broadcast::Receiver<PlaybackState>,
    mut progress: Option<broadcast::Receiver<ProgressTick>>,
    database: Database,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tracker = ListenTracker::default();
        // The last library track id / status seen on a full state frame, used to give a
        // bare progress tick its context.
        let mut last_track: Option<String> = None;
        let mut last_status = PlaybackStatus::Stopped;
        loop {
            let action = tokio::select! {
                state = states.recv() => match state {
                    Ok(state) => {
                        let track_id = state.now_playing.and_then(|n| n.track_id);
                        last_track = track_id.clone();
                        last_status = state.status;
                        tracker.observe(
                            state.status,
                            track_id.as_deref(),
                            state.elapsed_seconds,
                            state.duration_seconds,
                        )
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                tick = next_progress(&mut progress) => {
                    // Progress only flows while the output is rendering audio, so treat it
                    // as Playing on the last-known track.
                    if last_status != PlaybackStatus::Playing {
                        continue;
                    }
                    tracker.observe(
                        PlaybackStatus::Playing,
                        last_track.as_deref(),
                        Some(tick.elapsed_seconds),
                        tick.duration_seconds,
                    )
                }
            };
            record_action(&database, &player_id, action).await;
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

/// Stable id of the always-present Snapcast multi-room player (created when the feature is
/// enabled in settings, like the browser player).
#[cfg(feature = "snapcast")]
pub const SNAPCAST_PLAYER_ID: &str = "snapcast-local";

/// Registry of registered players and zones, backed by the database.
pub struct PlayerManager {
    database: Database,
    public_base_url: String,
    /// Needed by the Snapcast player to read library audio for server-side decode.
    #[cfg_attr(not(feature = "snapcast"), allow(dead_code))]
    providers: Arc<RwLock<ProviderRegistry>>,
    players: RwLock<BTreeMap<String, PlayerEntry>>,
    zones: RwLock<BTreeMap<String, ZoneEntry>>,
    /// The managed snapserver + FIFO, present once Snapcast is enabled in settings.
    #[cfg(feature = "snapcast")]
    snapcast: RwLock<Option<Arc<SnapcastManager>>>,
}

impl PlayerManager {
    /// Build the manager and bring up a runtime entry for every persisted player.
    pub async fn load(
        database: Database,
        public_base_url: String,
        providers: Arc<RwLock<ProviderRegistry>>,
    ) -> Result<Arc<Self>> {
        let manager = Arc::new(Self {
            database,
            public_base_url,
            providers,
            players: RwLock::new(BTreeMap::new()),
            zones: RwLock::new(BTreeMap::new()),
            #[cfg(feature = "snapcast")]
            snapcast: RwLock::new(None),
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
        let recorder = spawn_listen_recorder(
            zone.id.clone(),
            player.subscribe(),
            None,
            self.database.clone(),
        );
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

    /// Activate the Snapcast subsystem: store the managed snapserver, ensure the
    /// always-present `snapcast-local` player exists, and bring it up. Idempotent.
    #[cfg(feature = "snapcast")]
    pub async fn enable_snapcast(&self, manager: Arc<SnapcastManager>) -> Result<()> {
        *self.snapcast.write().await = Some(manager);
        if self
            .database
            .player_record(SNAPCAST_PLAYER_ID)
            .await?
            .is_none()
        {
            self.database
                .upsert_player(&PlayerRecord {
                    id: SNAPCAST_PLAYER_ID.to_string(),
                    kind: "snapcast".to_string(),
                    address: "local".to_string(),
                    name: "Multi-room (Snapcast)".to_string(),
                    zone_id: None,
                })
                .await?;
        }
        if !self.players.read().await.contains_key(SNAPCAST_PLAYER_ID) {
            let record = self.require_record(SNAPCAST_PLAYER_ID).await?;
            self.bring_up(&record).await;
        }
        Ok(())
    }

    /// The managed snapserver, once enabled — used by the control/admin layer.
    #[cfg(feature = "snapcast")]
    pub async fn snapcast_manager(&self) -> Option<Arc<SnapcastManager>> {
        self.snapcast.read().await.clone()
    }

    /// Bring up the runtime entry for a persisted player record.
    async fn bring_up(&self, record: &PlayerRecord) {
        let (handle, task, poll) = match record.kind.as_str() {
            "browser" => {
                let player = Arc::new(BrowserPlayer::new(self.database.clone(), record.id.clone()));
                // Reload any queue persisted before the last shutdown so it survives a
                // server restart, not just a page refresh.
                player.restore().await;
                (PlayerHandle::Browser(player), None, None)
            }
            #[cfg(feature = "snapcast")]
            "snapcast" => {
                // Only brought up once the managed snapserver is running (set by
                // `enable_snapcast`); without it there's no FIFO to write to.
                let Some(manager) = self.snapcast.read().await.clone() else {
                    tracing::warn!(player = %record.id, "snapcast player skipped: subsystem not enabled");
                    return;
                };
                let player = SnapcastPlayer::new(
                    record.id.clone(),
                    self.database.clone(),
                    self.providers.clone(),
                    manager,
                );
                player.restore().await;
                let task = player.clone().spawn_control_task();
                (PlayerHandle::Snapcast(player), Some(task), None)
            }
            _ => {
                let player = Arc::new(MpdPlayer::new(
                    record.id.clone(),
                    record.address.clone(),
                    self.database.clone(),
                    self.public_base_url.clone(),
                ));
                // Restore the server-owned queue (server wins) and push it to MPD before
                // the idle loop starts; with no persisted queue, adopt MPD's current one.
                player.restore().await;
                let task = player.clone().spawn_state_task(self.database.clone());
                let poll = player.clone().spawn_position_poll();
                (PlayerHandle::Mpd(player), Some(task), Some(poll))
            }
        };
        let recorder = spawn_listen_recorder(
            record.id.clone(),
            handle.subscribe(),
            handle.subscribe_progress(),
            self.database.clone(),
        );
        self.players.write().await.insert(
            record.id.clone(),
            PlayerEntry {
                handle,
                task,
                poll,
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

/// An MPD-backed player whose **queue is server-owned**: Musicata owns the queue
/// content and order (persisted to the `player_queue` tables, like the browser
/// player), and reconciles MPD to mirror it; **MPD owns the playback cursor** (which
/// index is playing, elapsed, and its native shuffle/repeat/auto-advance), which the
/// server reads back. On startup the server queue wins (it's pushed onto MPD, paused);
/// an external client editing MPD's queue is re-asserted over (server always wins).
pub struct MpdPlayer {
    id: String,
    addr: String,
    database: Database,
    public_base_url: String,
    /// Command connection, reconnected on demand. A separate connection is used
    /// for the blocking idle loop.
    connection: Mutex<Option<MpdConnection>>,
    online: AtomicBool,
    /// The server-owned queue. MPD is reconciled to match it; the cursor fields are
    /// refreshed from MPD.
    state: Mutex<QueueState>,
    state_tx: broadcast::Sender<PlaybackState>,
    /// MPD's queue version after our last reconcile. An idle `playlist` event with a
    /// different version is an external edit we re-assert over (queue version bumps
    /// only on queue edits, not on seeks/song changes).
    expected_playlist_version: AtomicU64,
    /// Set after a restore — MPD has the queue loaded but its cursor isn't established,
    /// so the first `Play` resumes at the saved position rather than MPD's index 0.
    restored: AtomicBool,
}

impl MpdPlayer {
    fn new(id: String, addr: String, database: Database, public_base_url: String) -> Self {
        let (state_tx, _) = broadcast::channel(16);
        Self {
            id,
            addr,
            database,
            public_base_url,
            connection: Mutex::new(None),
            online: AtomicBool::new(false),
            state: Mutex::new(QueueState::default()),
            state_tx,
            expected_playlist_version: AtomicU64::new(0),
            restored: AtomicBool::new(false),
        }
    }

    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PlaybackState> {
        self.state_tx.subscribe()
    }

    /// Build the controller-facing state: the server-owned queue plus the
    /// MPD-derived cursor (now-playing is the queue item at the cursor position).
    async fn snapshot(&self) -> PlaybackState {
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

    /// Persist the server-owned queue (failures logged, never propagated).
    async fn persist(&self, persist: QueuePersist) {
        let result = match &persist {
            QueuePersist::Playback(playback) => {
                self.database.save_player_playback(&self.id, playback).await
            }
            QueuePersist::Queue(playback, items) => {
                self.database
                    .save_player_queue(&self.id, playback, items)
                    .await
            }
        };
        if let Err(error) = result {
            tracing::warn!(player = %self.id, %error, "failed to persist mpd queue");
        }
    }

    /// Fold MPD's reported cursor (status + current song) into the server queue state:
    /// status/position/elapsed/volume/options come from MPD; the queue stays
    /// server-owned. Records MPD's queue version so our own edits aren't mistaken for
    /// an external one.
    async fn apply_cursor(&self, status: MpdStatus) {
        let mut state = self.state.lock().await;
        let cursor = status.state;
        state.status = cursor.status;
        state.position = cursor
            .queue_position
            .filter(|&index| index < state.queue.len());
        state.elapsed_seconds = cursor.elapsed_seconds;
        state.duration_seconds = cursor.duration_seconds;
        if cursor.volume.is_some() {
            state.volume = cursor.volume;
        }
        state.repeat = cursor.repeat;
        state.shuffle = cursor.shuffle;
        self.expected_playlist_version
            .store(status.playlist_version, Ordering::Relaxed);
    }

    /// Restore the persisted server queue (server wins) and push it to MPD paused. If
    /// no queue was persisted, adopt MPD's current queue so the UI matches what MPD is
    /// already playing. Best-effort: an unreachable MPD just leaves the (restored)
    /// server queue to be re-pushed when it returns.
    async fn restore(&self) {
        match self.database.load_player_queue(&self.id).await {
            Ok(Some(snapshot)) => {
                {
                    let mut state = self.state.lock().await;
                    let playback = snapshot.playback;
                    state.queue = snapshot.items;
                    state.position = playback.position.filter(|&i| i < state.queue.len());
                    // A queue we can't point into can't be paused/playing.
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
                self.restored.store(true, Ordering::Relaxed);
                // Push the restored queue onto MPD without playing (resumed on first Play).
                let uris = {
                    let state = self.state.lock().await;
                    queue_to_mpd_uris(&state.queue, &self.public_base_url)
                };
                let mut guard = self.connection.lock().await;
                if let Ok(connection) = ensure_connected(&mut guard, &self.addr).await {
                    if connection.load_queue(&uris).await.is_ok() {
                        self.online.store(true, Ordering::Relaxed);
                        if let Ok(status) = connection.read_status().await {
                            self.expected_playlist_version
                                .store(status.playlist_version, Ordering::Relaxed);
                        }
                    }
                }
            }
            Ok(None) => {
                // No server queue yet: adopt MPD's current queue (mirror its state — it
                // may already be playing on its own; don't interrupt it).
                let mut guard = self.connection.lock().await;
                let read = async {
                    let connection = ensure_connected(&mut guard, &self.addr).await?;
                    let mut adopted = connection.playback_state().await?;
                    let version = connection.read_status().await?.playlist_version;
                    Ok::<_, anyhow::Error>((std::mem::take(&mut adopted), version))
                }
                .await;
                drop(guard);
                if let Ok((mut adopted, version)) = read {
                    enrich_state(&mut adopted, &self.database).await;
                    let mut state = self.state.lock().await;
                    state.status = adopted.status;
                    state.position = adopted.queue_position.filter(|&i| i < adopted.queue.len());
                    state.elapsed_seconds = adopted.elapsed_seconds;
                    state.duration_seconds = adopted.duration_seconds;
                    state.volume = adopted.volume;
                    state.repeat = adopted.repeat;
                    state.shuffle = adopted.shuffle;
                    state.queue = adopted.queue;
                    self.online.store(true, Ordering::Relaxed);
                    self.expected_playlist_version
                        .store(version, Ordering::Relaxed);
                }
            }
            Err(error) => {
                tracing::warn!(player = %self.id, %error, "failed to load persisted mpd queue");
            }
        }
    }

    /// Serve the current state: refresh the cursor from MPD if reachable, but the
    /// server queue is always authoritative (so a player offline still shows its queue).
    pub async fn state(&self, _database: &Database) -> Result<PlaybackState> {
        let status = {
            let mut guard = self.connection.lock().await;
            let result = async {
                let connection = ensure_connected(&mut guard, &self.addr).await?;
                connection.read_status().await
            }
            .await;
            match result {
                Ok(status) => {
                    self.online.store(true, Ordering::Relaxed);
                    Some(status)
                }
                Err(_) => {
                    self.online.store(false, Ordering::Relaxed);
                    *guard = None;
                    None
                }
            }
        };
        if let Some(status) = status {
            self.apply_cursor(status).await;
        }
        Ok(self.snapshot().await)
    }

    /// Apply a command: mutate the server queue for content commands, reconcile MPD,
    /// read its cursor back, persist, and broadcast.
    pub async fn execute(
        &self,
        command: PlayerCommand,
        database: &Database,
        public_base_url: &str,
    ) -> Result<()> {
        let mutates_queue = command_mutates_queue(&command);
        if mutates_queue {
            let mut state = self.state.lock().await;
            apply_to_queue_state(&mut state, command.clone(), database).await?;
        }

        // First Play after a restore resumes at the saved position instead of MPD's 0.
        let resume =
            if matches!(command, PlayerCommand::Play) && self.restored.load(Ordering::Relaxed) {
                let state = self.state.lock().await;
                Some((state.position, state.elapsed_seconds))
            } else {
                None
            };

        let status = {
            let mut guard = self.connection.lock().await;
            let result = async {
                let connection = ensure_connected(&mut guard, &self.addr).await?;
                match resume {
                    Some((Some(position), elapsed)) => {
                        connection.play_index(position).await?;
                        if let Some(seconds) = elapsed.filter(|value| *value > 0.0) {
                            connection.seek(seconds).await?;
                        }
                    }
                    Some((None, _)) => connection.play().await?,
                    None => apply_command(connection, &command, database, public_base_url).await?,
                }
                connection.read_status().await
            }
            .await;
            match result {
                Ok(status) => {
                    self.online.store(true, Ordering::Relaxed);
                    Some(status)
                }
                Err(error) => {
                    // The server queue is authoritative, so an unreachable MPD doesn't
                    // fail the command: we still persist + broadcast below, leaving the
                    // track queued and controllers updated. Drop the dead connection so
                    // the next call (or the idle loop) reconnects and re-asserts; reuse
                    // the held guard — re-locking the same mutex here would deadlock.
                    self.online.store(false, Ordering::Relaxed);
                    *guard = None;
                    tracing::debug!(player = %self.id, %error, "mpd command failed; player offline");
                    None
                }
            }
        };

        let online = status.is_some();
        // Spend the restore-resume only once the command actually reached MPD — an
        // offline command keeps the resume so it still applies when MPD reconnects.
        if online
            && (resume.is_some()
                || matches!(
                    command,
                    PlayerCommand::Play
                        | PlayerCommand::PlayQueueIndex { .. }
                        | PlayerCommand::PlayTracks { .. }
                        | PlayerCommand::PlayStream { .. }
                        | PlayerCommand::Next
                        | PlayerCommand::Previous
                ))
        {
            self.restored.store(false, Ordering::Relaxed);
        }

        if let Some(status) = status {
            self.apply_cursor(status).await;
        }
        let persist = {
            let state = self.state.lock().await;
            if mutates_queue {
                QueuePersist::Queue(state.playback(), state.queue.clone())
            } else {
                QueuePersist::Playback(state.playback())
            }
        };
        self.persist(persist).await;
        self.broadcast().await;
        Ok(())
    }

    /// Re-assert the server queue onto MPD (load it and resume the server's position if
    /// it was playing) after an external client edited MPD's queue.
    async fn reassert(&self, connection: &mut MpdConnection) -> Result<()> {
        let (uris, position, status) = {
            let state = self.state.lock().await;
            (
                queue_to_mpd_uris(&state.queue, &self.public_base_url),
                state.position,
                state.status,
            )
        };
        connection.load_queue(&uris).await?;
        if status == PlaybackStatus::Playing
            && let Some(index) = position
        {
            connection.play_index(index).await?;
        }
        Ok(())
    }

    /// Background task: keep a dedicated idle connection open and broadcast a fresh
    /// snapshot whenever MPD reports a change. Reconnects with backoff.
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

    /// Background task: while MPD is playing, periodically refresh its cursor and
    /// re-broadcast, so elapsed advances between idle events (MPD's `idle` only fires on
    /// state *changes*, not every second). This is what lets the listen recorder apply
    /// the completion rule to MPD — its only elapsed source is the state broadcast. Uses
    /// the command connection (not the dedicated idle connection), so it never interferes
    /// with the idle loop; it broadcasts only (the throttled persist stays on the idle
    /// loop and commands).
    fn spawn_position_poll(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut poll = tokio::time::interval(Duration::from_secs(MPD_POLL_SECS));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                poll.tick().await;
                if !self.online.load(Ordering::Relaxed) {
                    continue;
                }
                // Only poll while playing — a paused/stopped MPD has nothing to advance,
                // and we don't want to spam the broadcast (re-sending the queue) at rest.
                if !matches!(self.state.lock().await.status, PlaybackStatus::Playing) {
                    continue;
                }
                let status = {
                    let mut guard = self.connection.lock().await;
                    match ensure_connected(&mut guard, &self.addr).await {
                        Ok(connection) => connection.read_status().await.ok(),
                        Err(_) => None,
                    }
                };
                if let Some(status) = status.filter(|s| s.state.status == PlaybackStatus::Playing) {
                    self.apply_cursor(status).await;
                    self.broadcast().await;
                }
            }
        })
    }

    async fn run_idle_loop(&self, database: &Database) -> Result<()> {
        let mut idle = MpdConnection::connect(&self.addr).await?;
        self.online.store(true, Ordering::Relaxed);
        // MPD just (re)connected — it may have restarted with an empty queue while we
        // were offline. Re-push the authoritative server queue and, if we were playing,
        // resume at the saved position (`reassert` does both), so playback continues
        // without the user having to click again. At startup `restore` has set the
        // status to Paused, so this won't surprise-autoplay then.
        {
            let mut guard = self.connection.lock().await;
            if let Ok(connection) = ensure_connected(&mut guard, &self.addr).await {
                if let Err(error) = self.reassert(connection).await {
                    tracing::debug!(player = %self.id, %error, "reassert on reconnect failed");
                } else if let Ok(status) = connection.read_status().await {
                    self.expected_playlist_version
                        .store(status.playlist_version, Ordering::Relaxed);
                }
            }
        }
        if let Ok(state) = self.state(database).await {
            let _ = self.state_tx.send(state);
        }
        loop {
            let subsystems = idle.idle().await?;

            // Read the cursor (brief command-connection lock).
            let status = {
                let mut guard = self.connection.lock().await;
                let connection = ensure_connected(&mut guard, &self.addr).await?;
                connection.read_status().await?
            };

            // An unexpected queue-version bump on a `playlist` event = an external edit.
            let drifted = subsystems.iter().any(|name| name == "playlist")
                && status.playlist_version
                    != self.expected_playlist_version.load(Ordering::Relaxed);

            let status = if drifted {
                let refreshed = {
                    let mut guard = self.connection.lock().await;
                    let connection = ensure_connected(&mut guard, &self.addr).await?;
                    self.reassert(connection).await?;
                    connection.read_status().await?
                };
                refreshed
            } else {
                status
            };

            self.apply_cursor(status).await;
            self.broadcast().await;
            // Persist the cursor (cheap row); the queue itself is unchanged here.
            let playback = { self.state.lock().await.playback() };
            self.persist(QueuePersist::Playback(playback)).await;
        }
    }
}

/// Turn the server queue into MPD URIs: library tracks get the public base URL
/// prepended (MPD fetches them over HTTP); external streams (no track id, e.g. a
/// radio station) are already absolute.
fn queue_to_mpd_uris(items: &[QueueItem], public_base_url: &str) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            if item.track_id.is_some() {
                format!("{public_base_url}{}", item.stream_url)
            } else {
                item.stream_url.clone()
            }
        })
        .collect()
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

/// How often the MPD position poll refreshes elapsed while playing. Frequent enough for
/// the listen recorder's completion rule (and a live-ish seek bar), infrequent enough to
/// avoid spamming the state broadcast.
const MPD_POLL_SECS: u64 = 5;

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
            // Browser members are driven by the zone broadcast; server-decoded members
            // (MPD, Snapcast) need an out-of-band command to mirror the zone queue.
            // (Forwarding to the single browser player would give it a second, competing
            // queue.)
            let driven = match member {
                PlayerHandle::Mpd(_) => true,
                #[cfg(feature = "snapcast")]
                PlayerHandle::Snapcast(_) => true,
                _ => false,
            };
            if !driven {
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
                ..Default::default()
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
            let loudness = database.track_loudness(&track.id).await?;
            items.push(QueueItem {
                track_id: Some(track.id),
                title: track.title,
                artist: track.artist_name,
                album: track.album_title,
                stream_url: track.stream_url,
                artwork_url,
                integrated_loudness_lufs: loudness.map(|(lufs, _)| lufs),
                true_peak_dbtp: loudness.map(|(_, peak)| peak),
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

/// The queue index to play *after* `position`, honoring repeat — used to preload the next
/// track for gapless Snapcast output. `None` means "stop after this one".
#[cfg(feature = "snapcast")]
fn next_index(position: Option<usize>, len: usize, repeat: RepeatMode) -> Option<usize> {
    let pos = position?;
    if len == 0 {
        return None;
    }
    match repeat {
        RepeatMode::One => Some(pos),
        RepeatMode::All => Some((pos + 1) % len),
        RepeatMode::Off => {
            let next = pos + 1;
            (next < len).then_some(next)
        }
    }
}

/// A Snapcast-backed player. Like [`MpdPlayer`] its queue is server-owned (persisted to the
/// `player_queue` tables, reconciled command-by-command), but instead of speaking a protocol
/// to an external player it **decodes the queue to PCM server-side** and streams it into the
/// FIFO a managed `snapserver` reads — so every assigned snapclient plays it sample-accurate
/// in sync. The decode loop *is* the playback cursor: play/pause/seek/skip reposition it. See
/// `crate::snapcast` and `docs/snapcast.md`.
#[cfg(feature = "snapcast")]
pub struct SnapcastPlayer {
    id: String,
    database: Database,
    providers: Arc<RwLock<ProviderRegistry>>,
    /// Kept so the snapserver + FIFO outlive the player; also the control handle home.
    #[allow(dead_code)]
    manager: Arc<SnapcastManager>,
    sample_rate: u32,
    state: Mutex<QueueState>,
    state_tx: broadcast::Sender<PlaybackState>,
    progress_tx: broadcast::Sender<ProgressTick>,
    last_progress_persist: AtomicI64,
    /// Commands to the FIFO writer thread (std channel: the thread blocks on it while idle).
    writer_tx: std::sync::mpsc::Sender<WriterMsg>,
    /// Writer→player events (advance/drain). Taken once by the control task.
    events_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<WriterEvent>>>,
    /// What the writer currently has loaded, so reconcile avoids re-decoding.
    loaded: Mutex<LoadedTrack>,
}

/// What the writer thread is currently rendering / has preloaded.
#[cfg(feature = "snapcast")]
#[derive(Default)]
struct LoadedTrack {
    track_id: Option<String>,
    position: Option<usize>,
    next_track_id: Option<String>,
}

#[cfg(feature = "snapcast")]
impl SnapcastPlayer {
    /// Build the player and spawn its FIFO writer thread (which blocks opening the FIFO
    /// until snapserver is reading). Call [`restore`](Self::restore) then
    /// [`spawn_control_task`](Self::spawn_control_task) to bring it fully up.
    fn new(
        id: String,
        database: Database,
        providers: Arc<RwLock<ProviderRegistry>>,
        manager: Arc<SnapcastManager>,
    ) -> Arc<Self> {
        let (state_tx, _) = broadcast::channel(32);
        let (progress_tx, _) = broadcast::channel(32);
        let (writer_tx, writer_rx) = std::sync::mpsc::channel();
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let sample_rate = manager.sample_rate();
        let fifo = manager.fifo_path();
        std::thread::Builder::new()
            .name(format!("snapcast-writer-{id}"))
            .spawn(move || crate::snapcast::run_writer(fifo, writer_rx, events_tx))
            .expect("spawn snapcast writer thread");
        Arc::new(Self {
            id,
            database,
            providers,
            manager,
            sample_rate,
            state: Mutex::new(QueueState::default()),
            state_tx,
            progress_tx,
            last_progress_persist: AtomicI64::new(0),
            writer_tx,
            events_rx: std::sync::Mutex::new(Some(events_rx)),
            loaded: Mutex::new(LoadedTrack::default()),
        })
    }

    pub fn is_online(&self) -> bool {
        true
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

    /// Reload the queue persisted before the last shutdown (restored paused — no audio is
    /// rendering at startup), exactly like the browser and zone players.
    async fn restore(&self) {
        let snapshot = match self.database.load_player_queue(&self.id).await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(player = %self.id, %error, "snapcast: failed to load persisted queue");
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
                self.database.save_player_playback(&self.id, playback).await
            }
            QueuePersist::Queue(playback, items) => {
                self.database
                    .save_player_queue(&self.id, playback, items)
                    .await
            }
        };
        if let Err(error) = result {
            tracing::warn!(player = %self.id, %error, "snapcast: failed to persist queue");
        }
    }

    async fn persist_playback(&self) {
        let playback = self.state.lock().await.playback();
        self.persist(QueuePersist::Playback(playback)).await;
    }

    pub async fn execute(
        &self,
        command: PlayerCommand,
        database: &Database,
        _base_url: &str,
    ) -> Result<()> {
        let mutates_queue = command_mutates_queue(&command);
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
        self.reconcile(&command).await;
        Ok(())
    }

    /// Drive the FIFO writer to match the queue state after a command: pause/stop, resume,
    /// seek, or decode+load a new current track (and preload the next for gapless output).
    async fn reconcile(&self, command: &PlayerCommand) {
        if let PlayerCommand::SetVolume { volume } = command {
            let _ = self.writer_tx.send(WriterMsg::SetVolume(*volume));
        }
        let (status, position, item, elapsed, next_item) = {
            let state = self.state.lock().await;
            let position = state.position;
            let item = position.and_then(|index| state.queue.get(index).cloned());
            let elapsed = state.elapsed_seconds.unwrap_or(0.0);
            let next = next_index(position, state.queue.len(), state.repeat)
                .and_then(|index| state.queue.get(index).cloned());
            (state.status, position, item, elapsed, next)
        };

        match status {
            PlaybackStatus::Stopped => {
                let _ = self.writer_tx.send(WriterMsg::Stop);
                *self.loaded.lock().await = LoadedTrack::default();
                return;
            }
            PlaybackStatus::Paused => {
                let _ = self.writer_tx.send(WriterMsg::SetPlaying(false));
                return;
            }
            PlaybackStatus::Playing => {}
        }

        let Some(item) = item else {
            let _ = self.writer_tx.send(WriterMsg::Stop);
            *self.loaded.lock().await = LoadedTrack::default();
            return;
        };
        let Some(track_id) = item.track_id.clone() else {
            // Snapcast decodes library files; it can't play a bare remote stream URL.
            tracing::warn!(player = %self.id, "snapcast: skipping non-library stream item");
            let _ = self.writer_tx.send(WriterMsg::SetPlaying(false));
            return;
        };

        let same = {
            let loaded = self.loaded.lock().await;
            loaded.track_id.as_deref() == Some(track_id.as_str()) && loaded.position == position
        };
        let is_seek = matches!(command, PlayerCommand::Seek { .. });
        if same {
            if is_seek {
                let frame = (elapsed * self.sample_rate as f64) as usize;
                let _ = self.writer_tx.send(WriterMsg::Seek { frame });
            }
            let _ = self.writer_tx.send(WriterMsg::SetPlaying(true));
            self.preload(next_item).await;
            return;
        }

        // A different track is now current — decode and load it.
        let decoded = match self.decode(&track_id).await {
            Ok(decoded) => decoded,
            Err(error) => {
                tracing::warn!(player = %self.id, track = %track_id, %error, "snapcast: decode failed");
                return;
            }
        };
        let duration = decoded.frames() as f64 / self.sample_rate as f64;
        let gain = leveling_gain(&item);
        let frame = (elapsed * self.sample_rate as f64) as usize;
        let _ = self.writer_tx.send(WriterMsg::Load {
            track: decoded,
            start_frame: frame,
            gain,
        });
        let _ = self.writer_tx.send(WriterMsg::SetPlaying(true));
        {
            let mut loaded = self.loaded.lock().await;
            loaded.track_id = Some(track_id);
            loaded.position = position;
            loaded.next_track_id = None;
        }
        {
            let mut state = self.state.lock().await;
            state.duration_seconds = Some(duration);
        }
        self.broadcast().await;
        self.preload(next_item).await;
    }

    /// Decode the next queue item and hand it to the writer for gapless continuation —
    /// unless it is already preloaded (so this is cheap to call on every command).
    async fn preload(&self, next_item: Option<QueueItem>) {
        let next_track_id = next_item.as_ref().and_then(|item| item.track_id.clone());
        {
            let mut loaded = self.loaded.lock().await;
            if loaded.next_track_id == next_track_id {
                return;
            }
            loaded.next_track_id = next_track_id.clone();
        }
        let (Some(track_id), Some(item)) = (next_track_id, next_item) else {
            return;
        };
        match self.decode(&track_id).await {
            Ok(decoded) => {
                let _ = self.writer_tx.send(WriterMsg::Preload {
                    track: decoded,
                    gain: leveling_gain(&item),
                });
            }
            Err(error) => {
                tracing::debug!(player = %self.id, track = %track_id, %error, "snapcast: preload decode failed");
            }
        }
    }

    async fn decode(&self, track_id: &str) -> Result<Arc<DecodedTrack>, String> {
        crate::snapcast::decode_queue_item(
            &self.database,
            &self.providers,
            track_id,
            self.sample_rate,
        )
        .await
        .map(Arc::new)
    }

    /// Spawn the control task: it advances the queue on writer drain/advance events and
    /// ticks the elapsed position while playing. Returns its handle (aborted on drop).
    fn spawn_control_task(self: Arc<Self>) -> JoinHandle<()> {
        let mut events = self
            .events_rx
            .lock()
            .expect("events lock")
            .take()
            .expect("control task spawned once");
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Some(WriterEvent::Advanced) => self.on_advanced().await,
                        Some(WriterEvent::Drained) => self.on_drained().await,
                        None => break,
                    },
                    _ = ticker.tick() => self.on_tick().await,
                }
            }
        })
    }

    /// The writer rolled gaplessly into the preloaded next track — advance our cursor.
    async fn on_advanced(&self) {
        let (new_position, new_track_id, next_item) = {
            let mut state = self.state.lock().await;
            let new_position = next_index(state.position, state.queue.len(), state.repeat);
            state.position = new_position;
            state.elapsed_seconds = Some(0.0);
            state.duration_seconds = None;
            let new_track_id = new_position
                .and_then(|index| state.queue.get(index))
                .and_then(|item| item.track_id.clone());
            let next = next_index(new_position, state.queue.len(), state.repeat)
                .and_then(|index| state.queue.get(index).cloned());
            (new_position, new_track_id, next)
        };
        {
            let mut loaded = self.loaded.lock().await;
            loaded.position = new_position;
            loaded.track_id = new_track_id.clone();
            loaded.next_track_id = None;
        }
        // Best-effort: set the seek-bar duration for the new current track.
        if let Some(track_id) = &new_track_id {
            if let Ok(Some(track)) = self.database.track(track_id).await {
                let mut state = self.state.lock().await;
                state.duration_seconds = track.duration_seconds;
            }
        }
        self.broadcast().await;
        self.persist_playback().await;
        self.preload(next_item).await;
    }

    /// The queue drained with nothing preloaded — stop.
    async fn on_drained(&self) {
        {
            let mut state = self.state.lock().await;
            state.status = PlaybackStatus::Stopped;
            state.elapsed_seconds = Some(0.0);
        }
        *self.loaded.lock().await = LoadedTrack::default();
        self.broadcast().await;
        self.persist_playback().await;
    }

    /// Advance the displayed elapsed position once per second while playing, mirroring the
    /// MPD position poll (the audio truth is snapserver's; this is the seek bar + the
    /// listen recorder's view).
    async fn on_tick(&self) {
        let (elapsed, duration, playback) = {
            let mut state = self.state.lock().await;
            if state.status != PlaybackStatus::Playing {
                return;
            }
            let mut elapsed = state.elapsed_seconds.unwrap_or(0.0) + 1.0;
            if let Some(duration) = state.duration_seconds {
                if elapsed > duration {
                    elapsed = duration;
                }
            }
            state.elapsed_seconds = Some(elapsed);
            (elapsed, state.duration_seconds, state.playback())
        };
        let _ = self.progress_tx.send(ProgressTick {
            elapsed_seconds: elapsed,
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

#[cfg(feature = "snapcast")]
impl Drop for SnapcastPlayer {
    fn drop(&mut self) {
        // Stop the writer thread (best-effort; it may be blocked writing the FIFO).
        let _ = self.writer_tx.send(WriterMsg::Shutdown);
    }
}

/// Per-track linear gain for EBU R128 volume leveling on the Snapcast path — the
/// server-side equivalent of the browser's Track-mode leveling (see `docs/loudness.md`).
/// Targets −18 LUFS, clamped so the result never exceeds 0 dBFS true peak, and bounded to
/// ±12 dB. `1.0` (unchanged) when the track has no measured loudness.
#[cfg(feature = "snapcast")]
fn leveling_gain(item: &QueueItem) -> f32 {
    const TARGET_LUFS: f64 = -18.0;
    const MAX_GAIN_DB: f64 = 12.0;
    let Some(loudness) = item.integrated_loudness_lufs else {
        return 1.0;
    };
    let mut gain_db = TARGET_LUFS - loudness;
    // Don't push true peak above 0 dBFS (clip guard), mirroring the browser leveling.
    if let Some(peak) = item.true_peak_dbtp {
        gain_db = gain_db.min(-peak);
    }
    gain_db = gain_db.clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
    10f64.powf(gain_db / 20.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use musicata_core::{Album, Artist, Library, ProviderMapping, Track};
    use std::path::PathBuf;

    // ---- ListenTracker (the completion-rule state machine) ----

    /// A Playing tick for `id` at `elapsed`, duration `dur`.
    fn tick(tracker: &mut ListenTracker, id: &str, elapsed: f64, dur: Option<f64>) -> ListenAction {
        tracker.observe(PlaybackStatus::Playing, Some(id), Some(elapsed), dur)
    }

    const D180: Option<f64> = Some(180.0); // threshold 90

    #[test]
    fn noise_floor_progress_is_neither_listen_nor_skip() {
        let mut t = ListenTracker::default();
        assert_eq!(tick(&mut t, "a", 0.0, D180), ListenAction::None);
        assert_eq!(tick(&mut t, "a", 2.0, D180), ListenAction::None);
        // Switch away after only 2s — below the floor, so not even a skip.
        assert_eq!(tick(&mut t, "b", 0.0, D180), ListenAction::None);
    }

    #[test]
    fn crossing_half_duration_records_one_listen() {
        let mut t = ListenTracker::default();
        assert_eq!(tick(&mut t, "a", 30.0, D180), ListenAction::None);
        assert_eq!(tick(&mut t, "a", 89.0, D180), ListenAction::None);
        assert_eq!(
            tick(&mut t, "a", 91.0, D180),
            ListenAction::RecordListen("a".into())
        );
        // No double-count past the threshold.
        assert_eq!(tick(&mut t, "a", 120.0, D180), ListenAction::None);
    }

    #[test]
    fn four_minute_cap_for_long_tracks() {
        let mut t = ListenTracker::default();
        let long = Some(1200.0); // half would be 600, but the cap is 240
        assert_eq!(tick(&mut t, "a", 239.0, long), ListenAction::None);
        assert_eq!(
            tick(&mut t, "a", 241.0, long),
            ListenAction::RecordListen("a".into())
        );
    }

    #[test]
    fn skip_when_track_changes_before_threshold() {
        let mut t = ListenTracker::default();
        assert_eq!(tick(&mut t, "a", 30.0, D180), ListenAction::None);
        assert_eq!(
            tick(&mut t, "b", 0.0, D180),
            ListenAction::RecordSkip("a".into())
        );
    }

    #[test]
    fn no_skip_when_track_changes_after_a_listen() {
        let mut t = ListenTracker::default();
        assert_eq!(
            tick(&mut t, "a", 95.0, D180),
            ListenAction::RecordListen("a".into())
        );
        // "a" already counted, so switching away is not a skip.
        assert_eq!(tick(&mut t, "b", 0.0, D180), ListenAction::None);
    }

    #[test]
    fn replay_of_the_same_track_counts_twice() {
        let mut t = ListenTracker::default();
        assert_eq!(
            tick(&mut t, "a", 95.0, D180),
            ListenAction::RecordListen("a".into())
        );
        // Repeat-one: elapsed resets near zero on the same id — a new play begins.
        assert_eq!(tick(&mut t, "a", 1.0, D180), ListenAction::None);
        assert_eq!(
            tick(&mut t, "a", 95.0, D180),
            ListenAction::RecordListen("a".into())
        );
    }

    #[test]
    fn mid_track_seek_back_is_not_a_new_play() {
        let mut t = ListenTracker::default();
        assert_eq!(tick(&mut t, "a", 60.0, D180), ListenAction::None);
        // Scrub back to 50 (not near zero, not yet a listen) — same play continues.
        assert_eq!(tick(&mut t, "a", 50.0, D180), ListenAction::None);
        // Continue past the threshold → exactly one listen, no spurious skip.
        assert_eq!(
            tick(&mut t, "a", 91.0, D180),
            ListenAction::RecordListen("a".into())
        );
    }

    #[test]
    fn pause_resume_does_not_finalize_or_double_count() {
        let mut t = ListenTracker::default();
        assert_eq!(
            tick(&mut t, "a", 95.0, D180),
            ListenAction::RecordListen("a".into())
        );
        // Pause holds; resume continues; neither records anything.
        assert_eq!(
            t.observe(PlaybackStatus::Paused, Some("a"), Some(95.0), D180),
            ListenAction::None
        );
        assert_eq!(tick(&mut t, "a", 96.0, D180), ListenAction::None);
    }

    #[test]
    fn stop_finalizes_a_partial_as_a_skip() {
        let mut t = ListenTracker::default();
        assert_eq!(tick(&mut t, "a", 30.0, D180), ListenAction::None);
        assert_eq!(
            t.observe(PlaybackStatus::Stopped, None, None, None),
            ListenAction::RecordSkip("a".into())
        );
        // Already finalized — a second stop does nothing.
        assert_eq!(
            t.observe(PlaybackStatus::Stopped, None, None, None),
            ListenAction::None
        );
    }

    #[test]
    fn stop_after_a_listen_is_not_a_skip() {
        let mut t = ListenTracker::default();
        assert_eq!(
            tick(&mut t, "a", 95.0, D180),
            ListenAction::RecordListen("a".into())
        );
        assert_eq!(
            t.observe(PlaybackStatus::Stopped, None, None, None),
            ListenAction::None
        );
    }

    #[test]
    fn missing_or_absurd_duration_falls_back_to_the_cap() {
        for dur in [None, Some(0.0), Some(-5.0), Some(f64::NAN)] {
            let mut t = ListenTracker::default();
            assert_eq!(tick(&mut t, "a", 200.0, dur), ListenAction::None);
            assert_eq!(
                tick(&mut t, "a", 241.0, dur),
                ListenAction::RecordListen("a".into())
            );
        }
    }

    #[test]
    fn radio_without_a_track_id_is_ignored() {
        let mut t = ListenTracker::default();
        // Playing, but no library track id (a radio stream).
        assert_eq!(
            t.observe(PlaybackStatus::Playing, None, Some(10.0), None),
            ListenAction::None
        );
        assert!(t.current.is_none());
    }

    #[test]
    fn coarse_tick_can_skip_outgoing_and_confirm_incoming() {
        let mut t = ListenTracker::default();
        assert_eq!(tick(&mut t, "a", 30.0, D180), ListenAction::None);
        // A single coarse poll jumps to a new short track already past its threshold.
        assert_eq!(
            tick(&mut t, "b", 250.0, None),
            ListenAction::RecordSkipThenListen("a".into(), "b".into())
        );
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
            artwork_url: None,
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

    // End-to-end through the real player + recorder wiring: play a track and report
    // progress past the listen threshold, and confirm each *completed* track lands in
    // Recently played. (A play with no progress past the threshold is not a listen —
    // that's the completion rule; this drives real progress.)
    #[tokio::test]
    async fn recently_played_reflects_completed_listens() {
        let db_path = temp_db("recent-plays");
        let database = Database::connect(&db_path).await.expect("connect");
        let mut library = library_with_tracks(5); // each track is 180s -> threshold 90s
        database.save_library(&mut library).await.expect("save");

        let manager = PlayerManager::load(
            database.clone(),
            "http://localhost".to_string(),
            Arc::new(RwLock::new(ProviderRegistry::new())),
        )
        .await
        .expect("manager");
        let browser = manager
            .get(BROWSER_PLAYER_ID)
            .await
            .expect("browser player present");
        let PlayerHandle::Browser(player) = &browser else {
            panic!("browser player should be a Browser handle");
        };

        // Play a track, then report progress past half its duration -> one listen.
        play(&browser, &database, "track_1").await;
        player.report_progress(95.0, Some(180.0)).await;
        let recent = wait_for_recent(&database, 1).await;
        assert_eq!(recent, vec!["track_1".to_string()]);

        // Four more, each driven past the threshold -> all five present.
        for id in ["track_2", "track_3", "track_4", "track_5"] {
            play(&browser, &database, id).await;
            player.report_progress(95.0, Some(180.0)).await;
        }
        let recent = wait_for_recent(&database, 5).await;
        let found: std::collections::BTreeSet<&str> = recent.iter().map(String::as_str).collect();
        for id in ["track_1", "track_2", "track_3", "track_4", "track_5"] {
            assert!(found.contains(id), "recent missing {id}; got {recent:?}");
        }
        assert_eq!(recent.len(), 5, "expected exactly five distinct tracks");

        let _ = std::fs::remove_file(db_path);
    }

    // The flip side of the completion rule: a track abandoned early is a skip, not a
    // listen — it must NOT show in Recently played, but it must show in Most skipped.
    #[tokio::test]
    async fn early_abandon_records_a_skip_not_a_listen() {
        let db_path = temp_db("skip-not-listen");
        let database = Database::connect(&db_path).await.expect("connect");
        let mut library = library_with_tracks(2);
        database.save_library(&mut library).await.expect("save");

        let manager = PlayerManager::load(
            database.clone(),
            "http://localhost".to_string(),
            Arc::new(RwLock::new(ProviderRegistry::new())),
        )
        .await
        .expect("manager");
        let browser = manager.get(BROWSER_PLAYER_ID).await.expect("browser");
        let PlayerHandle::Browser(player) = &browser else {
            panic!("browser handle");
        };

        // Start track_1, play 30s (past the floor, below the 90s threshold), then jump
        // to track_2 and complete it.
        play(&browser, &database, "track_1").await;
        player.report_progress(30.0, Some(180.0)).await;
        play(&browser, &database, "track_2").await;
        player.report_progress(95.0, Some(180.0)).await;

        // Only the completed track_2 is a listen.
        let recent = wait_for_recent(&database, 1).await;
        assert_eq!(recent, vec!["track_2".to_string()]);

        // track_1 surfaces as a skip.
        for _ in 0..200 {
            let skipped = database.most_skipped(10).await.expect("skipped");
            if !skipped.is_empty() {
                assert_eq!(skipped[0].0.id, "track_1");
                let _ = std::fs::remove_file(&db_path);
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("track_1 was never recorded as a skip");
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
            let manager = PlayerManager::load(
                database.clone(),
                "http://localhost".to_string(),
                Arc::new(RwLock::new(ProviderRegistry::new())),
            )
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
        let manager = PlayerManager::load(
            database.clone(),
            "http://localhost".to_string(),
            Arc::new(RwLock::new(ProviderRegistry::new())),
        )
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
            let manager = PlayerManager::load(
                database.clone(),
                "http://localhost".to_string(),
                Arc::new(RwLock::new(ProviderRegistry::new())),
            )
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
        let manager = PlayerManager::load(
            database.clone(),
            "http://localhost".to_string(),
            Arc::new(RwLock::new(ProviderRegistry::new())),
        )
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

    // ---- MPD server-owned queue ----------------------------------------------

    #[test]
    fn queue_to_mpd_uris_prefixes_library_tracks_only() {
        let items = vec![
            QueueItem {
                track_id: Some("t1".to_string()),
                stream_url: "/api/tracks/t1/stream".to_string(),
                ..Default::default()
            },
            // An external stream (radio) is already absolute and must not be prefixed.
            QueueItem {
                track_id: None,
                stream_url: "http://radio.example/stream".to_string(),
                ..Default::default()
            },
        ];
        let uris = queue_to_mpd_uris(&items, "http://host:3030");
        assert_eq!(uris[0], "http://host:3030/api/tracks/t1/stream");
        assert_eq!(uris[1], "http://radio.example/stream");
    }

    // The MPD player's queue is server-owned and persisted like the browser's: a queue
    // saved under its id is restored Paused at its saved position. (MPD itself is
    // offline here — `127.0.0.1:1` refuses immediately — so the restore exercises the
    // server-state path; the push to MPD is best-effort and silently skipped.)
    #[tokio::test]
    async fn mpd_player_restores_persisted_queue_paused() {
        let db_path = temp_db("mpd-restore");
        let database = Database::connect(&db_path).await.expect("connect");
        let mut library = library_with_tracks(3);
        database.save_library(&mut library).await.expect("save");

        let items = resolve_queue_items(
            &database,
            &[
                "track_1".to_string(),
                "track_2".to_string(),
                "track_3".to_string(),
            ],
        )
        .await
        .expect("items");
        let playback = PlayerPlayback {
            status: PlaybackStatus::Playing,
            position: Some(2),
            elapsed_seconds: Some(9.0),
            volume: Some(50),
            repeat: RepeatMode::Off,
            shuffle: false,
        };
        database
            .save_player_queue("mpd-x", &playback, &items)
            .await
            .expect("save queue");

        let player = MpdPlayer::new(
            "mpd-x".to_string(),
            "127.0.0.1:1".to_string(),
            database.clone(),
            "http://host".to_string(),
        );
        player.restore().await;

        let snap = player.snapshot().await;
        assert_eq!(snap.queue.len(), 3, "queue restored");
        assert_eq!(snap.queue_position, Some(2), "position restored");
        assert_eq!(
            snap.now_playing.and_then(|n| n.track_id),
            Some("track_3".to_string())
        );
        assert_eq!(
            snap.status,
            PlaybackStatus::Paused,
            "a restored MPD queue is paused, never auto-resumed"
        );

        let _ = std::fs::remove_file(db_path);
    }

    // MPD owns the cursor: its reported position maps onto the server-owned queue, and
    // its queue version is recorded so our own edits aren't mistaken for external ones.
    #[tokio::test]
    async fn mpd_player_cursor_maps_onto_server_queue() {
        let db_path = temp_db("mpd-cursor");
        let database = Database::connect(&db_path).await.expect("connect");
        let mut library = library_with_tracks(3);
        database.save_library(&mut library).await.expect("save");

        let player = MpdPlayer::new(
            "mpd-y".to_string(),
            "127.0.0.1:1".to_string(),
            database.clone(),
            "http://host".to_string(),
        );
        {
            let mut state = player.state.lock().await;
            state.queue = resolve_queue_items(
                &database,
                &[
                    "track_1".to_string(),
                    "track_2".to_string(),
                    "track_3".to_string(),
                ],
            )
            .await
            .expect("items");
        }

        let status = MpdStatus {
            state: PlaybackState {
                status: PlaybackStatus::Playing,
                queue_position: Some(1),
                elapsed_seconds: Some(7.5),
                volume: Some(80),
                ..Default::default()
            },
            playlist_version: 4,
        };
        player.apply_cursor(status).await;

        let snap = player.snapshot().await;
        assert_eq!(snap.status, PlaybackStatus::Playing);
        assert_eq!(snap.queue_position, Some(1));
        assert_eq!(
            snap.now_playing.and_then(|n| n.track_id),
            Some("track_2".to_string()),
            "now-playing maps to the server queue item at MPD's cursor"
        );
        assert_eq!(snap.elapsed_seconds, Some(7.5));
        assert_eq!(
            player.expected_playlist_version.load(Ordering::Relaxed),
            4,
            "queue version recorded to distinguish our own edits from external ones"
        );

        let _ = std::fs::remove_file(db_path);
    }

    // ---- Snapcast helpers (pure) ----

    #[cfg(feature = "snapcast")]
    #[test]
    fn next_index_honors_repeat_mode() {
        // Off: advance, then stop at the end.
        assert_eq!(next_index(Some(0), 3, RepeatMode::Off), Some(1));
        assert_eq!(next_index(Some(2), 3, RepeatMode::Off), None);
        // All: wrap around.
        assert_eq!(next_index(Some(2), 3, RepeatMode::All), Some(0));
        // One: repeat the same track.
        assert_eq!(next_index(Some(1), 3, RepeatMode::One), Some(1));
        // No position / empty queue: nothing to play next.
        assert_eq!(next_index(None, 3, RepeatMode::All), None);
        assert_eq!(next_index(Some(0), 0, RepeatMode::All), None);
    }

    #[cfg(feature = "snapcast")]
    #[test]
    fn leveling_gain_targets_minus_18_and_guards_clipping() {
        let item = |lufs: Option<f64>, peak: Option<f64>| QueueItem {
            integrated_loudness_lufs: lufs,
            true_peak_dbtp: peak,
            ..Default::default()
        };
        // No measurement → unity gain.
        assert_eq!(leveling_gain(&item(None, None)), 1.0);
        // Quiet track (-30 LUFS) wants +12 dB; capped at the +12 dB ceiling, peak allows it.
        let quiet = leveling_gain(&item(Some(-30.0), Some(-20.0)));
        assert!((quiet - 10f32.powf(12.0 / 20.0)).abs() < 1e-3);
        // Loud track (-12 LUFS) → cut toward -18 (≈ -6 dB).
        let loud = leveling_gain(&item(Some(-12.0), Some(-1.0)));
        assert!(loud < 1.0 && loud > 0.0);
        // A high true peak clamps the gain so output never exceeds 0 dBFS (here +0.5 dB room).
        let near_clip = leveling_gain(&item(Some(-30.0), Some(-0.5)));
        assert!((near_clip - 10f32.powf(0.5 / 20.0)).abs() < 1e-3);
    }
}
