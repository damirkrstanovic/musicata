mod activity;
mod artwork;
mod artwork_providers;
mod fingerprint;
mod mpd;
mod musicbrainz;
mod players;
mod providers;
mod radiobrowser;
#[cfg(feature = "provider-smb")]
mod scan_concurrency;
#[cfg(feature = "provider-smb")]
mod smb;
mod subsonic;

use crate::players::{BrowserPlayer, PlayerHandle, PlayerManager, ProgressTick, ZonePlayer};
use crate::providers::{ProviderHandle, ProviderRegistry};
use anyhow::{Context, Result, anyhow};
use axum::{
    Json, Router,
    body::Body,
    extract::Request,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Path, Query, State},
    http::{
        HeaderMap, StatusCode,
        header::{
            ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
            IF_NONE_MATCH, RANGE,
        },
    },
    middleware::{self, Next},
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
};
use musicata_core::{
    Album, Artist, BrowseFilter, BrowseIndex, Library, LibrarySummary, LocalDiskProvider,
    MetadataApprovalState, MetadataFieldValue, PlaybackState, Player, PlayerCommand, Playlist,
    RadioStation, SearchResults, Track, TrackMetadataFieldObservation, Zone, album_artwork_url,
    artwork_asset_id, find_album_artwork_candidates, merge_libraries,
    regroup_library_with_overrides,
};
use musicata_storage::{Database, ResolvedMusicBrainzMetadata};
use musicbrainz::{
    CoverArtArchiveCandidateResponse, MusicBrainzAlbumCandidateSearchResponse, MusicBrainzClient,
    MusicBrainzTrackCandidateSearchResponse, MusicBrainzTrackLookupResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, RwLock};
use tokio_stream::{Stream, StreamExt, wrappers::IntervalStream};
use tracing_subscriber::EnvFilter;

const PLAYBACK_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const ARTWORK_CACHE_CONTROL: &str = "public, max-age=86400, must-revalidate";
const TAG_WRITE_BACK_DISABLED_REASON: &str =
    "File tag write-back is disabled. Musicata currently updates only its database.";

#[derive(Clone)]
struct AppState {
    database: Database,
    providers: Arc<RwLock<ProviderRegistry>>,
    players: Arc<PlayerManager>,
    musicbrainz: MusicBrainzClient,
    radio_browser: radiobrowser::RadioBrowserClient,
    activity: Arc<activity::ActivityLog>,
    rescan_lock: Arc<Mutex<()>>,
    most_played_cache: Arc<Mutex<Option<MostPlayedCache>>>,
    playback_sessions: Arc<RwLock<BTreeMap<String, PlaybackSession>>>,
    next_playback_session: Arc<AtomicU64>,
    subsonic: subsonic::SubsonicAuth,
    // Read only by network-backed providers (SMB) today; local disk serves directly.
    #[cfg_attr(not(feature = "provider-smb"), allow(dead_code))]
    artwork_cache: Arc<artwork::ArtworkCache>,
}

#[derive(Clone, Debug)]
struct PlaybackSession {
    created_at_unix_seconds: i64,
}

#[derive(Debug)]
struct Config {
    library: PathBuf,
    database: PathBuf,
    addr: SocketAddr,
    rescan: bool,
    no_incremental_rescan: bool,
    scan_once: bool,
    /// Serve the existing database as-is: skip the initial scan and all rescans/
    /// watching. Useful to serve a pre-populated snapshot DB without touching disk
    /// (and what the UI smoke suite uses to run against the real library).
    no_scan: bool,
    /// Comma-separated MPD player addresses (`host:port`) to control.
    mpd_addrs: Vec<String>,
    /// Base URL MPD uses to fetch streams from this server, e.g.
    /// `http://127.0.0.1:3030`. Defaults to `http://<addr>`.
    public_url: Option<String>,
    /// Username accepted by the OpenSubsonic API (`/rest`). Defaults to `musicata`.
    subsonic_user: String,
    /// Password for the OpenSubsonic API. When unset the API is unauthenticated
    /// (any credentials accepted) — convenient on a trusted LAN, hardened in M12.
    subsonic_password: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let config = Config::from_args()?;
    let database = Database::connect(&config.database)
        .await
        .with_context(|| format!("failed to open database {}", config.database.display()))?;
    let registry = build_registry(&database, &config).await?;
    let providers = Arc::new(RwLock::new(registry));
    // Restore the activity log from the database; mark any job left "running" as
    // interrupted (the server stopped mid-scan).
    let activity = Arc::new(load_activity_log(&database).await);

    // `--scan-once` is the batch path: scan synchronously, persist, exit.
    if config.scan_once {
        migrate_identity(&database).await;
        let registry = providers.read().await.clone();
        let mut scanned = registry.scan_all().await.context("library scan failed")?;
        database.save_library(&mut scanned).await?;
        tracing::info!(
            artists = scanned.artists.len(),
            albums = scanned.albums.len(),
            tracks = scanned.tracks.len(),
            "scan complete; exiting because --scan-once was set"
        );
        return Ok(());
    }

    // Serve immediately from whatever's already cached; the actual scan runs in the
    // background (below) so a slow or unreachable source — an SMB share on a flaky
    // network — never delays the web server from coming up.
    match database.load_library().await? {
        Some(library) => tracing::info!(
            artists = library.artists.len(),
            albums = library.albums.len(),
            tracks = library.tracks.len(),
            force_rescan = config.rescan,
            "serving cached library; scanning sources in the background"
        ),
        None => tracing::info!("no cached library yet; scanning sources in the background"),
    }

    let public_url = config
        .public_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", config.addr));
    let players = PlayerManager::load(database.clone(), public_url).await?;
    // Seed any players given on the command line / config (idempotent).
    for addr in &config.mpd_addrs {
        if let Err(error) = players
            .register("mpd", addr, &format!("MPD ({addr})"))
            .await
        {
            tracing::warn!(%addr, %error, "failed to register configured MPD player");
        }
    }

    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("failed to bind {}", config.addr))?;

    // Scan in the background: once right away to populate/refresh, then on a timer
    // so controllers never need a manual "rescan" — they re-read and see changes.
    // The shared lock serializes this with on-demand rescans. Scanning here (rather
    // than before binding) is what keeps the web port available even while a large
    // or offline source is being scanned.
    let rescan_lock = Arc::new(Mutex::new(()));
    // Cache fetched cover art next to the database (e.g. `.musicata/artwork/`).
    let artwork_cache = Arc::new(artwork::ArtworkCache::new(
        config
            .database
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("artwork"),
    ));
    // `--no-scan` serves the existing database untouched (no initial scan, no rescans,
    // no watcher) — for a snapshot DB or the UI smoke suite running on the real library.
    // After each scan, missing covers are auto-filled if enabled in the app's settings.
    if !config.no_scan {
        tokio::spawn(library_scan_loop(
            database.clone(),
            providers.clone(),
            rescan_lock.clone(),
            activity.clone(),
            LIBRARY_RESCAN_INTERVAL,
            !config.no_incremental_rescan,
            artwork_cache.clone(),
        ));
    }

    // Persist the activity log (debounced) so its history survives a restart.
    tokio::spawn(persist_activity_log(database.clone(), activity.clone()));

    // Watch the local library for changes so additions/edits show up immediately,
    // rather than waiting for the next periodic pass. Network shares have no such
    // notifications and rely on the periodic rescan.
    if !config.no_scan && !config.no_incremental_rescan {
        spawn_library_watcher(
            config.library.clone(),
            database.clone(),
            providers.clone(),
            rescan_lock.clone(),
            activity.clone(),
        );
    }

    // Keep only a rolling window of listening history.
    tokio::spawn(prune_old_listens(
        database.clone(),
        LISTEN_RETENTION,
        LISTEN_PRUNE_INTERVAL,
    ));

    let subsonic_auth = subsonic::SubsonicAuth {
        user: config.subsonic_user.clone(),
        password: config.subsonic_password.clone(),
    };
    if subsonic_auth.password.is_none() {
        tracing::warn!(
            "OpenSubsonic API is unauthenticated (no subsonic_password set); any \
             credentials are accepted — set --subsonic-password on untrusted networks"
        );
    }

    tracing::info!("listening on http://{}", config.addr);
    axum::serve(
        listener,
        app(
            database,
            providers,
            players,
            activity,
            rescan_lock,
            subsonic_auth,
            artwork_cache,
        ),
    )
    .await
    .context("server failed")?;

    Ok(())
}

/// How often the library is re-scanned against the filesystem in the background.
// Background rescans are incremental (only new/changed files have their tags read),
// so a pass is cheap; but the directory walk still costs round-trips on a network
// share, so we don't run it as tightly as a local-only scan would allow.
const LIBRARY_RESCAN_INTERVAL: Duration = Duration::from_secs(60);

/// Listening history is kept for this long; older listens are pruned.
const LISTEN_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How often the history retention prune runs.
const LISTEN_PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Periodically drops listens older than the retention window. The first tick fires
/// immediately, so stale history is trimmed at startup too.
async fn prune_old_listens(database: Database, retention: Duration, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let cutoff = now_unix_seconds() - retention.as_secs() as i64;
        match database.prune_listens_before(cutoff).await {
            Ok(removed) if removed > 0 => tracing::info!(removed, "pruned old listens"),
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "failed to prune old listens"),
        }
    }
}

/// Periodically re-scans the library and persists any changes. Incremental change
/// detection only stats files, so an unchanged library is cheap; nothing is written
/// when nothing changed. Errors are logged and the loop continues.
/// Build the active source registry: the local-disk source from `--library`, plus
/// any persisted sources (e.g. SMB shares) recorded in the database.
async fn build_registry(database: &Database, config: &Config) -> Result<ProviderRegistry> {
    let mut registry = ProviderRegistry::new();
    registry.push(ProviderHandle::local(LocalDiskProvider::new(
        &config.library,
    )));
    // Internet radio is a built-in, always-present non-scannable source backed by the
    // saved-stations table (like local-disk, it isn't persisted in `sources`).
    registry.push(ProviderHandle::radio(providers::RadioProvider::new(
        database.clone(),
    )));
    register_persisted_sources(database, &mut registry).await?;
    Ok(registry)
}

/// Add every enabled persisted source (e.g. SMB shares) to `registry`. A source
/// that can't be constructed (bad config, or built without its cargo feature) is
/// logged and skipped so one broken source never blocks startup.
async fn register_persisted_sources(
    database: &Database,
    registry: &mut ProviderRegistry,
) -> Result<()> {
    for record in database.list_sources().await? {
        if !record.enabled {
            continue;
        }
        match providers::provider_from_record(&record) {
            Ok(handle) => registry.push(handle),
            Err(error) => tracing::warn!(
                source = %record.id,
                %error,
                "skipping configured source"
            ),
        }
    }
    Ok(())
}

/// Scan every source once and persist any changes. Runs in the background so a
/// slow or unreachable source (an SMB share on a flaky network) never blocks the
/// web server. Each source's progress and any failure (with its root cause) are
/// recorded in the activity log for the admin page.
///
/// `transient` drops the activity entry when nothing changed and nothing failed —
/// used for the routine periodic rescans so they don't bury real events in noise.
async fn scan_and_persist(
    database: &Database,
    providers: &Arc<RwLock<ProviderRegistry>>,
    rescan_lock: &Arc<Mutex<()>>,
    activity: &Arc<activity::ActivityLog>,
    label: &str,
    transient: bool,
) {
    let _guard = rescan_lock.lock().await;
    let task = activity.start("scan", label.to_string());

    // The stored library is the "prior" for an incremental scan: unchanged files
    // (same size + mtime) reuse their parsed metadata, so only new/changed files are
    // re-read — cheap in steady state, especially over the network.
    let prior = database.load_library().await.ok().flatten().map(Arc::new);

    // Scan each source in turn. The progress callback updates the activity label
    // live (finding files → N/M processed); per-source results and any failure with
    // its root cause are collected for the final summary.
    let mut libraries = Vec::new();
    let mut errors = Vec::new();
    let mut summaries = Vec::new();
    for handle in providers.read().await.handles() {
        if !handle.capabilities().can_scan {
            continue;
        }
        let id = handle.provider_id();
        let progress = {
            let activity = activity.clone();
            let id = id.clone();
            move |update: musicata_core::ScanProgress| {
                let label = match update {
                    musicata_core::ScanProgress::Discovering { found } => {
                        if found == 0 {
                            format!("Scanning {id}: finding files…")
                        } else {
                            format!("Scanning {id}: finding files… ({found} found)")
                        }
                    }
                    musicata_core::ScanProgress::Discovered { files } => {
                        format!("Scanning {id}: found {files} files")
                    }
                    musicata_core::ScanProgress::Processing {
                        done,
                        total,
                        detail,
                    } => match detail {
                        Some(detail) => format!("Scanning {id}: {done}/{total} files · {detail}"),
                        None => format!("Scanning {id}: {done}/{total} files"),
                    },
                };
                activity.update(task, label);
            }
        };
        match handle.scan_with_progress(prior.clone(), progress).await {
            Ok(library) => {
                let skipped = library.scan_errors.len();
                summaries.push(format!(
                    "{id}: {} tracks{}",
                    library.tracks.len(),
                    if skipped > 0 {
                        format!(", {skipped} skipped")
                    } else {
                        String::new()
                    }
                ));
                libraries.push(library);
            }
            Err(error) => errors.push(format!("{id}: {error}")),
        }
    }
    let scanned = merge_libraries(libraries);

    let changes = match database.detect_library_changes(&scanned).await {
        Ok(changes) => changes,
        Err(error) => {
            activity.finish(
                task,
                false,
                Some(format!("change detection failed: {error}")),
            );
            return;
        }
    };

    // Backfill: a database written before the scanner read a field (e.g. track
    // duration, added in migration v12) has unchanged files but stale rows. When the
    // fresh scan now carries durations the stored rows lack, persist it even though
    // no file changed — a one-time cost after upgrading.
    let needs_backfill = !changes.has_changes()
        && database
            .load_library()
            .await
            .ok()
            .flatten()
            .is_some_and(|stored| {
                stored.tracks.iter().any(|t| t.duration_seconds.is_none())
                    && scanned.tracks.iter().any(|t| t.duration_seconds.is_some())
            });

    // A few sample skipped-file reasons (unreadable tags, etc.) so failures are
    // visible, not just counted.
    let skipped: Vec<String> = scanned
        .scan_errors
        .iter()
        .map(|issue| format!("{}: {}", issue.path, issue.message))
        .collect();

    if changes.has_changes() || needs_backfill {
        let mut scanned = scanned;
        if let Err(error) = database.save_library(&mut scanned).await {
            errors.push(format!("failed to save library: {error}"));
        }
    }

    let mut detail = summaries.join("\n");
    if changes.has_changes() {
        detail.push_str(&format!(
            "\n{} added, {} removed, {} updated",
            changes.added, changes.removed, changes.modified
        ));
    }
    if !skipped.is_empty() {
        detail.push_str(&format!("\n\n{} files skipped:", skipped.len()));
        for line in skipped.iter().take(8) {
            detail.push_str(&format!("\n• {line}"));
        }
        if skipped.len() > 8 {
            detail.push_str(&format!("\n… and {} more", skipped.len() - 8));
        }
    }

    if !errors.is_empty() {
        let mut message = errors.join("\n");
        if !detail.is_empty() {
            message.push_str("\n\n");
            message.push_str(&detail);
        }
        activity.finish(task, false, Some(message));
    } else if changes.has_changes() || !transient {
        // Manual/add scans always report; routine rescans only when something changed.
        activity.finish(
            task,
            true,
            Some(if detail.is_empty() {
                "no changes".to_string()
            } else {
                detail
            }),
        );
    } else {
        // Routine rescan with nothing to do — don't keep a log entry.
        activity.remove(task);
    }
}

/// How long to coalesce a burst of filesystem-change events before rescanning.
const WATCH_DEBOUNCE: Duration = Duration::from_secs(2);

fn activity_to_record(activity: &activity::Activity) -> musicata_storage::ActivityRecord {
    musicata_storage::ActivityRecord {
        id: activity.id as i64,
        kind: activity.kind.clone(),
        label: activity.label.clone(),
        status: activity.status.clone(),
        started_at_unix_seconds: activity.started_at_unix_seconds,
        finished_at_unix_seconds: activity.finished_at_unix_seconds,
        message: activity.message.clone(),
    }
}

/// Load the persisted activity log, marking any job still "running" as interrupted
/// (the server stopped before it finished), and persisting that correction.
async fn load_activity_log(database: &Database) -> activity::ActivityLog {
    let records = database.load_activities().await.unwrap_or_default();
    let now = now_unix_seconds();
    let initial = records
        .into_iter()
        .map(|record| {
            let mut entry = activity::Activity {
                id: record.id as u64,
                kind: record.kind,
                label: record.label,
                status: record.status,
                started_at_unix_seconds: record.started_at_unix_seconds,
                finished_at_unix_seconds: record.finished_at_unix_seconds,
                message: record.message,
            };
            if entry.status == "running" {
                entry.status = "interrupted".to_string();
                entry.finished_at_unix_seconds = Some(now);
                entry.message = Some("Interrupted — server restarted".to_string());
            }
            entry
        })
        .collect();
    let log = activity::ActivityLog::seeded(initial);
    // Persist the interrupted-marking now, even if nothing else changes this run.
    let records: Vec<_> = log.list().iter().map(activity_to_record).collect();
    let _ = database.replace_activities(&records).await;
    log
}

/// Persist the activity log to the database whenever it changes (debounced, so a
/// burst of progress updates collapses into one write).
async fn persist_activity_log(database: Database, activity: Arc<activity::ActivityLog>) {
    let mut changes = activity.subscribe();
    loop {
        if changes.changed().await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let records: Vec<_> = activity.list().iter().map(activity_to_record).collect();
        if let Err(error) = database.replace_activities(&records).await {
            tracing::warn!(%error, "failed to persist activity log");
        }
    }
}

/// Watch `root` recursively for filesystem changes and trigger an (incremental)
/// rescan after a short debounce. Best-effort: if a watcher can't be created the
/// periodic rescan still covers everything.
fn spawn_library_watcher(
    root: PathBuf,
    database: Database,
    providers: Arc<RwLock<ProviderRegistry>>,
    rescan_lock: Arc<Mutex<()>>,
    activity: Arc<activity::ActivityLog>,
) {
    use notify::{EventKind, RecursiveMode, Watcher};

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = match notify::recommended_watcher(
        move |result: notify::Result<notify::Event>| {
            if let Ok(event) = result
                && matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                )
            {
                let _ = tx.send(());
            }
        },
    ) {
        Ok(watcher) => watcher,
        Err(error) => {
            tracing::warn!(%error, "filesystem watcher unavailable; relying on periodic rescan");
            return;
        }
    };
    if let Err(error) = watcher.watch(&root, RecursiveMode::Recursive) {
        tracing::warn!(%error, root = %root.display(), "cannot watch library; relying on periodic rescan");
        return;
    }
    tracing::info!(root = %root.display(), "watching library for changes");

    tokio::spawn(async move {
        let _watcher = watcher; // keep the watcher alive for the task's lifetime
        while rx.recv().await.is_some() {
            // Coalesce a burst of events into one rescan.
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(WATCH_DEBOUNCE) => break,
                    message = rx.recv() => {
                        if message.is_none() {
                            return;
                        }
                    }
                }
            }
            scan_and_persist(
                &database,
                &providers,
                &rescan_lock,
                &activity,
                "Library change detected",
                true,
            )
            .await;
        }
    });
}

/// Spawn a one-off background library scan (e.g. after a source is added/removed,
/// or an explicit rescan request) without blocking the caller.
fn spawn_library_scan(state: &AppState, label: &str) {
    let database = state.database.clone();
    let providers = state.providers.clone();
    let rescan_lock = state.rescan_lock.clone();
    let activity = state.activity.clone();
    let label = label.to_string();
    tokio::spawn(async move {
        scan_and_persist(
            &database,
            &providers,
            &rescan_lock,
            &activity,
            &label,
            false,
        )
        .await;
    });
}

/// Background library task: scan once right away (so a fresh database is populated
/// and an existing one refreshed), then re-scan on the interval unless incremental
/// rescanning is disabled. Never blocks serving.
async fn library_scan_loop(
    database: Database,
    providers: Arc<RwLock<ProviderRegistry>>,
    rescan_lock: Arc<Mutex<()>>,
    activity: Arc<activity::ActivityLog>,
    interval: Duration,
    incremental: bool,
    artwork_cache: Arc<artwork::ArtworkCache>,
) {
    // Re-derive ids if the identity scheme changed (normalization rollout), moving
    // favorites/artwork onto the new ids. Runs here (in the background loop, before the
    // first scan saves) so it never delays the server binding; it's a one-time pass
    // (guarded by identity_version) and serializes ahead of the scan below.
    migrate_identity(&database).await;
    scan_and_persist(
        &database,
        &providers,
        &rescan_lock,
        &activity,
        "Initial library scan",
        true,
    )
    .await;
    // Fingerprint untagged tracks first (resolves MBIDs), then enrich their metadata
    // from MusicBrainz, then fill covers (which can now reach the id-exact providers).
    fingerprint_pass(&database, &providers, &activity).await;
    musicbrainz_enrich_pass(&database, &activity).await;
    // Re-apply resolved enrichment AND user artist-merges over the folder-derived grouping
    // the scan just reset — unconditionally (so merges apply even with enrichment off),
    // before artwork fetching so covers key on the final grouping.
    if let Err(error) = database.reapply_canonical_grouping().await {
        tracing::warn!(%error, "reapply canonical grouping failed");
    }
    artwork_fill_pass(&database, &activity, &artwork_cache).await;
    artist_artwork_fill_pass(&database, &activity, &artwork_cache).await;
    if !incremental {
        return;
    }
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await; // Consume the immediate tick; we just scanned.
    loop {
        ticker.tick().await;
        scan_and_persist(
            &database,
            &providers,
            &rescan_lock,
            &activity,
            "Library rescan",
            true,
        )
        .await;
        fingerprint_pass(&database, &providers, &activity).await;
        musicbrainz_enrich_pass(&database, &activity).await;
        if let Err(error) = database.reapply_canonical_grouping().await {
            tracing::warn!(%error, "reapply canonical grouping failed");
        }
        artwork_fill_pass(&database, &activity, &artwork_cache).await;
        artist_artwork_fill_pass(&database, &activity, &artwork_cache).await;
    }
}

/// Setting keys edited in the web UI (see `crate::Database::get_setting`).
const SETTING_ARTWORK_FETCH: &str = "artwork_fetch";
const SETTING_FANART_TV_KEY: &str = "fanart_tv_key";
const SETTING_FINGERPRINT: &str = "fingerprint_enabled";
const SETTING_MB_ENRICH: &str = "musicbrainz_enrich_enabled";

