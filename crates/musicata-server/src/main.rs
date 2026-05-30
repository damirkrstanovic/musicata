mod mpd;
mod musicbrainz;
mod players;

use crate::players::{BrowserPlayer, PlayerHandle, PlayerManager};
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
    response::{Html, IntoResponse, Response},
    routing::{delete, get, patch, post},
};
use musicata_core::{
    Album, Artist, BrowseFilter, BrowseIndex, Library, LibrarySummary, LocalDiskProvider,
    MetadataApprovalState, MetadataFieldValue, MusicProvider, PlaybackState, Player, PlayerCommand,
    SearchResults, Track, TrackMetadataFieldObservation, Zone, album_artwork_url, artwork_asset_id,
    find_album_artwork_candidates,
};
use musicata_storage::Database;
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
    provider: LocalDiskProvider,
    players: Arc<PlayerManager>,
    musicbrainz: MusicBrainzClient,
    rescan_lock: Arc<Mutex<()>>,
    playback_sessions: Arc<RwLock<BTreeMap<String, PlaybackSession>>>,
    next_playback_session: Arc<AtomicU64>,
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
    /// Comma-separated MPD player addresses (`host:port`) to control.
    mpd_addrs: Vec<String>,
    /// Base URL MPD uses to fetch streams from this server, e.g.
    /// `http://127.0.0.1:3030`. Defaults to `http://<addr>`.
    public_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let config = Config::from_args()?;
    let provider = LocalDiskProvider::new(&config.library);
    let database = Database::connect(&config.database)
        .await
        .with_context(|| format!("failed to open database {}", config.database.display()))?;
    let library = load_or_scan_library(
        &database,
        &provider,
        config.rescan,
        !config.no_incremental_rescan,
    )
    .await?;

    tracing::info!(
        artists = library.artists.len(),
        albums = library.albums.len(),
        tracks = library.tracks.len(),
        root = %library.source_root,
        "library ready"
    );

    if config.scan_once {
        tracing::info!("scan complete; exiting because --scan-once was set");
        return Ok(());
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

    // Keep the library current with the filesystem on its own, so controllers never
    // need a manual "rescan" — they just re-read and see new/changed tracks. The
    // shared lock serializes this with on-demand rescans.
    let rescan_lock = Arc::new(Mutex::new(()));
    if !config.no_incremental_rescan {
        tokio::spawn(periodic_rescan(
            database.clone(),
            provider.clone(),
            rescan_lock.clone(),
            LIBRARY_RESCAN_INTERVAL,
        ));
    }

    tracing::info!("listening on http://{}", config.addr);
    axum::serve(listener, app(database, provider, players, rescan_lock))
        .await
        .context("server failed")?;

    Ok(())
}

/// How often the library is re-scanned against the filesystem in the background.
const LIBRARY_RESCAN_INTERVAL: Duration = Duration::from_secs(30);

/// Periodically re-scans the library and persists any changes. Incremental change
/// detection only stats files, so an unchanged library is cheap; nothing is written
/// when nothing changed. Errors are logged and the loop continues.
async fn periodic_rescan(
    database: Database,
    provider: LocalDiskProvider,
    rescan_lock: Arc<Mutex<()>>,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await; // The first tick fires immediately; skip it (startup already scanned).
    loop {
        ticker.tick().await;
        let _guard = rescan_lock.lock().await;
        let scan_provider = provider.clone();
        let scanned = match tokio::task::spawn_blocking(move || scan_provider.scan()).await {
            Ok(Ok(scanned)) => scanned,
            Ok(Err(error)) => {
                tracing::warn!(%error, "background rescan failed");
                continue;
            }
            Err(error) => {
                tracing::warn!(%error, "background rescan task panicked");
                continue;
            }
        };
        match database.detect_library_changes(&scanned).await {
            Ok(changes) if changes.has_changes() => {
                let mut scanned = scanned;
                if let Err(error) = database.save_library(&mut scanned).await {
                    tracing::warn!(%error, "failed to persist background rescan");
                } else {
                    tracing::info!(
                        added = changes.added,
                        removed = changes.removed,
                        modified = changes.modified,
                        "background rescan updated library"
                    );
                }
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "background change detection failed"),
        }
    }
}

