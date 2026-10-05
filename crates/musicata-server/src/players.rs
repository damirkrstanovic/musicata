// SPDX-License-Identifier: AGPL-3.0-or-later
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
use musicata_storage::{Database, ListenKind, PlayerPlayback, PlayerQueueSnapshot, PlayerRecord};
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio::task::JoinHandle;

use crate::auth::MpdStreamAuth;
use crate::mpd::{MpdConnection, MpdStatus};
use crate::providers::ProviderRegistry;
use crate::queue_persistence::{QueueOwner, QueuePersist, QueuePersistence};
#[cfg(feature = "snapcast")]
use crate::snapcast::{DecodedTrack, SnapcastManager, StereoEq, WriterEvent, WriterMsg};

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

    /// The transport features this backend supports, advertised on the player
    /// descriptor. All current backends support the full command set (their
    /// [`PlayerCommand`] handling is uniform); the per-variant match is the seam where a
    /// future bridged endpoint (Chromecast/UPnP/Squeezelite — "later" in M10) declares a
    /// reduced set in one place rather than callers probing.
    pub fn capabilities(&self) -> PlayerCapabilities {
        match self {
            PlayerHandle::Mpd(_) => PlayerCapabilities::FULL,
            PlayerHandle::Browser(_) => PlayerCapabilities::FULL,
            #[cfg(feature = "snapcast")]
            PlayerHandle::Snapcast(_) => PlayerCapabilities::FULL,
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

    fn subscribe_history(&self) -> broadcast::Receiver<ListenSample> {
        match self {
            Self::Mpd(player) => player.history_tx.subscribe(),
            Self::Browser(player) => player.history_tx.subscribe(),
            #[cfg(feature = "snapcast")]
            Self::Snapcast(player) => player.history_tx.subscribe(),
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

    /// Mark a terminal queue as waiting for an autoplay refill. The subsequent Next is
    /// deliberately suppressed by the backend so MPD does not stop before the refill lands.
    pub async fn request_autoplay_resume(&self) -> bool {
        match self {
            Self::Mpd(player) => player.request_autoplay_resume().await,
            Self::Browser(player) => player.request_autoplay_resume().await,
            #[cfg(feature = "snapcast")]
            Self::Snapcast(player) => player.request_autoplay_resume().await,
        }
    }

    pub async fn begin_autoplay_refill(&self, min_upcoming: usize) -> Option<(u64, PlaybackState)> {
        match self {
            Self::Mpd(player) => player.begin_autoplay_refill(min_upcoming).await,
            Self::Browser(player) => player.begin_autoplay_refill(min_upcoming).await,
            #[cfg(feature = "snapcast")]
            Self::Snapcast(player) => player.begin_autoplay_refill(min_upcoming).await,
        }
    }

    pub async fn finish_autoplay_refill(
        &self,
        revision: u64,
        track_ids: Vec<String>,
        database: &Database,
        base_url: &str,
    ) -> Result<()> {
        match self {
            Self::Mpd(player) => {
                player
                    .finish_autoplay_refill(revision, track_ids, database, base_url)
                    .await
            }
            Self::Browser(player) => {
                player
                    .finish_autoplay_refill(revision, track_ids, database)
                    .await
            }
            #[cfg(feature = "snapcast")]
            Self::Snapcast(player) => {
                player
                    .finish_autoplay_refill(revision, track_ids, database, base_url)
                    .await
            }
        }
    }

    pub async fn cancel_autoplay_refill(&self) {
        match self {
            Self::Mpd(player) => player.cancel_autoplay_refill().await,
            Self::Browser(player) => player.cancel_autoplay_refill().await,
            #[cfg(feature = "snapcast")]
            Self::Snapcast(player) => player.cancel_autoplay_refill().await,
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
    if matches!(action, ListenAction::None) {
        return;
    }
    // Privacy switch: when history recording is off, drop the event (plays and skips alike).
    if !crate::bool_setting(database, crate::SETTING_HISTORY_ENABLED).await {
        return;
    }
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
    let mut played: Vec<&str> = Vec::new();
    for (track_id, kind) in &writes {
        if let Err(error) = database
            .record_listen(track_id, player_id, now, *kind)
            .await
        {
            tracing::warn!(%player_id, %track_id, ?kind, %error, "failed to record listen");
            continue;
        }
        if matches!(kind, ListenKind::Played) {
            played.push(track_id);
        }
    }
    // Queue confirmed plays for outbound scrobbling (drained by `scrobble::scrobble_loop`).
    if !played.is_empty() && crate::scrobble::enabled(database).await {
        for track_id in played {
            if let Err(error) = database.enqueue_scrobble(track_id, now).await {
                tracing::warn!(%player_id, %track_id, %error, "failed to enqueue scrobble");
            }
        }
    }
}

/// Internal, ordered history events. Keep these separate from UI progress frames:
/// merging two independent channels can assign a tick to the wrong track when
/// controls no longer yield to SQLite between commands.
#[derive(Clone)]
struct ListenSample {
    status: PlaybackStatus,
    track_id: Option<String>,
    elapsed: Option<f64>,
    duration: Option<f64>,
}

fn spawn_listen_recorder(
    player_id: String,
    mut events: broadcast::Receiver<ListenSample>,
    database: Database,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tracker = ListenTracker::default();
        loop {
            match events.recv().await {
                Ok(event) => {
                    let action = tracker.observe(
                        event.status,
                        event.track_id.as_deref(),
                        event.elapsed,
                        event.duration,
                    );
                    record_action(&database, &player_id, action).await;
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

/// Stable id of the always-present Snapcast multi-room player (created when the feature is
/// enabled in settings, like the browser player).
#[cfg(feature = "snapcast")]
pub const SNAPCAST_PLAYER_ID: &str = "snapcast-local";

/// Registry of registered players and zones, backed by the database.
pub struct PlayerManager {
    database: Database,
    public_base_url: String,
    pub(crate) mpd_stream_auth: MpdStreamAuth,
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
            mpd_stream_auth: MpdStreamAuth::new(),
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
            player.history_tx.subscribe(),
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

    /// Set (or clear) the server-side EQ correction applied to the Snapcast stream.
    #[cfg(feature = "snapcast")]
    pub async fn set_snapcast_dsp(&self, eq: Option<StereoEq>) {
        if let Some(entry) = self.players.read().await.get(SNAPCAST_PLAYER_ID)
            && let PlayerHandle::Snapcast(player) = &entry.handle
        {
            player.set_dsp(eq);
        }
    }

    /// Bring up the runtime entry for a persisted player record.
    async fn bring_up(&self, record: &PlayerRecord) {
        let (handle, task, poll) = match record.kind.as_str() {
            // The browser tab and a self-registered native endpoint are the same server-side
            // player: a queue-output driven over the bidirectional WS (server owns the queue +
            // hands out stream URLs; the client plays them and reports progress/ended back). The
            // only difference is identity — the browser is the always-present singleton, a
            // native endpoint is a registered, removable, token-authenticated player.
            "browser" | "native" => {
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
                    self.mpd_stream_auth.clone(),
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
            handle.subscribe_history(),
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
        // `mpd` is a server-dialled backend (we connect to `address`); `native` is a
        // self-registering endpoint (it connects back to us over the player WS — see
        // crates/musicata-endpoint), driven by the same server-owned queue as the browser.
        if kind != "mpd" && kind != "native" {
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
        let previous = self.require_record(id).await?;
        if previous.zone_id.as_deref() != zone_id {
            if let Some(handle) = self.get(id).await {
                handle.cancel_autoplay_refill().await;
            }
            if let Some(previous_zone) = previous.zone_id.as_deref()
                && let Some(zone) = self.get_zone(previous_zone).await
            {
                zone.cancel_autoplay_refill().await;
            }
            if let Some(next_zone) = zone_id
                && let Some(zone) = self.get_zone(next_zone).await
            {
                zone.cancel_autoplay_refill().await;
            }
        }
        self.database.update_player_zone(id, zone_id).await?;
        self.descriptor(&self.require_record(id).await?).await
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        self.require_record(id).await?;
        let entry = self.players.write().await.remove(id);
        if let Some(entry) = entry {
            let handle = entry.handle.clone();
            drop(entry); // Abort polling/recording before stopping the writer.
            handle.cancel_autoplay_refill().await;
            match handle {
                PlayerHandle::Mpd(player) => player.persistence.stop().await,
                PlayerHandle::Browser(player) => player.persistence.stop().await,
                #[cfg(feature = "snapcast")]
                PlayerHandle::Snapcast(player) => player.persistence.stop().await,
            }
        }
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
        // Advertise the live backend's own capabilities. An offline player has no live
        // handle; all current backends are full-capability, so report that (a future
        // reduced-capability backend would key this off `record.kind` when offline).
        let (online, capabilities) = match self.players.read().await.get(&record.id) {
            Some(entry) => (entry.handle.is_online(), entry.handle.capabilities()),
            None => (false, PlayerCapabilities::FULL),
        };
        Ok(Player {
            id: record.id.clone(),
            name: record.name.clone(),
            kind: record.kind.clone(),
            address: record.address.clone(),
            zone_id: record.zone_id.clone(),
            online,
            capabilities,
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
        let entry = self.zones.write().await.remove(id);
        if let Some(entry) = entry {
            let player = entry.player.clone();
            drop(entry);
            player.cancel_autoplay_refill().await;
            player.persistence.stop().await;
        }
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

    pub async fn request_zone_autoplay_resume(&self, zone_id: &str) -> Result<bool> {
        let zone = self
            .get_zone(zone_id)
            .await
            .ok_or_else(|| anyhow!("unknown zone: {zone_id}"))?;
        Ok(zone.request_autoplay_resume().await)
    }

    pub async fn cancel_zone_autoplay_refill(&self, zone_id: &str) -> Result<()> {
        let zone = self
            .get_zone(zone_id)
            .await
            .ok_or_else(|| anyhow!("unknown zone: {zone_id}"))?;
        zone.cancel_autoplay_refill().await;
        Ok(())
    }

    pub async fn begin_zone_autoplay_refill(
        &self,
        zone_id: &str,
        min_upcoming: usize,
    ) -> Result<Option<(u64, PlaybackState)>> {
        let zone = self
            .get_zone(zone_id)
            .await
            .ok_or_else(|| anyhow!("unknown zone: {zone_id}"))?;
        Ok(zone.begin_autoplay_refill(min_upcoming).await)
    }

    pub async fn finish_zone_autoplay_refill(
        &self,
        zone_id: &str,
        revision: u64,
        track_ids: Vec<String>,
    ) -> Result<()> {
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
        zone.finish_autoplay_refill(
            revision,
            track_ids,
            &self.database,
            &self.public_base_url,
            &members,
        )
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
    persistence: QueuePersistence,
    public_base_url: String,
    stream_auth: MpdStreamAuth,
    /// Serialize queue mutations, MPD commands, cursor application and persistence.
    /// A transport lock alone leaves stale observations racing with newer commands.
    sync: Mutex<()>,
    /// Command connection, reconnected on demand. A separate connection is used
    /// for the blocking idle loop.
    connection: Mutex<Option<MpdConnection>>,
    online: AtomicBool,
    /// The server-owned queue. MPD is reconciled to match it; the cursor fields are
    /// refreshed from MPD.
    state: Mutex<QueueState>,
    state_tx: broadcast::Sender<PlaybackState>,
    history_tx: broadcast::Sender<ListenSample>,
    /// Last validated MPD queue version. Metadata updates also bump it, so a changed
    /// version triggers a URI comparison, never an unconditional queue reload.
    expected_playlist_version: AtomicU64,
    /// Set after a restore — MPD has the queue loaded but its cursor isn't established,
    /// so the first `Play` resumes at the saved position rather than MPD's index 0.
    restored: AtomicBool,
}

impl MpdPlayer {
    fn new(
        id: String,
        addr: String,
        database: Database,
        public_base_url: String,
        stream_auth: MpdStreamAuth,
    ) -> Self {
        let (state_tx, _) = broadcast::channel(16);
        let (history_tx, _) = broadcast::channel(128);
        Self {
            persistence: QueuePersistence::new(database.clone(), QueueOwner::Player(id.clone())),
            id,
            addr,
            database,
            public_base_url,
            stream_auth,
            sync: Mutex::new(()),
            connection: Mutex::new(None),
            online: AtomicBool::new(false),
            state: Mutex::new(QueueState::default()),
            state_tx,
            history_tx,
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

    async fn request_autoplay_resume(&self) -> bool {
        let mut state = self.state.lock().await;
        let requested = state.can_request_refill_resume();
        if requested {
            state.request_refill_resume();
        }
        drop(state);
        if requested {
            self.broadcast().await;
        }
        requested
    }

    async fn begin_autoplay_refill(&self, min_upcoming: usize) -> Option<(u64, PlaybackState)> {
        let mut state = self.state.lock().await;
        let refill = state.begin_refill(min_upcoming);
        drop(state);
        if refill.is_some() {
            self.broadcast().await;
        }
        refill
    }

    async fn cancel_autoplay_refill(&self) {
        let _sync = self.sync.lock().await;
        self.state.lock().await.cancel_refill();
        self.broadcast().await;
    }

    /// Build the controller-facing state: the server-owned queue plus the
    /// MPD-derived cursor (now-playing is the queue item at the cursor position).
    async fn snapshot(&self) -> PlaybackState {
        self.state.lock().await.snapshot()
    }

    async fn broadcast(&self) {
        let state = self.state.lock().await;
        let _ = self.history_tx.send(state.listen_sample());
        let _ = self.state_tx.send(state.snapshot());
    }

    /// Persist the server-owned queue (failures logged, never propagated).
    fn persist(&self, persist: QueuePersist) {
        self.persistence.submit(persist);
    }

    /// Fold MPD's reported cursor (status + current song) into the server queue state:
    /// status/position/elapsed/volume/options come from MPD; the queue stays
    /// server-owned. Records MPD's queue version so our own edits aren't mistaken for
    /// an external one.
    async fn apply_cursor(&self, status: MpdStatus) {
        let mut state = self.state.lock().await;
        let cursor = status.state;
        if cursor.status == PlaybackStatus::Stopped
            && state.queue_activity.is_some()
            && state
                .position
                .is_some_and(|position| position + 1 == state.queue.len())
        {
            // MPD owns native auto-advance. If it naturally drains while the lookup is
            // running, retain the exact first appended index to start on completion.
            state.preserve_refill_resume();
        }
        state.status = cursor.status;
        let next = cursor
            .queue_position
            .filter(|&index| index < state.queue.len());
        if cursor.status != PlaybackStatus::Stopped {
            state.clear_exhaustion_after_advance(next);
        }
        state.position = next;
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
                    apply_restored_snapshot(&mut state, snapshot);
                }
                self.restored.store(true, Ordering::Relaxed);
                // Push the restored queue onto MPD without playing (resumed on first Play).
                let uris = {
                    let state = self.state.lock().await;
                    queue_to_mpd_uris(&state.queue, &self.public_base_url, &self.stream_auth)
                };
                let mut guard = self.connection.lock().await;
                if let Ok(connection) = ensure_connected(&mut guard, &self.addr).await
                    && connection.load_queue(&uris).await.is_ok()
                {
                    self.online.store(true, Ordering::Relaxed);
                    if let Ok(status) = connection.read_status().await {
                        self.expected_playlist_version
                            .store(status.playlist_version, Ordering::Relaxed);
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
        let _sync = self.sync.lock().await;
        let status = {
            let mut guard = self.connection.lock().await;
            let result = async {
                let connection = ensure_connected(&mut guard, &self.addr).await?;
                self.read_reconciled_status(connection).await
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
        let _sync = self.sync.lock().await;
        let defer_next = matches!(command, PlayerCommand::Next)
            && self.state.lock().await.resume_refill_at.is_some();
        if !defer_next && cancels_autoplay_refill(&command) {
            self.state.lock().await.cancel_refill();
        }
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
                    None if defer_next => {}
                    None => {
                        apply_command(
                            connection,
                            &command,
                            database,
                            public_base_url,
                            &self.stream_auth,
                        )
                        .await?
                    }
                }
                self.read_reconciled_status(connection).await
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
        {
            let state = self.state.lock().await;
            let persist = if mutates_queue {
                QueuePersist::Queue(state.playback(), state.queue.clone())
            } else {
                QueuePersist::Playback(state.playback())
            };
            self.persist(persist);
        }
        self.broadcast().await;
        Ok(())
    }

    async fn finish_autoplay_refill(
        &self,
        revision: u64,
        track_ids: Vec<String>,
        database: &Database,
        _base_url: &str,
    ) -> Result<()> {
        let items = resolve_queue_items(database, &track_ids).await?;
        let appended_uris = queue_to_mpd_uris(&items, &self.public_base_url, &self.stream_auth);
        let _sync = self.sync.lock().await;
        let (resume, appended_at, continue_playback) = {
            let mut state = self.state.lock().await;
            if state.refill_revision != revision || state.queue_activity.is_none() {
                return Ok(());
            }
            if items.is_empty() {
                state.queue_activity = Some("No more tracks found".into());
                drop(state);
                self.broadcast().await;
                return Ok(());
            }
            let resume = state.resume_refill_at.take();
            let appended_at = state.queue.len();
            let continue_playback = state.status == PlaybackStatus::Playing || resume.is_some();
            state.queue.extend(items);
            if let Some(position) = resume {
                state.position = Some(position);
                state.status = PlaybackStatus::Playing;
                state.elapsed_seconds = Some(0.0);
                state.duration_seconds = None;
            }
            state.queue_activity = None;
            state.refill_started = false;
            (resume, appended_at, continue_playback)
        };
        let status = {
            let mut guard = self.connection.lock().await;
            let connection = ensure_connected(&mut guard, &self.addr).await?;
            // Do not reload the queue: that stops MPD and restarts a still-playing seed.
            for uri in &appended_uris {
                connection.add(uri).await?;
            }
            if let Some(position) = resume {
                connection.play_index(position).await?;
            }
            let mut status = connection.read_status().await?;
            // The daemon can drain between its last poll and this append. Commands share
            // `sync`, so a Stop cannot be mistaken for that natural end here.
            if resume.is_none()
                && continue_playback
                && status.state.status == PlaybackStatus::Stopped
            {
                connection.play_index(appended_at).await?;
                status = connection.read_status().await?;
            }
            status
        };
        self.apply_cursor(status).await;
        let state = self.state.lock().await;
        self.persist(QueuePersist::Queue(state.playback(), state.queue.clone()));
        drop(state);
        self.broadcast().await;
        Ok(())
    }

    /// A playlist event can mean new stream tags, not changed content/order. Validate
    /// the URLs before reloading; otherwise auto-advance can restart the previous song.
    /// Callers hold `sync` until the returned cursor has been applied.
    async fn read_reconciled_status(&self, connection: &mut MpdConnection) -> Result<MpdStatus> {
        let mut status = connection.read_status().await?;
        if status.playlist_version != self.expected_playlist_version.load(Ordering::Relaxed) {
            let desired = {
                let state = self.state.lock().await;
                queue_to_mpd_uris(&state.queue, &self.public_base_url, &self.stream_auth)
            };
            if connection.queue_uris().await? != desired {
                self.reassert(connection).await?;
                status = connection.read_status().await?;
            }
        }
        Ok(status)
    }

    /// Re-assert the server queue onto MPD (load it and resume the server's position if
    /// it was playing) after an external client edited MPD's queue.
    async fn reassert(&self, connection: &mut MpdConnection) -> Result<()> {
        let (uris, position, status) = {
            let state = self.state.lock().await;
            (
                queue_to_mpd_uris(&state.queue, &self.public_base_url, &self.stream_auth),
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
                let _sync = self.sync.lock().await;
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
                        Ok(connection) => self.read_reconciled_status(connection).await.ok(),
                        Err(_) => None,
                    }
                };
                if let Some(status) = status {
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
            let _sync = self.sync.lock().await;
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
        if self.state(database).await.is_ok() {
            self.broadcast().await;
        }
        loop {
            idle.idle().await?;
            let _sync = self.sync.lock().await;
            let status = {
                let mut guard = self.connection.lock().await;
                let connection = ensure_connected(&mut guard, &self.addr).await?;
                self.read_reconciled_status(connection).await?
            };

            self.apply_cursor(status).await;
            self.broadcast().await;
            // Persist the cursor (cheap row); the queue itself is unchanged here.
            let playback = { self.state.lock().await.playback() };
            self.persist(QueuePersist::Playback(playback));
        }
    }
}

/// Build fresh authenticated MPD URLs from track ids, including restored/adopted queues
/// whose URLs may contain a stale credential. External streams never receive our token.
fn queue_to_mpd_uris(
    items: &[QueueItem],
    public_base_url: &str,
    stream_auth: &MpdStreamAuth,
) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            if let Some(track_id) = &item.track_id {
                stream_auth.track_url(public_base_url, track_id)
            } else {
                stream_auth.stream_url(public_base_url, &item.stream_url)
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
    stream_auth: &MpdStreamAuth,
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
            let urls = resolve_urls(database, public_base_url, track_ids, stream_auth).await?;
            connection.replace_queue(&urls).await?;
            // Start at the requested queue position (default 0) in the same command,
            // so the controller needn't follow up with a separate play_queue_index.
            if *start_index > 0 && (*start_index) < urls.len() {
                connection.play_index(*start_index).await?;
            }
            Ok(())
        }
        PlayerCommand::Enqueue { track_ids } => {
            let urls = resolve_urls(database, public_base_url, track_ids, stream_auth).await?;
            for url in &urls {
                connection.add(url).await?;
            }
            Ok(())
        }
        PlayerCommand::RemoveQueueItem { index } => connection.delete_index(*index).await,
        PlayerCommand::MoveQueueItem { from, to } => connection.move_item(*from, *to).await,
        // Local radio relays need an absolute audio-only credential; external URLs stay unchanged.
        PlayerCommand::PlayStream { url, .. } => {
            connection
                .replace_queue(&[stream_auth.stream_url(public_base_url, url)])
                .await
        }
    }
}

/// Resolve library track ids to absolute stream URLs MPD can fetch over HTTP.
async fn resolve_urls(
    database: &Database,
    public_base_url: &str,
    track_ids: &[String],
    stream_auth: &MpdStreamAuth,
) -> Result<Vec<String>> {
    let mut urls = Vec::with_capacity(track_ids.len());
    for id in track_ids {
        if let Some(track) = database.track(id).await? {
            urls.push(stream_auth.track_url(public_base_url, &track.id));
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
        // Adopted MPD URLs can carry credentials; controllers and persisted queues use
        // the ordinary relative URL. Fresh credentials are added only when sending to MPD.
        item.stream_url = format!("/api/tracks/{track_id}/stream");
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
        } else if item.artwork_url.is_none()
            && let Ok(Some(track)) = database.track(&track_id).await
        {
            item.artwork_url = database
                .album_artwork_url(&track.album_id)
                .await
                .ok()
                .flatten();
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
    persistence: QueuePersistence,
    state: Mutex<QueueState>,
    state_tx: broadcast::Sender<PlaybackState>,
    history_tx: broadcast::Sender<ListenSample>,
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
    /// When shuffle is on, the play order: a permutation of the queue indices that
    /// `advance`/`step_previous` walk so every track plays once before any repeats.
    /// Empty when shuffle is off; rebuilt lazily when the queue changes.
    shuffle_order: Vec<usize>,
    /// UI feedback for a queued autoplay refill; deliberately excluded from persistence.
    queue_activity: Option<String>,
    refill_started: bool,
    /// Bumps whenever a user action invalidates an in-flight refill result.
    refill_revision: u64,
    /// First newly appended index to start after an explicit Next or natural drain.
    resume_refill_at: Option<usize>,
}

impl QueueState {
    fn snapshot(&self) -> PlaybackState {
        PlaybackState {
            status: self.status,
            now_playing: self
                .position
                .and_then(|index| self.queue.get(index).cloned()),
            elapsed_seconds: self.elapsed_seconds,
            duration_seconds: self.duration_seconds,
            volume: self.volume,
            repeat: self.repeat,
            shuffle: self.shuffle,
            queue: self.queue.clone(),
            queue_position: self.position,
            // Prefetch is internal work while music is playing. Only announce generation
            // when Next or a drained queue is waiting for the newly appended tracks.
            queue_activity: self.queue_activity.clone().filter(|activity| {
                activity != "Finding more tracks…" || self.resume_refill_at.is_some()
            }),
            next_up: peek_next_index(self).and_then(|index| self.queue.get(index).cloned()),
        }
    }

    fn listen_sample(&self) -> ListenSample {
        ListenSample {
            status: self.status,
            track_id: self
                .position
                .and_then(|i| self.queue.get(i))
                .and_then(|item| item.track_id.clone()),
            elapsed: self.elapsed_seconds,
            duration: self.duration_seconds,
        }
    }

    /// The lightweight, persistable playback row (everything but the queue items).
    fn playback(&self) -> PlayerPlayback {
        PlayerPlayback {
            status: self.status,
            position: self.position,
            elapsed_seconds: self.elapsed_seconds,
            volume: self.volume,
            repeat: self.repeat,
            shuffle: self.shuffle,
            shuffle_order: self.shuffle_order.clone(),
        }
    }

    fn cancel_refill(&mut self) {
        self.refill_revision = self.refill_revision.wrapping_add(1);
        self.queue_activity = None;
        self.refill_started = false;
        self.resume_refill_at = None;
    }

    fn begin_refill(&mut self, min_upcoming: usize) -> Option<(u64, PlaybackState)> {
        let seed_position = self.position.or_else(|| {
            self.resume_refill_at
                .filter(|&resume| resume > 0)
                .map(|resume| resume - 1)
        });
        let eligible = self.repeat == RepeatMode::Off
            && (self.status == PlaybackStatus::Playing
                || self.queue_activity.as_deref() == Some("Finding more tracks…"))
            && seed_position.is_some_and(|position| {
                self.queue
                    .get(position)
                    .is_some_and(|item| item.track_id.is_some())
                    && self.queue.len().saturating_sub(position + 1) < min_upcoming
            });
        if !eligible || self.refill_started {
            return None;
        }
        self.refill_started = true;
        self.queue_activity = Some("Finding more tracks…".into());
        let mut snapshot = self.snapshot();
        if snapshot.queue_position.is_none() {
            snapshot.queue_position = seed_position;
            snapshot.now_playing =
                seed_position.and_then(|position| self.queue.get(position).cloned());
        }
        Some((self.refill_revision, snapshot))
    }

    fn request_refill_resume(&mut self) {
        self.resume_refill_at = Some(self.queue.len());
        self.refill_started = false;
        self.queue_activity = Some("Finding more tracks…".into());
    }

    fn preserve_refill_resume(&mut self) {
        self.resume_refill_at = Some(self.queue.len());
    }

    fn can_request_refill_resume(&self) -> bool {
        let position = self
            .position
            .or_else(|| self.resume_refill_at.and_then(|at| at.checked_sub(1)));
        self.repeat == RepeatMode::Off
            && position.is_some_and(|position| {
                position + 1 == self.queue.len() && self.queue[position].track_id.is_some()
            })
            && (self.status == PlaybackStatus::Playing || self.resume_refill_at.is_some())
    }

    fn clear_exhaustion_after_advance(&mut self, next: Option<usize>) {
        if self.position != next && self.queue_activity.as_deref() == Some("No more tracks found") {
            self.cancel_refill();
        }
    }
}

impl BrowserPlayer {
    fn new(database: Database, player_id: String) -> Self {
        let (state_tx, _) = broadcast::channel(32);
        let (history_tx, _) = broadcast::channel(128);
        let (progress_tx, _) = broadcast::channel(32);
        Self {
            persistence: QueuePersistence::new(
                database.clone(),
                QueueOwner::Player(player_id.clone()),
            ),
            player_id,
            database,
            state: Mutex::new(QueueState::default()),
            state_tx,
            history_tx,
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
        apply_restored_snapshot(&mut state, snapshot);
    }

    /// Write a queue change back to the database. Failures are logged, never
    /// propagated — persistence must not break live playback control.
    fn persist(&self, persist: QueuePersist) {
        self.persistence.submit(persist);
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
        self.state.lock().await.snapshot()
    }

    async fn request_autoplay_resume(&self) -> bool {
        let mut state = self.state.lock().await;
        let requested = state.can_request_refill_resume();
        if requested {
            state.request_refill_resume();
        }
        drop(state);
        if requested {
            self.broadcast().await;
        }
        requested
    }

    async fn begin_autoplay_refill(&self, min_upcoming: usize) -> Option<(u64, PlaybackState)> {
        let mut state = self.state.lock().await;
        let refill = state.begin_refill(min_upcoming);
        drop(state);
        if refill.is_some() {
            self.broadcast().await;
        }
        refill
    }

    async fn cancel_autoplay_refill(&self) {
        self.state.lock().await.cancel_refill();
        self.broadcast().await;
    }

    async fn finish_autoplay_refill(
        &self,
        revision: u64,
        track_ids: Vec<String>,
        database: &Database,
    ) -> Result<()> {
        let items = resolve_queue_items(database, &track_ids).await?;
        let persist = {
            let mut state = self.state.lock().await;
            if state.refill_revision != revision || state.queue_activity.is_none() {
                return Ok(());
            }
            if items.is_empty() {
                state.queue_activity = Some("No more tracks found".into());
                drop(state);
                self.broadcast().await;
                return Ok(());
            }
            if let Some(position) = state.resume_refill_at.take() {
                state.position = Some(position);
                state.status = PlaybackStatus::Playing;
                state.elapsed_seconds = Some(0.0);
                state.duration_seconds = None;
            }
            state.queue.extend(items);
            state.queue_activity = None;
            state.refill_started = false;
            QueuePersist::Queue(state.playback(), state.queue.clone())
        };
        self.persist(persist);
        self.broadcast().await;
        Ok(())
    }

    async fn broadcast(&self) {
        let state = self.state.lock().await;
        let _ = self.history_tx.send(state.listen_sample());
        let _ = self.state_tx.send(state.snapshot());
    }

    pub async fn execute(&self, command: PlayerCommand, database: &Database) -> Result<()> {
        // Commands that change the queue itself need the item list rewritten;
        // the rest only touch the lightweight playback row.
        let mutates_queue = command_mutates_queue(&command);
        {
            let mut state = self.state.lock().await;
            let defer_next =
                matches!(command, PlayerCommand::Next) && state.resume_refill_at.is_some();
            if !defer_next {
                if cancels_autoplay_refill(&command) {
                    state.cancel_refill();
                }
                apply_to_queue_state(&mut state, command, database).await?;
            }
            let playback = state.playback();
            let persist = if mutates_queue {
                QueuePersist::Queue(playback, state.queue.clone())
            } else {
                QueuePersist::Playback(playback)
            };
            self.persist(persist);
        }
        self.broadcast().await;
        Ok(())
    }

    /// The output tab finished the current track: advance (honoring repeat).
    pub async fn track_ended(&self) {
        let mut state = self.state.lock().await;
        let playback = {
            if state.repeat == RepeatMode::One {
                state.elapsed_seconds = Some(0.0);
            } else {
                advance(&mut state, true);
                if state.status == PlaybackStatus::Stopped && state.queue_activity.is_some() {
                    state.preserve_refill_resume();
                }
            }
            state.playback()
        };
        // Advancing only moves the position within the same queue.
        self.persist(QueuePersist::Playback(playback));
        drop(state);
        self.broadcast().await;
    }

    /// The output tab reports its real playback position and track duration. This
    /// fires ~1×/second, so it emits only a lightweight position tick — not a full
    /// `PlaybackState` (which would re-send the whole queue every second). The stored
    /// state is still updated so the next full snapshot/refresh reflects the position.
    pub async fn report_progress(&self, elapsed_seconds: f64, duration_seconds: Option<f64>) {
        let mut state = self.state.lock().await;
        let (duration, playback) = {
            state.elapsed_seconds = Some(elapsed_seconds);
            if let Some(duration) = duration_seconds {
                state.duration_seconds = Some(duration);
            }
            (state.duration_seconds, state.playback())
        };
        let _ = self.history_tx.send(state.listen_sample());
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
            self.persist(QueuePersist::Playback(playback));
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
    persistence: QueuePersistence,
    /// Serializes zone commands, refill completion, natural drain, and member driving.
    sync: Mutex<()>,
    state: Mutex<QueueState>,
    state_tx: broadcast::Sender<PlaybackState>,
    history_tx: broadcast::Sender<ListenSample>,
    progress_tx: broadcast::Sender<ProgressTick>,
    last_progress_persist: AtomicI64,
}

impl ZonePlayer {
    fn new(database: Database, zone_id: String) -> Self {
        let (state_tx, _) = broadcast::channel(32);
        let (history_tx, _) = broadcast::channel(128);
        let (progress_tx, _) = broadcast::channel(32);
        Self {
            persistence: QueuePersistence::new(database.clone(), QueueOwner::Zone(zone_id.clone())),
            zone_id,
            database,
            sync: Mutex::new(()),
            state: Mutex::new(QueueState::default()),
            state_tx,
            history_tx,
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
        apply_restored_snapshot(&mut state, snapshot);
    }

    fn persist(&self, persist: QueuePersist) {
        self.persistence.submit(persist);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PlaybackState> {
        self.state_tx.subscribe()
    }

    pub fn subscribe_progress(&self) -> broadcast::Receiver<ProgressTick> {
        self.progress_tx.subscribe()
    }

    pub async fn snapshot(&self) -> PlaybackState {
        self.state.lock().await.snapshot()
    }

    async fn request_autoplay_resume(&self) -> bool {
        let _sync = self.sync.lock().await;
        let mut state = self.state.lock().await;
        let requested = if state.can_request_refill_resume() {
            state.request_refill_resume();
            true
        } else {
            false
        };
        drop(state);
        if requested {
            self.broadcast().await;
        }
        requested
    }

    async fn begin_autoplay_refill(&self, min_upcoming: usize) -> Option<(u64, PlaybackState)> {
        let _sync = self.sync.lock().await;
        let mut state = self.state.lock().await;
        let refill = state.begin_refill(min_upcoming);
        drop(state);
        if refill.is_some() {
            self.broadcast().await;
        }
        refill
    }

    async fn cancel_autoplay_refill(&self) {
        let _sync = self.sync.lock().await;
        self.state.lock().await.cancel_refill();
        self.broadcast().await;
    }

    async fn finish_autoplay_refill(
        &self,
        revision: u64,
        track_ids: Vec<String>,
        database: &Database,
        public_base_url: &str,
        members: &[PlayerHandle],
    ) -> Result<()> {
        let _sync = self.sync.lock().await;
        let items = resolve_queue_items(database, &track_ids).await?;
        let resume = {
            let mut state = self.state.lock().await;
            if state.refill_revision != revision || state.queue_activity.is_none() {
                return Ok(());
            }
            if items.is_empty() {
                state.queue_activity = Some("No more tracks found".into());
                drop(state);
                self.broadcast().await;
                return Ok(());
            }
            let resume = state.resume_refill_at.take();
            if let Some(position) = resume {
                state.position = Some(position);
                state.status = PlaybackStatus::Playing;
                state.elapsed_seconds = Some(0.0);
                state.duration_seconds = None;
            }
            state.queue.extend(items);
            state.queue_activity = None;
            state.refill_started = false;
            self.persist(QueuePersist::Queue(state.playback(), state.queue.clone()));
            resume
        };
        self.broadcast().await;
        self.drive_members(
            &PlayerCommand::Enqueue { track_ids },
            members,
            database,
            public_base_url,
        )
        .await;
        if let Some(index) = resume {
            self.drive_members(
                &PlayerCommand::PlayQueueIndex { index },
                members,
                database,
                public_base_url,
            )
            .await;
        }
        Ok(())
    }

    async fn broadcast(&self) {
        let state = self.state.lock().await;
        let _ = self.history_tx.send(state.listen_sample());
        let _ = self.state_tx.send(state.snapshot());
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
        let _sync = self.sync.lock().await;
        let mutates_queue = command_mutates_queue(&command);
        // Phase 1: update the canonical queue exactly like the browser player.
        {
            let mut state = self.state.lock().await;
            let defer_next =
                matches!(command, PlayerCommand::Next) && state.resume_refill_at.is_some();
            if !defer_next {
                if cancels_autoplay_refill(&command) {
                    state.cancel_refill();
                }
                apply_to_queue_state(&mut state, command.clone(), database).await?;
            }
            let playback = state.playback();
            let persist = if mutates_queue {
                QueuePersist::Queue(playback, state.queue.clone())
            } else {
                QueuePersist::Playback(playback)
            };
            self.persist(persist);
        }
        self.broadcast().await;
        // Phase 2: drive members. Browser members render the zone's now-playing
        // straight off the broadcast above (no extra work). MPD members are driven
        // by forwarding the command — queue ops map 1:1 to MPD and indices stay
        // aligned because MPD's queue mirrors the zone's.
        if !matches!(command, PlayerCommand::Next)
            || !self.state.lock().await.resume_refill_at.is_some()
        {
            self.drive_members(&command, members, database, public_base_url)
                .await;
        }
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
        let _sync = self.sync.lock().await;
        let mut state = self.state.lock().await;
        let playback = {
            if state.repeat == RepeatMode::One {
                state.elapsed_seconds = Some(0.0);
            } else {
                advance(&mut state, true);
                if state.status == PlaybackStatus::Stopped && state.queue_activity.is_some() {
                    state.preserve_refill_resume();
                }
            }
            state.playback()
        };
        self.persist(QueuePersist::Playback(playback));
        drop(state);
        self.broadcast().await;
    }

    /// The browser output reports its real position/duration for this zone. Emits a
    /// lightweight tick (not full state) and persists the elapsed position, throttled.
    pub async fn report_progress(&self, elapsed_seconds: f64, duration_seconds: Option<f64>) {
        let mut state = self.state.lock().await;
        let (duration, playback) = {
            state.elapsed_seconds = Some(elapsed_seconds);
            if let Some(duration) = duration_seconds {
                state.duration_seconds = Some(duration);
            }
            (state.duration_seconds, state.playback())
        };
        let _ = self.history_tx.send(state.listen_sample());
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
            self.persist(QueuePersist::Playback(playback));
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

/// A user-directed queue/transport change makes a prior background recommendation stale.
fn cancels_autoplay_refill(command: &PlayerCommand) -> bool {
    matches!(
        command,
        PlayerCommand::SetRepeat { .. }
            | PlayerCommand::SetShuffle { .. }
            | PlayerCommand::Pause
            | PlayerCommand::Stop
            | PlayerCommand::Clear
            | PlayerCommand::PlayQueueIndex { .. }
            | PlayerCommand::PlayTracks { .. }
            | PlayerCommand::Enqueue { .. }
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
        PlayerCommand::Previous => step_previous(state),
        PlayerCommand::Seek { position_seconds } => {
            state.elapsed_seconds = Some(clamp_seek(position_seconds, state.duration_seconds));
        }
        PlayerCommand::SetVolume { volume } => state.volume = Some(volume.min(100)),
        PlayerCommand::SetRepeat { mode } => state.repeat = mode,
        PlayerCommand::SetShuffle { enabled } => {
            state.shuffle = enabled;
            if enabled {
                // Build the play order now (current track first) so it's stable and
                // persisted; clear it when shuffle is turned off.
                state.shuffle_order =
                    build_shuffle_order(state.queue.len(), state.position, shuffle_seed());
            } else {
                state.shuffle_order.clear();
            }
        }
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

/// Populate a freshly-locked `QueueState` from a persisted snapshot, applying the
/// "restore as paused; drop a status the queue can't support" remap shared by every
/// server-owned player's `restore`.
fn apply_restored_snapshot(state: &mut QueueState, snapshot: PlayerQueueSnapshot) {
    let playback = snapshot.playback;
    state.queue = snapshot.items;
    // Guard against a position that no longer indexes the queue.
    state.position = playback.position.filter(|&index| index < state.queue.len());
    state.status = match playback.status {
        PlaybackStatus::Playing => PlaybackStatus::Paused,
        // A queue we can't point into can't be paused/playing.
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
    state.shuffle_order = playback.shuffle_order;
}

/// Advance to the next queue item. When `stop_at_end` is set (a track finished),
/// stop after the last item unless repeat-all is on; otherwise (an explicit Next)
/// clamp at the last item. When shuffle is on, "next" follows the shuffled play order.
fn advance(state: &mut QueueState, stop_at_end: bool) {
    let Some(index) = state.position else {
        return;
    };
    let len = state.queue.len();
    if len == 0 {
        return;
    }
    // Decide the next position without mutating yet, so an explicit Next that can't move
    // (the last track, no repeat) is a true no-op rather than a restart (elapsed reset).
    let next = if state.shuffle {
        ensure_shuffle_order(state, index);
        let cursor = shuffle_cursor(&state.shuffle_order, index);
        if cursor + 1 < len {
            Some(state.shuffle_order[cursor + 1])
        } else if state.repeat == RepeatMode::All {
            // Exhausted the shuffled order — reshuffle for the next cycle.
            state.shuffle_order = build_shuffle_order(len, None, shuffle_seed());
            state.shuffle_order.first().copied()
        } else {
            None
        }
    } else if index + 1 < len {
        Some(index + 1)
    } else if state.repeat == RepeatMode::All {
        Some(0)
    } else {
        None
    };

    match next {
        Some(position) => {
            state.clear_exhaustion_after_advance(Some(position));
            state.position = Some(position);
            state.elapsed_seconds = Some(0.0);
            state.duration_seconds = None;
        }
        // End of the queue: a finished track stops; an explicit Next is a no-op.
        None if stop_at_end => {
            state.status = PlaybackStatus::Stopped;
            state.elapsed_seconds = Some(0.0);
            state.duration_seconds = None;
        }
        None => {}
    }
}

/// Peek the position the server would advance to next, **without mutating** — the `next_up`
/// broadcast hint that lets a prefetching client (the native endpoint) load the next track
/// ahead of the boundary for gapless playback. Mirrors `advance`'s decision but returns
/// `None` where the real next item can't be predicted: the end of a shuffle cycle under
/// repeat-all (where `advance` *reshuffles* with a fresh seed) and the end of the queue with
/// no repeat. Under repeat-one the next playback is the same track, so it returns the current
/// position.
fn peek_next_index(state: &QueueState) -> Option<usize> {
    let pos = state.position?;
    let len = state.queue.len();
    if len == 0 {
        return None;
    }
    if state.repeat == RepeatMode::One {
        return Some(pos);
    }
    if state.shuffle {
        if !shuffle_order_valid(&state.shuffle_order, len, Some(pos)) {
            return None;
        }
        let cursor = shuffle_cursor(&state.shuffle_order, pos);
        // Last in the shuffled cycle: advance reshuffles, so the next track is unpredictable.
        return (cursor + 1 < len).then(|| state.shuffle_order[cursor + 1]);
    }
    if pos + 1 < len {
        Some(pos + 1)
    } else if state.repeat == RepeatMode::All {
        Some(0)
    } else {
        None
    }
}

/// Clamp a requested seek position to the playable range: never below 0, and never past
/// the track duration when it is known.
fn clamp_seek(position_seconds: f64, duration_seconds: Option<f64>) -> f64 {
    let floored = position_seconds.max(0.0);
    match duration_seconds {
        Some(duration) => floored.min(duration),
        None => floored,
    }
}

/// Step to the previous item, following the shuffled play order when shuffle is on.
/// Repeat-all wraps from the first item to the last; otherwise the first item holds.
fn step_previous(state: &mut QueueState) {
    state.elapsed_seconds = Some(0.0);
    state.duration_seconds = None;
    let len = state.queue.len();
    let Some(index) = state.position else {
        return;
    };
    if state.shuffle && len > 0 {
        ensure_shuffle_order(state, index);
        let cursor = shuffle_cursor(&state.shuffle_order, index);
        state.position = if cursor > 0 {
            Some(state.shuffle_order[cursor - 1])
        } else if state.repeat == RepeatMode::All {
            state.shuffle_order.last().copied()
        } else {
            Some(index)
        };
        return;
    }
    state.position = match state.position {
        Some(index) if index > 0 => Some(index - 1),
        Some(_) if state.repeat == RepeatMode::All && len > 0 => Some(len - 1),
        other => other,
    };
}

/// Make sure `state.shuffle_order` is a valid play order over the current queue that still
/// includes `current`; rebuild it (with `current` first) when it's stale (queue changed)
/// or empty.
fn ensure_shuffle_order(state: &mut QueueState, current: usize) {
    if !shuffle_order_valid(&state.shuffle_order, state.queue.len(), Some(current)) {
        state.shuffle_order = build_shuffle_order(state.queue.len(), Some(current), shuffle_seed());
    }
}

/// The position of queue index `index` within the shuffled `order` (0 if absent).
fn shuffle_cursor(order: &[usize], index: usize) -> usize {
    order.iter().position(|&i| i == index).unwrap_or(0)
}

/// A seed for the shuffle PRNG, from the wall clock so each fresh shuffle differs.
fn shuffle_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1
}

/// A shuffled play order over `len` queue indices. When `first` is `Some(i)`, that index
/// leads (so playback continues from the current track) and the rest are Fisher-Yates
/// shuffled; otherwise the whole range is shuffled. `seed` makes it deterministic.
fn build_shuffle_order(len: usize, first: Option<usize>, seed: u64) -> Vec<usize> {
    let lead = first.filter(|&i| i < len);
    let mut rest: Vec<usize> = (0..len).filter(|&i| Some(i) != lead).collect();
    let mut rng = seed | 1; // SplitMix64 state; never 0
    let mut next = || {
        rng = rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    for i in (1..rest.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        rest.swap(i, j);
    }
    let mut order = Vec::with_capacity(len);
    order.extend(lead);
    order.extend(rest);
    order
}

/// Whether `order` is a usable play order over a queue of `len` items that still includes
/// `current`: a permutation of `0..len` (so every track plays once, none twice) containing
/// the currently-playing index.
fn shuffle_order_valid(order: &[usize], len: usize, current: Option<usize>) -> bool {
    if order.len() != len {
        return false;
    }
    let mut seen = vec![false; len];
    for &i in order {
        if i >= len || seen[i] {
            return false;
        }
        seen[i] = true;
    }
    current.is_none_or(|c| c < len && order.contains(&c))
}

/// Resolve library track ids to queue items with relative stream URLs the browser
/// fetches directly. Unknown ids are skipped.
async fn resolve_queue_items(database: &Database, track_ids: &[String]) -> Result<Vec<QueueItem>> {
    let mut items = Vec::with_capacity(track_ids.len());
    for id in track_ids {
        if let Some(track) = database.track(id).await? {
            let artwork_url = database.album_artwork_url(&track.album_id).await?;
            let loudness = database.track_loudness(&track.id).await?;
            let album_loudness = database.album_loudness(&track.album_id).await?;
            items.push(QueueItem {
                track_id: Some(track.id),
                title: track.title,
                artist: track.artist_name,
                album: track.album_title,
                stream_url: track.stream_url,
                artwork_url,
                integrated_loudness_lufs: loudness.map(|(lufs, _)| lufs),
                true_peak_dbtp: loudness.map(|(_, peak)| peak),
                album_integrated_loudness_lufs: album_loudness.map(|(lufs, _)| lufs),
                album_true_peak_dbtp: album_loudness.map(|(_, peak)| peak),
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
                // A different track now occupies this slot — clear the removed track's
                // duration so the seek bar doesn't pair it with the new now-playing.
                state.position = Some(pos.min(state.queue.len() - 1));
                state.elapsed_seconds = Some(0.0);
                state.duration_seconds = None;
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
/// track for gapless Snapcast output. `None` means "stop after this one". When `shuffle`
/// is a valid play order it is followed instead of the sequential queue order.
#[cfg(feature = "snapcast")]
fn next_index(
    position: Option<usize>,
    len: usize,
    repeat: RepeatMode,
    shuffle: &[usize],
) -> Option<usize> {
    let pos = position?;
    if len == 0 {
        return None;
    }
    if repeat == RepeatMode::One {
        return Some(pos);
    }
    if shuffle_order_valid(shuffle, len, Some(pos)) {
        let cursor = shuffle_cursor(shuffle, pos);
        return if cursor + 1 < len {
            Some(shuffle[cursor + 1])
        } else if repeat == RepeatMode::All {
            shuffle.first().copied()
        } else {
            None
        };
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

/// The shuffle play order to pass to [`next_index`]: the live order when shuffle is on,
/// otherwise empty (sequential).
#[cfg(feature = "snapcast")]
fn active_shuffle(state: &QueueState) -> &[usize] {
    if state.shuffle {
        &state.shuffle_order
    } else {
        &[]
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
    persistence: QueuePersistence,
    providers: Arc<RwLock<ProviderRegistry>>,
    /// Kept so the snapserver + FIFO outlive the player; also the control handle home.
    #[allow(dead_code)]
    manager: Arc<SnapcastManager>,
    sample_rate: u32,
    state: Mutex<QueueState>,
    state_tx: broadcast::Sender<PlaybackState>,
    history_tx: broadcast::Sender<ListenSample>,
    last_progress_persist: AtomicI64,
    /// Commands to the FIFO writer thread (std channel: the thread blocks on it while idle).
    writer_tx: std::sync::mpsc::Sender<WriterMsg>,
    /// Writer→player events (advance/drain). Taken once by the control task.
    events_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<WriterEvent>>>,
    /// What the writer currently has loaded, so reconcile avoids re-decoding.
    loaded: Mutex<LoadedTrack>,
    /// Identifies each writer load so an event from a replaced track cannot move the queue.
    writer_generation: AtomicU64,
    audio_tap: Arc<musicata_core::pcm_dsp::AudioTap>,
    writer_alive: Arc<AtomicBool>,
}

/// What the writer thread is currently rendering / has preloaded.
#[cfg(feature = "snapcast")]
#[derive(Clone, Default)]
struct LoadedTrack {
    track_id: Option<String>,
    stream_url: Option<String>,
    position: Option<usize>,
    next_track_id: Option<String>,
    generation: u64,
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
        let (history_tx, _) = broadcast::channel(128);
        let (writer_tx, writer_rx) = std::sync::mpsc::channel();
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let sample_rate = manager.sample_rate();
        let fifo = manager.fifo_path();
        let audio_tap = Arc::new(musicata_core::pcm_dsp::AudioTap::default());
        let writer_tap = audio_tap.clone();
        let writer_alive = Arc::new(AtomicBool::new(true));
        let alive = writer_alive.clone();
        std::thread::Builder::new()
            .name(format!("snapcast-writer-{id}"))
            .spawn(move || {
                struct Liveness(Arc<AtomicBool>);
                impl Drop for Liveness {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let _liveness = Liveness(alive);
                crate::snapcast::run_writer(fifo, writer_rx, events_tx, writer_tap);
            })
            .expect("spawn snapcast writer thread");
        Arc::new(Self {
            persistence: QueuePersistence::new(database.clone(), QueueOwner::Player(id.clone())),
            id,
            database,
            providers,
            manager,
            sample_rate,
            state: Mutex::new(QueueState::default()),
            state_tx,
            history_tx,
            last_progress_persist: AtomicI64::new(0),
            writer_tx,
            events_rx: std::sync::Mutex::new(Some(events_rx)),
            loaded: Mutex::new(LoadedTrack::default()),
            writer_generation: AtomicU64::new(1),
            audio_tap,
            writer_alive,
        })
    }

    /// Set (or clear) the server-side EQ correction applied to the outgoing PCM (before the FIFO).
    pub fn set_dsp(&self, eq: Option<StereoEq>) {
        self.set_output_dsp(eq, 0);
    }

    pub fn set_output_dsp(&self, eq: Option<StereoEq>, revision: u64) {
        let _ = self.writer_tx.send(WriterMsg::SetDsp {
            chain: Box::new(eq),
            revision,
        });
    }

    pub fn writer_alive(&self) -> bool {
        self.writer_alive.load(Ordering::Acquire)
    }

    pub fn audio_tap(&self) -> Arc<musicata_core::pcm_dsp::AudioTap> {
        self.audio_tap.clone()
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn is_online(&self) -> bool {
        true
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PlaybackState> {
        self.state_tx.subscribe()
    }

    pub async fn snapshot(&self) -> PlaybackState {
        self.state.lock().await.snapshot()
    }

    async fn request_autoplay_resume(&self) -> bool {
        let mut state = self.state.lock().await;
        let requested = state.can_request_refill_resume();
        if requested {
            state.request_refill_resume();
        }
        drop(state);
        if requested {
            self.broadcast().await;
        }
        requested
    }

    async fn begin_autoplay_refill(&self, min_upcoming: usize) -> Option<(u64, PlaybackState)> {
        let mut state = self.state.lock().await;
        let refill = state.begin_refill(min_upcoming);
        drop(state);
        if refill.is_some() {
            self.broadcast().await;
        }
        refill
    }

    async fn cancel_autoplay_refill(&self) {
        self.state.lock().await.cancel_refill();
        self.broadcast().await;
    }

    async fn finish_autoplay_refill(
        &self,
        revision: u64,
        track_ids: Vec<String>,
        database: &Database,
        _base_url: &str,
    ) -> Result<()> {
        let items = resolve_queue_items(database, &track_ids).await?;
        let resume = {
            let mut state = self.state.lock().await;
            if state.refill_revision != revision || state.queue_activity.is_none() {
                return Ok(());
            }
            if items.is_empty() {
                state.queue_activity = Some("No more tracks found".into());
                drop(state);
                self.broadcast().await;
                return Ok(());
            }
            let resume = state.resume_refill_at.take();
            if let Some(position) = resume {
                state.position = Some(position);
                state.status = PlaybackStatus::Playing;
                state.elapsed_seconds = Some(0.0);
                state.duration_seconds = None;
            }
            state.queue.extend(items);
            state.queue_activity = None;
            state.refill_started = false;
            QueuePersist::Queue(state.playback(), state.queue.clone())
        };
        self.persist(resume);
        self.broadcast().await;
        self.reconcile(&PlayerCommand::Enqueue { track_ids }).await;
        Ok(())
    }

    async fn broadcast(&self) {
        let state = self.state.lock().await;
        let _ = self.history_tx.send(state.listen_sample());
        let _ = self.state_tx.send(state.snapshot());
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
        apply_restored_snapshot(&mut state, snapshot);
    }

    fn persist(&self, persist: QueuePersist) {
        self.persistence.submit(persist);
    }

    async fn persist_playback(&self) {
        let playback = self.state.lock().await.playback();
        self.persist(QueuePersist::Playback(playback));
    }

    pub async fn execute(
        &self,
        command: PlayerCommand,
        database: &Database,
        _base_url: &str,
    ) -> Result<()> {
        let mutates_queue = command_mutates_queue(&command);
        {
            let mut state = self.state.lock().await;
            let defer_next =
                matches!(command, PlayerCommand::Next) && state.resume_refill_at.is_some();
            if !defer_next {
                if cancels_autoplay_refill(&command) {
                    state.cancel_refill();
                }
                apply_to_queue_state(&mut state, command.clone(), database).await?;
            }
            let playback = state.playback();
            let persist = if mutates_queue {
                QueuePersist::Queue(playback, state.queue.clone())
            } else {
                QueuePersist::Playback(playback)
            };
            self.persist(persist);
        }
        self.broadcast().await;
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
            let next = next_index(
                position,
                state.queue.len(),
                state.repeat,
                active_shuffle(&state),
            )
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
                let mut loaded = self.loaded.lock().await;
                if loaded.stream_url.is_some() {
                    let _ = self.writer_tx.send(WriterMsg::Stop);
                    *loaded = LoadedTrack::default();
                    self.state.lock().await.queue_activity = None;
                } else {
                    let _ = self.writer_tx.send(WriterMsg::SetPlaying(false));
                }
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
            self.reconcile_radio(&item, position).await;
            return;
        };

        let is_seek = matches!(command, PlayerCommand::Seek { .. });
        let same = {
            let loaded = self.loaded.lock().await;
            if loaded.track_id.as_deref() != Some(track_id.as_str()) || loaded.position != position
            {
                false
            } else {
                // `reconcile` releases its initial state snapshot before taking `loaded`.
                // Recheck before waking the writer so Pause/Stop cannot be overtaken by an
                // old reconcile that would otherwise send SetPlaying(true).
                let state = self.state.lock().await;
                let current_matches = state.status == PlaybackStatus::Playing
                    && state.position == position
                    && state
                        .position
                        .and_then(|index| state.queue.get(index))
                        .and_then(|item| item.track_id.as_deref())
                        == Some(track_id.as_str());
                if current_matches {
                    if is_seek {
                        let frame = (elapsed * self.sample_rate as f64) as usize;
                        let _ = self.writer_tx.send(WriterMsg::Seek { frame });
                    }
                    let _ = self.writer_tx.send(WriterMsg::SetPlaying(true));
                }
                current_matches
            }
        };
        if same {
            self.preload(next_item).await;
            return;
        }

        // Disconnect live radio before waiting on a potentially slow library source.
        {
            let mut loaded = self.loaded.lock().await;
            let mut state = self.state.lock().await;
            if state.status != PlaybackStatus::Playing
                || state.position != position
                || state
                    .position
                    .and_then(|i| state.queue.get(i))
                    .and_then(|item| item.track_id.as_deref())
                    != Some(track_id.as_str())
            {
                return;
            }
            if loaded.stream_url.is_some() {
                let _ = self.writer_tx.send(WriterMsg::Stop);
                *loaded = LoadedTrack::default();
                state.queue_activity = None;
            }
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
        {
            let mut loaded = self.loaded.lock().await;
            let mut state = self.state.lock().await;
            if state.status != PlaybackStatus::Playing
                || state.position != position
                || state
                    .position
                    .and_then(|i| state.queue.get(i))
                    .and_then(|item| item.track_id.as_deref())
                    != Some(track_id.as_str())
            {
                return; // A user command overtook the decode; it owns the output now.
            }
            let generation = self.writer_generation.fetch_add(1, Ordering::Relaxed);
            let _ = self.writer_tx.send(WriterMsg::Load {
                track: decoded,
                start_frame: frame,
                gain,
                generation,
            });
            let _ = self.writer_tx.send(WriterMsg::SetPlaying(true));
            loaded.track_id = Some(track_id);
            loaded.stream_url = None;
            loaded.position = position;
            loaded.next_track_id = None;
            loaded.generation = generation;
            state.duration_seconds = Some(duration);
        }
        self.broadcast().await;
        self.preload(next_item).await;
    }

    /// Decode the next queue item and hand it to the writer for gapless continuation —
    /// unless it is already preloaded (so this is cheap to call on every command).
    async fn preload(&self, next_item: Option<QueueItem>) {
        let next_track_id = next_item.as_ref().and_then(|item| item.track_id.clone());
        let generation = {
            let mut loaded = self.loaded.lock().await;
            if loaded.next_track_id == next_track_id {
                return;
            }
            loaded.next_track_id = next_track_id.clone();
            loaded.generation
        };
        let (Some(track_id), Some(item)) = (next_track_id, next_item) else {
            return;
        };
        match self.decode(&track_id).await {
            Ok(decoded) => {
                let loaded = self.loaded.lock().await;
                let state = self.state.lock().await;
                let current_matches = state.status == PlaybackStatus::Playing
                    && state.position == loaded.position
                    && state
                        .position
                        .and_then(|index| state.queue.get(index))
                        .and_then(|item| item.track_id.as_deref())
                        == loaded.track_id.as_deref();
                let actual_next = next_index(
                    state.position,
                    state.queue.len(),
                    state.repeat,
                    active_shuffle(&state),
                )
                .and_then(|index| state.queue.get(index))
                .and_then(|item| item.track_id.as_deref());
                if loaded.generation == generation
                    && loaded.next_track_id.as_deref() == Some(track_id.as_str())
                    && current_matches
                    && actual_next == Some(track_id.as_str())
                {
                    let _ = self.writer_tx.send(WriterMsg::Preload {
                        track: decoded,
                        gain: leveling_gain(&item),
                        generation,
                    });
                }
            }
            Err(error) => {
                tracing::debug!(player = %self.id, track = %track_id, %error, "snapcast: preload decode failed");
            }
        }
    }

    /// Live streams are decoded in the background; never wait for their network on Play.
    async fn reconcile_radio(&self, item: &QueueItem, position: Option<usize>) {
        let generation = {
            let mut loaded = self.loaded.lock().await;
            let mut state = self.state.lock().await;
            if state.status != PlaybackStatus::Playing
                || state.position != position
                || state
                    .position
                    .and_then(|i| state.queue.get(i))
                    .map(|i| i.stream_url.as_str())
                    != Some(item.stream_url.as_str())
            {
                return;
            }
            if loaded.stream_url.as_deref() == Some(item.stream_url.as_str())
                && loaded.position == position
            {
                let _ = self.writer_tx.send(WriterMsg::SetPlaying(true));
                return;
            }
            let generation = self.writer_generation.fetch_add(1, Ordering::Relaxed);
            let _ = self.writer_tx.send(WriterMsg::Stop);
            *loaded = LoadedTrack {
                stream_url: Some(item.stream_url.clone()),
                position,
                generation,
                ..Default::default()
            };
            state.elapsed_seconds = Some(0.0);
            state.duration_seconds = None;
            state.queue_activity = Some("Connecting to radio…".into());
            generation
        };
        self.broadcast().await;
        let url = if let Some(id) = item
            .stream_url
            .strip_prefix("/api/radio/")
            .and_then(|p| p.strip_suffix("/stream"))
            .filter(|id| !id.is_empty() && !id.contains('/'))
        {
            match self.database.radio_station(id).await {
                Ok(Some(station)) => Ok(station.stream_url),
                Ok(None) => Err("This radio station was removed.".to_string()),
                Err(error) => Err(error.to_string()),
            }
        } else {
            Err("Save this station in Browse radio before playing it on Snapcast.".to_string())
        };
        let url = match url {
            Ok(url) => url,
            Err(error) => {
                self.on_stream_failed(generation, error).await;
                return;
            }
        };
        let sent = {
            let loaded = self.loaded.lock().await;
            let state = self.state.lock().await;
            if loaded.generation != generation
                || state.status != PlaybackStatus::Playing
                || state.position != position
                || state
                    .position
                    .and_then(|i| state.queue.get(i))
                    .map(|i| i.stream_url.as_str())
                    != Some(item.stream_url.as_str())
            {
                return;
            }
            let stream = crate::snapcast::stream_radio(url, self.sample_rate);
            self.writer_tx
                .send(WriterMsg::LoadStream { stream, generation })
                .is_ok()
                && self.writer_tx.send(WriterMsg::SetPlaying(true)).is_ok()
        };
        if !sent {
            self.on_stream_failed(
                generation,
                "The Snapcast audio writer is unavailable.".into(),
            )
            .await;
        }
    }

    async fn on_stream_started(&self, generation: u64) {
        {
            let loaded = self.loaded.lock().await;
            let mut state = self.state.lock().await;
            if loaded.generation != generation
                || loaded.stream_url.is_none()
                || state.status != PlaybackStatus::Playing
                || state.position != loaded.position
                || state
                    .position
                    .and_then(|i| state.queue.get(i))
                    .map(|i| &i.stream_url)
                    != loaded.stream_url.as_ref()
            {
                return;
            }
            state.queue_activity = None;
        }
        self.broadcast().await;
    }

    async fn on_stream_failed(&self, generation: u64, error: String) {
        {
            let mut loaded = self.loaded.lock().await;
            let mut state = self.state.lock().await;
            if loaded.generation != generation
                || loaded.stream_url.is_none()
                || state.status != PlaybackStatus::Playing
                || state.position != loaded.position
                || state
                    .position
                    .and_then(|i| state.queue.get(i))
                    .map(|i| &i.stream_url)
                    != loaded.stream_url.as_ref()
            {
                return;
            }
            let _ = self.writer_tx.send(WriterMsg::Stop);
            state.status = PlaybackStatus::Stopped;
            state.queue_activity = Some(format!("Radio playback failed: {error}"));
            *loaded = LoadedTrack::default();
        }
        self.broadcast().await;
        self.persist_playback().await;
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
                        Some(WriterEvent::Advanced { generation }) => self.on_advanced(generation).await,
                        Some(WriterEvent::Drained { generation }) => self.on_drained(generation).await,
                        Some(WriterEvent::StreamStarted { generation }) => self.on_stream_started(generation).await,
                        Some(WriterEvent::StreamFailed { generation, error }) => self.on_stream_failed(generation, error).await,
                        None => break,
                    },
                    _ = ticker.tick() => self.on_tick().await,
                }
            }
        })
    }

    /// The writer rolled gaplessly into the preloaded next track — advance our cursor.
    async fn on_advanced(&self, generation: u64) {
        let (new_position, new_track_id, next_item) = {
            // Keep the output generation locked through the cursor transition. A Load that
            // wins this lock owns the output; a writer event for the superseded Load must
            // leave both the state and loaded marker untouched.
            let mut loaded = self.loaded.lock().await;
            if loaded.generation != generation {
                return;
            }
            let mut state = self.state.lock().await;
            if state.status != PlaybackStatus::Playing
                || state.position != loaded.position
                || state
                    .position
                    .and_then(|index| state.queue.get(index))
                    .and_then(|item| item.track_id.as_deref())
                    != loaded.track_id.as_deref()
            {
                return;
            }
            let new_position = next_index(
                state.position,
                state.queue.len(),
                state.repeat,
                active_shuffle(&state),
            );
            state.clear_exhaustion_after_advance(new_position);
            state.position = new_position;
            state.elapsed_seconds = Some(0.0);
            state.duration_seconds = None;
            let new_track_id = new_position
                .and_then(|index| state.queue.get(index))
                .and_then(|item| item.track_id.clone());
            let next = next_index(
                new_position,
                state.queue.len(),
                state.repeat,
                active_shuffle(&state),
            )
            .and_then(|index| state.queue.get(index).cloned());
            loaded.position = new_position;
            loaded.track_id = new_track_id.clone();
            loaded.stream_url = None;
            loaded.next_track_id = None;
            (new_position, new_track_id, next)
        };
        // Best-effort: set the seek-bar duration for the new current track.
        if let Some(track_id) = &new_track_id
            && let Ok(Some(track)) = self.database.track(track_id).await
        {
            let mut state = self.state.lock().await;
            if state.status == PlaybackStatus::Playing
                && state.position == new_position
                && state
                    .position
                    .and_then(|index| state.queue.get(index))
                    .and_then(|item| item.track_id.as_deref())
                    == Some(track_id.as_str())
            {
                state.duration_seconds = track.duration_seconds;
            }
        }
        self.broadcast().await;
        self.persist_playback().await;
        self.preload(next_item).await;
    }

    /// The queue drained with nothing preloaded — stop.
    async fn on_drained(&self, generation: u64) {
        let advance = {
            // See on_advanced: do not release `loaded` between validating an event and
            // clearing it, or a completed replacement decode can be wiped by this drain.
            let mut loaded = self.loaded.lock().await;
            if loaded.generation != generation {
                return;
            }
            let mut state = self.state.lock().await;
            if state.status != PlaybackStatus::Playing
                || state.position != loaded.position
                || state
                    .position
                    .and_then(|index| state.queue.get(index))
                    .and_then(|item| item.track_id.as_deref())
                    != loaded.track_id.as_deref()
            {
                return;
            }
            let next = next_index(
                state.position,
                state.queue.len(),
                state.repeat,
                active_shuffle(&state),
            );
            if let Some(position) = next {
                state.position = Some(position);
                state.elapsed_seconds = Some(0.0);
                state.duration_seconds = None;
                *loaded = LoadedTrack::default();
                true
            } else {
                if state.queue_activity.is_some()
                    && state
                        .position
                        .is_some_and(|position| position + 1 == state.queue.len())
                {
                    state.preserve_refill_resume();
                }
                state.status = PlaybackStatus::Stopped;
                state.elapsed_seconds = Some(0.0);
                *loaded = LoadedTrack::default();
                false
            }
        };
        self.broadcast().await;
        self.persist_playback().await;
        if advance {
            self.reconcile(&PlayerCommand::Next).await;
        }
    }

    /// Advance the displayed elapsed position once per second while playing, mirroring the
    /// MPD position poll (the audio truth is snapserver's; this is the seek bar + the
    /// listen recorder's view).
    async fn on_tick(&self) {
        let mut state = self.state.lock().await;
        let playback = {
            if state.status != PlaybackStatus::Playing {
                return;
            }
            let mut elapsed = state.elapsed_seconds.unwrap_or(0.0) + 1.0;
            if let Some(duration) = state.duration_seconds
                && elapsed > duration
            {
                elapsed = duration;
            }
            state.elapsed_seconds = Some(elapsed);
            state.playback()
        };
        let _ = self.history_tx.send(state.listen_sample());
        let now = now_unix();
        let last = self.last_progress_persist.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= PROGRESS_PERSIST_SECS
            && self
                .last_progress_persist
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.persist(QueuePersist::Playback(playback));
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

    #[test]
    fn autoplay_prefetch_is_quiet_until_playback_waits_for_tracks() {
        let mut state = QueueState {
            status: PlaybackStatus::Playing,
            queue: vec![QueueItem {
                track_id: Some("seed".into()),
                ..Default::default()
            }],
            position: Some(0),
            ..Default::default()
        };
        let (_, seed) = state.begin_refill(5).expect("prefetch starts");
        assert!(seed.queue_activity.is_none(), "prefetch is not a user wait");
        assert!(state.snapshot().queue_activity.is_none());
        assert!(
            state.begin_refill(5).is_none(),
            "quiet work stays deduplicated"
        );

        state.preserve_refill_resume();
        assert_eq!(
            state.snapshot().queue_activity.as_deref(),
            Some("Finding more tracks…")
        );
        state.cancel_refill();
        assert!(state.snapshot().queue_activity.is_none());

        state.request_refill_resume();
        assert_eq!(
            state.snapshot().queue_activity.as_deref(),
            Some("Finding more tracks…")
        );
        state.cancel_refill();
        assert!(state.snapshot().queue_activity.is_none());
    }

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

    // A busy SQLite writer must not stall the output WebSocket's progress handler
    // or the next Stop command. This uses a real independent write transaction.
    #[tokio::test]
    async fn browser_controls_remain_responsive_while_database_is_locked() {
        use sqlx::Connection;
        let path = temp_db("busy-playback");
        let database = Database::connect(&path).await.unwrap();
        let player = BrowserPlayer::new(database.clone(), "busy-browser".into());
        player
            .execute(
                PlayerCommand::PlayStream {
                    url: "/radio".into(),
                    title: "Radio".into(),
                },
                &database,
            )
            .await
            .unwrap();
        let mut lock = sqlx::SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut lock)
            .await
            .unwrap();
        let mut updates = player.subscribe();
        let result = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            player
                .execute(
                    PlayerCommand::PlayStream {
                        url: "/new-radio".into(),
                        title: "New radio".into(),
                    },
                    &database,
                )
                .await
                .unwrap();
            assert_eq!(
                updates.recv().await.unwrap().status,
                PlaybackStatus::Playing
            );
            player.report_progress(12.0, Some(60.0)).await;
            player
                .execute(PlayerCommand::Stop, &database)
                .await
                .unwrap();
            assert_eq!(
                updates.recv().await.unwrap().status,
                PlaybackStatus::Stopped
            );
        })
        .await;
        sqlx::query("ROLLBACK").execute(&mut lock).await.unwrap();
        assert!(result.is_ok(), "database persistence blocked progress/Stop");
        let saved = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(saved) = database.load_player_queue("busy-browser").await.unwrap()
                    && saved.playback.status == PlaybackStatus::Stopped
                {
                    break saved;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            saved.items.len(),
            1,
            "progress/Stop must preserve the queue"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn removed_zone_cannot_checkpoint_from_a_surviving_controller() {
        let path = temp_db("deleted-zone-checkpoint");
        let database = Database::connect(&path).await.unwrap();
        let manager = PlayerManager::load(
            database.clone(),
            "http://localhost".into(),
            Arc::new(RwLock::new(ProviderRegistry::new())),
        )
        .await
        .unwrap();
        let zone = manager.create_zone("Temporary").await.unwrap();
        let stale = manager.get_zone(&zone.id).await.unwrap();
        manager
            .command_zone(
                &zone.id,
                PlayerCommand::PlayStream {
                    url: "/radio".into(),
                    title: "Radio".into(),
                },
            )
            .await
            .unwrap();
        manager.delete_zone(&zone.id).await.unwrap();
        // A connected controller may still hold this Arc after removal.
        stale.report_progress(20.0, Some(60.0)).await;
        assert!(database.load_zone_queue(&zone.id).await.unwrap().is_none());
        let recreated = manager.create_zone("Temporary").await.unwrap();
        assert!(
            manager
                .zone_state(&recreated.id)
                .await
                .unwrap()
                .queue
                .is_empty()
        );
        let _ = std::fs::remove_file(path);
    }

    /// The recorder runs in a background task, so poll the history until it has at
    /// least `expected` rows (or fail after a generous timeout). Returns track ids,
    /// most-recent first.
    /// Play `id` on the browser player and report progress past the completion threshold,
    /// retrying the progress tick until the async recorder confirms the listen. This is robust
    /// to the recorder consuming the (separate-channel) progress tick before its state frame, or
    /// dropping a frame under load — both of which only bite the test's one-shot ticks, not
    /// production's continuous progress stream. Waiting per track keeps at most one track's
    /// frames in flight, so there's no cross-track mis-attribution.
    async fn play_until_listened(
        browser: &PlayerHandle,
        player: &BrowserPlayer,
        database: &Database,
        id: &str,
    ) {
        play(browser, database, id).await;
        for _ in 0..200 {
            player.report_progress(95.0, Some(180.0)).await;
            let recent = database.recently_played(50).await.expect("recent");
            if recent.iter().any(|(track, _)| track.id == id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("listen for {id} was never recorded");
    }

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

        // Play each track and drive it past the completion threshold, confirming its listen
        // landed before moving on (the recorder reads state + progress over two independent
        // broadcast channels, so a one-shot progress tick can otherwise race ahead of its state
        // frame under load — see `play_until_listened`).
        for id in ["track_1", "track_2", "track_3", "track_4", "track_5"] {
            play_until_listened(&browser, player, &database, id).await;
        }

        let recent: std::collections::BTreeSet<String> = database
            .recently_played(50)
            .await
            .expect("recent")
            .into_iter()
            .map(|(track, _)| track.id)
            .collect();
        for id in ["track_1", "track_2", "track_3", "track_4", "track_5"] {
            assert!(recent.contains(id), "recent missing {id}; got {recent:?}");
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

    // A player handle advertises its transport capabilities (M10), so the descriptor
    // reports the backend's own set instead of a hardcoded constant.
    #[tokio::test]
    async fn browser_handle_advertises_full_capabilities() {
        let db_path = temp_db("capabilities");
        let database = Database::connect(&db_path).await.expect("connect");
        let player = Arc::new(BrowserPlayer::new(database, BROWSER_PLAYER_ID.to_string()));
        let handle = PlayerHandle::Browser(player);
        assert_eq!(handle.capabilities(), PlayerCapabilities::FULL);
        assert!(PlayerCapabilities::FULL.seek);
        assert!(PlayerCapabilities::FULL.queue);
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

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(saved) = database.load_player_queue(BROWSER_PLAYER_ID).await.unwrap()
                    && saved.items.len() == 3
                    && saved.playback.repeat == RepeatMode::All
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("queue checkpoint before restart");

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

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(saved) = database.load_zone_queue(&zone_id).await.unwrap()
                    && saved.items.len() == 3
                    && saved.playback.position == Some(1)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("zone checkpoint before restart");

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

    #[tokio::test]
    async fn mpd_library_urls_include_stream_credentials() {
        let db_path = temp_db("mpd-stream-auth");
        let database = Database::connect(&db_path).await.unwrap();
        let mut library = library_with_tracks(1);
        database.save_library(&mut library).await.unwrap();
        let stream_auth = MpdStreamAuth::new();
        let urls = resolve_urls(
            &database,
            "http://host:3030",
            &["track_1".into()],
            &stream_auth,
        )
        .await
        .unwrap();
        assert!(
            urls[0].contains("?token="),
            "MPD needs credentials to fetch audio"
        );
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn adopted_mpd_queue_does_not_expose_stream_credentials() {
        let db_path = temp_db("mpd-adopt-auth");
        let database = Database::connect(&db_path).await.unwrap();
        let mut library = library_with_tracks(1);
        database.save_library(&mut library).await.unwrap();
        let mut state = PlaybackState::default();
        state.queue = vec![QueueItem {
            track_id: Some("track_1".into()),
            stream_url: "http://old-host/api/tracks/track_1/stream?token=old-secret".into(),
            ..Default::default()
        }];
        enrich_state(&mut state, &database).await;
        assert_eq!(state.queue[0].stream_url, "/api/tracks/track_1/stream");
        let _ = std::fs::remove_file(db_path);
    }

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
        let mut items = items;
        items.push(QueueItem {
            stream_url: "/api/radio/station-1/stream".into(),
            ..Default::default()
        });
        items[0].stream_url = "http://old-host/api/tracks/t1/stream?token=expired".into();
        let stream_auth = MpdStreamAuth::new();
        let uris = queue_to_mpd_uris(&items, "http://host:3030", &stream_auth);
        assert_eq!(uris[0], stream_auth.track_url("http://host:3030", "t1"));
        assert_eq!(uris[1], "http://radio.example/stream");
        assert!(uris[2].starts_with("http://host:3030/api/radio/station-1/stream?token="));
        let token = uris[2].split_once("?token=").unwrap().1;
        assert!(stream_auth.accepts(token));
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
            shuffle_order: Vec::new(),
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
            MpdStreamAuth::new(),
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
            MpdStreamAuth::new(),
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

    #[tokio::test]
    async fn mpd_queue_commands_do_not_mutate_state_out_of_order() {
        use std::future::{Future, poll_fn};
        use std::task::Poll;
        let db_path = temp_db("mpd-command-order");
        let database = Database::connect(&db_path).await.unwrap();
        let player = MpdPlayer::new(
            "mpd-order".into(),
            "127.0.0.1:1".into(),
            database.clone(),
            "http://host".into(),
            MpdStreamAuth::new(),
        );
        // Hold the transport so the first command is in flight while a replacement arrives.
        let _connection = player.connection.lock().await;
        let first = player.execute(PlayerCommand::Clear, &database, "http://host");
        tokio::pin!(first);
        assert!(poll_fn(|cx| Poll::Ready(first.as_mut().poll(cx).is_pending())).await);
        let second = player.execute(
            PlayerCommand::PlayStream {
                url: "http://radio.example/new".into(),
                title: "Replacement".into(),
            },
            &database,
            "http://host",
        );
        tokio::pin!(second);
        assert!(poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx).is_pending())).await);
        assert!(
            player.snapshot().await.queue.is_empty(),
            "the waiting command must not overwrite the in-flight command's queue"
        );
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn mpd_terminal_next_waits_for_a_pending_refill_without_sending_next() {
        let db_path = temp_db("mpd-pending-refill");
        let database = Database::connect(&db_path).await.unwrap();
        let player = MpdPlayer::new(
            "mpd-pending".into(),
            "unused".into(),
            database.clone(),
            "http://host".into(),
            MpdStreamAuth::new(),
        );
        {
            let mut state = player.state.lock().await;
            state.queue = vec![QueueItem {
                track_id: Some("seed".into()),
                stream_url: "http://radio.example/seed".into(),
                ..Default::default()
            }];
            state.position = Some(0);
            state.status = PlaybackStatus::Playing;
        }
        assert!(player.request_autoplay_resume().await);
        assert!(player.begin_autoplay_refill(5).await.is_some());
        let (connection, script) = scripted_mpd(vec![
            ("status".into(), "playlist: 0\nstate: stop\n".into()),
            ("currentsong".into(), String::new()),
        ])
        .await;
        *player.connection.lock().await = Some(connection);
        player
            .execute(PlayerCommand::Next, &database, "http://host")
            .await
            .unwrap();
        script.await.unwrap(); // Script contains no `next`: terminal intent held locally.
        assert_eq!(player.state.lock().await.resume_refill_at, Some(1));
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn mpd_refill_restarts_at_appended_track_if_daemon_drains_after_add() {
        let db_path = temp_db("mpd-refill-drain");
        let database = Database::connect(&db_path).await.unwrap();
        let mut library = library_with_tracks(2);
        database.save_library(&mut library).await.unwrap();
        let player = MpdPlayer::new(
            "mpd-refill".into(),
            "unused".into(),
            database.clone(),
            "http://host".into(),
            MpdStreamAuth::new(),
        );
        {
            let mut state = player.state.lock().await;
            state.queue = resolve_queue_items(&database, &["track_1".into()])
                .await
                .unwrap();
            state.position = Some(0);
            state.status = PlaybackStatus::Playing;
        }
        let (revision, _) = player.begin_autoplay_refill(5).await.unwrap();
        let appended = player.stream_auth.track_url("http://host", "track_2");
        let (connection, script) = scripted_mpd(vec![
            (format!("add \"{appended}\""), String::new()),
            ("status".into(), "playlist: 1\nstate: stop\n".into()),
            ("currentsong".into(), String::new()),
            ("play 1".into(), String::new()),
            (
                "status".into(),
                "playlist: 1\nstate: play\nsong: 1\n".into(),
            ),
            ("currentsong".into(), format!("file: {appended}\n")),
        ])
        .await;
        *player.connection.lock().await = Some(connection);
        player
            .finish_autoplay_refill(revision, vec!["track_2".into()], &database, "http://host")
            .await
            .unwrap();
        script.await.unwrap();
        assert_eq!(player.snapshot().await.queue_position, Some(1));
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn autoplay_pending_next_resumes_at_the_first_appended_track_and_cancels_stale_work() {
        let db_path = temp_db("autoplay-pending-next");
        let database = Database::connect(&db_path).await.unwrap();
        let mut library = library_with_tracks(3);
        database.save_library(&mut library).await.unwrap();
        let player = Arc::new(BrowserPlayer::new(database.clone(), "autoplay-test".into()));
        {
            let mut state = player.state.lock().await;
            state.queue = resolve_queue_items(&database, &["track_1".into()])
                .await
                .unwrap();
            state.position = Some(0);
            state.status = PlaybackStatus::Playing;
        }
        let handle = PlayerHandle::Browser(player.clone());
        assert!(handle.request_autoplay_resume().await);
        let (revision, _) = handle.begin_autoplay_refill(5).await.unwrap();
        handle
            .execute(PlayerCommand::Next, &database, "")
            .await
            .unwrap();
        handle
            .finish_autoplay_refill(revision, vec!["track_2".into()], &database, "")
            .await
            .unwrap();
        let state = player.snapshot().await;
        assert_eq!(state.queue_position, Some(1));
        assert_eq!(
            state.now_playing.unwrap().track_id.as_deref(),
            Some("track_2")
        );
        assert!(state.queue_activity.is_none());

        let (revision, _) = handle.begin_autoplay_refill(5).await.unwrap();
        handle
            .execute(PlayerCommand::Stop, &database, "")
            .await
            .unwrap();
        handle
            .finish_autoplay_refill(revision, vec!["track_3".into()], &database, "")
            .await
            .unwrap();
        assert_eq!(
            player.snapshot().await.queue.len(),
            2,
            "stop cancels stale refill"
        );

        for (command, expected_len) in [
            (PlayerCommand::Pause, 1),
            (PlayerCommand::Clear, 0),
            (
                PlayerCommand::PlayTracks {
                    track_ids: vec!["track_1".into()],
                    start_index: 0,
                },
                1,
            ),
        ] {
            {
                let mut state = player.state.lock().await;
                state.queue = resolve_queue_items(&database, &["track_1".into()])
                    .await
                    .unwrap();
                state.position = Some(0);
                state.status = PlaybackStatus::Playing;
            }
            let (revision, _) = handle.begin_autoplay_refill(5).await.unwrap();
            handle.execute(command, &database, "").await.unwrap();
            handle
                .finish_autoplay_refill(revision, vec!["track_2".into()], &database, "")
                .await
                .unwrap();
            assert_eq!(
                player.snapshot().await.queue.len(),
                expected_len,
                "user command cancels refill"
            );
        }
        {
            let mut state = player.state.lock().await;
            state.status = PlaybackStatus::Playing;
            state.position = Some(0);
        }
        let (revision, _) = handle.begin_autoplay_refill(5).await.unwrap();
        player.track_ended().await;
        handle
            .finish_autoplay_refill(revision, vec!["track_2".into()], &database, "")
            .await
            .unwrap();
        assert_eq!(
            player.snapshot().await.queue_position,
            Some(1),
            "natural drain resumes refill"
        );

        let (revision, _) = handle.begin_autoplay_refill(5).await.unwrap();
        handle
            .finish_autoplay_refill(revision, Vec::new(), &database, "")
            .await
            .unwrap();
        assert_eq!(
            player.snapshot().await.queue_activity.as_deref(),
            Some("No more tracks found")
        );
        assert!(
            handle.begin_autoplay_refill(5).await.is_none(),
            "empty results do not retry each poll"
        );
        player.track_ended().await;
        assert!(
            handle.begin_autoplay_refill(5).await.is_none(),
            "draining an exhausted mix does not retry"
        );
        assert!(
            handle.request_autoplay_resume().await,
            "Next explicitly retries an exhausted mix"
        );
        let (revision, seed) = handle.begin_autoplay_refill(5).await.unwrap();
        assert_eq!(seed.queue_position, Some(1));
        handle
            .finish_autoplay_refill(revision, vec!["track_3".into()], &database, "")
            .await
            .unwrap();
        assert_eq!(player.snapshot().await.queue_position, Some(2));
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn zone_autoplay_refill_serializes_next_drain_and_stop() {
        let db_path = temp_db("zone-autoplay-pending");
        let database = Database::connect(&db_path).await.unwrap();
        let mut library = library_with_tracks(3);
        database.save_library(&mut library).await.unwrap();
        let manager = PlayerManager::load(
            database.clone(),
            "http://host".into(),
            Arc::new(RwLock::new(ProviderRegistry::new())),
        )
        .await
        .unwrap();
        let zone = manager.create_zone("Pending zone").await.unwrap();
        manager
            .command_zone(
                &zone.id,
                PlayerCommand::PlayTracks {
                    track_ids: vec!["track_1".into()],
                    start_index: 0,
                },
            )
            .await
            .unwrap();
        let (revision, _) = manager
            .begin_zone_autoplay_refill(&zone.id, 5)
            .await
            .unwrap()
            .unwrap();
        assert!(
            manager
                .request_zone_autoplay_resume(&zone.id)
                .await
                .unwrap()
        );
        manager
            .command_zone(&zone.id, PlayerCommand::Next)
            .await
            .unwrap();
        manager
            .finish_zone_autoplay_refill(&zone.id, revision, vec!["track_2".into()])
            .await
            .unwrap();
        assert_eq!(
            manager.zone_state(&zone.id).await.unwrap().queue_position,
            Some(1)
        );

        let (revision, _) = manager
            .begin_zone_autoplay_refill(&zone.id, 5)
            .await
            .unwrap()
            .unwrap();
        manager
            .command_zone(&zone.id, PlayerCommand::Stop)
            .await
            .unwrap();
        manager
            .finish_zone_autoplay_refill(&zone.id, revision, vec!["track_3".into()])
            .await
            .unwrap();
        assert_eq!(manager.zone_state(&zone.id).await.unwrap().queue.len(), 2);
        let _ = std::fs::remove_file(db_path);
    }

    async fn scripted_mpd(
        script: Vec<(String, String)>,
    ) -> (MpdConnection, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (read, mut write) = socket.into_split();
            write.write_all(b"OK MPD 0.24.0\n").await.unwrap();
            let mut lines = BufReader::new(read).lines();
            for (expected, response) in script {
                let command = lines.next_line().await.unwrap().expect("MPD command");
                assert_eq!(command, expected);
                write
                    .write_all(format!("{response}OK\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        (
            MpdConnection::connect(&addr.to_string()).await.unwrap(),
            task,
        )
    }

    #[tokio::test]
    async fn mpd_metadata_update_keeps_queue_and_accepts_advanced_cursor() {
        let db_path = temp_db("mpd-metadata-event");
        let database = Database::connect(&db_path).await.unwrap();
        let mut library = library_with_tracks(2);
        database.save_library(&mut library).await.unwrap();
        let player = MpdPlayer::new(
            "mpd-tags".into(),
            "unused".into(),
            database.clone(),
            "http://host".into(),
            MpdStreamAuth::new(),
        );
        {
            let mut state = player.state.lock().await;
            state.queue = resolve_queue_items(&database, &["track_1".into(), "track_2".into()])
                .await
                .unwrap();
            state.position = Some(0);
            state.status = PlaybackStatus::Playing;
        }
        let urls = queue_to_mpd_uris(
            &player.state.lock().await.queue,
            "http://host",
            &player.stream_auth,
        );
        let (connection, script) = scripted_mpd(vec![
            (
                "status".into(),
                "playlist: 7\nstate: play\nsong: 1\nelapsed: 0.5\n".into(),
            ),
            ("currentsong".into(), format!("file: {}\n", urls[1])),
            (
                "playlistinfo".into(),
                format!("file: {}\nTitle: New tag\nfile: {}\n", urls[0], urls[1]),
            ),
        ])
        .await;
        *player.connection.lock().await = Some(connection);
        let state = tokio::time::timeout(Duration::from_secs(2), player.state(&database))
            .await
            .unwrap()
            .unwrap();
        script.await.unwrap();
        assert_eq!(state.queue_position, Some(1));
        assert_eq!(state.elapsed_seconds, Some(0.5));
        assert_eq!(
            state.now_playing.unwrap().track_id.as_deref(),
            Some("track_2")
        );
        assert_eq!(player.expected_playlist_version.load(Ordering::Relaxed), 7);
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn mpd_state_poll_repairs_real_queue_edits_before_acknowledging_version() {
        let db_path = temp_db("mpd-real-edit");
        let database = Database::connect(&db_path).await.unwrap();
        let player = MpdPlayer::new(
            "mpd-edit".into(),
            "unused".into(),
            database.clone(),
            "http://host".into(),
            MpdStreamAuth::new(),
        );
        // The authoritative queue is empty, while another MPD client added a song.
        let (connection, script) = scripted_mpd(vec![
            ("status".into(), "playlist: 7\nstate: stop\n".into()),
            ("currentsong".into(), String::new()),
            (
                "playlistinfo".into(),
                "file: http://external.example/song\n".into(),
            ),
            ("clear".into(), String::new()),
            ("status".into(), "playlist: 8\nstate: stop\n".into()),
            ("currentsong".into(), String::new()),
        ])
        .await;
        *player.connection.lock().await = Some(connection);
        let state = tokio::time::timeout(Duration::from_secs(2), player.state(&database))
            .await
            .unwrap()
            .unwrap();
        script.await.unwrap();
        assert!(state.queue.is_empty());
        assert_eq!(player.expected_playlist_version.load(Ordering::Relaxed), 8);
        let _ = std::fs::remove_file(db_path);
    }

    // ---- Shuffle helpers (pure) ----

    #[test]
    fn build_shuffle_order_is_a_permutation_with_current_first() {
        let order = build_shuffle_order(6, Some(3), 0xC0FFEE);
        assert_eq!(order.len(), 6);
        assert_eq!(
            order[0], 3,
            "the current track leads so playback continues from it"
        );
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec![0, 1, 2, 3, 4, 5],
            "every index appears exactly once"
        );
    }

    #[test]
    fn build_shuffle_order_is_deterministic_and_not_sequential() {
        // Same seed → same order (so it can be persisted/restored); and it actually
        // permutes rather than returning the sequential order.
        let a = build_shuffle_order(8, None, 42);
        let b = build_shuffle_order(8, None, 42);
        assert_eq!(a, b);
        assert_ne!(
            a,
            vec![0, 1, 2, 3, 4, 5, 6, 7],
            "shuffle must reorder the queue"
        );
    }

    fn shuffled_state(order: Vec<usize>, position: usize, repeat: RepeatMode) -> QueueState {
        QueueState {
            status: PlaybackStatus::Playing,
            queue: vec![QueueItem::default(); order.len()],
            position: Some(position),
            shuffle: true,
            shuffle_order: order,
            repeat,
            ..Default::default()
        }
    }

    #[test]
    fn advance_and_previous_follow_the_shuffle_order() {
        // BUG-05: with shuffle on, next/prev must walk the shuffled order, not queue order.
        let mut state = shuffled_state(vec![0, 2, 1, 3], 0, RepeatMode::Off);
        advance(&mut state, false);
        assert_eq!(
            state.position,
            Some(2),
            "next after 0 is the next in shuffle order"
        );
        advance(&mut state, false);
        assert_eq!(state.position, Some(1));
        step_previous(&mut state);
        assert_eq!(
            state.position,
            Some(2),
            "previous walks the shuffle order backwards"
        );
    }

    #[test]
    fn shuffle_advance_stops_at_end_without_repeat() {
        let mut state = shuffled_state(vec![0, 2, 1, 3], 3, RepeatMode::Off);
        advance(&mut state, true); // last in order, track finished
        assert_eq!(state.status, PlaybackStatus::Stopped);
    }

    #[test]
    fn shuffle_advance_reshuffles_at_end_with_repeat_all() {
        let mut state = shuffled_state(vec![0, 2, 1, 3], 3, RepeatMode::All);
        advance(&mut state, true);
        // It keeps playing (a fresh cycle) rather than stopping, and lands on a valid index.
        assert_eq!(state.status, PlaybackStatus::Playing);
        let pos = state.position.expect("a next track");
        assert!(pos < 4);
        assert!(
            shuffle_order_valid(&state.shuffle_order, 4, Some(pos)),
            "a fresh shuffle order is generated for the next cycle"
        );
    }

    fn sequential_state(count: usize, position: usize, repeat: RepeatMode) -> QueueState {
        QueueState {
            status: PlaybackStatus::Playing,
            queue: vec![QueueItem::default(); count],
            position: Some(position),
            repeat,
            ..Default::default()
        }
    }

    #[test]
    fn next_on_last_track_without_repeat_does_not_restart() {
        // ISS-01: an explicit Next at the end (no repeat) must be a no-op, not reset elapsed.
        let mut state = sequential_state(2, 1, RepeatMode::Off);
        state.elapsed_seconds = Some(50.0);
        state.duration_seconds = Some(180.0);
        advance(&mut state, false);
        assert_eq!(state.position, Some(1), "stays on the last track");
        assert_eq!(state.elapsed_seconds, Some(50.0), "does not restart from 0");
        assert_eq!(state.duration_seconds, Some(180.0));
        assert_eq!(state.status, PlaybackStatus::Playing);
    }

    #[test]
    fn track_end_on_last_track_without_repeat_stops() {
        // The other advance caller (a track finished) still stops at the end.
        let mut state = sequential_state(2, 1, RepeatMode::Off);
        advance(&mut state, true);
        assert_eq!(state.status, PlaybackStatus::Stopped);
    }

    #[test]
    fn previous_wraps_to_last_with_repeat_all() {
        // ISS-02: Previous on the first track with repeat-all wraps to the last.
        let mut state = sequential_state(3, 0, RepeatMode::All);
        step_previous(&mut state);
        assert_eq!(state.position, Some(2));
    }

    #[test]
    fn remove_current_item_clears_stale_duration() {
        // ISS-04: removing the playing item advances onto a new track, so its old
        // duration must be cleared (not paired with the new now-playing).
        let mut state = sequential_state(3, 1, RepeatMode::Off);
        state.elapsed_seconds = Some(30.0);
        state.duration_seconds = Some(200.0);
        remove_queue_item(&mut state, 1);
        assert_eq!(
            state.position,
            Some(1),
            "the item that shifted into slot 1 now plays"
        );
        assert_eq!(state.duration_seconds, None, "stale duration cleared");
        assert_eq!(state.elapsed_seconds, Some(0.0));
    }

    #[test]
    fn seek_clamps_to_track_bounds() {
        // ISS-03: a seek is clamped to [0, duration]; with unknown duration only floored at 0.
        assert_eq!(clamp_seek(-10.0, Some(180.0)), 0.0);
        assert_eq!(clamp_seek(999.0, Some(180.0)), 180.0);
        assert_eq!(clamp_seek(42.0, Some(180.0)), 42.0);
        assert_eq!(clamp_seek(-5.0, None), 0.0);
        assert_eq!(clamp_seek(5000.0, None), 5000.0);
    }

    #[test]
    fn shuffle_order_valid_detects_stale_orders() {
        assert!(shuffle_order_valid(&[2, 0, 1], 3, Some(0)));
        // Wrong length (queue changed).
        assert!(!shuffle_order_valid(&[2, 0, 1], 4, Some(0)));
        // Not a permutation (duplicate).
        assert!(!shuffle_order_valid(&[2, 2, 1], 3, Some(0)));
        // Current track no longer present (shouldn't happen, but guard).
        assert!(!shuffle_order_valid(&[2, 0, 1], 3, Some(9)));
    }

    #[test]
    fn peek_next_index_predicts_the_prefetch_hint() {
        let base = |position, repeat, shuffle, order: &[usize]| QueueState {
            queue: vec![
                QueueItem::default(),
                QueueItem::default(),
                QueueItem::default(),
            ],
            position: Some(position),
            repeat,
            shuffle,
            shuffle_order: order.to_vec(),
            ..Default::default()
        };
        // Linear: advance; stop at the end with no repeat; wrap with repeat-all.
        assert_eq!(
            peek_next_index(&base(0, RepeatMode::Off, false, &[])),
            Some(1)
        );
        assert_eq!(peek_next_index(&base(2, RepeatMode::Off, false, &[])), None);
        assert_eq!(
            peek_next_index(&base(2, RepeatMode::All, false, &[])),
            Some(0)
        );
        // Repeat-one: the same track plays next.
        assert_eq!(
            peek_next_index(&base(1, RepeatMode::One, false, &[])),
            Some(1)
        );
        // Shuffle, mid-cycle: follow the shuffled order.
        assert_eq!(
            peek_next_index(&base(1, RepeatMode::Off, true, &[1, 0, 2])),
            Some(0)
        );
        // Shuffle, last in the cycle under repeat-all: unpredictable (advance reshuffles).
        assert_eq!(
            peek_next_index(&base(2, RepeatMode::All, true, &[1, 0, 2])),
            None
        );
        // No position / empty queue: nothing to prefetch.
        let mut none_pos = base(0, RepeatMode::All, false, &[]);
        none_pos.position = None;
        assert_eq!(peek_next_index(&none_pos), None);
    }

    // ---- Snapcast helpers (pure) ----

    #[cfg(feature = "snapcast")]
    #[test]
    fn next_index_honors_repeat_mode() {
        // Off: advance, then stop at the end.
        assert_eq!(next_index(Some(0), 3, RepeatMode::Off, &[]), Some(1));
        assert_eq!(next_index(Some(2), 3, RepeatMode::Off, &[]), None);
        // All: wrap around.
        assert_eq!(next_index(Some(2), 3, RepeatMode::All, &[]), Some(0));
        // One: repeat the same track.
        assert_eq!(next_index(Some(1), 3, RepeatMode::One, &[]), Some(1));
        // No position / empty queue: nothing to play next.
        assert_eq!(next_index(None, 3, RepeatMode::All, &[]), None);
        assert_eq!(next_index(Some(0), 0, RepeatMode::All, &[]), None);
    }

    #[cfg(feature = "snapcast")]
    #[test]
    fn next_index_follows_shuffle_order() {
        let order = [1, 0, 2];
        // After 1 comes 0 (the next entry in the shuffled order), not 2.
        assert_eq!(next_index(Some(1), 3, RepeatMode::Off, &order), Some(0));
        // Off: stop at the end of the shuffled order.
        assert_eq!(next_index(Some(2), 3, RepeatMode::Off, &order), None);
        // All: wrap to the first of the shuffled order.
        assert_eq!(next_index(Some(2), 3, RepeatMode::All, &order), Some(1));
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

    #[cfg(feature = "snapcast")]
    #[tokio::test]
    async fn snapcast_drain_after_refill_advances_appended_track_and_ignores_stale_event() {
        let path = temp_db("snapcast-drain-refill-race");
        let database = Database::connect(&path).await.unwrap();
        let mut library = library_with_tracks(2);
        database.save_library(&mut library).await.unwrap();
        let manager = PlayerManager::load(
            database.clone(),
            "http://localhost".into(),
            Arc::new(RwLock::new(ProviderRegistry::new())),
        )
        .await
        .unwrap();
        let settings = crate::snapcast::SnapcastSettings {
            manage_server: false,
            fifo_path: path.with_extension("fifo"),
            ..Default::default()
        };
        let snap = Arc::new(SnapcastManager::start(settings).await.unwrap());
        let fifo_reader = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(snap.fifo_path())
            .unwrap();
        manager.enable_snapcast(snap).await.unwrap();
        let player = match manager.get(SNAPCAST_PLAYER_ID).await.unwrap() {
            PlayerHandle::Snapcast(player) => player,
            _ => unreachable!("Snapcast handle"),
        };
        {
            let mut state = player.state.lock().await;
            state.queue = resolve_queue_items(&database, &["track_1".into(), "track_2".into()])
                .await
                .unwrap();
            state.position = Some(0);
            state.status = PlaybackStatus::Playing;
        }
        *player.loaded.lock().await = LoadedTrack {
            track_id: Some("track_1".into()),
            position: Some(0),
            next_track_id: None,
            generation: 41,
            ..Default::default()
        };

        // The writer drained just after a refill appended track_2 but before it consumed the
        // preload. The event must advance to that appended item instead of stopping.
        player.on_drained(41).await;
        let state = player.snapshot().await;
        assert_eq!(state.queue_position, Some(1));
        assert_eq!(state.status, PlaybackStatus::Playing);

        // A queued drain from the replaced output must not move the new output, even when it
        // happens to be at the same queue index and track as the newly loaded state.
        *player.loaded.lock().await = LoadedTrack {
            track_id: Some("track_2".into()),
            position: Some(1),
            next_track_id: None,
            generation: 42,
            ..Default::default()
        };
        player.on_drained(41).await;
        assert_eq!(player.snapshot().await.queue_position, Some(1));

        drop(fifo_reader);
        drop(manager);
        let _ = std::fs::remove_file(path.with_extension("fifo"));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "snapcast")]
    #[tokio::test]
    async fn snapcast_saved_radio_reaches_fifo_with_volume_meter_and_live_controls() {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/live.wav", listener.local_addr().unwrap());
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let opened = connections.clone();
        let server = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted.unwrap();
                        opened.fetch_add(1, Ordering::Relaxed);
                        clients.spawn(async move {
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            let mut wave = Vec::new();
                            wave.extend_from_slice(b"RIFF"); wave.extend_from_slice(&0x7fff0024u32.to_le_bytes());
                            wave.extend_from_slice(b"WAVEfmt "); wave.extend_from_slice(&16u32.to_le_bytes());
                            wave.extend_from_slice(&1u16.to_le_bytes()); wave.extend_from_slice(&1u16.to_le_bytes());
                            wave.extend_from_slice(&44100u32.to_le_bytes()); wave.extend_from_slice(&88200u32.to_le_bytes());
                            wave.extend_from_slice(&2u16.to_le_bytes()); wave.extend_from_slice(&16u16.to_le_bytes());
                            wave.extend_from_slice(b"data"); wave.extend_from_slice(&0x7fff0000u32.to_le_bytes());
                            if socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n").await.is_err() { return; }
                            if socket.write_all(&wave).await.is_err() { return; }
                            let pcm: Vec<u8> = (0..2048).flat_map(|_| 16000i16.to_le_bytes()).collect();
                            loop {
                                if socket.write_all(&pcm).await.is_err() { break; }
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                        });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        let path = temp_db("snapcast-radio");
        let database = Database::connect(&path).await.unwrap();
        let station = database
            .create_radio_station("Test Radio", &url, None, 1)
            .await
            .unwrap();
        let manager = PlayerManager::load(
            database.clone(),
            "http://localhost".into(),
            Arc::new(RwLock::new(ProviderRegistry::new())),
        )
        .await
        .unwrap();
        let snap = Arc::new(
            SnapcastManager::start(crate::snapcast::SnapcastSettings {
                manage_server: false,
                fifo_path: path.with_extension("fifo"),
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let mut reader = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(snap.fifo_path())
            .unwrap();
        manager.enable_snapcast(snap).await.unwrap();
        let player = match manager.get(SNAPCAST_PLAYER_ID).await.unwrap() {
            PlayerHandle::Snapcast(p) => p,
            _ => unreachable!(),
        };
        player
            .execute(
                PlayerCommand::SetVolume { volume: 50 },
                &database,
                "http://localhost",
            )
            .await
            .unwrap();
        let profile = musicata_core::dsp::DspProfile {
            id: "radio".into(),
            name: "Radio".into(),
            preamp_db: -6.,
            bands: vec![],
            kind: None,
            room_ir: None,
        };
        player.set_output_dsp(StereoEq::from_profile(&profile, 48000).unwrap(), 7);
        let begin = std::time::Instant::now();
        player
            .execute(
                PlayerCommand::PlayStream {
                    url: format!("/api/radio/{station}/stream"),
                    title: "Test Radio".into(),
                },
                &database,
                "http://localhost",
            )
            .await
            .unwrap();
        assert!(
            begin.elapsed() < Duration::from_millis(150),
            "radio connection blocked Play"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut received = Vec::new();
        while tokio::time::Instant::now() < deadline && received.len() < 16000 {
            let mut chunk = [0u8; 8192];
            match reader.read(&mut chunk) {
                Ok(n) => received.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("{e}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            received.len() >= 16000,
            "saved radio never reached the Snapcast FIFO"
        );
        let peak = received
            .chunks_exact(2)
            .map(|v| i16::from_le_bytes([v[0], v[1]]).unsigned_abs())
            .max()
            .unwrap();
        assert!(
            peak > 3000 && peak < 5000,
            "radio bypassed shared EQ/volume processing: {peak}"
        );
        assert!(
            player.audio_tap().read().is_some(),
            "radio did not publish the output meter"
        );
        let draining = Arc::new(AtomicBool::new(true));
        let drain_active = draining.clone();
        let drain = std::thread::spawn(move || {
            let mut bytes = [0u8; 8192];
            while drain_active.load(Ordering::Relaxed) {
                let _ = reader.read(&mut bytes);
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let generation = player.loaded.lock().await.generation;
        player
            .execute(PlayerCommand::Pause, &database, "http://localhost")
            .await
            .unwrap();
        assert_eq!(player.snapshot().await.status, PlaybackStatus::Paused);
        assert!(player.loaded.lock().await.stream_url.is_none());
        player
            .execute(PlayerCommand::Play, &database, "http://localhost")
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while connections.load(Ordering::Relaxed) < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            connections.load(Ordering::Relaxed) >= 2,
            "resuming radio must reconnect the live broadcast"
        );
        assert_ne!(player.loaded.lock().await.generation, generation);
        // Events from the cancelled connection must not stop the resumed broadcast.
        player
            .on_stream_failed(generation, "stale connection".into())
            .await;
        assert_eq!(player.snapshot().await.status, PlaybackStatus::Playing);
        let second = database
            .create_radio_station("Second Radio", &url, None, 2)
            .await
            .unwrap();
        let resumed_generation = player.loaded.lock().await.generation;
        player
            .execute(
                PlayerCommand::PlayStream {
                    url: format!("/api/radio/{second}/stream"),
                    title: "Second Radio".into(),
                },
                &database,
                "http://localhost",
            )
            .await
            .unwrap();
        assert_ne!(player.loaded.lock().await.generation, resumed_generation);
        player
            .on_stream_failed(resumed_generation, "replaced station".into())
            .await;
        assert_eq!(player.snapshot().await.status, PlaybackStatus::Playing);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while connections.load(Ordering::Relaxed) < 3 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            connections.load(Ordering::Relaxed) >= 3,
            "station replacement did not connect"
        );
        // Hold provider access to model a slow library source during the handoff.
        let mut library = library_with_tracks(1);
        library.tracks[0].path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata-fixture/The Meridian/Neon Hours/01 Track.mp3");
        database.save_library(&mut library).await.unwrap();
        let providers = player.providers.clone();
        let provider_guard = providers.write().await;
        let changing = player.clone();
        let db = database.clone();
        let change = tokio::spawn(async move {
            changing
                .execute(
                    PlayerCommand::PlayTracks {
                        track_ids: vec!["track_1".into()],
                        start_index: 0,
                    },
                    &db,
                    "http://localhost",
                )
                .await
                .unwrap();
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while player
            .state
            .lock()
            .await
            .queue
            .first()
            .and_then(|item| item.track_id.as_deref())
            != Some("track_1")
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        #[cfg(feature = "provider-smb")]
        {
            tokio::time::sleep(Duration::from_millis(80)).await;
            assert!(
                player.loaded.lock().await.stream_url.is_none(),
                "radio remained loaded during a delayed library decode"
            );
            assert!(
                player.audio_tap().read().is_none(),
                "old radio audio continued while the library source was blocked"
            );
        }
        drop(provider_guard);
        change.await.unwrap();
        assert_eq!(
            player.loaded.lock().await.track_id.as_deref(),
            Some("track_1")
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while player.audio_tap().read().is_none() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            player.audio_tap().read().is_some(),
            "library playback did not resume after radio"
        );
        player
            .execute(
                PlayerCommand::PlayStream {
                    url: format!("/api/radio/{station}/stream"),
                    title: "Test Radio".into(),
                },
                &database,
                "http://localhost",
            )
            .await
            .unwrap();
        server.abort();
        // An interrupted broadcast must leave an actionable playback failure.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while player.snapshot().await.status == PlaybackStatus::Playing
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(player.snapshot().await.status, PlaybackStatus::Stopped);
        assert!(player.snapshot().await.queue_activity.is_some());
        player
            .execute(
                PlayerCommand::PlayTracks {
                    track_ids: vec!["track_1".into()],
                    start_index: 0,
                },
                &database,
                "http://localhost",
            )
            .await
            .unwrap();
        assert!(
            player.snapshot().await.queue_activity.is_none(),
            "radio failure remained visible during library playback"
        );
        let previous_connections = connections.load(Ordering::Relaxed);
        player
            .execute(
                PlayerCommand::PlayStream {
                    url: url.clone(),
                    title: "Unsaved URL".into(),
                },
                &database,
                "http://localhost",
            )
            .await
            .unwrap();
        assert_eq!(player.snapshot().await.status, PlaybackStatus::Stopped);
        assert!(
            player
                .snapshot()
                .await
                .queue_activity
                .unwrap()
                .contains("Save this station")
        );
        assert_eq!(connections.load(Ordering::Relaxed), previous_connections);
        player
            .execute(PlayerCommand::Stop, &database, "http://localhost")
            .await
            .unwrap();
        draining.store(false, Ordering::Relaxed);
        drain.join().unwrap();
        drop(manager);
        let _ = std::fs::remove_file(path.with_extension("fifo"));
        let _ = std::fs::remove_file(path);
    }

    // Drive real backend queues without requiring audio hardware or external services.
    // The old browser-only pass leaves both of these playing queues at one track.
    async fn assert_autoplay_refills(kind: &str) {
        let path = temp_db("autoplay-output");
        let database = Database::connect(&path).await.unwrap();
        let mut library = library_with_tracks(12);
        database.save_library(&mut library).await.unwrap();
        let manager = PlayerManager::load(
            database.clone(),
            "http://localhost".into(),
            Arc::new(RwLock::new(ProviderRegistry::new())),
        )
        .await
        .unwrap();
        let mut fifo_reader: Option<std::fs::File> = None;
        let id = match kind {
            "mpd" => {
                manager
                    .register("mpd", "127.0.0.1:1", "Test MPD")
                    .await
                    .unwrap()
                    .id
            }
            #[cfg(feature = "snapcast")]
            "snapcast" => {
                let settings = crate::snapcast::SnapcastSettings {
                    manage_server: false,
                    fifo_path: path.with_extension("fifo"),
                    ..Default::default()
                };
                let snap = Arc::new(SnapcastManager::start(settings).await.unwrap());
                fifo_reader = Some(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(snap.fifo_path())
                        .unwrap(),
                );
                manager.enable_snapcast(snap).await.unwrap();
                SNAPCAST_PLAYER_ID.to_string()
            }
            _ => BROWSER_PLAYER_ID.to_string(),
        };
        let handle = manager.get(&id).await.unwrap();
        let seed = resolve_queue_items(&database, &["track_1".into()])
            .await
            .unwrap();
        let state = match &handle {
            PlayerHandle::Mpd(p) => &p.state,
            PlayerHandle::Browser(p) => &p.state,
            #[cfg(feature = "snapcast")]
            PlayerHandle::Snapcast(p) => &p.state,
        };
        {
            let mut state = state.lock().await;
            state.queue = seed;
            state.position = Some(0);
            state.status = PlaybackStatus::Playing;
            state.elapsed_seconds = Some(12.0);
        }
        let lb = Arc::new(crate::recommendations::ListenBrainzClient::with_base_url(
            "http://127.0.0.1:1",
        ));
        database
            .set_bool_setting(crate::SETTING_AUTOPLAY, false)
            .await
            .unwrap();
        crate::autoplay_pass(manager.clone(), database.clone(), lb.clone()).await;
        assert_eq!(
            state.lock().await.queue.len(),
            1,
            "disabled autoplay must not append"
        );
        database
            .set_bool_setting(crate::SETTING_AUTOPLAY, true)
            .await
            .unwrap();
        crate::autoplay_pass(manager.clone(), database.clone(), lb.clone()).await;
        {
            let state = state.lock().await;
            assert!(
                state.queue.len() > 1,
                "{kind}: autoplay must append similar tracks"
            );
            assert_eq!(state.position, Some(0), "refill must not restart playback");
            // Snapcast's live control task may tick while the refill is in flight.
            assert!(
                state.elapsed_seconds.is_some_and(|elapsed| elapsed >= 12.0),
                "refill must not reset playback progress"
            );
            assert_eq!(state.queue[0].track_id.as_deref(), Some("track_1"));
            let ids: std::collections::HashSet<_> =
                state.queue.iter().map(|i| &i.track_id).collect();
            assert_eq!(
                ids.len(),
                state.queue.len(),
                "refill must exclude queued tracks"
            );
        }
        let filled = state.lock().await.queue.len();
        crate::autoplay_pass(manager.clone(), database.clone(), lb.clone()).await;
        assert_eq!(
            state.lock().await.queue.len(),
            filled,
            "a full queue needs no refill"
        );
        // A zone owns its members' queue: never refill a member independently.
        let zone = manager.create_zone("Autoplay test zone").await.unwrap();
        manager.set_zone(&id, Some(&zone.id)).await.unwrap();
        state.lock().await.queue.truncate(1);
        crate::autoplay_pass(manager.clone(), database.clone(), lb.clone()).await;
        assert_eq!(
            state.lock().await.queue.len(),
            1,
            "zone members must not refill independently"
        );
        manager
            .command_zone(
                &zone.id,
                PlayerCommand::PlayTracks {
                    track_ids: vec!["track_1".into()],
                    start_index: 0,
                },
            )
            .await
            .unwrap();
        crate::autoplay_pass(manager.clone(), database.clone(), lb.clone()).await;
        let zone_state = manager.zone_state(&zone.id).await.unwrap();
        assert!(zone_state.queue.len() > 1, "zones must still refill");
        if kind == "browser" {
            // Browser clients consume the zone broadcast, not a mirrored standalone queue.
            assert_eq!(state.lock().await.queue.len(), 1);
        } else {
            assert_eq!(
                state.lock().await.queue.len(),
                zone_state.queue.len(),
                "zone refill reaches its member once"
            );
        }
        manager.set_zone(&id, None).await.unwrap();
        state.lock().await.queue.truncate(1);
        for status in [PlaybackStatus::Paused, PlaybackStatus::Stopped] {
            state.lock().await.status = status;
            crate::autoplay_pass(manager.clone(), database.clone(), lb.clone()).await;
            assert_eq!(state.lock().await.queue.len(), 1);
        }
        {
            let mut state = state.lock().await;
            state.status = PlaybackStatus::Playing;
            state.repeat = RepeatMode::All;
        }
        crate::autoplay_pass(manager.clone(), database.clone(), lb.clone()).await;
        assert_eq!(
            state.lock().await.queue.len(),
            1,
            "repeat must not grow the queue"
        );
        drop(handle);
        drop(manager);
        drop(fifo_reader.take());
        let _ = std::fs::remove_file(path.with_extension("fifo"));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn autoplay_refills_mpd() {
        assert_autoplay_refills("mpd").await;
    }

    #[cfg(feature = "snapcast")]
    #[tokio::test]
    async fn autoplay_refills_snapcast() {
        assert_autoplay_refills("snapcast").await;
    }

    #[tokio::test]
    async fn autoplay_refills_browser() {
        assert_autoplay_refills("browser").await;
    }
}