/// Read a default-on boolean setting (toggling it in the UI takes effect next pass).
async fn setting_enabled(database: &Database, key: &str) -> bool {
    database
        .get_setting(key)
        .await
        .ok()
        .flatten()
        .map(|value| value != "false" && value != "0")
        .unwrap_or(true)
}

async fn artwork_fetch_enabled(database: &Database) -> bool {
    setting_enabled(database, SETTING_ARTWORK_FETCH).await
}

/// Bump when the artist/album id derivation changes (normalization, MBID-first) so an
/// existing DB re-derives ids once and moves favorites/artwork onto them.
const IDENTITY_VERSION: i64 = 1;
const SETTING_IDENTITY_VERSION: &str = "identity_version";

/// One-time, idempotent identity migration: when the id derivation changed since this DB
/// was last grouped, move id-referencing rows (favorites, acquired artwork) onto the new
/// ids, then re-derive and save the library so the artist/album tables carry the new ids.
/// Forces the regroup+save even when files are unchanged (a plain rescan would skip the
/// save and leave the tables on old ids). Runs before any scan.
async fn migrate_identity(database: &Database) {
    let stored = database
        .get_setting(SETTING_IDENTITY_VERSION)
        .await
        .ok()
        .flatten()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    if stored >= IDENTITY_VERSION {
        return;
    }
    // Move favorites/acquired_* from old ids to new (reads the current, old-id rows) —
    // must precede the re-derive+save that rewrites the artist/album rows.
    match database.remap_identity_references().await {
        Ok(moved) if moved > 0 => {
            tracing::info!(moved, "identity: remapped favorite/artwork references")
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "identity: reference remap failed; skipping re-derive");
            return;
        }
    }
    // Re-derive ids by regrouping the cached library and saving (empty overrides → only
    // recomputes ids via the new derivation + any aliases), then regenerate artwork urls.
    if let Ok(Some(library)) = database.load_library().await {
        let aliases = database.artist_alias_map().await.unwrap_or_default();
        let mut regrouped =
            regroup_library_with_overrides(library, &std::collections::BTreeMap::new(), &aliases);
        if let Err(error) = database.save_library(&mut regrouped).await {
            tracing::warn!(%error, "identity: re-derive save failed");
            return;
        }
        let _ = database.reapply_acquired_artwork().await;
        let _ = database.reapply_acquired_artist_artwork().await;
    }
    if let Err(error) = database
        .set_setting(SETTING_IDENTITY_VERSION, &IDENTITY_VERSION.to_string())
        .await
    {
        tracing::warn!(%error, "identity: failed to record identity_version");
    }
}

/// How long to wait before re-querying providers for an album whose cover wasn't found,
/// so the periodic rescan doesn't hammer the APIs for genuinely-coverless albums.
const ARTWORK_RETRY_SECS: i64 = 7 * 24 * 60 * 60;
/// Albums fetched per fill pass, so one pass can't starve the rescan loop for hours on a
/// large coverless library; the rest are picked up on later passes.
const ARTWORK_FILL_BATCH: usize = 100;

/// After a scan, fill in missing album covers from the external artwork providers: first
/// re-apply already-acquired covers (a rescan rewrites the albums table, wiping their
/// `artwork_url`), then fetch for the still-coverless albums (rate-limited by the
/// providers, capped per pass, with a negative cache so misses aren't re-queried every
/// rescan). Reports progress on the same activity feed as the scan.
async fn artwork_fill_pass(
    database: &Database,
    activity: &Arc<activity::ActivityLog>,
    artwork_cache: &Arc<artwork::ArtworkCache>,
) {
    // Always restore already-acquired covers' artwork_url after the rescan's album
    // rewrite (DB-only) — even if *fetching* is off, existing covers should still show.
    if let Err(error) = database.reapply_acquired_artwork().await {
        tracing::warn!(%error, "artwork: failed to re-apply acquired covers");
    }

    // Fetching new covers is a user setting (web UI), not a flag — read it live.
    if !artwork_fetch_enabled(database).await {
        return;
    }
    let fanart_tv_key = database
        .get_setting(SETTING_FANART_TV_KEY)
        .await
        .ok()
        .flatten()
        .filter(|key| !key.is_empty());
    let registry = Arc::new(artwork_providers::ArtworkProviderRegistry::lane(
        fanart_tv_key,
    ));
    if registry.is_empty() {
        return;
    }

    let retry_before = now_unix_seconds() - ARTWORK_RETRY_SECS;
    let targets = match database.albums_missing_artwork(retry_before).await {
        Ok(targets) => targets,
        Err(error) => {
            tracing::warn!(%error, "artwork: failed to list albums missing artwork");
            return;
        }
    };
    if targets.is_empty() {
        return;
    }

    let batch: Vec<_> = targets.into_iter().take(ARTWORK_FILL_BATCH).collect();
    let total = batch.len();
    let task = activity.start("artwork", format!("Finding album artwork (0/{total})"));
    let mut found = 0usize;

    for (index, target) in batch.into_iter().enumerate() {
        activity.update(
            task,
            format!(
                "Finding album artwork ({}/{total}): {}",
                index + 1,
                target.title
            ),
        );
        let (release_mbid, release_group_mbid) = database
            .album_musicbrainz_ids(&target.album_id)
            .await
            .unwrap_or((None, None));
        let query = artwork_providers::ArtworkQuery {
            artist: target.artist_name.clone(),
            album: target.title.clone(),
            release_mbid,
            release_group_mbid,
        };

        // Provider search + image download are synchronous (ureq) → off the runtime.
        let registry = registry.clone();
        let fetched = tokio::task::spawn_blocking(move || {
            let cover = registry.find_cover(&query)?;
            let (bytes, ext) = artwork_providers::download_image(&cover.image_url).ok()?;
            Some((cover, bytes, ext))
        })
        .await
        .ok()
        .flatten();

        let now = now_unix_seconds();
        match fetched {
            Some((cover, bytes, ext)) => {
                let cache_key =
                    artwork_providers::acquired_cache_key(&target.album_id, &cover.image_url);
                artwork_cache.put(&cache_key, ext, &bytes).await;
                let _ = database
                    .upsert_acquired_artwork(
                        &target.album_id,
                        cover.provider,
                        Some(&cover.image_url),
                        Some(&cache_key),
                        Some(ext),
                        cover.width,
                        "acquired",
                        now,
                    )
                    .await;
                let url = format!(
                    "/api/albums/{}/artwork?asset={}",
                    target.album_id, cache_key
                );
                let _ = database
                    .set_album_artwork(&target.album_id, None, Some(&url))
                    .await;
                found += 1;
            }
            None => {
                // Negative cache: don't re-query this album until the retry window.
                let _ = database
                    .upsert_acquired_artwork(
                        &target.album_id,
                        "none",
                        None,
                        None,
                        None,
                        None,
                        "not_found",
                        now,
                    )
                    .await;
            }
        }
    }

    if found > 0 {
        activity.finish(
            task,
            true,
            Some(format!("Found artwork for {found} of {total} albums")),
        );
    } else {
        // Nothing found this pass — don't leave a noisy entry in the activity log.
        activity.remove(task);
    }
}

/// After a scan, fetch missing **artist** images. Name-based (the library carries no
/// artist MBIDs), so only Deezer's artist search applies. Mirrors `artwork_fill_pass`:
/// re-apply already-acquired images first (the rescan wiped the artists table), then —
/// if fetching is enabled — fetch for the still-imageless artists, rate-limited and
/// batched, with a `not_found` negative cache.
async fn artist_artwork_fill_pass(
    database: &Database,
    activity: &Arc<activity::ActivityLog>,
    artwork_cache: &Arc<artwork::ArtworkCache>,
) {
    if let Err(error) = database.reapply_acquired_artist_artwork().await {
        tracing::warn!(%error, "artist artwork: failed to re-apply acquired images");
    }
    if !artwork_fetch_enabled(database).await {
        return;
    }
    let registry = Arc::new(artwork_providers::ArtworkProviderRegistry::new(vec![
        Box::new(artwork_providers::DeezerProvider::new()),
    ]));

    let retry_before = now_unix_seconds() - ARTWORK_RETRY_SECS;
    let targets = match database.artists_missing_artwork(retry_before).await {
        Ok(targets) => targets,
        Err(error) => {
            tracing::warn!(%error, "artist artwork: failed to list artists missing images");
            return;
        }
    };
    if targets.is_empty() {
        return;
    }

    let batch: Vec<_> = targets.into_iter().take(ARTWORK_FILL_BATCH).collect();
    let total = batch.len();
    let task = activity.start("artwork", format!("Finding artist images (0/{total})"));
    let mut found = 0usize;

    for (index, target) in batch.into_iter().enumerate() {
        activity.update(
            task,
            format!("Finding artist images ({}/{total}): {}", index + 1, target.name),
        );
        let query = artwork_providers::ArtistArtworkQuery {
            name: target.name.clone(),
        };
        let registry = registry.clone();
        let fetched = tokio::task::spawn_blocking(move || {
            let image = registry.find_artist(&query)?;
            let (bytes, ext) = artwork_providers::download_image(&image.image_url).ok()?;
            Some((image, bytes, ext))
        })
        .await
        .ok()
        .flatten();

        let now = now_unix_seconds();
        match fetched {
            Some((image, bytes, ext)) => {
                let cache_key =
                    artwork_providers::acquired_cache_key(&target.artist_id, &image.image_url);
                artwork_cache.put(&cache_key, ext, &bytes).await;
                let _ = database
                    .upsert_acquired_artist_artwork(
                        &target.artist_id,
                        image.provider,
                        Some(&image.image_url),
                        Some(&cache_key),
                        Some(ext),
                        image.width,
                        "acquired",
                        now,
                    )
                    .await;
                found += 1;
            }
            None => {
                let _ = database
                    .upsert_acquired_artist_artwork(
                        &target.artist_id,
                        "none",
                        None,
                        None,
                        None,
                        None,
                        "not_found",
                        now,
                    )
                    .await;
            }
        }
    }

    // Publish artwork_url for the artists we just acquired images for.
    let _ = database.reapply_acquired_artist_artwork().await;

    if found > 0 {
        activity.finish(
            task,
            true,
            Some(format!("Found images for {found} of {total} artists")),
        );
    } else {
        activity.remove(task);
    }
}

/// Retry window for a track AcoustID couldn't identify, so the rescan doesn't
/// re-fingerprint it every pass.
const FINGERPRINT_RETRY_SECS: i64 = 7 * 24 * 60 * 60;
/// Tracks fingerprinted per pass — decoding ~120 s of audio each is heavier than an
/// artwork fetch, so keep batches small; the rest are picked up on later passes.
const FINGERPRINT_BATCH: i64 = 25;

/// After a scan, identify untagged tracks by audio fingerprint (Chromaprint → AcoustID)
/// and store the resolved MusicBrainz ids, so `album_musicbrainz_ids` can feed the
/// id-exact artwork providers. Gated by a Settings toggle; no-ops without an AcoustID
/// key. Mirrors `artwork_fill_pass` (rate-limited, capped, negative cache, activity).
async fn fingerprint_pass(
    database: &Database,
    providers: &Arc<RwLock<ProviderRegistry>>,
    activity: &Arc<activity::ActivityLog>,
) {
    if !setting_enabled(database, SETTING_FINGERPRINT).await {
        return;
    }
    // Until the project compiles in its AcoustID application key, the feature no-ops.
    if fingerprint::ACOUSTID_CLIENT_KEY.is_empty() {
        return;
    }

    let retry_before = now_unix_seconds() - FINGERPRINT_RETRY_SECS;
    let targets = match database
        .tracks_missing_fingerprints(retry_before, FINGERPRINT_BATCH)
        .await
    {
        Ok(targets) => targets,
        Err(error) => {
            tracing::warn!(%error, "fingerprint: failed to list untagged tracks");
            return;
        }
    };
    if targets.is_empty() {
        return;
    }

    let client = Arc::new(fingerprint::AcoustIdClient::new(
        fingerprint::ACOUSTID_CLIENT_KEY,
    ));
    let total = targets.len();
    let task = activity.start("fingerprint", format!("Identifying tracks (0/{total})"));
    let mut resolved = 0usize;

    for (index, target) in targets.into_iter().enumerate() {
        activity.update(
            task,
            format!(
                "Identifying tracks ({}/{total}): {}",
                index + 1,
                target.title
            ),
        );
        // Read the audio (SMB or local); an unreadable file is skipped (not marked, so a
        // transient failure is retried).
        let audio = match read_track_source_file(providers, &target).await {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };

        let extension = target.extension.clone();
        let client = client.clone();
        // Decode + fingerprint + AcoustID lookup are synchronous + CPU-heavy.
        let outcome = tokio::task::spawn_blocking(move || {
            let (fingerprint, duration) =
                fingerprint::compute_fingerprint(&audio, &extension).ok()?;
            client.lookup(&fingerprint, duration).ok().flatten()
        })
        .await
        .ok()
        .flatten();

        let now = now_unix_seconds();
        match outcome {
            Some(found) => {
                let _ = database
                    .upsert_track_fingerprint(
                        &target.track_id,
                        "resolved",
                        found.recording_mbid.as_deref(),
                        found.release_mbid.as_deref(),
                        found.release_group_mbid.as_deref(),
                        now,
                    )
                    .await;
                resolved += 1;
            }
            None => {
                let _ = database
                    .upsert_track_fingerprint(&target.track_id, "not_found", None, None, None, now)
                    .await;
            }
        }
    }

    if resolved > 0 {
        activity.finish(
            task,
            true,
            Some(format!("Identified {resolved} of {total} tracks")),
        );
    } else {
        activity.remove(task);
    }
}

/// Read a track's audio bytes from its provider — SMB over the wire, else local disk.
#[cfg_attr(not(feature = "provider-smb"), allow(unused_variables))]
async fn read_track_source_file(
    providers: &Arc<RwLock<ProviderRegistry>>,
    target: &musicata_storage::TrackFingerprintTarget,
) -> Result<Vec<u8>, String> {
    #[cfg(feature = "provider-smb")]
    {
        if let Some(providers::ProviderHandle::Smb(smb)) =
            providers.read().await.get(&target.provider_id)
        {
            return smb
                .read_file(&target.provider_item_id)
                .await
                .map_err(|error| error.to_string());
        }
    }
    tokio::fs::read(&target.path)
        .await
        .map_err(|error| error.to_string())
}

/// Retry window for a fingerprinted recording MusicBrainz couldn't resolve.
const MB_ENRICH_RETRY_SECS: i64 = 7 * 24 * 60 * 60;
/// Tracks enriched per pass. MusicBrainz allows ~1 req/s and each track is up to two
/// requests, so a pass is paced; keep batches modest and pick up the rest next pass.
const MB_ENRICH_BATCH: i64 = 25;
/// MusicBrainz asks for ≤ 1 request/second; space successive tracks' first request out
/// (each track's two internal requests are already spaced by the client).
const MB_REQUEST_INTERVAL: Duration = Duration::from_millis(1100);

/// After fingerprinting, fetch real metadata for tracks whose recording MBID was resolved
/// and apply it to the canonical library (so an untagged track stops showing folder-derived
/// junk). DB-only; never overwrites embedded tags (enforced in `reapply_canonical_grouping`).
/// Gated by a Settings toggle. Mirrors `fingerprint_pass` (capped, paced, negative cache,
/// activity). Always re-applies resolved enrichment so a fresh scan's folder grouping is
/// corrected even when no new track is fetched.
async fn musicbrainz_enrich_pass(database: &Database, activity: &Arc<activity::ActivityLog>) {
    if !setting_enabled(database, SETTING_MB_ENRICH).await {
        return;
    }

    let retry_before = now_unix_seconds() - MB_ENRICH_RETRY_SECS;
    let targets = match database
        .tracks_missing_musicbrainz_metadata(retry_before, MB_ENRICH_BATCH)
        .await
    {
        Ok(targets) => targets,
        Err(error) => {
            tracing::warn!(%error, "musicbrainz enrich: failed to list targets");
            return;
        }
    };
    if targets.is_empty() {
        // No new work; the unconditional reapply in the scan loop re-applies resolved
        // enrichment (and aliases) after the rescan reset grouping to folder-derived.
        return;
    }

    let client = Arc::new(MusicBrainzClient::default());
    let total = targets.len();
    let task = activity.start("musicbrainz", format!("Enriching metadata (0/{total})"));
    let mut resolved = 0usize;

    for (index, target) in targets.into_iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(MB_REQUEST_INTERVAL).await;
        }
        activity.update(task, format!("Enriching metadata ({}/{total})", index + 1));

        let client = client.clone();
        let recording = target.recording_mbid.clone();
        let release = target.release_mbid.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            client.fetch_enrichment(&recording, release.as_deref()).ok()
        })
        .await
        .ok()
        .flatten();

        let now = now_unix_seconds();
        match outcome {
            Some(enrichment) if !enrichment.is_empty() => {
                let meta = ResolvedMusicBrainzMetadata {
                    title: enrichment.title,
                    artist_name: enrichment.artist_name,
                    album_title: enrichment.album_title,
                    album_artist_name: enrichment.album_artist_name,
                    track_number: enrichment.track_number,
                    track_total: enrichment.track_total,
                    disc_number: enrichment.disc_number,
                    recording_date: enrichment.recording_date,
                    year: enrichment.year,
                    recording_mbid: Some(target.recording_mbid.clone()),
                    release_mbid: target.release_mbid.clone(),
                    release_group_mbid: enrichment
                        .release_group_mbid
                        .or_else(|| target.release_group_mbid.clone()),
                    artist_mbid: enrichment.artist_mbid,
                };
                let _ = database
                    .upsert_track_musicbrainz_metadata(&target.track_id, "resolved", &meta, now)
                    .await;
                resolved += 1;
            }
            _ => {
                let _ = database
                    .upsert_track_musicbrainz_metadata(
                        &target.track_id,
                        "not_found",
                        &ResolvedMusicBrainzMetadata::default(),
                        now,
                    )
                    .await;
            }
        }
    }

    if resolved > 0 {
        activity.finish(
            task,
            true,
            Some(format!("Enriched {resolved} of {total} tracks")),
        );
    } else {
        activity.remove(task);
    }
}

fn app(
    database: Database,
    providers: Arc<RwLock<ProviderRegistry>>,
    players: Arc<PlayerManager>,
    activity: Arc<activity::ActivityLog>,
    rescan_lock: Arc<Mutex<()>>,
    subsonic_auth: subsonic::SubsonicAuth,
    artwork_cache: Arc<artwork::ArtworkCache>,
) -> Router {
    Router::new()
        .merge(subsonic::routes())
        .route("/", get(index))
        .route("/admin", get(admin_page))
        .route("/admin.js", get(admin_js))
        .route("/app.js", get(app_js))
        .route("/styles.css", get(styles_css))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon.svg", get(app_icon))
        .route("/sw.js", get(service_worker))
        .route("/api/health", get(health))
        .route("/api/library/summary", get(library_summary))
        .route("/api/library/rescan", post(rescan_library))
        .route(
            "/api/metadata/write-back",
            get(metadata_write_back_policy).post(reject_metadata_write_back),
        )
        .route("/api/playback/sessions", post(create_playback_session))
        .route(
            "/api/playback/sessions/{id}",
            delete(delete_playback_session),
        )
        .route(
            "/api/playback/sessions/{id}/events",
            get(playback_session_events),
        )
        .route("/api/players", get(list_players).post(register_player))
        .route(
            "/api/players/{id}",
            patch(update_player).delete(delete_player),
        )
        .route("/api/players/{id}/state", get(player_state))
        .route("/api/players/{id}/commands", post(player_command))
        .route("/api/players/{id}/ws", get(player_ws))
        .route("/api/zones", get(list_zones).post(create_zone))
        .route("/api/zones/{id}", patch(update_zone).delete(delete_zone))
        .route("/api/zones/{id}/state", get(zone_state))
        .route("/api/zones/{id}/commands", post(zone_command))
        .route("/api/zones/{id}/ws", get(zone_ws))
        .route("/api/artists", get(artists))
        .route("/api/artists/aliases", get(list_artist_aliases_route))
        .route("/api/artists/aliases/{alias_key}", delete(unmerge_artists))
        .route("/api/artists/merge", post(merge_artists))
        .route("/api/artists/{id}", get(artist_detail))
        .route("/api/artists/{id}/artwork", get(artist_artwork))
        .route("/api/albums", get(albums))
        .route("/api/albums/{id}", get(album_detail))
        .route("/api/browse", get(browse))
        .route("/api/browse/recently-added", get(recently_added))
        .route("/api/history/recent", get(recently_played))
        .route("/api/history/most-played", get(most_played))
        .route("/api/smart-playlists", get(list_smart_playlists))
        .route("/api/smart-playlists/{id}", get(smart_playlist_detail))
        .route(
            "/api/playlists",
            get(list_playlists_route).post(create_playlist_route),
        )
        .route(
            "/api/playlists/{id}",
            get(playlist_detail)
                .patch(update_playlist_route)
                .delete(delete_playlist_route),
        )
        .route("/api/favorites", get(list_favorites))
        .route(
            "/api/favorites/{kind}/{id}",
            put(star_favorite).delete(unstar_favorite),
        )
        .route("/api/radio", get(list_radio).post(create_radio))
        .route("/api/radio/directory", get(radio_directory))
        .route("/api/radio/{id}", patch(update_radio).delete(delete_radio))
        .route("/api/sources", get(list_sources).post(create_source))
        .route("/api/sources/{id}", delete(delete_source))
        .route("/api/sources/{id}/rescan", post(rescan_source))
        .route("/api/sources/{id}/browse", get(browse_source))
        .route("/api/sources/{id}/resolve", get(resolve_source))
        .route("/api/activity", get(list_activity))
        .route("/api/activity/ws", get(activity_ws))
        .route("/api/settings", get(get_settings).patch(update_settings))
        .route(
            "/api/albums/{id}/metadata/musicbrainz/candidates",
            get(album_musicbrainz_candidates),
        )
        .route(
            "/api/albums/{id}/artwork/cover-art-archive/candidates",
            get(album_cover_art_archive_candidates),
        )
        .route("/api/albums/{id}/artwork/review", get(album_artwork_review))
        .route(
            "/api/albums/{id}/artwork/candidates/{artwork_id}",
            get(album_artwork_candidate),
        )
        .route(
            "/api/albums/{id}/artwork",
            get(album_artwork).patch(update_album_artwork),
        )
        .route("/api/tracks", get(tracks))
        .route("/api/search", get(search))
        .route(
            "/api/tracks/{id}/metadata/review",
            get(track_metadata_review),
        )
        .route(
            "/api/tracks/{id}/metadata/review/fields",
            patch(update_track_metadata_field_review),
        )
        .route(
            "/api/tracks/{id}/metadata/musicbrainz",
            get(track_musicbrainz_lookup),
        )
        .route(
            "/api/tracks/{id}/metadata/musicbrainz/candidates",
            get(track_musicbrainz_candidates),
        )
        .route("/api/tracks/{id}/stream", get(stream_track))
        .fallback(fallback)
        .layer(middleware::from_fn(log_request))
        .with_state(AppState {
            database,
            providers,
            players,
            musicbrainz: MusicBrainzClient::default(),
            radio_browser: radiobrowser::RadioBrowserClient::default(),
            activity,
            rescan_lock,
            most_played_cache: Arc::new(Mutex::new(None)),
            playback_sessions: Arc::new(RwLock::new(BTreeMap::new())),
            next_playback_session: Arc::new(AtomicU64::new(1)),
            subsonic: subsonic_auth,
            artwork_cache,
        })
}