async fn load_or_scan_library(
    database: &Database,
    provider: &LocalDiskProvider,
    rescan: bool,
    incremental_rescan: bool,
) -> Result<Library> {
    if !rescan
        && !incremental_rescan
        && let Some(library) = database.load_library().await?
    {
        tracing::info!(
            artists = library.artists.len(),
            albums = library.albums.len(),
            tracks = library.tracks.len(),
            "loaded library from database"
        );
        return Ok(library);
    }

    let mut scanned = provider
        .scan()
        .with_context(|| format!("failed to scan {}", provider.root().display()))?;

    if !rescan
        && incremental_rescan
        && let Some(stored) = database.load_library().await?
    {
        let changes = database.detect_library_changes(&scanned).await?;
        if changes.has_changes() {
            tracing::info!(
                added = changes.added,
                removed = changes.removed,
                modified = changes.modified,
                "library changes detected"
            );
            database.save_library(&mut scanned).await?;
            return Ok(scanned);
        }

        tracing::info!(
            artists = stored.artists.len(),
            albums = stored.albums.len(),
            tracks = stored.tracks.len(),
            "loaded unchanged library from database"
        );
        return Ok(stored);
    }

    database.save_library(&mut scanned).await?;

    Ok(scanned)
}

fn app(
    database: Database,
    provider: LocalDiskProvider,
    players: Arc<PlayerManager>,
    rescan_lock: Arc<Mutex<()>>,
) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/styles.css", get(styles_css))
        .route("/manifest.webmanifest", get(manifest))
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
        .route("/api/zones/{id}/commands", post(zone_command))
        .route("/api/artists", get(artists))
        .route("/api/artists/{id}", get(artist_detail))
        .route("/api/albums", get(albums))
        .route("/api/albums/{id}", get(album_detail))
        .route("/api/browse", get(browse))
        .route("/api/browse/recently-added", get(recently_added))
        .route("/api/history/recent", get(recently_played))
        .route("/api/history/most-played", get(most_played))
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
            provider,
            players,
            musicbrainz: MusicBrainzClient::default(),
            rescan_lock,
            playback_sessions: Arc::new(RwLock::new(BTreeMap::new())),
            next_playback_session: Arc::new(AtomicU64::new(1)),
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

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn app_js() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "application/javascript; charset=utf-8")],
        include_str!("../static/app.js"),
    )
}

async fn styles_css() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../static/styles.css"),
    )
}

async fn manifest() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "application/manifest+json; charset=utf-8")],
        include_str!("../static/manifest.webmanifest"),
    )
}

async fn service_worker() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "application/javascript; charset=utf-8")],
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
    })))
}

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
    let provider = state.provider.clone();
    let mut scanned = tokio::task::spawn_blocking(move || provider.scan())
        .await
        .map_err(|error| AppError::internal(error.to_string()))?
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
    Query(query): Query<ListQuery>,
) -> Result<Json<Page<Album>>, AppError> {
    let (items, total) = state
        .database
        .list_albums(query.sort.as_deref(), limit_arg(&query), offset_arg(&query))
        .await
        .map_err(db_error)?;
    Ok(Json(page_envelope(items, total, &query)))
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
) -> Result<StatusCode, AppError> {
    state
        .players
        .command_zone(&id, command)
        .await
        .map_err(db_error)?;
    Ok(StatusCode::NO_CONTENT)
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
        PlayerHandle::Browser(browser) => browser_ws_loop(socket, browser, database).await,
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

/// Bidirectional loop for the browser player: pushes state, and applies inbound
/// `progress`/`ended` frames from the tab that is rendering audio.
async fn browser_ws_loop(mut socket: WebSocket, browser: Arc<BrowserPlayer>, _database: Database) {
    let mut updates = browser.subscribe();

    if let Ok(text) = serde_json::to_string(&browser.snapshot().await)
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
                Some(Ok(Message::Text(text))) => handle_browser_frame(&browser, text.as_str()).await,
                Some(Ok(_)) => {}
                _ => break,
            },
        }
    }
}