async fn log_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started_at = Instant::now();
    let response = next.run(request).await;
    let status = response.status();
    let elapsed_ms = started_at.elapsed().as_millis();

    tracing::info!(
        method = %method,
        path = %path,
        status = status.as_u16(),
        elapsed_ms,
        "request"
    );

    response
}

// The app shell (HTML/JS/CSS) is served with `no-cache` so a browser always
// revalidates and picks up a rebuilt binary's assets on reload, instead of running
// a stale copy. Content is embedded, so revalidation is a tiny full refetch.
const APP_SHELL_CACHE: &str = "no-cache";

async fn index() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CACHE_CONTROL, APP_SHELL_CACHE),
        ],
        include_str!("../static/index.html"),
    )
}

async fn admin_page() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CACHE_CONTROL, APP_SHELL_CACHE),
        ],
        include_str!("../static/admin.html"),
    )
}

async fn admin_js() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (CACHE_CONTROL, APP_SHELL_CACHE),
        ],
        include_str!("../static/admin.js"),
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (CACHE_CONTROL, APP_SHELL_CACHE),
        ],
        include_str!("../static/app.js"),
    )
}

async fn styles_css() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "text/css; charset=utf-8"),
            (CACHE_CONTROL, APP_SHELL_CACHE),
        ],
        include_str!("../static/styles.css"),
    )
}

async fn manifest() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "application/manifest+json; charset=utf-8")],
        include_str!("../static/manifest.webmanifest"),
    )
}

async fn app_icon() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../static/icon.svg"),
    )
}

async fn service_worker() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (CACHE_CONTROL, APP_SHELL_CACHE),
        ],
        include_str!("../static/sw.js"),
    )
}

async fn fallback() -> AppError {
    AppError::not_found("route not found")
}

async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    let summary = state.database.summary().await.map_err(db_error)?;
    let provider = summary
        .as_ref()
        .map(|summary| summary.provider_id.clone())
        .unwrap_or_else(|| "local-disk".to_string());
    let tracks = summary.map(|summary| summary.track_count).unwrap_or(0);

    Ok(Json(json!({
        "status": "ok",
        "provider": provider,
        "tracks": tracks,
        // Native API version (see docs/api.md). The `/api` surface is v1.
        "api_version": NATIVE_API_VERSION,
        "server_version": env!("CARGO_PKG_VERSION"),
    })))
}

/// Version of the native HTTP/WebSocket API exposed under `/api`. Documented in
/// docs/api.md; bump on breaking changes to routes or payloads.
const NATIVE_API_VERSION: &str = "1";

async fn library_summary(State(state): State<AppState>) -> Result<Json<LibrarySummary>, AppError> {
    let summary = state
        .database
        .summary()
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::internal("library not initialized"))?;
    Ok(Json(summary))
}

#[derive(Debug, Serialize)]
struct PlaybackSessionResponse {
    id: String,
    event_url: String,
}

async fn create_playback_session(State(state): State<AppState>) -> Json<PlaybackSessionResponse> {
    let sequence = state.next_playback_session.fetch_add(1, Ordering::Relaxed);
    let id = new_playback_session_id(sequence);
    state.playback_sessions.write().await.insert(
        id.clone(),
        PlaybackSession {
            created_at_unix_seconds: now_unix_seconds(),
        },
    );

    Json(PlaybackSessionResponse {
        event_url: format!("/api/playback/sessions/{id}/events"),
        id,
    })
}

async fn delete_playback_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> StatusCode {
    state.playback_sessions.write().await.remove(&id);
    StatusCode::NO_CONTENT
}

async fn playback_session_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let created_at_unix_seconds = state
        .playback_sessions
        .read()
        .await
        .get(&id)
        .map(|session| session.created_at_unix_seconds)
        .ok_or_else(|| AppError::not_found(format!("unknown playback session: {id}")))?;
    let stream_id = id.clone();
    let mut sequence = 0_u64;
    let stream =
        IntervalStream::new(tokio::time::interval(PLAYBACK_HEARTBEAT_INTERVAL)).map(move |_| {
            let current_sequence = sequence;
            sequence += 1;
            let payload = json!({
                "session_id": stream_id,
                "sequence": current_sequence,
                "created_at_unix_seconds": created_at_unix_seconds,
            });
            Ok(Event::default()
                .event("heartbeat")
                .data(payload.to_string()))
        });

    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(PLAYBACK_HEARTBEAT_INTERVAL)))
}

#[derive(Debug, Deserialize)]
struct RescanQuery {
    force: Option<bool>,
}

#[derive(Debug, Serialize)]
struct RescanResponse {
    changed: bool,
    updated: bool,
    forced: bool,
    added: usize,
    removed: usize,
    modified: usize,
    summary: LibrarySummary,
}

async fn rescan_library(
    State(state): State<AppState>,
    Query(query): Query<RescanQuery>,
) -> Result<Json<RescanResponse>, AppError> {
    let _guard = state.rescan_lock.lock().await;
    let forced = query.force.unwrap_or(false);
    let registry = state.providers.read().await.clone();
    let mut scanned = registry
        .scan_all()
        .await
        .map_err(|error| AppError::internal(error.to_string()))?;
    let changes = state.database.detect_library_changes(&scanned).await?;
    let changed = changes.has_changes();
    let updated = forced || changed;

    // The scan reflects the current files; when unchanged its counts equal the
    // stored library's, so this summary is correct whether or not we save.
    let summary = scanned.summary();
    if updated {
        state.database.save_library(&mut scanned).await?;
    }

    Ok(Json(RescanResponse {
        changed,
        updated,
        forced,
        added: changes.added,
        removed: changes.removed,
        modified: changes.modified,
        summary,
    }))
}

/// Common pagination/sorting query parameters for list endpoints.
#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    limit: Option<usize>,
    offset: Option<usize>,
    sort: Option<String>,
}

/// A single page of list results plus the totals needed to drive pagination.
#[derive(Debug, Serialize)]
struct Page<T> {
    items: Vec<T>,
    total: usize,
    limit: usize,
    offset: usize,
    sort: Option<String>,
}

/// SQLite `LIMIT` argument: a missing limit becomes -1, which SQLite treats as
/// "no limit" so the page returns every remaining row.
fn limit_arg(query: &ListQuery) -> i64 {
    query.limit.map(|limit| limit as i64).unwrap_or(-1)
}

fn offset_arg(query: &ListQuery) -> i64 {
    query.offset.unwrap_or(0) as i64
}

/// Wrap SQL page results in the response envelope. A missing limit reports the
/// total (the whole list was returned in one page), matching the prior behavior.
fn page_envelope<T>(items: Vec<T>, total: usize, query: &ListQuery) -> Page<T> {
    Page {
        items,
        total,
        limit: query.limit.unwrap_or(total),
        offset: query.offset.unwrap_or(0),
        sort: query.sort.clone(),
    }
}

fn db_error(error: anyhow::Error) -> AppError {
    AppError::internal(error.to_string())
}

async fn artists(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Page<Artist>>, AppError> {
    let (items, total) = state
        .database
        .list_artists(query.sort.as_deref(), limit_arg(&query), offset_arg(&query))
        .await
        .map_err(db_error)?;
    Ok(Json(page_envelope(items, total, &query)))
}

async fn albums(
    State(state): State<AppState>,
    Query(query): Query<TrackListQuery>,
) -> Result<Json<Page<Album>>, AppError> {
    let filter = BrowseFilter {
        genre: query.genre,
        year: query.year,
        composer: query.composer,
        folder: query.folder,
    };
    let page = ListQuery {
        limit: query.limit,
        offset: query.offset,
        sort: query.sort,
    };
    // When a browse filter is set, narrow to albums whose tracks match it (the album
    // grid follows the track filter); otherwise list every album (cheaper, no join).
    let (items, total) = if browse_filter_active(&filter) {
        state
            .database
            .list_albums_filtered(
                &filter,
                page.sort.as_deref(),
                limit_arg(&page),
                offset_arg(&page),
            )
            .await
    } else {
        state
            .database
            .list_albums(page.sort.as_deref(), limit_arg(&page), offset_arg(&page))
            .await
    }
    .map_err(db_error)?;
    Ok(Json(page_envelope(items, total, &page)))
}

/// Whether a browse filter narrows anything (ignoring empty-string params).
fn browse_filter_active(filter: &BrowseFilter) -> bool {
    let set = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
    };
    filter.year.is_some() || set(&filter.genre) || set(&filter.composer) || set(&filter.folder)
}

async fn list_players(State(state): State<AppState>) -> Result<Json<Vec<Player>>, AppError> {
    Ok(Json(state.players.descriptors().await.map_err(db_error)?))
}

#[derive(Debug, Deserialize)]
struct RegisterPlayerRequest {
    kind: Option<String>,
    address: String,
    name: Option<String>,
}

async fn register_player(
    State(state): State<AppState>,
    Json(request): Json<RegisterPlayerRequest>,
) -> Result<Json<Player>, AppError> {
    let kind = request.kind.as_deref().unwrap_or("mpd");
    let name = request
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| format!("MPD ({})", request.address));
    let player = state
        .players
        .register(kind, &request.address, &name)
        .await
        .map_err(|error| AppError::bad_request(error.to_string()))?;
    Ok(Json(player))
}

async fn update_player(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<Player>, AppError> {
    // Apply only the fields present in the body: `name` renames; a `zone_id` key
    // (string to assign, null to clear) changes the zone.
    if let Some(name) = body.get("name").and_then(|value| value.as_str()) {
        state.players.rename(&id, name).await.map_err(db_error)?;
    }
    if let Some(zone) = body.get("zone_id") {
        let zone_id = zone.as_str().filter(|value| !value.is_empty());
        state
            .players
            .set_zone(&id, zone_id)
            .await
            .map_err(db_error)?;
    }
    let players = state.players.descriptors().await.map_err(db_error)?;
    players
        .into_iter()
        .find(|player| player.id == id)
        .map(Json)
        .ok_or_else(|| AppError::not_found(format!("unknown player: {id}")))
}

async fn delete_player(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    state
        .players
        .remove(&id)
        .await
        .map_err(|error| AppError::not_found(error.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn player_state(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<PlaybackState>, AppError> {
    let player = state
        .players
        .get(&id)
        .await
        .ok_or_else(|| AppError::not_found(format!("unknown player: {id}")))?;
    let playback = player.state(&state.database).await.map_err(db_error)?;
    Ok(Json(playback))
}

async fn player_command(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(command): Json<PlayerCommand>,
) -> Result<Json<PlaybackState>, AppError> {
    let player = state
        .players
        .get(&id)
        .await
        .ok_or_else(|| AppError::not_found(format!("unknown player: {id}")))?;
    player
        .execute(command, &state.database, state.players.public_base_url())
        .await
        .map_err(db_error)?;
    // The command always updates the server queue; an MPD player that couldn't be
    // reached reports itself offline. Surface that so the controller can show a clear
    // message — the track is queued and resumes when the player reconnects — rather
    // than re-probing the dead daemon (another bounded connect) via `state()`.
    if !player.is_online() {
        return Err(AppError::player_offline(
            "This player is offline. The track is queued and will play when it reconnects.",
        ));
    }
    let playback = player.state(&state.database).await.map_err(db_error)?;
    Ok(Json(playback))
}

#[derive(Debug, Deserialize)]
struct ZoneRequest {
    name: String,
}

async fn list_zones(State(state): State<AppState>) -> Result<Json<Vec<Zone>>, AppError> {
    Ok(Json(state.players.zones().await.map_err(db_error)?))
}

async fn create_zone(
    State(state): State<AppState>,
    Json(request): Json<ZoneRequest>,
) -> Result<Json<Zone>, AppError> {
    let zone = state
        .players
        .create_zone(&request.name)
        .await
        .map_err(|error| AppError::bad_request(error.to_string()))?;
    Ok(Json(zone))
}

async fn update_zone(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ZoneRequest>,
) -> Result<StatusCode, AppError> {
    state
        .players
        .rename_zone(&id, &request.name)
        .await
        .map_err(db_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_zone(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    state.players.delete_zone(&id).await.map_err(db_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn zone_command(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(command): Json<PlayerCommand>,
) -> Result<Json<PlaybackState>, AppError> {
    state
        .players
        .command_zone(&id, command)
        .await
        .map_err(db_error)?;
    let playback = state.players.zone_state(&id).await.map_err(db_error)?;
    Ok(Json(playback))
}

async fn zone_state(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<PlaybackState>, AppError> {
    let playback = state.players.zone_state(&id).await.map_err(db_error)?;
    Ok(Json(playback))
}

async fn zone_ws(
    State(state): State<AppState>,
    Path(id): Path<String>,
    upgrade: WebSocketUpgrade,
) -> Response {
    match state.players.get_zone(&id).await {
        Some(zone) => upgrade.on_upgrade(move |socket| queue_output_ws_loop(socket, zone)),
        None => AppError::not_found(format!("unknown zone: {id}")).into_response(),
    }
}

async fn player_ws(
    State(state): State<AppState>,
    Path(id): Path<String>,
    upgrade: WebSocketUpgrade,
) -> Response {
    match state.players.get(&id).await {
        Some(handle) => {
            let database = state.database.clone();
            upgrade.on_upgrade(move |socket| player_ws_loop(socket, handle, database))
        }
        None => AppError::not_found(format!("unknown player: {id}")).into_response(),
    }
}

async fn player_ws_loop(socket: WebSocket, handle: PlayerHandle, database: Database) {
    match handle {
        // The browser player is bidirectional: the tab also reports progress and
        // track-ended back to the server.
        PlayerHandle::Browser(browser) => queue_output_ws_loop(socket, browser).await,
        other => state_push_loop(socket, other, database).await,
    }
}

/// Push player state to a controller: an initial snapshot, then every broadcast
/// update (idle- or command-driven) until the socket closes. Inbound frames are
/// ignored (read-only controller).
async fn state_push_loop(mut socket: WebSocket, handle: PlayerHandle, database: Database) {
    let mut updates = handle.subscribe();

    if let Ok(state) = handle.state(&database).await
        && let Ok(text) = serde_json::to_string(&state)
        && socket.send(Message::Text(text.into())).await.is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            update = updates.recv() => match update {
                Ok(state) => {
                    let text = serde_json::to_string(&state).unwrap_or_default();
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(_)) => {}
                _ => break,
            },
        }
    }
}

/// A server-owned queue a controller drives bidirectionally over a WebSocket: it
/// pushes full state + lightweight progress ticks, and accepts `progress`/`ended`
/// frames from whichever output is rendering audio. Both the browser player and a
/// zone implement it, so they share one socket loop.
trait QueueOutput {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PlaybackState>;
    fn subscribe_progress(&self) -> tokio::sync::broadcast::Receiver<ProgressTick>;
    async fn snapshot(&self) -> PlaybackState;
    async fn report_progress(&self, elapsed_seconds: f64, duration_seconds: Option<f64>);
    async fn track_ended(&self);
}

impl QueueOutput for BrowserPlayer {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PlaybackState> {
        BrowserPlayer::subscribe(self)
    }
    fn subscribe_progress(&self) -> tokio::sync::broadcast::Receiver<ProgressTick> {
        BrowserPlayer::subscribe_progress(self)
    }
    async fn snapshot(&self) -> PlaybackState {
        BrowserPlayer::snapshot(self).await
    }
    async fn report_progress(&self, elapsed_seconds: f64, duration_seconds: Option<f64>) {
        BrowserPlayer::report_progress(self, elapsed_seconds, duration_seconds).await
    }
    async fn track_ended(&self) {
        BrowserPlayer::track_ended(self).await
    }
}

impl QueueOutput for ZonePlayer {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PlaybackState> {
        ZonePlayer::subscribe(self)
    }
    fn subscribe_progress(&self) -> tokio::sync::broadcast::Receiver<ProgressTick> {
        ZonePlayer::subscribe_progress(self)
    }
    async fn snapshot(&self) -> PlaybackState {
        ZonePlayer::snapshot(self).await
    }
    async fn report_progress(&self, elapsed_seconds: f64, duration_seconds: Option<f64>) {
        ZonePlayer::report_progress(self, elapsed_seconds, duration_seconds).await
    }
    async fn track_ended(&self) {
        ZonePlayer::track_ended(self).await
    }
}

/// Bidirectional loop for a server-owned queue (browser player or zone): pushes
/// state, and applies inbound `progress`/`ended` frames from the rendering output.
async fn queue_output_ws_loop<T: QueueOutput>(mut socket: WebSocket, output: Arc<T>) {
    let mut updates = output.subscribe();
    // Position ticks arrive on a separate channel as tiny `{type:"progress"}` frames,
    // so a playing track doesn't re-send the whole queue every second.
    let mut progress = output.subscribe_progress();

    if let Ok(text) = serde_json::to_string(&output.snapshot().await)
        && socket.send(Message::Text(text.into())).await.is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            update = updates.recv() => match update {
                Ok(state) => {
                    let text = serde_json::to_string(&state).unwrap_or_default();
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            tick = progress.recv() => match tick {
                Ok(tick) => {
                    let frame = serde_json::json!({
                        "type": "progress",
                        "elapsed_seconds": tick.elapsed_seconds,
                        "duration_seconds": tick.duration_seconds,
                    });
                    if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => handle_queue_frame(&*output, text.as_str()).await,
                Some(Ok(_)) => {}
                _ => break,
            },
        }
    }
}

async fn handle_queue_frame<T: QueueOutput>(output: &T, text: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    match value.get("type").and_then(|kind| kind.as_str()) {
        Some("ended") => output.track_ended().await,
        Some("progress") => {
            if let Some(elapsed) = value
                .get("elapsed_seconds")
                .and_then(|value| value.as_f64())
            {
                let duration = value
                    .get("duration_seconds")
                    .and_then(|value| value.as_f64())
                    .filter(|duration| duration.is_finite() && *duration > 0.0);
                output.report_progress(elapsed, duration).await;
            }
        }
        _ => {}
    }
}

#[derive(Debug, Serialize)]
struct ArtistDetailResponse {
    artist: Artist,
    albums: Vec<Album>,
    tracks: Vec<Track>,
}

async fn artist_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ArtistDetailResponse>, AppError> {
    let artist = state
        .database
        .artist(&id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::not_found(format!("unknown artist: {id}")))?;
    let albums = state
        .database
        .albums_for_artist(&id)
        .await
        .map_err(db_error)?;
    let tracks = state
        .database
        .tracks_for_artist(&id)
        .await
        .map_err(db_error)?;

    Ok(Json(ArtistDetailResponse {
        artist,
        albums,
        tracks,
    }))
}

// ---- Artist merges (user-curated identity) --------------------------------

/// A group of artist-name variants merged under one canonical artist.
#[derive(Debug, Serialize)]
struct ArtistAliasGroup {
    canonical_key: String,
    canonical_name: String,
    /// The normalized keys of the merged-in members.
    members: Vec<String>,
}

async fn alias_groups(database: &Database) -> Result<Vec<ArtistAliasGroup>, AppError> {
    let rows = database.list_artist_aliases().await.map_err(db_error)?;
    let mut groups: std::collections::BTreeMap<String, ArtistAliasGroup> =
        std::collections::BTreeMap::new();
    for alias in rows {
        groups
            .entry(alias.canonical_key.clone())
            .or_insert_with(|| ArtistAliasGroup {
                canonical_key: alias.canonical_key.clone(),
                canonical_name: alias.canonical_name.clone(),
                members: Vec::new(),
            })
            .members
            .push(alias.alias_key);
    }
    Ok(groups.into_values().collect())
}

async fn list_artist_aliases_route(
    State(state): State<AppState>,
) -> Result<Json<Vec<ArtistAliasGroup>>, AppError> {
    Ok(Json(alias_groups(&state.database).await?))
}

#[derive(Debug, Deserialize)]
struct MergeArtistsRequest {
    /// The artist that survives; the others fold into it.
    canonical_id: String,
    /// Artist ids to merge into the canonical (the canonical id, if present, is ignored).
    member_ids: Vec<String>,
}

/// Re-derive grouping and regenerate artwork urls after an alias change. Heavy (regroups
/// the whole library), so it runs in the background — the alias itself was recorded
/// synchronously; the library view reflects it once the regroup completes.
fn spawn_apply_aliases(database: Database) {
    tokio::spawn(async move {
        let _ = database.remap_identity_references().await;
        if let Err(error) = database.reapply_canonical_grouping().await {
            tracing::warn!(%error, "merge: reapply canonical grouping failed");
        }
        let _ = database.reapply_acquired_artwork().await;
        let _ = database.reapply_acquired_artist_artwork().await;
    });
}

async fn merge_artists(
    State(state): State<AppState>,
    Json(request): Json<MergeArtistsRequest>,
) -> Result<Json<Vec<ArtistAliasGroup>>, AppError> {
    let canonical = state
        .database
        .artist(&request.canonical_id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::not_found("unknown canonical artist"))?;
    let canonical_key = musicata_core::normalize_artist_key(&canonical.name);
    let members: Vec<&String> = request
        .member_ids
        .iter()
        .filter(|id| **id != request.canonical_id)
        .collect();
    if members.is_empty() {
        return Err(AppError::bad_request(
            "select at least one other artist to merge into the canonical one",
        ));
    }
    let now = now_unix_seconds();
    for member_id in members {
        let member = state
            .database
            .artist(member_id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| AppError::not_found(format!("unknown artist: {member_id}")))?;
        let member_key = musicata_core::normalize_artist_key(&member.name);
        state
            .database
            .add_artist_alias(&member_key, &canonical_key, &canonical.name, now)
            .await
            .map_err(|error| AppError::bad_request(error.to_string()))?;
    }
    spawn_apply_aliases(state.database.clone());
    Ok(Json(alias_groups(&state.database).await?))
}

async fn unmerge_artists(
    State(state): State<AppState>,
    Path(alias_key): Path<String>,
) -> Result<Json<Vec<ArtistAliasGroup>>, AppError> {
    state
        .database
        .remove_artist_alias(&alias_key)
        .await
        .map_err(db_error)?;
    spawn_apply_aliases(state.database.clone());
    Ok(Json(alias_groups(&state.database).await?))
}

#[derive(Debug, Serialize)]
struct AlbumDetailResponse {
    album: Album,
    artist: Option<Artist>,
    tracks: Vec<Track>,
}

async fn album_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AlbumDetailResponse>, AppError> {
    let album = state
        .database
        .album(&id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::not_found(format!("unknown album: {id}")))?;
    let artist = state
        .database
        .artist(&album.artist_id)
        .await
        .map_err(db_error)?;
    // tracks_for_album returns tracks already ordered by disc, track, then title.
    let tracks = state
        .database
        .tracks_for_album(&id)
        .await
        .map_err(db_error)?;

    Ok(Json(AlbumDetailResponse {
        album,
        artist,
        tracks,
    }))
}

async fn browse(State(state): State<AppState>) -> Result<Json<BrowseIndex>, AppError> {
    let index = state.database.browse_index().await.map_err(db_error)?;
    Ok(Json(index))
}

/// Tracks ordered newest-first by when they entered the database. Tracks without
/// a recorded timestamp (not yet saved) sort last.
async fn recently_added(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Page<Track>>, AppError> {
    let (items, total) = state
        .database
        .list_tracks(
            &BrowseFilter::default(),
            Some("recent"),
            limit_arg(&query),
            offset_arg(&query),
        )
        .await
        .map_err(db_error)?;
    Ok(Json(page_envelope(items, total, &query)))
}

/// Query parameters for the history endpoints: a single optional row cap.
#[derive(Debug, Deserialize)]
struct HistoryQuery {
    limit: Option<usize>,
}

/// History row cap: default 100, clamped to a sane ceiling.
fn history_limit(query: &HistoryQuery) -> usize {
    query.limit.unwrap_or(100).clamp(1, 500)
}

/// A track paired with when it was last listened to (Unix seconds). Recently-played
/// is a cheap indexed read, so this is served fresh on every request.
#[derive(Clone, Debug, Serialize)]
struct RecentListen {
    #[serde(flatten)]
    track: Track,
    last_listened_at: i64,
}

async fn recently_played(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<RecentListen>>, AppError> {
    let items = state
        .database
        .recently_played(history_limit(&query))
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|(track, last_listened_at)| RecentListen {
            track,
            last_listened_at,
        })
        .collect();
    Ok(Json(items))
}

/// A track paired with how many times it has been listened to.
#[derive(Clone, Debug, Serialize)]
struct PlayCount {
    #[serde(flatten)]
    track: Track,
    play_count: i64,
}

/// How long a computed most-played ranking is reused before recomputing. The ranking
/// is a full aggregation over the history, so — unlike recently-played — we don't want
/// to recompute it on every request; a short cache bounds that cost.
const MOST_PLAYED_TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct MostPlayedCache {
    computed_at: Instant,
    limit: usize,
    items: Vec<PlayCount>,
}

async fn most_played(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<PlayCount>>, AppError> {
    let limit = history_limit(&query);

    // Serve a recent ranking from cache when it matches the requested size and is
    // still fresh, so frequent opens don't re-aggregate the whole history.
    {
        let cache = state.most_played_cache.lock().await;
        if let Some(cached) = cache.as_ref()
            && cached.limit == limit
            && cached.computed_at.elapsed() < MOST_PLAYED_TTL
        {
            return Ok(Json(cached.items.clone()));
        }
    }

    let items: Vec<PlayCount> = state
        .database
        .most_played(limit)
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|(track, play_count)| PlayCount { track, play_count })
        .collect();

    *state.most_played_cache.lock().await = Some(MostPlayedCache {
        computed_at: Instant::now(),
        limit,
        items: items.clone(),
    });

    Ok(Json(items))
}

// ---- Smart playlists ----

/// One entry in the fixed catalog of computed ("smart") playlists. Unlike user
/// playlists these aren't stored — each is a live query over listening history /
/// favorites, so they stay current as you listen.
#[derive(Debug, Serialize)]
struct SmartPlaylist {
    id: &'static str,
    name: &'static str,
    description: &'static str,
}

const SMART_PLAYLISTS: &[SmartPlaylist] = &[
    SmartPlaylist {
        id: "top-30-days",
        name: "Top: last 30 days",
        description: "Your most-played tracks over the past month.",
    },
    SmartPlaylist {
        id: "never-played",
        name: "Never played",
        description: "Library tracks you haven't listened to yet.",
    },
    SmartPlaylist {
        id: "forgotten-favorites",
        name: "Forgotten favorites",
        description: "Starred tracks you haven't played in a while.",
    },
    SmartPlaylist {
        id: "most-skipped",
        name: "Most skipped",
        description: "Tracks you tend to skip before they finish.",
    },
];

/// How many tracks a smart playlist returns, and the rediscovery/recency window.
const SMART_PLAYLIST_LIMIT: usize = 100;
const THIRTY_DAYS_SECONDS: i64 = 30 * 24 * 60 * 60;

async fn list_smart_playlists() -> Json<&'static [SmartPlaylist]> {
    Json(SMART_PLAYLISTS)
}

/// A smart playlist plus its computed tracks.
#[derive(Debug, Serialize)]
struct SmartPlaylistDetail {
    #[serde(flatten)]
    playlist: &'static SmartPlaylist,
    tracks: Vec<Track>,
}

async fn smart_playlist_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<SmartPlaylistDetail>, AppError> {
    let playlist = SMART_PLAYLISTS
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| AppError::not_found(format!("unknown smart playlist: {id}")))?;
    let db = &state.database;
    let since = now_unix_seconds() - THIRTY_DAYS_SECONDS;
    let tracks = match playlist.id {
        "top-30-days" => db
            .most_played_since(SMART_PLAYLIST_LIMIT, since)
            .await
            .map_err(db_error)?
            .into_iter()
            .map(|(track, _)| track)
            .collect(),
        "never-played" => db.never_played(SMART_PLAYLIST_LIMIT).await.map_err(db_error)?,
        "forgotten-favorites" => db
            .forgotten_favorites(SMART_PLAYLIST_LIMIT, since)
            .await
            .map_err(db_error)?,
        "most-skipped" => db
            .most_skipped(SMART_PLAYLIST_LIMIT)
            .await
            .map_err(db_error)?
            .into_iter()
            .map(|(track, _)| track)
            .collect(),
        other => unreachable!("smart playlist id validated above: {other}"),
    };
    Ok(Json(SmartPlaylistDetail { playlist, tracks }))
}

// ---- Playlists ----

/// A playlist plus its ordered tracks.
#[derive(Debug, Serialize)]
struct PlaylistDetail {
    #[serde(flatten)]
    playlist: Playlist,
    tracks: Vec<Track>,
}

#[derive(Debug, Deserialize)]
struct CreatePlaylistRequest {
    name: String,
    comment: Option<String>,
    track_ids: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct UpdatePlaylistRequest {
    name: Option<String>,
    comment: Option<String>,
    /// Replace the whole track list, in this order (used for reorder).
    track_ids: Option<Vec<String>>,
    /// Append these tracks.
    add_track_ids: Option<Vec<String>>,
    /// Remove the tracks currently at these positions.
    remove_indices: Option<Vec<usize>>,
}

async fn list_playlists_route(
    State(state): State<AppState>,
) -> Result<Json<Vec<Playlist>>, AppError> {
    Ok(Json(
        state.database.list_playlists().await.map_err(db_error)?,
    ))
}

async fn playlist_detail_response(state: &AppState, id: &str) -> Result<PlaylistDetail, AppError> {
    let playlist = state
        .database
        .playlist(id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::not_found(format!("unknown playlist: {id}")))?;
    let tracks = state.database.playlist_tracks(id).await.map_err(db_error)?;
    Ok(PlaylistDetail { playlist, tracks })
}

async fn create_playlist_route(
    State(state): State<AppState>,
    Json(request): Json<CreatePlaylistRequest>,
) -> Result<Json<PlaylistDetail>, AppError> {
    let name = request.name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request("playlist name is required"));
    }
    let id = state
        .database
        .create_playlist(
            name,
            request.comment.as_deref(),
            &request.track_ids.unwrap_or_default(),
            now_unix_seconds(),
        )
        .await
        .map_err(db_error)?;
    Ok(Json(playlist_detail_response(&state, &id).await?))
}

async fn playlist_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<PlaylistDetail>, AppError> {
    Ok(Json(playlist_detail_response(&state, &id).await?))
}

async fn update_playlist_route(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<UpdatePlaylistRequest>,
) -> Result<Json<PlaylistDetail>, AppError> {
    // Confirm it exists first.
    playlist_detail_response(&state, &id).await?;
    let now = now_unix_seconds();

    if request.name.is_some() || request.comment.is_some() {
        state
            .database
            .update_playlist_meta(
                &id,
                request.name.as_deref(),
                request.comment.as_deref(),
                now,
            )
            .await
            .map_err(db_error)?;
    }

    if let Some(track_ids) = request.track_ids {
        state
            .database
            .set_playlist_tracks(&id, &track_ids, now)
            .await
            .map_err(db_error)?;
    } else if request.add_track_ids.is_some() || request.remove_indices.is_some() {
        let mut ids = state
            .database
            .playlist_track_ids(&id)
            .await
            .map_err(db_error)?;
        if let Some(remove) = request.remove_indices {
            let mut remove: Vec<usize> = remove;
            remove.sort_unstable();
            remove.dedup();
            for index in remove.into_iter().rev() {
                if index < ids.len() {
                    ids.remove(index);
                }
            }
        }
        if let Some(add) = request.add_track_ids {
            ids.extend(add);
        }
        state
            .database
            .set_playlist_tracks(&id, &ids, now)
            .await
            .map_err(db_error)?;
    }

    Ok(Json(playlist_detail_response(&state, &id).await?))
}

async fn delete_playlist_route(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    state
        .database
        .delete_playlist(&id)
        .await
        .map_err(db_error)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Favorites ----

#[derive(Debug, Serialize)]
struct FavoritesResponse {
    tracks: Vec<Track>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
}

async fn list_favorites(
    State(state): State<AppState>,
) -> Result<Json<FavoritesResponse>, AppError> {
    Ok(Json(FavoritesResponse {
        tracks: state.database.starred_tracks().await.map_err(db_error)?,
        albums: state.database.starred_albums().await.map_err(db_error)?,
        artists: state.database.starred_artists().await.map_err(db_error)?,
    }))
}

/// Validate the favorite item kind from the path.
fn favorite_kind(kind: &str) -> Result<&str, AppError> {
    match kind {
        "track" | "album" | "artist" => Ok(kind),
        other => Err(AppError::bad_request(format!(
            "unknown favorite kind: {other}"
        ))),
    }
}

async fn star_favorite(
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let kind = favorite_kind(&kind)?;
    state
        .database
        .set_favorite(kind, &id, now_unix_seconds())
        .await
        .map_err(db_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unstar_favorite(
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let kind = favorite_kind(&kind)?;
    state
        .database
        .clear_favorite(kind, &id)
        .await
        .map_err(db_error)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Internet radio ----

#[derive(Debug, Deserialize)]
struct CreateRadioRequest {
    name: String,
    stream_url: String,
    homepage_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateRadioRequest {
    name: Option<String>,
    stream_url: Option<String>,
    homepage_url: Option<String>,
}

async fn list_radio(State(state): State<AppState>) -> Result<Json<Vec<RadioStation>>, AppError> {
    Ok(Json(
        state
            .database
            .list_radio_stations()
            .await
            .map_err(db_error)?,
    ))
}

async fn create_radio(
    State(state): State<AppState>,
    Json(request): Json<CreateRadioRequest>,
) -> Result<Json<RadioStation>, AppError> {
    let name = request.name.trim();
    let stream_url = request.stream_url.trim();
    if name.is_empty() || stream_url.is_empty() {
        return Err(AppError::bad_request("name and stream_url are required"));
    }
    let id = state
        .database
        .create_radio_station(
            name,
            stream_url,
            request.homepage_url.as_deref(),
            now_unix_seconds(),
        )
        .await
        .map_err(db_error)?;
    let station = state
        .database
        .radio_station(&id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::internal("station vanished after creation"))?;
    Ok(Json(station))
}

async fn update_radio(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateRadioRequest>,
) -> Result<Json<RadioStation>, AppError> {
    state
        .database
        .update_radio_station(
            &id,
            request.name.as_deref(),
            request.stream_url.as_deref(),
            request.homepage_url.as_deref(),
        )
        .await
        .map_err(db_error)?;
    let station = state
        .database
        .radio_station(&id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::not_found(format!("unknown radio station: {id}")))?;
    Ok(Json(station))
}

async fn delete_radio(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    state
        .database
        .delete_radio_station(&id)
        .await
        .map_err(db_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
struct SourceView {
    id: String,
    kind: String,
    display_name: String,
    enabled: bool,
    capabilities: musicata_core::ProviderCapabilities,
}

#[derive(Debug, Deserialize)]
struct CreateSourceRequest {
    /// Source kind; currently only `"smb"` (the local-disk source comes from `--library`).
    kind: String,
    display_name: Option<String>,
    host: Option<String>,
    share: Option<String>,
    base_path: Option<String>,
    username: Option<String>,
    password: Option<String>,
    domain: Option<String>,
}

/// Recent and in-progress background activities (scans, rescans), newest first.
async fn list_activity(State(state): State<AppState>) -> Json<Vec<activity::Activity>> {
    Json(state.activity.list())
}

/// Push the activity list to a client whenever it changes — no polling. Sends an
/// initial snapshot, then a fresh list on every change (coalesced by the watch
/// channel, so a burst of progress updates collapses into one push).
async fn activity_ws(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| activity_ws_loop(socket, state.activity))
}

async fn activity_ws_loop(mut socket: WebSocket, activity: Arc<activity::ActivityLog>) {
    let mut changes = activity.subscribe();
    loop {
        let text = serde_json::to_string(&activity.list()).unwrap_or_else(|_| "[]".to_string());
        if socket.send(Message::Text(text.into())).await.is_err() {
            return;
        }
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    return; // log dropped
                }
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(_)) => {} // ignore client frames (read-only)
                _ => return,      // closed or errored
            },
        }
    }
}

/// List configured sources: the always-present local library plus any persisted
/// network sources (SMB shares).
/// User-editable app settings (the `/admin` Settings panel; persisted in the DB).
#[derive(Debug, Serialize)]
struct AppSettings {
    /// Automatically fetch missing album covers from external providers.
    artwork_fetch: bool,
    /// Optional fanart.tv personal API key (empty = unset).
    fanart_tv_key: String,
    /// Identify untagged tracks by audio fingerprint (AcoustID) to resolve MusicBrainz
    /// ids, which the id-exact artwork providers then use.
    fingerprint_enabled: bool,
    /// Apply real MusicBrainz metadata (title/artist/album/track number) to fingerprinted
    /// tracks, without overwriting embedded tags. DB-only; files are never modified.
    musicbrainz_enrich_enabled: bool,
}

async fn get_settings(State(state): State<AppState>) -> Result<Json<AppSettings>, AppError> {
    let artwork_fetch = artwork_fetch_enabled(&state.database).await;
    let fingerprint_enabled = setting_enabled(&state.database, SETTING_FINGERPRINT).await;
    let musicbrainz_enrich_enabled = setting_enabled(&state.database, SETTING_MB_ENRICH).await;
    let fanart_tv_key = state
        .database
        .get_setting(SETTING_FANART_TV_KEY)
        .await
        .map_err(db_error)?
        .unwrap_or_default();
    Ok(Json(AppSettings {
        artwork_fetch,
        fanart_tv_key,
        fingerprint_enabled,
        musicbrainz_enrich_enabled,
    }))
}

#[derive(Debug, Deserialize)]
struct SettingsUpdate {
    artwork_fetch: Option<bool>,
    fanart_tv_key: Option<String>,
    fingerprint_enabled: Option<bool>,
    musicbrainz_enrich_enabled: Option<bool>,
}

async fn update_settings(
    State(state): State<AppState>,
    Json(update): Json<SettingsUpdate>,
) -> Result<Json<AppSettings>, AppError> {
    if let Some(enabled) = update.artwork_fetch {
        state
            .database
            .set_setting(
                SETTING_ARTWORK_FETCH,
                if enabled { "true" } else { "false" },
            )
            .await
            .map_err(db_error)?;
    }
    if let Some(key) = update.fanart_tv_key {
        state
            .database
            .set_setting(SETTING_FANART_TV_KEY, key.trim())
            .await
            .map_err(db_error)?;
    }
    if let Some(enabled) = update.fingerprint_enabled {
        state
            .database
            .set_setting(SETTING_FINGERPRINT, if enabled { "true" } else { "false" })
            .await
            .map_err(db_error)?;
    }
    if let Some(enabled) = update.musicbrainz_enrich_enabled {
        state
            .database
            .set_setting(SETTING_MB_ENRICH, if enabled { "true" } else { "false" })
            .await
            .map_err(db_error)?;
    }
    get_settings(State(state)).await
}

async fn list_sources(State(state): State<AppState>) -> Result<Json<Vec<SourceView>>, AppError> {
    let mut views = vec![
        SourceView {
            id: "local-disk".to_string(),
            kind: "local".to_string(),
            display_name: "Local library".to_string(),
            enabled: true,
            capabilities: musicata_core::ProviderCapabilities::DISK,
        },
        SourceView {
            id: providers::RADIO_PROVIDER_ID.to_string(),
            kind: "radio".to_string(),
            display_name: "Internet radio".to_string(),
            enabled: true,
            capabilities: musicata_core::ProviderCapabilities::STREAM_ONLY,
        },
    ];
    for record in state.database.list_sources().await.map_err(db_error)? {
        views.push(source_view(&record));
    }
    Ok(Json(views))
}

fn source_view(record: &musicata_storage::SourceRecord) -> SourceView {
    SourceView {
        id: record.id.clone(),
        kind: record.kind.clone(),
        display_name: record.display_name.clone(),
        enabled: record.enabled,
        // Every source kind we persist today is a scannable disk-like source.
        capabilities: musicata_core::ProviderCapabilities::DISK,
    }
}

/// Add a network source, register it, and rescan so its tracks merge into the
/// library immediately.
async fn create_source(
    State(state): State<AppState>,
    Json(request): Json<CreateSourceRequest>,
) -> Result<Json<SourceView>, AppError> {
    if request.kind != "smb" {
        return Err(AppError::bad_request(format!(
            "unsupported source kind: {}",
            request.kind
        )));
    }
    let host = request
        .host
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::bad_request("host is required for an SMB source"))?;
    let share = request
        .share
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::bad_request("share is required for an SMB source"))?;
    let base_path = request
        .base_path
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_string();

    let id = providers::smb_provider_id(host, share, &base_path);
    let display_name = request
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{host}/{share}"));
    let record = musicata_storage::SourceRecord {
        id: id.clone(),
        kind: "smb".to_string(),
        display_name,
        enabled: true,
        host: Some(host.to_string()),
        share: Some(share.to_string()),
        base_path: (!base_path.is_empty()).then(|| base_path.clone()),
        username: request.username.clone(),
        password: request.password.clone(),
        domain: request.domain.clone(),
        created_at_unix_seconds: now_unix_seconds(),
    };

    // Build the provider, then verify it's actually reachable and readable (connect
    // + list a directory) before persisting anything. This catches a bad host, wrong
    // share, or rejected credentials up front and returns the root cause to the
    // caller — we don't save a dead source or start a doomed scan. The check is
    // bounded by the connect timeout; the heavy recursive scan still runs in the
    // background afterwards (its progress shows in the activity log).
    let handle = providers::provider_from_record(&record)
        .map_err(|error| AppError::bad_request(error.to_string()))?;
    handle
        .validate()
        .await
        .map_err(|error| AppError::bad_request(format!("could not access share: {error}")))?;

    state.database.add_source(&record).await.map_err(db_error)?;
    state.providers.write().await.push(handle);
    spawn_library_scan(
        &state,
        &format!("Scanning new source {}", record.display_name),
    );

    Ok(Json(source_view(&record)))
}

async fn delete_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    if id == "local-disk" {
        return Err(AppError::bad_request(
            "the local library source can't be removed (set it with --library)",
        ));
    }
    if id == providers::RADIO_PROVIDER_ID {
        return Err(AppError::bad_request(
            "the internet-radio source is built in; remove individual stations instead",
        ));
    }
    state.database.delete_source(&id).await.map_err(db_error)?;
    state.providers.write().await.remove(&id);
    // Re-merge the library without the removed source's tracks, in the background.
    spawn_library_scan(&state, "Rescan after removing a source");
    Ok(StatusCode::NO_CONTENT)
}

async fn rescan_source(State(state): State<AppState>, Path(_id): Path<String>) -> StatusCode {
    spawn_library_scan(&state, "Manual rescan");
    StatusCode::ACCEPTED
}

#[derive(Debug, Serialize)]
struct BrowseResponse {
    entries: Vec<musicata_core::BrowseEntry>,
}

/// Browse a non-scannable source's catalogue (internet-radio stations). Scannable
/// sources (local disk, SMB) merge into the library and are browsed through the
/// library endpoints, so they return 400 here.
async fn browse_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<BrowseResponse>, AppError> {
    let handle = state
        .providers
        .read()
        .await
        .get(&id)
        .ok_or_else(|| AppError::not_found(format!("unknown source: {id}")))?;
    let entries = handle
        .browse()
        .await
        .map_err(|error| AppError::bad_request(error.to_string()))?;
    Ok(Json(BrowseResponse { entries }))
}

#[derive(Debug, Deserialize)]
struct ResolveQuery {
    item: String,
}

/// Resolve an item within a source to a playable stream (a radio station id → its
/// stream URL). For sources whose items are library tracks, callers stream through
/// `/api/tracks/{id}/stream` instead, so those return 400 here.
async fn resolve_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ResolveQuery>,
) -> Result<Json<musicata_core::StreamSpec>, AppError> {
    let handle = state
        .providers
        .read()
        .await
        .get(&id)
        .ok_or_else(|| AppError::not_found(format!("unknown source: {id}")))?;
    let spec = handle
        .resolve(&query.item)
        .await
        .map_err(|error| AppError::bad_request(error.to_string()))?;
    Ok(Json(spec))
}

#[derive(Debug, Deserialize)]
struct DirectoryQuery {
    query: Option<String>,
    limit: Option<usize>,
}

/// Browse the public Radio Browser directory (proxied server-side). An empty query
/// returns the most-voted stations.
async fn radio_directory(
    State(state): State<AppState>,
    Query(query): Query<DirectoryQuery>,
) -> Result<Json<Vec<radiobrowser::DirectoryStation>>, AppError> {
    let client = state.radio_browser.clone();
    let text = query.query.unwrap_or_default();
    let limit = query.limit.unwrap_or(40).clamp(1, 100);
    let stations = tokio::task::spawn_blocking(move || client.search(&text, limit))
        .await
        .map_err(|error| AppError::internal(error.to_string()))?
        .map_err(AppError::internal)?;
    Ok(Json(stations))
}

/// Query parameters for `/api/tracks`: browse filters plus pagination/sorting.
///
/// Kept as one flat struct because `serde_urlencoded` (used by axum's `Query`)
/// does not support `#[serde(flatten)]`.
#[derive(Debug, Deserialize)]
struct TrackListQuery {
    genre: Option<String>,
    year: Option<u16>,
    composer: Option<String>,
    folder: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    sort: Option<String>,
}

async fn tracks(
    State(state): State<AppState>,
    Query(query): Query<TrackListQuery>,
) -> Result<Json<Page<Track>>, AppError> {
    let filter = BrowseFilter {
        genre: query.genre,
        year: query.year,
        composer: query.composer,
        folder: query.folder,
    };
    let page = ListQuery {
        limit: query.limit,
        offset: query.offset,
        sort: query.sort,
    };
    let (items, total) = state
        .database
        .list_tracks(
            &filter,
            page.sort.as_deref(),
            limit_arg(&page),
            offset_arg(&page),
        )
        .await
        .map_err(db_error)?;
    Ok(Json(page_envelope(items, total, &page)))
}

#[derive(Debug, Serialize)]
struct TrackMetadataReviewResponse {
    track_id: String,
    canonical: TrackCanonicalMetadataResponse,
    observations: Vec<MetadataObservationReviewResponse>,
}

#[derive(Debug, Serialize)]
struct TrackCanonicalMetadataResponse {
    title: String,
    artist_name: String,
    album_title: String,
    year: Option<u16>,
    track_number: Option<u16>,
}

#[derive(Debug, Serialize)]
struct MetadataObservationReviewResponse {
    source: String,
    confidence: f32,
    observed_at_unix_seconds: i64,
    approval_state: MetadataApprovalState,
    fields: Vec<TrackMetadataFieldObservation>,
}

#[derive(Debug, Deserialize)]
struct MetadataFieldReviewUpdate {
    source: String,
    observed_at_unix_seconds: i64,
    field_name: String,
    value: MetadataFieldValue,
    approval_state: MetadataApprovalState,
}

#[derive(Clone, Debug)]
struct AlbumArtworkCandidate {
    id: String,
    path: PathBuf,
    file_name: String,
    mime_type: String,
    file_size_bytes: Option<u64>,
    modified_at_unix_seconds: Option<i64>,
    selected: bool,
    rank: usize,
}

#[derive(Debug, Serialize)]
struct AlbumArtworkReviewResponse {
    album_id: String,
    selected_artwork_id: Option<String>,
    selected_artwork_url: Option<String>,
    candidates: Vec<AlbumArtworkCandidateResponse>,
}

#[derive(Debug, Serialize)]
struct AlbumArtworkCandidateResponse {
    id: String,
    source: &'static str,
    file_name: String,
    mime_type: String,
    file_size_bytes: Option<u64>,
    modified_at_unix_seconds: Option<i64>,
    selected: bool,
    preview_url: String,
}

#[derive(Debug, Deserialize)]
struct AlbumArtworkSelectionUpdate {
    artwork_id: String,
}

#[derive(Debug, Serialize)]
struct MetadataWriteBackPolicyResponse {
    enabled: bool,
    supports_file_tag_write_back: bool,
    supports_file_rename: bool,
    supports_artwork_write_back: bool,
    requires_preview: bool,
    reason: &'static str,
    future_enablement: Vec<&'static str>,
}

async fn track_metadata_review(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TrackMetadataReviewResponse>, AppError> {
    let library = state
        .database
        .load_library()
        .await?
        .ok_or_else(|| AppError::not_found(format!("unknown track: {id}")))?;
    let track = library
        .track(&id)
        .ok_or_else(|| AppError::not_found(format!("unknown track: {id}")))?;

    Ok(Json(metadata_review_response(track)))
}

async fn update_track_metadata_field_review(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(update): Json<MetadataFieldReviewUpdate>,
) -> Result<Json<TrackMetadataReviewResponse>, AppError> {
    let mut updated_library = state
        .database
        .load_library()
        .await?
        .ok_or_else(|| AppError::not_found(format!("unknown track: {id}")))?;
    let track = updated_library
        .tracks
        .iter_mut()
        .find(|track| track.id == id)
        .ok_or_else(|| AppError::not_found(format!("unknown track: {id}")))?;

    update_metadata_field_approval(track, &update)?;
    let response = metadata_review_response(track);

    state.database.save_library(&mut updated_library).await?;

    Ok(Json(response))
}

fn metadata_review_response(track: &Track) -> TrackMetadataReviewResponse {
    TrackMetadataReviewResponse {
        track_id: track.id.clone(),
        canonical: TrackCanonicalMetadataResponse {
            title: track.title.clone(),
            artist_name: track.artist_name.clone(),
            album_title: track.album_title.clone(),
            year: track.year,
            track_number: track.track_number,
        },
        observations: track
            .observed_metadata
            .iter()
            .map(|observation| MetadataObservationReviewResponse {
                source: observation.source.clone(),
                confidence: observation.confidence,
                observed_at_unix_seconds: observation.observed_at_unix_seconds,
                approval_state: observation.approval_state.clone(),
                fields: observation.effective_field_observations(),
            })
            .collect(),
    }
}

fn update_metadata_field_approval(
    track: &mut Track,
    update: &MetadataFieldReviewUpdate,
) -> Result<(), AppError> {
    let mut matched_field = None;

    for (observation_index, observation) in track.observed_metadata.iter_mut().enumerate() {
        if observation.field_observations.is_empty() {
            observation.field_observations = observation.effective_field_observations();
        }

        for (field_index, field) in observation.field_observations.iter().enumerate() {
            if metadata_field_matches(field, update) {
                if matched_field.is_some() {
                    return Err(AppError::bad_request(format!(
                        "metadata field selector is ambiguous for track: {}",
                        track.id
                    )));
                }
                matched_field = Some((observation_index, field_index));
            }
        }
    }

    let Some((observation_index, field_index)) = matched_field else {
        return Err(AppError::not_found(format!(
            "metadata field not found for track: {}",
            track.id
        )));
    };

    track.observed_metadata[observation_index].field_observations[field_index].approval_state =
        update.approval_state.clone();

    Ok(())
}

fn metadata_field_matches(
    field: &TrackMetadataFieldObservation,
    update: &MetadataFieldReviewUpdate,
) -> bool {
    field.source == update.source
        && field.observed_at_unix_seconds == update.observed_at_unix_seconds
        && field.field_name == update.field_name
        && field.value == update.value
}

async fn track_musicbrainz_lookup(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<MusicBrainzTrackLookupResponse>, AppError> {
    let track = {
        let library = state
            .database
            .load_library()
            .await?
            .ok_or_else(|| AppError::not_found(format!("unknown track: {id}")))?;
        library
            .track(&id)
            .cloned()
            .ok_or_else(|| AppError::not_found(format!("unknown track: {id}")))?
    };

    let musicbrainz = state.musicbrainz.clone();
    let lookup = tokio::task::spawn_blocking(move || musicbrainz.lookup_track(&track))
        .await
        .map_err(|error| AppError::internal(error.to_string()))?;

    Ok(Json(lookup))
}

#[derive(Debug, Deserialize)]
struct CandidateSearchQuery {
    limit: Option<usize>,
}

async fn track_musicbrainz_candidates(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<CandidateSearchQuery>,
) -> Result<Json<MusicBrainzTrackCandidateSearchResponse>, AppError> {
    let track = {
        let library = state
            .database
            .load_library()
            .await?
            .ok_or_else(|| AppError::not_found(format!("unknown track: {id}")))?;
        library
            .track(&id)
            .cloned()
            .ok_or_else(|| AppError::not_found(format!("unknown track: {id}")))?
    };

    let limit = query.limit.unwrap_or(5);
    let musicbrainz = state.musicbrainz.clone();
    let candidates =
        tokio::task::spawn_blocking(move || musicbrainz.search_track_candidates(&track, limit))
            .await
            .map_err(|error| AppError::internal(error.to_string()))?;

    Ok(Json(candidates))
}

async fn album_musicbrainz_candidates(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<CandidateSearchQuery>,
) -> Result<Json<MusicBrainzAlbumCandidateSearchResponse>, AppError> {
    let (album, album_tracks) = {
        let library = state
            .database
            .load_library()
            .await?
            .ok_or_else(|| AppError::not_found(format!("unknown album: {id}")))?;
        let album = library
            .album(&id)
            .cloned()
            .ok_or_else(|| AppError::not_found(format!("unknown album: {id}")))?;
        let album_tracks = library
            .tracks
            .iter()
            .filter(|track| track.album_id == id)
            .cloned()
            .collect::<Vec<_>>();
        (album, album_tracks)
    };

    let limit = query.limit.unwrap_or(5);
    let musicbrainz = state.musicbrainz.clone();
    let candidates = tokio::task::spawn_blocking(move || {
        musicbrainz.search_album_candidates(&album, &album_tracks, limit)
    })
    .await
    .map_err(|error| AppError::internal(error.to_string()))?;

    Ok(Json(candidates))
}

async fn album_cover_art_archive_candidates(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<CandidateSearchQuery>,
) -> Result<Json<CoverArtArchiveCandidateResponse>, AppError> {
    let (album, tracks) = {
        let library = state
            .database
            .load_library()
            .await?
            .ok_or_else(|| AppError::not_found(format!("unknown album: {id}")))?;
        let album = library
            .album(&id)
            .cloned()
            .ok_or_else(|| AppError::not_found(format!("unknown album: {id}")))?;
        let tracks = library
            .tracks
            .iter()
            .filter(|track| track.album_id == id)
            .cloned()
            .collect::<Vec<_>>();

        (album, tracks)
    };

    let limit = query.limit.unwrap_or(10);
    let musicbrainz = state.musicbrainz.clone();
    let candidates = tokio::task::spawn_blocking(move || {
        musicbrainz.cover_art_archive_candidates(&album, &tracks, limit)
    })
    .await
    .map_err(|error| AppError::internal(error.to_string()))?;

    Ok(Json(candidates))
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    playback_session: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

const DEFAULT_SEARCH_LIMIT: usize = 50;

async fn search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResults>, AppError> {
    let q = query.q.unwrap_or_default();
    let limit = query.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    let offset = query.offset.unwrap_or(0);
    let results = state
        .database
        .search(&q, limit, offset)
        .await
        .map_err(|error| AppError::internal(error.to_string()))?;
    Ok(Json(results))
}

async fn metadata_write_back_policy() -> impl IntoResponse {
    Json(MetadataWriteBackPolicyResponse {
        enabled: false,
        supports_file_tag_write_back: false,
        supports_file_rename: false,
        supports_artwork_write_back: false,
        requires_preview: true,
        reason: TAG_WRITE_BACK_DISABLED_REASON,
        future_enablement: vec![
            "provider must declare write support",
            "library config must opt in",
            "operation must include a preview diff",
            "operation must create a restorable metadata snapshot",
        ],
    })
}

async fn reject_metadata_write_back() -> AppError {
    AppError::write_back_disabled(TAG_WRITE_BACK_DISABLED_REASON)
}

async fn stream_track(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(session_id) = query.playback_session {
        let exists = state
            .playback_sessions
            .read()
            .await
            .contains_key(&session_id);
        if !exists {
            return Err(AppError::not_found(format!(
                "unknown playback session: {session_id}"
            )));
        }
    }

    let track = state
        .database
        .track(&id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::not_found(format!("unknown track: {id}")))?;
    let content_type = audio_content_type(&track.extension);
    let range_header = headers.get(RANGE).and_then(|value| value.to_str().ok());

    // Route by the track's source. SMB reads only the requested byte window over
    // the wire; the local provider serves the file from disk.
    #[cfg(feature = "provider-smb")]
    if let Some(providers::ProviderHandle::Smb(smb)) = state
        .providers
        .read()
        .await
        .get(&track.provider.provider_id)
    {
        return smb_stream_response(&smb, &track, range_header, content_type).await;
    }

    let bytes = tokio::fs::read(&track.path).await?;
    ranged_response(bytes, range_header, content_type)
}

/// Serve an SMB-backed track by **streaming** the requested byte range in chunks
/// (see `smb::read_range_stream`), so the first audio bytes reach the browser after a
/// single SMB read rather than buffering the whole range. A browser's `<audio>` opens
/// a track with `Range: bytes=0-`; under the old whole-range buffering that meant
/// pulling the entire file (~6.5 MB) across the wire before playback could start
/// (~600 ms) — chunked streaming drops first-byte latency to one read (~tens of ms)
/// while still honoring the full range and seeks. Mirrors Jellyfin's chunked-copy model.
#[cfg(feature = "provider-smb")]
async fn smb_stream_response(
    smb: &smb::SmbProvider,
    track: &musicata_core::Track,
    range: Option<&str>,
    content_type: &str,
) -> Result<Response, AppError> {
    let total = track.file_size_bytes.unwrap_or(0) as usize;
    let item_id = &track.provider.item_id;

    if total == 0 {
        return Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, content_type)
            .header(ACCEPT_RANGES, "bytes")
            .header(CONTENT_LENGTH, "0")
            .body(Body::empty())
            .map_err(AppError::from);
    }

    let had_range = range.is_some();
    // An unparseable/absent range is the whole file from offset 0.
    let (start, end) = range
        .and_then(|range| parse_range(range, total))
        .unwrap_or((0, total - 1));
    let length = end - start + 1;

    let stream = smb
        .read_range_stream(item_id, start as u64, end as u64)
        .await
        .map_err(|error| AppError::internal(error.to_string()))?;

    // Content-Length is known exactly (the range length) even though the body streams.
    let mut builder = Response::builder()
        .header(CONTENT_TYPE, content_type)
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, length.to_string());
    builder = if had_range {
        builder
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_RANGE, format!("bytes {start}-{end}/{total}"))
    } else {
        builder.status(StatusCode::OK)
    };
    builder
        .body(Body::from_stream(stream))
        .map_err(AppError::from)
}

fn new_playback_session_id(sequence: u64) -> String {
    format!("{:x}-{:x}", now_unix_seconds(), sequence)
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

async fn album_artwork_review(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AlbumArtworkReviewResponse>, AppError> {
    let library = state
        .database
        .load_library()
        .await?
        .ok_or_else(|| AppError::not_found(format!("unknown album: {id}")))?;
    Ok(Json(album_artwork_review_response(&library, &id)?))
}

async fn update_album_artwork(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(update): Json<AlbumArtworkSelectionUpdate>,
) -> Result<Json<AlbumArtworkReviewResponse>, AppError> {
    let mut library = state
        .database
        .load_library()
        .await?
        .ok_or_else(|| AppError::not_found(format!("unknown album: {id}")))?;
    let candidate = album_artwork_candidates(&library, &id)?
        .into_iter()
        .find(|candidate| candidate.id == update.artwork_id)
        .ok_or_else(|| {
            AppError::not_found(format!(
                "unknown artwork candidate for album {id}: {}",
                update.artwork_id
            ))
        })?;
    let album = library
        .albums
        .iter_mut()
        .find(|album| album.id == id)
        .ok_or_else(|| AppError::not_found(format!("unknown album: {id}")))?;

    album.artwork_path = Some(candidate.path);
    album.artwork_url = Some(album_artwork_url(
        &album.id,
        album.artwork_path.as_ref().unwrap(),
    ));

    // Persist just the album's artwork columns rather than rewriting the library.
    state
        .database
        .set_album_artwork(
            &id,
            album.artwork_path.as_deref(),
            album.artwork_url.as_deref(),
        )
        .await
        .map_err(db_error)?;

    let response = album_artwork_review_response(&library, &id)?;
    Ok(Json(response))
}

async fn album_artwork(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ArtworkQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let size = normalize_size(query.size);
    // An externally-acquired cover (iTunes/Deezer/Cover Art Archive) takes precedence —
    // its bytes live in the artwork cache, keyed by the stored cache_key.
    if let Ok(Some(acquired)) = state.database.acquired_artwork(&id).await
        && acquired.status == "acquired"
        && let Some(key) = acquired.cache_key.clone()
    {
        let ext = acquired.ext.clone().unwrap_or_else(|| "jpg".to_string());
        let etag = artwork_etag(&key, size);
        if etag_matches(headers.get(IF_NONE_MATCH), &etag) {
            return artwork_not_modified(&etag);
        }
        let content_type = image_content_type(&ext);
        if let Some(bytes) = state.artwork_cache.get(&key, &ext).await {
            return serve_sized_artwork(&state.artwork_cache, &key, content_type, bytes, size, etag)
                .await;
        }
        // Cache miss (cleared/evicted) but we recorded the source — re-fetch once.
        if let Some(url) = acquired.remote_url.clone() {
            let downloaded =
                tokio::task::spawn_blocking(move || artwork_providers::download_image(&url))
                    .await
                    .ok()
                    .and_then(Result::ok);
            if let Some((bytes, _)) = downloaded {
                state.artwork_cache.put(&key, &ext, &bytes).await;
                return serve_sized_artwork(
                    &state.artwork_cache,
                    &key,
                    content_type,
                    bytes,
                    size,
                    etag,
                )
                .await;
            }
        }
        // Couldn't produce it — fall through to the local/embedded logic (likely 404).
    }

    let album = state
        .database
        .album(&id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::not_found(format!("unknown album: {id}")))?;
    let path = album
        .artwork_path
        .ok_or_else(|| AppError::not_found(format!("album has no artwork: {id}")))?;

    // An album with no folder cover falls back to a track's *embedded* artwork — in that
    // case `artwork_path` points at the audio file, so extract the picture (once) and
    // cache it, rather than trying to serve the audio file as an image.
    if is_embedded_artwork(&path) {
        const EMBEDDED_CACHE_EXT: &str = "embedded";
        let key = artwork_asset_id(&path);
        let etag = artwork_etag(&key, size);
        if etag_matches(headers.get(IF_NONE_MATCH), &etag) {
            return artwork_not_modified(&etag);
        }
        if let Some(bytes) = state.artwork_cache.get(&key, EMBEDDED_CACHE_EXT).await {
            let content_type = sniff_image_content_type(&bytes);
            return serve_sized_artwork(&state.artwork_cache, &key, content_type, bytes, size, etag)
                .await;
        }
        // Read the audio file (over the source provider or local disk) once, extract its
        // embedded cover, and cache the image so the network stays off the hot path.
        let audio = read_album_source_file(&state, &id, &path).await?;
        let image = musicata_core::extract_embedded_cover(&audio, &path)
            .ok_or_else(|| AppError::not_found(format!("album has no embedded artwork: {id}")))?;
        state
            .artwork_cache
            .put(&key, EMBEDDED_CACHE_EXT, &image)
            .await;
        let content_type = sniff_image_content_type(&image);
        return serve_sized_artwork(&state.artwork_cache, &key, content_type, image, size, etag)
            .await;
    }

    // Route by the album's source. An album whose tracks live on a network source
    // (e.g. SMB) stores its cover's share-relative path, which isn't a local file, so
    // it must be fetched through the provider rather than read off the local disk.
    #[cfg(feature = "provider-smb")]
    {
        let provider_id = state
            .database
            .album_provider_id(&id)
            .await
            .map_err(db_error)?;
        if let Some(provider_id) = provider_id
            && let Some(providers::ProviderHandle::Smb(smb)) =
                state.providers.read().await.get(&provider_id)
        {
            let key = artwork_asset_id(&path);
            let extension = artwork_extension(&path);
            let etag = artwork_etag(&key, size);
            if etag_matches(headers.get(IF_NONE_MATCH), &etag) {
                return artwork_not_modified(&etag);
            }
            let content_type = image_content_type(extension);
            // Serve the cached copy if we've fetched this cover before; otherwise fetch
            // it over the wire once, cache it, and serve. Caching keeps the network off
            // the per-request hot path and lets covers show even when the share is down.
            if let Some(bytes) = state.artwork_cache.get(&key, extension).await {
                return serve_sized_artwork(
                    &state.artwork_cache,
                    &key,
                    content_type,
                    bytes,
                    size,
                    etag,
                )
                .await;
            }
            // A missing/unreadable cover on the share is "no artwork" (404 → the UI
            // shows its monogram fallback), never a 500.
            let item_id = path.to_string_lossy().into_owned();
            let bytes = smb.read_file(&item_id).await.map_err(|error| {
                AppError::not_found(format!("album artwork unavailable: {error}"))
            })?;
            state.artwork_cache.put(&key, extension, &bytes).await;
            return serve_sized_artwork(&state.artwork_cache, &key, content_type, bytes, size, etag)
                .await;
        }
    }

    serve_artwork(&state.artwork_cache, path, size, headers).await
}

/// Serve an artist's externally-acquired image (`?size=` aware). Artist art is
/// acquired-only — there's no folder/embedded source — so a coverless artist is a 404
/// and the UI shows its monogram. Mirrors the acquired-cover branch of `album_artwork`.
async fn artist_artwork(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ArtworkQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let size = normalize_size(query.size);
    if let Ok(Some(acquired)) = state.database.acquired_artist_artwork(&id).await
        && acquired.status == "acquired"
        && let Some(key) = acquired.cache_key.clone()
    {
        let ext = acquired.ext.clone().unwrap_or_else(|| "jpg".to_string());
        let etag = artwork_etag(&key, size);
        if etag_matches(headers.get(IF_NONE_MATCH), &etag) {
            return artwork_not_modified(&etag);
        }
        let content_type = image_content_type(&ext);
        if let Some(bytes) = state.artwork_cache.get(&key, &ext).await {
            return serve_sized_artwork(&state.artwork_cache, &key, content_type, bytes, size, etag)
                .await;
        }
        // Cache miss but we recorded the source — re-fetch once.
        if let Some(url) = acquired.remote_url.clone() {
            let downloaded =
                tokio::task::spawn_blocking(move || artwork_providers::download_image(&url))
                    .await
                    .ok()
                    .and_then(Result::ok);
            if let Some((bytes, _)) = downloaded {
                state.artwork_cache.put(&key, &ext, &bytes).await;
                return serve_sized_artwork(
                    &state.artwork_cache,
                    &key,
                    content_type,
                    bytes,
                    size,
                    etag,
                )
                .await;
            }
        }
    }
    Err(AppError::not_found(format!("artist has no artwork: {id}")))
}

async fn album_artwork_candidate(
    State(state): State<AppState>,
    Path((id, artwork_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let path = {
        let library = state
            .database
            .load_library()
            .await?
            .ok_or_else(|| AppError::not_found(format!("unknown album: {id}")))?;
        album_artwork_candidates(&library, &id)?
            .into_iter()
            .find(|candidate| candidate.id == artwork_id)
            .map(|candidate| candidate.path)
            .ok_or_else(|| {
                AppError::not_found(format!(
                    "unknown artwork candidate for album {id}: {artwork_id}"
                ))
            })?
    };

    // Review candidates are shown full-size for inspection — no thumbnailing.
    serve_artwork(&state.artwork_cache, path, None, headers).await
}

fn album_artwork_review_response(
    library: &Library,
    album_id: &str,
) -> Result<AlbumArtworkReviewResponse, AppError> {
    let album = library
        .album(album_id)
        .ok_or_else(|| AppError::not_found(format!("unknown album: {album_id}")))?;
    let candidates = album_artwork_candidates(library, album_id)?;

    Ok(AlbumArtworkReviewResponse {
        album_id: album_id.to_string(),
        selected_artwork_id: album
            .artwork_path
            .as_ref()
            .map(|path| artwork_asset_id(path)),
        selected_artwork_url: album
            .artwork_path
            .as_ref()
            .map(|path| album_artwork_url(album_id, path)),
        candidates: candidates
            .into_iter()
            .map(|candidate| AlbumArtworkCandidateResponse {
                preview_url: format!(
                    "/api/albums/{album_id}/artwork/candidates/{}?asset={}",
                    candidate.id, candidate.id
                ),
                id: candidate.id,
                source: "local_file",
                file_name: candidate.file_name,
                mime_type: candidate.mime_type,
                file_size_bytes: candidate.file_size_bytes,
                modified_at_unix_seconds: candidate.modified_at_unix_seconds,
                selected: candidate.selected,
            })
            .collect(),
    })
}

fn album_artwork_candidates(
    library: &Library,
    album_id: &str,
) -> Result<Vec<AlbumArtworkCandidate>, AppError> {
    let album = library
        .album(album_id)
        .ok_or_else(|| AppError::not_found(format!("unknown album: {album_id}")))?;
    let selected_id = album
        .artwork_path
        .as_ref()
        .map(|path| artwork_asset_id(path));
    let mut paths = Vec::new();

    if let Some(path) = album.artwork_path.as_ref() {
        push_unique_path(&mut paths, path.clone());
    }

    for track in library
        .tracks
        .iter()
        .filter(|track| track.album_id == album_id)
    {
        if let Some(parent) = track.path.parent() {
            for path in find_album_artwork_candidates(parent) {
                push_unique_path(&mut paths, path);
            }
        }
    }

    let mut candidates = paths
        .into_iter()
        .enumerate()
        .map(|(rank, path)| album_artwork_candidate_from_path(path, selected_id.as_deref(), rank))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .selected
            .cmp(&left.selected)
            .then_with(|| left.rank.cmp(&right.rank))
            .then_with(|| left.path.cmp(&right.path))
    });

    Ok(candidates)
}

fn album_artwork_candidate_from_path(
    path: PathBuf,
    selected_id: Option<&str>,
    rank: usize,
) -> AlbumArtworkCandidate {
    let id = artwork_asset_id(&path);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let metadata = fs::metadata(&path).ok();

    AlbumArtworkCandidate {
        selected: selected_id == Some(id.as_str()),
        id,
        file_name: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artwork")
            .to_string(),
        mime_type: image_content_type(extension).to_string(),
        file_size_bytes: metadata.as_ref().map(|metadata| metadata.len()),
        modified_at_unix_seconds: metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(system_time_to_unix_seconds),
        path,
        rank,
    }
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

async fn serve_artwork(
    cache: &artwork::ArtworkCache,
    path: PathBuf,
    size: Option<u32>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let key = artwork_asset_id(&path);
    let etag = artwork_etag(&key, size);

    if etag_matches(headers.get(IF_NONE_MATCH), &etag) {
        return artwork_not_modified(&etag);
    }

    // A cover that can't be read (missing file, stale path, permissions) is "no
    // artwork" — return 404 so the client falls back to its monogram, never a 500.
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| AppError::not_found(format!("album artwork unavailable: {error}")))?;

    let content_type = image_content_type(artwork_extension(&path));
    serve_sized_artwork(cache, &key, content_type, bytes, size, etag).await
}

fn artwork_extension(path: &std::path::Path) -> &str {
    path.extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
}

fn artwork_not_modified(etag: &str) -> Result<Response, AppError> {
    Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(CACHE_CONTROL, ARTWORK_CACHE_CONTROL)
        .header(ETAG, etag)
        .body(Body::empty())
        .map_err(AppError::from)
}

/// Build a 200 artwork response with an explicit content type. The content type is the
/// image format — from the source path's extension, sniffed embedded bytes, or
/// `image/jpeg` for a resized variant.
fn artwork_response_typed(
    bytes: Vec<u8>,
    content_type: &'static str,
    etag: String,
) -> Result<Response, AppError> {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type)
        .header(CONTENT_LENGTH, bytes.len().to_string())
        .header(CACHE_CONTROL, ARTWORK_CACHE_CONTROL)
        .header(ETAG, etag)
        .body(Body::from(bytes))
        .map_err(AppError::from)
}

/// Cover-art thumbnail sizes the server will produce, smallest first. A requested size
/// snaps up to the nearest rung; anything past the top serves the original. Keeping the
/// ladder tiny bounds the number of cached variants per cover.
const ARTWORK_SIZES: [u32; 3] = [128, 300, 600];

#[derive(Debug, Deserialize)]
struct ArtworkQuery {
    /// Desired max edge in pixels. Absent (or beyond the ladder) → the full original.
    size: Option<u32>,
}

/// Snap a requested size up to the nearest ladder rung; `None` (serve the original) when
/// no size is asked for or it's at/above the largest rung.
fn normalize_size(requested: Option<u32>) -> Option<u32> {
    let requested = requested?;
    ARTWORK_SIZES.iter().copied().find(|&rung| requested <= rung)
}

/// A size-aware ETag so a client can't reuse a thumbnail body for the full cover (or vice
/// versa); the unsized form matches the original-bytes etag used elsewhere.
fn artwork_etag(key: &str, size: Option<u32>) -> String {
    match size {
        Some(size) => format!("\"{key}-{size}\""),
        None => format!("\"{key}\""),
    }
}

/// Downscale cover bytes to fit a `size`×`size` box (preserving aspect, never upscaling)
/// and re-encode as JPEG. `None` if the bytes can't be decoded — the caller then serves
/// the original untouched. Synchronous + CPU-bound: call inside `spawn_blocking`.
fn resize_to_jpeg(original: &[u8], size: u32) -> Option<Vec<u8>> {
    let thumbnail = image::load_from_memory(original).ok()?.thumbnail(size, size);
    let mut buffer = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, 82)
        .encode_image(&thumbnail.to_rgb8())
        .ok()?;
    Some(buffer)
}

/// Serve cover art at the requested size: the original bytes when `size` is `None`,
/// otherwise a cached `size`×`size` JPEG variant (resized once, then served from the
/// artwork cache). A decode/resize failure falls back to the original — never a 500.
async fn serve_sized_artwork(
    cache: &artwork::ArtworkCache,
    key: &str,
    original_content_type: &'static str,
    original: Vec<u8>,
    size: Option<u32>,
    etag: String,
) -> Result<Response, AppError> {
    let Some(size) = size else {
        return artwork_response_typed(original, original_content_type, etag);
    };
    let variant_ext = format!("{size}.jpg");
    if let Some(bytes) = cache.get(key, &variant_ext).await {
        return artwork_response_typed(bytes, "image/jpeg", etag);
    }
    let source = original.clone();
    let resized = tokio::task::spawn_blocking(move || resize_to_jpeg(&source, size))
        .await
        .ok()
        .flatten();
    match resized {
        Some(bytes) => {
            cache.put(key, &variant_ext, &bytes).await;
            artwork_response_typed(bytes, "image/jpeg", etag)
        }
        // Undecodable bytes: serve the original (unscaled) rather than failing.
        None => artwork_response_typed(original, original_content_type, etag),
    }
}

/// Whether an album's `artwork_path` points at an audio file (an embedded-cover
/// fallback) rather than an image file (a folder cover).
fn is_embedded_artwork(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("mp3" | "flac" | "m4a" | "aac" | "ogg" | "opus" | "wav")
    )
}

/// Image content type by magic bytes — embedded covers carry no filename, so the
/// format is detected from the data (defaulting to JPEG, the common case).
fn sniff_image_content_type(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

/// Read an album's source file (the cover image or, for embedded art, the audio file)
/// from its provider — over SMB for a network album, else from local disk.
#[cfg_attr(not(feature = "provider-smb"), allow(unused_variables))]
async fn read_album_source_file(
    state: &AppState,
    album_id: &str,
    path: &std::path::Path,
) -> Result<Vec<u8>, AppError> {
    #[cfg(feature = "provider-smb")]
    {
        let provider_id = state
            .database
            .album_provider_id(album_id)
            .await
            .map_err(db_error)?;
        if let Some(provider_id) = provider_id
            && let Some(providers::ProviderHandle::Smb(smb)) =
                state.providers.read().await.get(&provider_id)
        {
            let item_id = path.to_string_lossy().into_owned();
            return smb.read_file(&item_id).await.map_err(|error| {
                AppError::not_found(format!("artwork source unavailable: {error}"))
            });
        }
    }
    tokio::fs::read(path)
        .await
        .map_err(|error| AppError::not_found(format!("artwork source unavailable: {error}")))
}

fn etag_matches(value: Option<&axum::http::HeaderValue>, etag: &str) -> bool {
    value
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate == etag || candidate == "*")
        })
}

fn system_time_to_unix_seconds(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

fn ranged_response(
    bytes: Vec<u8>,
    range: Option<&str>,
    content_type: &str,
) -> Result<Response, AppError> {
    let total_len = bytes.len();

    if let Some((start, end)) = range.and_then(|range| parse_range(range, total_len)) {
        let body = bytes[start..=end].to_vec();
        return Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_TYPE, content_type)
            .header(ACCEPT_RANGES, "bytes")
            .header(CONTENT_LENGTH, body.len().to_string())
            .header(CONTENT_RANGE, format!("bytes {start}-{end}/{total_len}"))
            .body(Body::from(body))
            .map_err(AppError::from);
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type)
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, total_len.to_string())
        .body(Body::from(bytes))
        .map_err(AppError::from)
}

fn parse_range(range: &str, total_len: usize) -> Option<(usize, usize)> {
    let range = range.strip_prefix("bytes=")?;
    let (start, end) = range.split_once('-')?;

    if total_len == 0 {
        return None;
    }

    match (start.trim(), end.trim()) {
        ("", suffix_len) => {
            let suffix_len = suffix_len.parse::<usize>().ok()?;
            let start = total_len.saturating_sub(suffix_len);
            Some((start, total_len - 1))
        }
        (start, "") => {
            let start = start.parse::<usize>().ok()?;
            if start >= total_len {
                None
            } else {
                Some((start, total_len - 1))
            }
        }
        (start, end) => {
            let start = start.parse::<usize>().ok()?;
            let end = end.parse::<usize>().ok()?.min(total_len - 1);
            if start > end || start >= total_len {
                None
            } else {
                Some((start, end))
            }
        }
    }
}

fn audio_content_type(extension: &str) -> &'static str {
    match extension.to_ascii_lowercase().as_str() {
        "flac" => "audio/flac",
        "m4a" | "aac" => "audio/aac",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        _ => "audio/mpeg",
    }
}