async fn handle_browser_frame(browser: &BrowserPlayer, text: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    match value.get("type").and_then(|kind| kind.as_str()) {
        Some("ended") => browser.track_ended().await,
        Some("progress") => {
            if let Some(elapsed) = value
                .get("elapsed_seconds")
                .and_then(|value| value.as_f64())
            {
                let duration = value
                    .get("duration_seconds")
                    .and_then(|value| value.as_f64())
                    .filter(|duration| duration.is_finite() && *duration > 0.0);
                browser.report_progress(elapsed, duration).await;
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

async fn recently_played(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<Track>>, AppError> {
    let items = state
        .database
        .recently_played(history_limit(&query))
        .await
        .map_err(db_error)?;
    Ok(Json(items))
}

/// A track paired with how many times it has been listened to.
#[derive(Debug, Serialize)]
struct PlayCount {
    #[serde(flatten)]
    track: Track,
    play_count: i64,
}

async fn most_played(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<PlayCount>>, AppError> {
    let items = state
        .database
        .most_played(history_limit(&query))
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|(track, play_count)| PlayCount { track, play_count })
        .collect();
    Ok(Json(items))
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
}

const DEFAULT_SEARCH_LIMIT: usize = 50;

async fn search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResults>, AppError> {
    let q = query.q.unwrap_or_default();
    let limit = query.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    let results = state
        .database
        .search(&q, limit)
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
    let bytes = tokio::fs::read(&track.path).await?;
    let content_type = audio_content_type(&track.extension);

    ranged_response(
        bytes,
        headers.get(RANGE).and_then(|value| value.to_str().ok()),
        content_type,
    )
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
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let album = state
        .database
        .album(&id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::not_found(format!("unknown album: {id}")))?;
    let path = album
        .artwork_path
        .ok_or_else(|| AppError::not_found(format!("album has no artwork: {id}")))?;

    serve_artwork(path, headers).await
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

    serve_artwork(path, headers).await
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

async fn serve_artwork(path: PathBuf, headers: HeaderMap) -> Result<Response, AppError> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let _metadata = tokio::fs::metadata(&path).await?;
    let etag = format!("\"{}\"", artwork_asset_id(&path));

    if etag_matches(headers.get(IF_NONE_MATCH), &etag) {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(CACHE_CONTROL, ARTWORK_CACHE_CONTROL)
            .header(ETAG, etag)
            .body(Body::empty())
            .map_err(AppError::from);
    }

    let bytes = tokio::fs::read(&path).await?;

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, image_content_type(extension))
        .header(CONTENT_LENGTH, bytes.len().to_string())
        .header(CACHE_CONTROL, ARTWORK_CACHE_CONTROL)
        .header(ETAG, etag)
        .body(Body::from(bytes))
        .map_err(AppError::from)
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
            mpd_addrs: env("MUSICATA_MPD").map(|value| parse_addr_list(&value)),
            public_url: env("MUSICATA_PUBLIC_URL"),
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
            mpd_addrs: Vec::new(),
            public_url: None,
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
    mpd_addrs: Option<Vec<String>>,
    public_url: Option<String>,
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
                "--help" | "-h" => {
                    println!(
                        "Usage: musicata-server [--config PATH] [--library PATH] [--database PATH] [--addr HOST:PORT] [--rescan] [--no-incremental-rescan] [--scan-once] [--mpd HOST:PORT[,HOST:PORT]] [--public-url URL]\n\nConfig precedence: defaults < config file < environment < CLI\nEnvironment: MUSICATA_CONFIG, MUSICATA_LIBRARY, MUSICATA_DATABASE, MUSICATA_ADDR, MUSICATA_RESCAN, MUSICATA_INCREMENTAL_RESCAN, MUSICATA_SCAN_ONCE, MUSICATA_MPD, MUSICATA_PUBLIC_URL\nConfig file keys: library, database, addr, rescan, incremental_rescan, scan_once, mpd, public_url\nDefaults: --library testdata --database .musicata/musicata.db --addr 127.0.0.1:3030"
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
                "mpd" | "mpd_addrs" => overrides.mpd_addrs = Some(parse_addr_list(value)),
                "public_url" => overrides.public_url = Some(value.to_string()),
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

        if let Some(mpd_addrs) = self.mpd_addrs {
            config.mpd_addrs = mpd_addrs;
        }

        if let Some(public_url) = self.public_url {
            config.public_url = Some(public_url);
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
    use super::{ARTWORK_CACHE_CONTROL, Config, PlayerManager, app, parse_range};
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
            app(
                database,
                LocalDiskProvider::new(&self.root),
                players,
                std::sync::Arc::new(tokio::sync::Mutex::new(())),
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