fn image_content_type(extension: &str) -> &'static str {
    match extension.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "webp" => "image/webp",
        _ => "image/jpeg",
    }
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl AppError {
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.into(),
        }
    }

    fn write_back_disabled(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "write_back_disabled",
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: message.into(),
        }
    }

    /// The target player is unreachable. The command still updated the authoritative
    /// server queue (so the track is queued); this tells the controller to surface a
    /// clear "offline" message instead of a silent failure.
    fn player_offline(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "player_offline",
            message: message.into(),
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<axum::http::Error> for AppError {
    fn from(error: axum::http::Error) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<anyhow::Error> for AppError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(error.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = Json(ErrorEnvelope {
            error: ErrorBody {
                code: self.code,
                message: self.message,
            },
        });
        (self.status, body).into_response()
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

impl Config {
    fn from_args() -> Result<Self> {
        Self::from_sources(std::env::args().skip(1), |name| std::env::var(name).ok())
    }

    fn from_sources(
        args: impl IntoIterator<Item = String>,
        mut env: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self> {
        let cli = ConfigOverrides::from_args(args)?;
        let config_path = cli
            .config_path
            .clone()
            .or_else(|| env("MUSICATA_CONFIG").map(PathBuf::from));

        let mut config = Self::default();

        if let Some(path) = config_path {
            ConfigOverrides::from_file(&path)?.apply_to(&mut config);
        }

        let incremental_rescan = env("MUSICATA_INCREMENTAL_RESCAN")
            .map(|value| parse_bool(&value, "MUSICATA_INCREMENTAL_RESCAN"))
            .transpose()?;
        let no_incremental_rescan = env("MUSICATA_NO_INCREMENTAL_RESCAN")
            .map(|value| parse_bool(&value, "MUSICATA_NO_INCREMENTAL_RESCAN"))
            .transpose()?
            .or_else(|| incremental_rescan.map(|value| !value));

        ConfigOverrides {
            config_path: None,
            library: env("MUSICATA_LIBRARY")
                .or_else(|| env("MUSICATA_LIBRARY_PATH"))
                .map(PathBuf::from),
            database: env("MUSICATA_DATABASE")
                .or_else(|| env("MUSICATA_DATABASE_PATH"))
                .map(PathBuf::from),
            addr: env("MUSICATA_ADDR")
                .or_else(|| env("MUSICATA_BIND_ADDR"))
                .map(|value| parse_addr(&value, "MUSICATA_ADDR"))
                .transpose()?,
            rescan: env("MUSICATA_RESCAN")
                .map(|value| parse_bool(&value, "MUSICATA_RESCAN"))
                .transpose()?,
            no_incremental_rescan,
            scan_once: env("MUSICATA_SCAN_ONCE")
                .map(|value| parse_bool(&value, "MUSICATA_SCAN_ONCE"))
                .transpose()?,
            no_scan: env("MUSICATA_NO_SCAN")
                .map(|value| parse_bool(&value, "MUSICATA_NO_SCAN"))
                .transpose()?,
            mpd_addrs: env("MUSICATA_MPD").map(|value| parse_addr_list(&value)),
            public_url: env("MUSICATA_PUBLIC_URL"),
            subsonic_user: env("MUSICATA_SUBSONIC_USER"),
            subsonic_password: env("MUSICATA_SUBSONIC_PASSWORD"),
        }
        .apply_to(&mut config);

        cli.apply_to(&mut config);

        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            library: PathBuf::from("testdata"),
            database: PathBuf::from(".musicata/musicata.db"),
            addr: "127.0.0.1:3030"
                .parse()
                .expect("default socket address is valid"),
            rescan: false,
            no_incremental_rescan: false,
            scan_once: false,
            no_scan: false,
            mpd_addrs: Vec::new(),
            public_url: None,
            subsonic_user: "musicata".to_string(),
            subsonic_password: None,
        }
    }
}

#[derive(Debug, Default)]
struct ConfigOverrides {
    config_path: Option<PathBuf>,
    library: Option<PathBuf>,
    database: Option<PathBuf>,
    addr: Option<SocketAddr>,
    rescan: Option<bool>,
    no_incremental_rescan: Option<bool>,
    scan_once: Option<bool>,
    no_scan: Option<bool>,
    mpd_addrs: Option<Vec<String>>,
    public_url: Option<String>,
    subsonic_user: Option<String>,
    subsonic_password: Option<String>,
}

impl ConfigOverrides {
    fn from_args(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut overrides = Self::default();
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--config" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--config requires a path"))?;
                    overrides.config_path = Some(PathBuf::from(value));
                }
                "--library" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--library requires a path"))?;
                    overrides.library = Some(PathBuf::from(value));
                }
                "--database" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--database requires a path"))?;
                    overrides.database = Some(PathBuf::from(value));
                }
                "--addr" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--addr requires host:port"))?;
                    overrides.addr = Some(parse_addr(&value, "--addr")?);
                }
                "--rescan" => overrides.rescan = Some(true),
                "--no-incremental-rescan" => overrides.no_incremental_rescan = Some(true),
                "--scan-once" => overrides.scan_once = Some(true),
                "--no-scan" => overrides.no_scan = Some(true),
                "--mpd" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--mpd requires host:port[,host:port]"))?;
                    overrides.mpd_addrs = Some(parse_addr_list(&value));
                }
                "--public-url" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--public-url requires a URL"))?;
                    overrides.public_url = Some(value);
                }
                "--subsonic-user" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--subsonic-user requires a username"))?;
                    overrides.subsonic_user = Some(value);
                }
                "--subsonic-password" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--subsonic-password requires a password"))?;
                    overrides.subsonic_password = Some(value);
                }
                "--help" | "-h" => {
                    println!(
                        "Usage: musicata-server [--config PATH] [--library PATH] [--database PATH] [--addr HOST:PORT] [--rescan] [--no-incremental-rescan] [--scan-once] [--no-scan] [--mpd HOST:PORT[,HOST:PORT]] [--public-url URL] [--subsonic-user USER] [--subsonic-password PASS]\n\nConfig precedence: defaults < config file < environment < CLI\nEnvironment: MUSICATA_CONFIG, MUSICATA_LIBRARY, MUSICATA_DATABASE, MUSICATA_ADDR, MUSICATA_RESCAN, MUSICATA_INCREMENTAL_RESCAN, MUSICATA_SCAN_ONCE, MUSICATA_NO_SCAN, MUSICATA_MPD, MUSICATA_PUBLIC_URL, MUSICATA_SUBSONIC_USER, MUSICATA_SUBSONIC_PASSWORD\nConfig file keys: library, database, addr, rescan, incremental_rescan, scan_once, no_scan, mpd, public_url, subsonic_user, subsonic_password\nDefaults: --library testdata --database .musicata/musicata.db --addr 127.0.0.1:3030"
                    );
                    std::process::exit(0);
                }
                value => return Err(anyhow!("unknown argument: {value}")),
            }
        }

        Ok(overrides)
    }

    fn from_file(path: &PathBuf) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let mut overrides = Self::default();

        for (index, line) in content.lines().enumerate() {
            let line = line.split_once('#').map(|(line, _)| line).unwrap_or(line);
            let line = line.trim();

            if line.is_empty() {
                continue;
            }

            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| anyhow!("{}:{}: expected key = value", path.display(), index + 1))?;
            let key = key.trim();
            let value = unquote(value.trim());

            match key {
                "library" | "library_path" => overrides.library = Some(PathBuf::from(value)),
                "database" | "database_path" => overrides.database = Some(PathBuf::from(value)),
                "addr" | "bind_addr" => overrides.addr = Some(parse_addr(value, "config addr")?),
                "rescan" => overrides.rescan = Some(parse_bool(value, "config rescan")?),
                "incremental_rescan" => {
                    overrides.no_incremental_rescan =
                        Some(!parse_bool(value, "config incremental_rescan")?)
                }
                "no_incremental_rescan" => {
                    overrides.no_incremental_rescan =
                        Some(parse_bool(value, "config no_incremental_rescan")?)
                }
                "scan_once" => overrides.scan_once = Some(parse_bool(value, "config scan_once")?),
                "no_scan" => overrides.no_scan = Some(parse_bool(value, "config no_scan")?),
                "mpd" | "mpd_addrs" => overrides.mpd_addrs = Some(parse_addr_list(value)),
                "public_url" => overrides.public_url = Some(value.to_string()),
                "subsonic_user" => overrides.subsonic_user = Some(value.to_string()),
                "subsonic_password" => overrides.subsonic_password = Some(value.to_string()),
                value => {
                    return Err(anyhow!(
                        "{}:{}: unknown config key `{value}`",
                        path.display(),
                        index + 1
                    ));
                }
            }
        }

        Ok(overrides)
    }

    fn apply_to(self, config: &mut Config) {
        if let Some(library) = self.library {
            config.library = library;
        }

        if let Some(database) = self.database {
            config.database = database;
        }

        if let Some(addr) = self.addr {
            config.addr = addr;
        }

        if let Some(rescan) = self.rescan {
            config.rescan = rescan;
        }

        if let Some(no_incremental_rescan) = self.no_incremental_rescan {
            config.no_incremental_rescan = no_incremental_rescan;
        }

        if let Some(scan_once) = self.scan_once {
            config.scan_once = scan_once;
        }
        if let Some(no_scan) = self.no_scan {
            config.no_scan = no_scan;
        }

        if let Some(mpd_addrs) = self.mpd_addrs {
            config.mpd_addrs = mpd_addrs;
        }

        if let Some(public_url) = self.public_url {
            config.public_url = Some(public_url);
        }

        if let Some(subsonic_user) = self.subsonic_user {
            config.subsonic_user = subsonic_user;
        }

        if let Some(subsonic_password) = self.subsonic_password {
            config.subsonic_password = Some(subsonic_password);
        }
    }
}

/// Parse a comma-separated list of `host:port` addresses, trimming blanks.
fn parse_addr_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_addr(value: &str, source: &str) -> Result<SocketAddr> {
    value
        .parse()
        .with_context(|| format!("invalid {source} value: {value}"))
}

fn parse_bool(value: &str, source: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("invalid {source} value: {value}")),
    }
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("musicata_server=info,musicata_core=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::{
        ARTWORK_CACHE_CONTROL, Config, PlayerManager, ProviderHandle, ProviderRegistry, app,
        artwork_etag, normalize_size, parse_range, resize_to_jpeg,
    };
    use axum::{
        body::{Body, to_bytes},
        http::{
            Request, StatusCode,
            header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH, RANGE},
        },
    };
    use musicata_core::{Library, LocalDiskProvider, scan_local_library};
    use musicata_storage::Database;
    use std::{collections::HashMap, fs, path::PathBuf, time::SystemTime};
    use tower::ServiceExt;

    #[test]
    fn parses_common_byte_ranges() {
        assert_eq!(parse_range("bytes=0-", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=150-", 100), None);
    }

    #[test]
    fn loads_default_config() {
        let config = Config::from_sources(Vec::<String>::new(), |_| None).expect("config");

        assert_eq!(config.library, PathBuf::from("testdata"));
        assert_eq!(config.database, PathBuf::from(".musicata/musicata.db"));
        assert_eq!(config.addr.to_string(), "127.0.0.1:3030");
        assert!(!config.rescan);
        assert!(!config.no_incremental_rescan);
        assert!(!config.scan_once);
    }

    #[test]
    fn layers_config_file_env_and_cli() {
        let config_file = TempConfigFile::new(
            "layered",
            r#"
            # Musicata config
            library = "/from/file"
            database = "/from/file.db"
            addr = "127.0.0.1:4000"
            rescan = false
            incremental_rescan = false
            scan_once = false
            "#,
        );
        let env = HashMap::from([
            (
                "MUSICATA_CONFIG".to_string(),
                config_file.path.to_string_lossy().to_string(),
            ),
            ("MUSICATA_LIBRARY".to_string(), "/from/env".to_string()),
            ("MUSICATA_DATABASE".to_string(), "/from/env.db".to_string()),
            ("MUSICATA_ADDR".to_string(), "127.0.0.1:5000".to_string()),
            ("MUSICATA_RESCAN".to_string(), "false".to_string()),
            (
                "MUSICATA_INCREMENTAL_RESCAN".to_string(),
                "true".to_string(),
            ),
            ("MUSICATA_SCAN_ONCE".to_string(), "false".to_string()),
        ]);
        let config = Config::from_sources(
            [
                "--library".to_string(),
                "/from/cli".to_string(),
                "--database".to_string(),
                "/from/cli.db".to_string(),
                "--addr".to_string(),
                "127.0.0.1:6000".to_string(),
                "--rescan".to_string(),
                "--no-incremental-rescan".to_string(),
                "--scan-once".to_string(),
            ],
            |name| env.get(name).cloned(),
        )
        .expect("config");

        assert_eq!(config.library, PathBuf::from("/from/cli"));
        assert_eq!(config.database, PathBuf::from("/from/cli.db"));
        assert_eq!(config.addr.to_string(), "127.0.0.1:6000");
        assert!(config.rescan);
        assert!(config.no_incremental_rescan);
        assert!(config.scan_once);
    }

    #[tokio::test]
    async fn serves_library_summary_json() {
        let fixture = TestFixture::new("summary");
        let app = fixture.app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/library/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_text(response.into_body()).await;

        assert!(body.contains(r#""track_count":3"#));
        assert!(body.contains(r#""album_count":2"#));
    }

    #[tokio::test]
    async fn serves_album_detail_json() {
        let fixture = TestFixture::new("album-detail");
        let library = fixture.library();
        let album = library
            .albums
            .iter()
            .find(|album| album.title == "Paramparcad")
            .expect("album")
            .clone();
        let app = fixture.app_with_library(library).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/albums/{}", album.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_text(response.into_body()).await;
        let value: serde_json::Value = serde_json::from_str(&body).expect("album detail json");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["album"]["id"], album.id);
        assert_eq!(value["artist"]["name"], "Darkwood Dub");
        assert_eq!(value["tracks"].as_array().expect("tracks").len(), 2);
        assert!(
            value["tracks"]
                .as_array()
                .expect("tracks")
                .iter()
                .all(|track| track["album_id"] == album.id)
        );
    }

    #[tokio::test]
    async fn unknown_album_detail_returns_not_found() {
        let fixture = TestFixture::new("missing-album-detail");
        let app = fixture.app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/albums/missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_text(response.into_body()).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("unknown album: missing"));
    }

    #[tokio::test]
    async fn serves_artist_detail_json() {
        let fixture = TestFixture::new("artist-detail");
        let library = fixture.library();
        let artist = library
            .artists
            .iter()
            .find(|artist| artist.name == "Darkwood Dub")
            .expect("artist")
            .clone();
        let app = fixture.app_with_library(library).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/artists/{}", artist.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_text(response.into_body()).await;
        let value: serde_json::Value = serde_json::from_str(&body).expect("artist detail json");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["artist"]["name"], "Darkwood Dub");
        assert_eq!(value["albums"].as_array().expect("albums").len(), 2);
        assert_eq!(value["tracks"].as_array().expect("tracks").len(), 3);
        assert!(
            value["tracks"]
                .as_array()
                .expect("tracks")
                .iter()
                .all(|track| track["artist_id"] == artist.id)
        );
    }

    #[tokio::test]
    async fn unknown_artist_detail_returns_not_found() {
        let fixture = TestFixture::new("missing-artist-detail");
        let app = fixture.app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/artists/missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_text(response.into_body()).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("unknown artist: missing"));
    }

    #[tokio::test]
    async fn artist_artwork_is_404_until_acquired() {
        let fixture = TestFixture::new("artist-artwork");
        let library = fixture.library();
        let artist = library
            .artists
            .iter()
            .find(|artist| artist.name == "Darkwood Dub")
            .expect("artist")
            .clone();
        let app = fixture.app_with_library(library).await;

        // The list response carries an artwork_url field, null when none is acquired.
        let list: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .uri("/api/artists")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        assert!(list["items"][0].as_object().unwrap().contains_key("artwork_url"));

        // No image acquired yet → the artwork endpoint 404s (UI shows the monogram).
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/artists/{}/artwork", artist.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn merge_and_unmerge_artists() {
        let fixture = TestFixture::new("merge-artists");
        // The default fixture has "Darkwood Dub"; add a second artist to merge.
        fixture.write("2001 - Other/01 - Other Band - A Song.mp3");
        let library = fixture.library();
        let darkwood = library
            .artists
            .iter()
            .find(|a| a.name == "Darkwood Dub")
            .expect("darkwood")
            .id
            .clone();
        let other = library
            .artists
            .iter()
            .find(|a| a.name == "Other Band")
            .expect("other")
            .id
            .clone();
        let app = fixture.app_with_library(library).await;

        // Empty / self-merge are rejected.
        let bad = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/artists/merge")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"canonical_id":"{darkwood}","member_ids":["{darkwood}"]}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

        // Merge "Other Band" into "Darkwood Dub".
        let merged: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/artists/merge")
                            .header(CONTENT_TYPE, "application/json")
                            .body(Body::from(format!(
                                r#"{{"canonical_id":"{darkwood}","member_ids":["{other}"]}}"#
                            )))
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        let groups = merged.as_array().expect("alias groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["canonical_name"], "Darkwood Dub");
        assert_eq!(groups[0]["members"].as_array().unwrap().len(), 1);
        let alias_key = groups[0]["members"][0].as_str().unwrap().to_string();
        let alias_key_enc = alias_key.replace(' ', "%20"); // the UI uses encodeURIComponent

        // Unmerge removes the alias.
        let after: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .method("DELETE")
                            .uri(format!("/api/artists/aliases/{alias_key_enc}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        assert!(after.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn paginates_and_sorts_album_listing() {
        let fixture = TestFixture::new("albums-page");
        let app = fixture.app().await;

        // Full listing is wrapped in a pagination envelope.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/albums")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).expect("albums json");
        assert_eq!(value["total"], 2);
        assert_eq!(value["offset"], 0);
        assert_eq!(value["items"].as_array().expect("items").len(), 2);

        // A single-item page at offset 1 returns one item but the full total.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/albums?limit=1&offset=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).expect("albums page json");
        assert_eq!(value["total"], 2);
        assert_eq!(value["limit"], 1);
        assert_eq!(value["offset"], 1);
        assert_eq!(value["items"].as_array().expect("items").len(), 1);

        // Sorting by title orders Paramparcad before U Nedogled.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/albums?sort=title")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).expect("albums sort json");
        assert_eq!(value["items"][0]["title"], "Paramparcad");
        assert_eq!(value["items"][1]["title"], "U Nedogled");
    }

    #[tokio::test]
    async fn serves_recently_added_tracks() {
        let fixture = TestFixture::new("recently-added");
        let app = fixture.app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/browse/recently-added")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let value: serde_json::Value = serde_json::from_str(&body_text(response.into_body()).await)
            .expect("recently added json");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["total"], 3);
        let items = value["items"].as_array().expect("items");
        assert_eq!(items.len(), 3);
        // Saving the library assigns an "added at" timestamp to every track.
        assert!(
            items
                .iter()
                .all(|track| track["added_at_unix_seconds"].is_number())
        );
    }

    #[tokio::test]
    async fn recently_added_orders_newest_first() {
        let fixture = TestFixture::new("recently-added-order");
        let mut library = fixture.library();
        // Assign distinct timestamps; saving preserves already-set values, so the
        // last track is the newest.
        for (index, track) in library.tracks.iter_mut().enumerate() {
            track.added_at_unix_seconds = Some(1_000 + index as i64);
        }
        let newest_title = library.tracks.last().expect("track").title.clone();
        let app = fixture.app_with_library(library).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/browse/recently-added")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body_text(response.into_body()).await)
            .expect("recently added json");

        let items = value["items"].as_array().expect("items");
        assert_eq!(items[0]["title"], newest_title);
        let timestamps: Vec<i64> = items
            .iter()
            .map(|track| track["added_at_unix_seconds"].as_i64().expect("timestamp"))
            .collect();
        assert!(
            timestamps.windows(2).all(|pair| pair[0] >= pair[1]),
            "expected newest-first ordering, got {timestamps:?}"
        );
    }

    #[tokio::test]
    async fn registers_renames_zones_and_removes_players() {
        let fixture = TestFixture::new("players-api");
        let app = fixture.app().await;

        let json_post = |uri: &str, body: &str| {
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };

        // Register a player (offline; the bogus address is never reached).
        let response = app
            .clone()
            .oneshot(json_post(
                "/api/players",
                r#"{"address":"127.0.0.1:6699","name":"Den"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let player: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        let id = player["id"].as_str().expect("id").to_string();
        assert_eq!(player["name"], "Den");
        assert_eq!(player["online"], false);

        // Create a zone and assign the player to it.
        let response = app
            .clone()
            .oneshot(json_post("/api/zones", r#"{"name":"Main Floor"}"#))
            .await
            .unwrap();
        let zone: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        let zone_id = zone["id"].as_str().expect("zone id").to_string();

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/players/{id}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"name":"Den Speaker","zone_id":"{zone_id}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // The listing reflects the rename and zone assignment.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/players")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let players: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        assert_eq!(players[0]["name"], "Den Speaker");
        assert_eq!(players[0]["zone_id"], zone_id);

        // Remove the player.
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/players/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn browser_player_is_present_and_plays_tracks() {
        let fixture = TestFixture::new("browser-player");
        let app = fixture.app().await;

        // The local browser player always exists.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/players")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let players: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        let browser = players
            .as_array()
            .expect("players")
            .iter()
            .find(|player| player["kind"] == "browser")
            .expect("browser player");
        let browser_id = browser["id"].as_str().expect("id").to_string();
        assert_eq!(browser["online"], true);

        // Grab a real track id from the library.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/tracks?limit=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let tracks: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        let track_id = tracks["items"][0]["id"]
            .as_str()
            .expect("track id")
            .to_string();

        // Play it on the browser player; server-owned state reflects the queue.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/players/{browser_id}/commands"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"command":"play_tracks","track_ids":["{track_id}"]}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let playback: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        assert_eq!(playback["status"], "playing");
        assert_eq!(playback["queue"].as_array().expect("queue").len(), 1);
        assert_eq!(playback["now_playing"]["track_id"], track_id);

        // Pause is reflected in state.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/players/{browser_id}/commands"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"command":"pause"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let playback: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        assert_eq!(playback["status"], "paused");

        // Volume is settable per player and reflected in state.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/players/{browser_id}/commands"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"command":"set_volume","volume":37}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let playback: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        assert_eq!(playback["volume"], 37);
    }

    #[tokio::test]
    async fn offline_mpd_command_queues_track_and_reports_offline() {
        let fixture = TestFixture::new("offline-mpd-command");
        let app = fixture.app().await;

        // Register an MPD player at an address with nothing listening (refused fast).
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/players")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"address":"127.0.0.1:6699","name":"Den"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let player: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        let id = player["id"].as_str().expect("id").to_string();
        assert_eq!(player["online"], false);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/tracks?limit=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let tracks: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        let track_id = tracks["items"][0]["id"]
            .as_str()
            .expect("track id")
            .to_string();

        // Playing on the offline player reports offline (503) rather than hanging or a
        // bare 500 — the controller can show a clear message.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/players/{id}/commands"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"command":"play_tracks","track_ids":["{track_id}"]}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let error: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        assert_eq!(error["error"]["code"], "player_offline");

        // But the server queue is authoritative: the track is queued and survives, so it
        // plays once MPD reconnects.
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/players/{id}/state"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let playback: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        assert_eq!(playback["queue"].as_array().expect("queue").len(), 1);
        assert_eq!(playback["now_playing"]["track_id"], track_id);
    }

    #[tokio::test]
    async fn zone_owns_a_queue_and_serves_state() {
        let fixture = TestFixture::new("zone-queue-api");
        let app = fixture.app().await;

        let json_post = |uri: String, body: String| {
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap()
        };

        // Create a zone and assign an (offline) MPD member to it.
        let response = app
            .clone()
            .oneshot(json_post(
                "/api/zones".to_string(),
                r#"{"name":"Living Room"}"#.to_string(),
            ))
            .await
            .unwrap();
        let zone: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        let zone_id = zone["id"].as_str().expect("zone id").to_string();

        let response = app
            .clone()
            .oneshot(json_post(
                "/api/players".to_string(),
                r#"{"address":"127.0.0.1:6699","name":"Den"}"#.to_string(),
            ))
            .await
            .unwrap();
        let player: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        let player_id = player["id"].as_str().expect("id").to_string();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/players/{player_id}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"zone_id":"{zone_id}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Grab three real track ids from the library.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/tracks?limit=3")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let tracks: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        let ids: Vec<String> = tracks["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|track| track["id"].as_str().expect("track id").to_string())
            .collect();
        assert!(ids.len() >= 2, "fixture library has at least two tracks");
        let id_list = ids
            .iter()
            .map(|id| format!("\"{id}\""))
            .collect::<Vec<_>>()
            .join(",");

        // Command the zone (not a member): the zone owns the resulting queue.
        let response = app
            .clone()
            .oneshot(json_post(
                format!("/api/zones/{zone_id}/commands"),
                format!(r#"{{"command":"play_tracks","track_ids":[{id_list}],"start_index":1}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let playback: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        assert_eq!(playback["status"], "playing");
        assert_eq!(
            playback["queue"].as_array().expect("queue").len(),
            ids.len()
        );
        assert_eq!(playback["queue_position"], 1);
        assert_eq!(playback["now_playing"]["track_id"], ids[1]);

        // The dedicated state endpoint reports the same canonical queue.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/zones/{zone_id}/state"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let state: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        assert_eq!(state["queue_position"], 1);

        // A reorder runs against the zone's queue.
        let response = app
            .clone()
            .oneshot(json_post(
                format!("/api/zones/{zone_id}/commands"),
                r#"{"command":"move_queue_item","from":0,"to":1}"#.to_string(),
            ))
            .await
            .unwrap();
        let playback: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).unwrap();
        // The previously-playing track (index 1) is now at index 0; still now-playing.
        assert_eq!(playback["queue_position"], 0);
        assert_eq!(playback["now_playing"]["track_id"], ids[1]);

        // Deleting the zone tears down its runtime; state then 404/500s, not panics.
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/zones/{zone_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn detects_embedded_vs_folder_artwork_paths() {
        use super::is_embedded_artwork;
        use std::path::Path;
        assert!(is_embedded_artwork(Path::new("Album/track.flac")));
        assert!(is_embedded_artwork(Path::new("Album/track.MP3")));
        assert!(!is_embedded_artwork(Path::new("Album/cover.jpg")));
        assert!(!is_embedded_artwork(Path::new("Album/folder.png")));
    }

    #[test]
    fn sniffs_image_content_type_from_magic_bytes() {
        use super::sniff_image_content_type;
        assert_eq!(
            sniff_image_content_type(&[0xff, 0xd8, 0xff, 0xe0]),
            "image/jpeg"
        );
        assert_eq!(
            sniff_image_content_type(&[0x89, b'P', b'N', b'G']),
            "image/png"
        );
        assert_eq!(sniff_image_content_type(b"GIF89a"), "image/gif");
        assert_eq!(
            sniff_image_content_type(b"RIFF\0\0\0\0WEBPVP8 "),
            "image/webp"
        );
        // Unknown bytes default to JPEG (the common embedded-cover case).
        assert_eq!(sniff_image_content_type(b"\0\0\0\0"), "image/jpeg");
    }

    #[tokio::test]
    async fn serves_health_json() {
        let fixture = TestFixture::new("health");
        let app = fixture.app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_text(response.into_body()).await;

        assert!(body.contains(r#""status":"ok""#));
        assert!(body.contains(r#""provider":"local-disk""#));
    }

    #[tokio::test]
    async fn serves_search_results_json() {
        let fixture = TestFixture::new("search");
        let app = fixture.app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/search?q=vavilon")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_text(response.into_body()).await;

        assert!(body.contains("Vavilon"));
    }

    #[tokio::test]
    async fn serves_browse_facets_and_filtered_tracks() {
        let fixture = TestFixture::new("browse");
        let mut library = fixture.library();
        let track = library
            .tracks
            .iter_mut()
            .find(|track| track.title.contains("Brzi"))
            .expect("browse track");
        track.observed_metadata[0].genres.push("Dub".to_string());
        track.observed_metadata[0]
            .composers
            .push("Composer One".to_string());
        let app = fixture.app_with_library(library).await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/browse")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_text(response.into_body()).await;

        assert!(body.contains(r#""value":"Dub""#));
        assert!(body.contains(r#""value":"Composer One""#));
        assert!(body.contains(r#""value":1994"#));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/tracks?genre=Dub&composer=Composer%20One&year=1994")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_text(response.into_body()).await;

        assert!(body.contains("Brzi Vavilon"));
        assert!(!body.contains("Spori Vavilon"));
    }

    #[tokio::test]
    async fn metadata_write_back_policy_is_disabled() {
        let fixture = TestFixture::new("write-back");
        let app = fixture.app().await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/metadata/write-back")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_text(response.into_body()).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""enabled":false"#));
        assert!(body.contains(r#""requires_preview":true"#));
        assert!(body.contains("provider must declare write support"));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/metadata/write-back")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_text(response.into_body()).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains(r#""code":"write_back_disabled""#));
        assert!(body.contains("currently updates only its database"));
    }

    #[tokio::test]
    async fn metadata_review_api_updates_field_approval() {
        let fixture = TestFixture::new("metadata-review");
        let library = fixture.library();
        let track = library.tracks.first().expect("track").clone();
        let field = track
            .observed_metadata
            .iter()
            .flat_map(|observation| observation.field_observations.iter())
            .find(|field| field.source == "folder_path" && field.field_name == "title")
            .expect("title field")
            .clone();
        let app = fixture.app_with_library(library).await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/tracks/{}/metadata/review", track.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_text(response.into_body()).await;

        assert!(body.contains(r#""canonical""#));
        assert!(body.contains(r#""field_name":"title""#));

        let update = serde_json::json!({
            "source": field.source,
            "observed_at_unix_seconds": field.observed_at_unix_seconds,
            "field_name": field.field_name,
            "value": field.value,
            "approval_state": "approved"
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/tracks/{}/metadata/review/fields", track.id))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(update.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_text(response.into_body()).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""approval_state":"approved""#));

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/tracks/{}/metadata/review", track.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_text(response.into_body()).await;

        assert!(body.contains(r#""approval_state":"approved""#));
    }

    #[tokio::test]
    async fn musicbrainz_lookup_returns_empty_result_without_existing_mbids() {
        let fixture = TestFixture::new("musicbrainz-empty");
        let library = fixture.library();
        let track_id = library.tracks.first().expect("track").id.clone();
        let app = fixture.app_with_library(library).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/tracks/{track_id}/metadata/musicbrainz"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_text(response.into_body()).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""targets":[]"#));
        assert!(body.contains(r#""recordings":[]"#));
        assert!(body.contains(r#""issues":[]"#));
    }

    #[tokio::test]
    async fn musicbrainz_candidate_routes_skip_items_with_existing_mbids() {
        let fixture = TestFixture::new("musicbrainz-candidates");
        let mut library = fixture.library();
        let track = library.tracks.first_mut().expect("track");
        track.observed_metadata[0].musicbrainz_recording_id =
            Some("e3e2ace1-1312-4f76-94b8-e6c7d969b730".to_string());
        track.observed_metadata[0].musicbrainz_release_id =
            Some("d08ef3f3-7c5d-4a1f-a28d-d81bead9e165".to_string());
        let track_id = track.id.clone();
        let album_id = track.album_id.clone();
        let app = fixture.app_with_library(library).await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/tracks/{track_id}/metadata/musicbrainz/candidates"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_text(response.into_body()).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""searched":false"#));
        assert!(body.contains("track already has MusicBrainz identifiers"));

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/albums/{album_id}/metadata/musicbrainz/candidates"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_text(response.into_body()).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""searched":false"#));
        assert!(body.contains("album already has MusicBrainz release identifiers"));
    }

    #[tokio::test]
    async fn serves_track_stream_ranges() {
        let fixture = TestFixture::new("stream");
        let library = fixture.library();
        let track_id = library.tracks.first().expect("track").id.clone();
        let app = fixture.app_with_library(library).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/tracks/{track_id}/stream"))
                    .header(RANGE, "bytes=0-31")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();

        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body.len(), 32);
    }

    #[tokio::test]
    async fn playback_sessions_scope_browser_streams() {
        let fixture = TestFixture::new("playback-session");
        let library = fixture.library();
        let track_id = library.tracks.first().expect("track").id.clone();
        let app = fixture.app_with_library(library).await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/playback/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_text(response.into_body()).await;
        let session: serde_json::Value = serde_json::from_str(&body).expect("session json");
        let session_id = session["id"].as_str().expect("session id");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/tracks/{track_id}/stream?playback_session={session_id}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/tracks/{track_id}/stream?playback_session=missing"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serves_album_artwork() {
        let fixture = TestFixture::new("artwork");
        let library = fixture.library();
        let album_id = library
            .albums
            .iter()
            .find(|album| album.artwork_url.is_some())
            .expect("album artwork")
            .id
            .clone();
        let app = fixture.app_with_library(library).await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/albums/{album_id}/artwork"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let cache_control = response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "image/jpeg");
        assert_eq!(cache_control, ARTWORK_CACHE_CONTROL);
        assert!(!etag.is_empty());
        assert!(!body.is_empty());

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/albums/{album_id}/artwork"))
                    .header(IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    }

    #[test]
    fn normalize_size_snaps_up_and_caps() {
        assert_eq!(normalize_size(None), None);
        assert_eq!(normalize_size(Some(50)), Some(128));
        assert_eq!(normalize_size(Some(128)), Some(128));
        assert_eq!(normalize_size(Some(200)), Some(300));
        assert_eq!(normalize_size(Some(600)), Some(600));
        // Beyond the ladder → serve the original.
        assert_eq!(normalize_size(Some(601)), None);
        assert_eq!(normalize_size(Some(5000)), None);
    }

    #[test]
    fn artwork_etag_is_size_aware() {
        assert_eq!(artwork_etag("abc", None), "\"abc\"");
        assert_eq!(artwork_etag("abc", Some(300)), "\"abc-300\"");
    }

    #[test]
    fn resize_to_jpeg_downscales_reencodes_and_tolerates_garbage() {
        let source = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            1000,
            800,
            image::Rgb([10, 20, 30]),
        ));
        let mut png = Vec::new();
        source
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let out = resize_to_jpeg(&png, 300).expect("resize");
        assert_eq!(&out[..2], &[0xFF, 0xD8], "JPEG SOI marker");
        let decoded = image::load_from_memory(&out).expect("decode jpeg");
        // 1000×800 fit into a 300 box → 300×240, preserving aspect.
        assert_eq!((decoded.width(), decoded.height()), (300, 240));

        // Undecodable bytes → None (the caller serves the original instead of 500-ing).
        assert!(resize_to_jpeg(b"not an image", 300).is_none());
    }

    #[tokio::test]
    async fn serves_resized_album_artwork_variant() {
        let fixture = TestFixture::new("artwork-size");
        // Replace the placeholder cover with a real (decodable) JPEG so the resize path
        // runs end to end rather than falling back to the original.
        let mut cover = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            1000,
            1000,
            image::Rgb([90, 120, 150]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut cover), image::ImageFormat::Jpeg)
        .unwrap();
        fs::write(fixture.root.join("1994 - Paramparcad/cover.jpg"), &cover).unwrap();

        let library = fixture.library();
        let album_id = library
            .albums
            .iter()
            .find(|album| album.artwork_url.is_some())
            .expect("album artwork")
            .id
            .clone();
        let app = fixture.app_with_library(library).await;

        let request = |etag: Option<&str>| {
            let mut builder = Request::builder()
                .uri(format!("/api/albums/{album_id}/artwork?size=300"));
            if let Some(etag) = etag {
                builder = builder.header(IF_NONE_MATCH, etag);
            }
            builder.body(Body::empty()).unwrap()
        };

        let response = app.clone().oneshot(request(None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert_eq!(content_type, "image/jpeg");
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(etag.contains("-300"), "size-aware etag, got {etag}");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let decoded = image::load_from_memory(&body).expect("variant decodes");
        assert!(decoded.width() <= 300 && decoded.height() <= 300);

        // Second request hits the cached variant; with the etag it 304s.
        let response = app.oneshot(request(Some(&etag))).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    }

    // A stored cover path that can't be read (stale path, offline/again-unreadable
    // source) must degrade to 404 so the UI shows its monogram fallback — never a 500
    // (which floods the log and surfaces as a broken request to the client).
    #[tokio::test]
    async fn missing_album_artwork_file_returns_404_not_500() {
        let fixture = TestFixture::new("artwork-missing");
        let mut library = fixture.library();
        let album_id = library
            .albums
            .iter()
            .find(|album| album.artwork_url.is_some())
            .expect("album artwork")
            .id
            .clone();
        // Point the album's artwork at a file that does not exist.
        for album in &mut library.albums {
            if album.id == album_id {
                album.artwork_path = Some(PathBuf::from("/no/such/cover.jpg"));
            }
        }
        let app = fixture.app_with_library(library).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/albums/{album_id}/artwork"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn reviews_and_selects_album_artwork() {
        let fixture = TestFixture::new("artwork-selection");
        let library = fixture.library();
        let album_id = library
            .albums
            .iter()
            .find(|album| album.artwork_url.is_some())
            .expect("album artwork")
            .id
            .clone();
        let app = fixture.app_with_library(library).await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/albums/{album_id}/artwork/review"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let review: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let candidates = review["candidates"].as_array().expect("candidates");
        assert!(candidates.len() >= 2);
        let front = candidates
            .iter()
            .find(|candidate| candidate["file_name"] == "front.png")
            .expect("front candidate");
        let front_id = front["id"].as_str().expect("front id").to_string();

        let update = serde_json::json!({ "artwork_id": front_id });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/albums/{album_id}/artwork"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(update.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let review: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            review["selected_artwork_id"].as_str().unwrap_or_default(),
            front_id
        );
        assert!(
            review["selected_artwork_url"]
                .as_str()
                .unwrap_or_default()
                .contains(&front_id)
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/albums/{album_id}/artwork"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(content_type, "image/png");
    }

    #[tokio::test]
    async fn cover_art_archive_candidates_require_musicbrainz_ids() {
        let fixture = TestFixture::new("cover-art-archive");
        let library = fixture.library();
        let album_id = library.albums.first().expect("album").id.clone();
        let app = fixture.app_with_library(library).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/albums/{album_id}/artwork/cover-art-archive/candidates"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_text(response.into_body()).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""searched":false"#));
        assert!(body.contains("no MusicBrainz release or release-group identifiers"));
    }

    #[tokio::test]
    async fn serves_stable_error_envelopes() {
        let fixture = TestFixture::new("missing");
        let app = fixture.app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = body_text(response.into_body()).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains(r#""code":"not_found""#));
        assert!(body.contains(r#""message":"route not found""#));
    }

    #[tokio::test]
    async fn rescans_library_and_updates_state() {
        let fixture = TestFixture::new("rescan");
        let app = fixture.app().await;
        fixture.write("1998 - Elektro Pionir/Darkwood Dub - Treci Talas.mp3");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/library/rescan")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_text(response.into_body()).await;

        assert!(body.contains(r#""changed":true"#));
        assert!(body.contains(r#""added":1"#));
        assert!(body.contains(r#""track_count":4"#));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/library/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_text(response.into_body()).await;

        assert!(body.contains(r#""track_count":4"#));
    }

    async fn body_text(body: Body) -> String {
        String::from_utf8(to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap()
    }

    // ---- OpenSubsonic API ----
    // The fixture configures Subsonic credentials u=u / p=p (see `app_with_library`).
    const SS_AUTH: &str = "u=u&p=p&v=1.16.1&c=test&f=json";

    async fn subsonic_json(app: &axum::Router, query: &str) -> serde_json::Value {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/rest/{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        serde_json::from_str(&body_text(response.into_body()).await).expect("subsonic json")
    }

    #[tokio::test]
    async fn subsonic_ping_authenticates() {
        let fixture = TestFixture::new("ss-ping");
        let app = fixture.app().await;

        let ok = subsonic_json(&app, "ping.view?u=u&p=p&f=json").await;
        assert_eq!(ok["subsonic-response"]["status"], "ok");
        assert_eq!(ok["subsonic-response"]["openSubsonic"], true);

        let bad = subsonic_json(&app, "ping.view?u=u&p=wrong&f=json").await;
        assert_eq!(bad["subsonic-response"]["status"], "failed");
        assert_eq!(bad["subsonic-response"]["error"]["code"], 40);
    }

    #[tokio::test]
    async fn subsonic_advertises_formpost_extension() {
        let fixture = TestFixture::new("ss-ext");
        let app = fixture.app().await;
        let value = subsonic_json(&app, &format!("getOpenSubsonicExtensions.view?{SS_AUTH}")).await;
        let names: Vec<&str> = value["subsonic-response"]["openSubsonicExtensions"]
            .as_array()
            .expect("extensions")
            .iter()
            .filter_map(|extension| extension["name"].as_str())
            .collect();
        assert!(names.contains(&"formPost"), "got {names:?}");
        assert!(names.contains(&"songLyrics"), "got {names:?}");
    }

    #[tokio::test]
    async fn subsonic_get_random_songs_and_music_directory() {
        let fixture = TestFixture::new("ss-random-dir");
        let app = fixture.app().await;

        let random = subsonic_json(&app, &format!("getRandomSongs.view?{SS_AUTH}&size=5")).await;
        assert!(
            !random["subsonic-response"]["randomSongs"]["song"]
                .as_array()
                .expect("randomSongs")
                .is_empty()
        );

        // An artist directory lists its albums as sub-directories.
        let artists = subsonic_json(&app, &format!("getArtists.view?{SS_AUTH}")).await;
        let artist_id = artists["subsonic-response"]["artists"]["index"][0]["artist"][0]["id"]
            .as_str()
            .expect("artist id")
            .to_string();
        let dir = subsonic_json(
            &app,
            &format!("getMusicDirectory.view?{SS_AUTH}&id={artist_id}"),
        )
        .await;
        let children = dir["subsonic-response"]["directory"]["child"]
            .as_array()
            .expect("children");
        assert!(!children.is_empty());
        assert_eq!(children[0]["isDir"], true);
    }

    #[tokio::test]
    async fn subsonic_get_lyrics_by_song_id_returns_list() {
        let fixture = TestFixture::new("ss-lyrics");
        let app = fixture.app().await;
        let search = subsonic_json(&app, &format!("search3.view?{SS_AUTH}&query=vavilon")).await;
        let song_id = search["subsonic-response"]["searchResult3"]["song"][0]["id"]
            .as_str()
            .expect("song id")
            .to_string();
        let value = subsonic_json(
            &app,
            &format!("getLyricsBySongId.view?{SS_AUTH}&id={song_id}"),
        )
        .await;
        // Whether or not the track has lyrics, the response is a well-formed lyricsList.
        assert_eq!(value["subsonic-response"]["status"], "ok");
        assert!(value["subsonic-response"]["lyricsList"].is_object());
    }

    #[tokio::test]
    async fn native_playlists_and_favorites_round_trip() {
        let fixture = TestFixture::new("native-pl-fav");
        let app = fixture.app().await;

        // A track id from the library.
        let page: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .uri("/api/tracks?limit=1")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        let track_id = page["items"][0]["id"]
            .as_str()
            .expect("track id")
            .to_string();

        // Create a playlist containing that track.
        let created: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/playlists")
                            .header(CONTENT_TYPE, "application/json")
                            .body(Body::from(format!(
                                r#"{{"name":"Mix","track_ids":["{track_id}"]}}"#
                            )))
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(created["name"], "Mix");
        assert_eq!(created["tracks"].as_array().unwrap().len(), 1);

        // It shows up in the list.
        let list: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .uri("/api/playlists")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);

        // Star the track, then it appears in favorites.
        let star = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/favorites/track/{track_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(star.status(), StatusCode::NO_CONTENT);

        let favorites: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .uri("/api/favorites")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(favorites["tracks"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn smart_playlists_list_and_detail() {
        let fixture = TestFixture::new("smart-playlists");
        let app = fixture.app().await;

        // The catalog lists the fixed set of computed playlists.
        let catalog: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .uri("/api/smart-playlists")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        let ids: Vec<&str> = catalog
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"never-played"));
        assert!(ids.contains(&"top-30-days"));

        // Nothing has been played, so every library track is "never played".
        let detail: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .uri("/api/smart-playlists/never-played")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(detail["id"], "never-played");
        assert!(!detail["tracks"].as_array().unwrap().is_empty());

        // An unknown id is a 404.
        let missing = app
            .oneshot(
                Request::builder()
                    .uri("/api/smart-playlists/does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn activity_endpoint_returns_array() {
        let fixture = TestFixture::new("activity");
        let app = fixture.app().await;
        let body = body_text(
            app.oneshot(
                Request::builder()
                    .uri("/api/activity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .into_body(),
        )
        .await;
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(value.is_array(), "activity must be a JSON array");
    }

    #[tokio::test]
    async fn delete_source_with_slashy_id_routes() {
        let fixture = TestFixture::new("del-probe");
        let app = fixture.app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/sources/smb%3Anas.local%2Fmusic")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Provider ids contain ':' and '/' (e.g. smb:host/share); the route must
        // still match the percent-encoded id and reach the handler.
        assert_ne!(resp.status(), StatusCode::NOT_FOUND, "route did not match");
        assert_ne!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn lists_sources_and_rejects_unknown_kind() {
        let fixture = TestFixture::new("sources");
        let app = fixture.app().await;

        // The local library is always listed, with full disk capabilities.
        let sources: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .uri("/api/sources")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        let list = sources.as_array().expect("sources array");
        // Two built-in sources: the scannable local library and internet radio.
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["id"], "local-disk");
        assert_eq!(list[0]["kind"], "local");
        assert_eq!(list[0]["capabilities"]["can_scan"], true);
        assert_eq!(list[0]["capabilities"]["can_stream"], true);
        // Radio is a built-in non-scannable, browsable, streamable source.
        assert_eq!(list[1]["id"], "radio");
        assert_eq!(list[1]["kind"], "radio");
        assert_eq!(list[1]["capabilities"]["can_scan"], false);
        assert_eq!(list[1]["capabilities"]["can_browse"], true);
        assert_eq!(list[1]["capabilities"]["can_stream"], true);

        // The built-in radio source can't be deleted (stations are removed individually).
        let undeletable = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/sources/radio")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(undeletable.status(), StatusCode::BAD_REQUEST);

        // An unknown source kind is rejected.
        let bad = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sources")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"kind":"ftp","host":"h","share":"s"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

        // An SMB source requires host/share; missing host is a 400. (Whether the
        // SMB provider can actually be built depends on the `provider-smb` feature;
        // either way an empty/invalid request must not 500.)
        let missing = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sources")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"kind":"smb"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn radio_native_and_subsonic() {
        let fixture = TestFixture::new("radio");
        let app = fixture.app().await;

        // Create via the native API.
        let created: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/radio")
                            .header(CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                r#"{"name":"Groove Salad","stream_url":"http://ice.somafm.com/groovesalad"}"#,
                            ))
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(created["name"], "Groove Salad");

        // Visible to a Subsonic client via getInternetRadioStations.
        let stations =
            subsonic_json(&app, &format!("getInternetRadioStations.view?{SS_AUTH}")).await;
        let list = stations["subsonic-response"]["internetRadioStations"]["internetRadioStation"]
            .as_array()
            .expect("stations");
        assert!(list.iter().any(|station| station["name"] == "Groove Salad"));
        assert!(
            list.iter()
                .any(|station| station["streamUrl"] == "http://ice.somafm.com/groovesalad")
        );

        // The station is also reachable through the generic provider browse endpoint,
        // which carries the playable stream URL inline.
        let station_id = created["id"].as_str().expect("station id").to_string();
        let browsed: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .uri("/api/sources/radio/browse")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        let entries = browsed["entries"].as_array().expect("browse entries");
        let entry = entries
            .iter()
            .find(|entry| entry["id"] == station_id)
            .expect("browsed station");
        assert_eq!(entry["title"], "Groove Salad");
        assert_eq!(entry["stream_url"], "http://ice.somafm.com/groovesalad");
        assert_eq!(entry["is_container"], false);

        // resolve turns the station id into a playable StreamSpec.
        let resolved: serde_json::Value = serde_json::from_str(
            &body_text(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .uri(&format!("/api/sources/radio/resolve?item={station_id}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(resolved["url"], "http://ice.somafm.com/groovesalad");
        assert_eq!(resolved["title"], "Groove Salad");

        // A scannable source isn't browsable through this endpoint (it merges into
        // the library); resolving an unknown station is a 400.
        let local_browse = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sources/local-disk/browse")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(local_browse.status(), StatusCode::BAD_REQUEST);

        let missing = app
            .oneshot(
                Request::builder()
                    .uri("/api/sources/radio/resolve?item=does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn subsonic_playlists_and_stars() {
        let fixture = TestFixture::new("ss-pl-stars");
        let app = fixture.app().await;
        let search = subsonic_json(&app, &format!("search3.view?{SS_AUTH}&query=vavilon")).await;
        let song_id = search["subsonic-response"]["searchResult3"]["song"][0]["id"]
            .as_str()
            .expect("song id")
            .to_string();

        // createPlaylist returns the new playlist with its entries.
        let created = subsonic_json(
            &app,
            &format!("createPlaylist.view?{SS_AUTH}&name=Mix&songId={song_id}"),
        )
        .await;
        assert_eq!(
            created["subsonic-response"]["playlist"]["entry"]
                .as_array()
                .expect("entries")
                .len(),
            1
        );

        let lists = subsonic_json(&app, &format!("getPlaylists.view?{SS_AUTH}")).await;
        assert!(
            !lists["subsonic-response"]["playlists"]["playlist"]
                .as_array()
                .expect("playlists")
                .is_empty()
        );

        // Star the song, then getStarred2 lists it.
        let starred = subsonic_json(&app, &format!("star.view?{SS_AUTH}&id={song_id}")).await;
        assert_eq!(starred["subsonic-response"]["status"], "ok");
        let starred2 = subsonic_json(&app, &format!("getStarred2.view?{SS_AUTH}")).await;
        let songs = starred2["subsonic-response"]["starred2"]["song"]
            .as_array()
            .expect("starred songs");
        assert!(songs.iter().any(|song| song["id"] == song_id));
    }

    #[tokio::test]
    async fn subsonic_reads_params_from_post_form_body() {
        // Real clients (e.g. py-sonic) POST parameters as a form body rather than in
        // the query string; the API must read both.
        let fixture = TestFixture::new("ss-post");
        let app = fixture.app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/rest/ping.view")
                    .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("u=u&p=p&f=json&v=1.16.1&c=test"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&body_text(response.into_body()).await).expect("json");
        assert_eq!(value["subsonic-response"]["status"], "ok");
    }

    #[tokio::test]
    async fn subsonic_ping_renders_xml_by_default() {
        let fixture = TestFixture::new("ss-xml");
        let app = fixture.app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/rest/ping.view?u=u&p=p")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = body_text(response.into_body()).await;
        assert!(content_type.contains("xml"), "got {content_type}");
        assert!(body.contains("<subsonic-response"));
        assert!(body.contains("status=\"ok\""));
    }

    #[tokio::test]
    async fn subsonic_getartists_indexes_artists() {
        let fixture = TestFixture::new("ss-artists");
        let app = fixture.app().await;
        let value = subsonic_json(&app, &format!("getArtists.view?{SS_AUTH}")).await;
        let index = value["subsonic-response"]["artists"]["index"]
            .as_array()
            .expect("index");
        assert!(!index.is_empty());
        let artist = &index[0]["artist"][0];
        assert!(artist["id"].is_string());
        assert!(artist["name"].is_string());
    }

    #[tokio::test]
    async fn subsonic_search3_finds_songs() {
        let fixture = TestFixture::new("ss-search");
        let app = fixture.app().await;
        let value = subsonic_json(&app, &format!("search3.view?{SS_AUTH}&query=vavilon")).await;
        let songs = value["subsonic-response"]["searchResult3"]["song"]
            .as_array()
            .expect("song array");
        assert!(!songs.is_empty(), "expected songs matching 'vavilon'");
    }

    #[tokio::test]
    async fn subsonic_stream_serves_audio_bytes() {
        let fixture = TestFixture::new("ss-stream");
        let app = fixture.app().await;
        let search = subsonic_json(&app, &format!("search3.view?{SS_AUTH}&query=vavilon")).await;
        let song_id = search["subsonic-response"]["searchResult3"]["song"][0]["id"]
            .as_str()
            .expect("song id")
            .to_string();

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/rest/stream.view?{SS_AUTH}&id={song_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(content_type.starts_with("audio/"), "got {content_type}");
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn subsonic_getcoverart_serves_image() {
        let fixture = TestFixture::new("ss-cover");
        let app = fixture.app().await;
        // The "1994 - Paramparcad" album folder has cover art in the fixture.
        let albums = subsonic_json(&app, &format!("getAlbumList2.view?{SS_AUTH}&size=50")).await;
        let album_id = albums["subsonic-response"]["albumList2"]["album"]
            .as_array()
            .expect("albums")
            .iter()
            .find(|album| {
                album["name"]
                    .as_str()
                    .is_some_and(|name| name.contains("Paramparcad"))
            })
            .and_then(|album| album["id"].as_str())
            .expect("paramparcad album id")
            .to_string();

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/rest/getCoverArt.view?{SS_AUTH}&id={album_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(content_type.starts_with("image/"), "got {content_type}");
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(!bytes.is_empty());
    }

    struct TestFixture {
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system time")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("musicata-server-{name}-{unique}"));
            fs::create_dir_all(&root).expect("create fixture root");
            let fixture = Self { root };
            fixture.write("1994 - Paramparcad/Darkwood Dub - Brzi Vavilon.mp3");
            fixture.write("1994 - Paramparcad/Darkwood Dub - Spori Vavilon.mp3");
            fixture.write("1994 - Paramparcad/cover.jpg");
            fixture.write("1994 - Paramparcad/front.png");
            fixture.write("1996 - U Nedogled/Darkwood Dub - U Nedogled.mp3");
            fixture
        }

        fn library(&self) -> Library {
            scan_local_library(&self.root).expect("scan fixture")
        }

        async fn app(&self) -> axum::Router {
            let library = self.library();
            self.app_with_library(library).await
        }

        async fn app_with_library(&self, mut library: Library) -> axum::Router {
            let database = Database::connect(self.root.join("musicata.db"))
                .await
                .expect("connect fixture database");
            database
                .save_library(&mut library)
                .await
                .expect("save fixture library");
            let players = PlayerManager::load(database.clone(), "http://127.0.0.1".to_string())
                .await
                .expect("player manager");
            let mut registry = ProviderRegistry::new();
            registry.push(ProviderHandle::local(LocalDiskProvider::new(&self.root)));
            registry.push(ProviderHandle::radio(crate::providers::RadioProvider::new(
                database.clone(),
            )));
            app(
                database,
                std::sync::Arc::new(tokio::sync::RwLock::new(registry)),
                players,
                std::sync::Arc::new(crate::activity::ActivityLog::default()),
                std::sync::Arc::new(tokio::sync::Mutex::new(())),
                crate::subsonic::SubsonicAuth {
                    user: "u".to_string(),
                    password: Some("p".to_string()),
                },
                std::sync::Arc::new(crate::artwork::ArtworkCache::new(self.root.join("artwork"))),
            )
        }

        fn write(&self, relative_path: &str) {
            let path = self.root.join(relative_path);
            fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture dir");
            fs::write(path, b"fixture audio bytes used for route range tests")
                .expect("write fixture file");
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct TempConfigFile {
        root: PathBuf,
        path: PathBuf,
    }

    impl TempConfigFile {
        fn new(name: &str, contents: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system time")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("musicata-config-{name}-{unique}"));
            fs::create_dir_all(&root).expect("create config fixture root");
            let path = root.join("musicata.conf");
            fs::write(&path, contents).expect("write config fixture");
            Self { root, path }
        }
    }

    impl Drop for TempConfigFile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
