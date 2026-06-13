//! OpenSubsonic API (`/rest`).
//!
//! Implements the subset of the Subsonic / OpenSubsonic REST API needed for a
//! third-party client to authenticate, browse the library (artists, albums, songs),
//! search, fetch cover art, and stream. Responses are emitted as XML (the Subsonic
//! default) or JSON (`f=json`) from a single `serde_json::Value` per endpoint:
//! scalar fields become XML attributes, nested objects/arrays become child elements,
//! which matches Subsonic's document shape.
//!
//! Authentication validates against a configured username/password (plaintext `p`,
//! hex `p=enc:`, or the legacy `t`+`s` MD5 token). When no password is configured the
//! API is open (any credentials accepted) for trusted-LAN use; real user auth and a
//! network security model arrive in Milestone 12.

use std::collections::{HashMap, HashSet};

use axum::{
    Router,
    body::to_bytes,
    extract::{Path, Request, State},
    http::{HeaderMap, Method, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};
use md5::{Digest, Md5};
use musicata_core::{Album, Artist, BrowseFilter, Playlist, SearchResults, Track};
use serde_json::{Map, Value, json};

use crate::{AppState, audio_content_type, image_content_type, ranged_response};

/// The Subsonic API level we report. Clients negotiate against this.
const API_VERSION: &str = "1.16.1";
const IGNORED_ARTICLES: &str = "The El La Los Las Le Les";

/// Subsonic credentials for the `/rest` API. `password = None` means open access.
#[derive(Clone, Debug)]
pub struct SubsonicAuth {
    pub user: String,
    pub password: Option<String>,
}

pub fn routes() -> Router<AppState> {
    // Subsonic methods are addressed as `/rest/<name>` or `/rest/<name>.view`.
    Router::new().route("/rest/{method}", get(handle).post(handle))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Xml,
    Json,
}

fn format_of(params: &HashMap<String, String>) -> Format {
    match params.get("f").map(String::as_str) {
        Some("json") | Some("jsonp") => Format::Json,
        _ => Format::Xml,
    }
}

fn is_form_body(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"))
}

/// Parse `a=1&b=2` (query string or form body) into ordered key/value pairs,
/// percent-decoding both. Order and repeats are preserved (Subsonic repeats keys
/// like `songId` and `id`).
fn parse_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

/// All values for a repeated parameter, in order.
fn multi<'a>(params: &'a HashMap<String, Vec<String>>, key: &str) -> &'a [String] {
    params.get(key).map(Vec::as_slice).unwrap_or(&[])
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&input[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn handle(
    State(state): State<AppState>,
    Path(method): Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();

    // Subsonic clients pass parameters in the query string (GET) or a form-urlencoded
    // body (POST); accept both.
    let mut pairs = parse_pairs(parts.uri.query().unwrap_or(""));
    if parts.method == Method::POST && is_form_body(&parts.headers) {
        if let Ok(bytes) = to_bytes(body, 64 * 1024).await {
            pairs.extend(parse_pairs(&String::from_utf8_lossy(&bytes)));
        }
    }
    // `all` keeps every value per key (for repeated params); `params` is first-value.
    let mut all: HashMap<String, Vec<String>> = HashMap::new();
    for (key, value) in pairs {
        all.entry(key).or_default().push(value);
    }
    let params: HashMap<String, String> = all
        .iter()
        .map(|(key, values)| (key.clone(), values[0].clone()))
        .collect();

    let format = format_of(&params);
    if let Err((code, message)) = authenticate_request(&state, &params).await {
        return error_response(format, code, message);
    }

    let headers = parts.headers;
    let method = method.strip_suffix(".view").unwrap_or(&method);
    match method {
        "ping" => ok_response(format, Map::new()),
        "getLicense" => ok_response(format, map1("license", json!({ "valid": true }))),
        "getOpenSubsonicExtensions" => ok_response(
            format,
            map1(
                "openSubsonicExtensions",
                json!([
                    { "name": "formPost", "versions": [1] },
                    { "name": "songLyrics", "versions": [1] }
                ]),
            ),
        ),
        "getMusicFolders" => ok_response(
            format,
            map1(
                "musicFolders",
                json!({ "musicFolder": [{ "id": 0, "name": "Music" }] }),
            ),
        ),
        "getGenres" => get_genres(&state, format).await,
        "getArtists" => artist_index(&state, format, "artists").await,
        "getIndexes" => artist_index(&state, format, "indexes").await,
        "getMusicDirectory" => get_music_directory(&state, format, &params).await,
        "getRandomSongs" => get_random_songs(&state, format, &params).await,
        "getSongsByGenre" => get_songs_by_genre(&state, format, &params).await,
        "getLyrics" => get_lyrics(&state, format, &params).await,
        "getLyricsBySongId" => get_lyrics_by_song_id(&state, format, &params).await,
        "getArtist" => get_artist(&state, format, &params).await,
        "getAlbum" => get_album(&state, format, &params).await,
        "getSong" => get_song(&state, format, &params).await,
        "getAlbumList" => get_album_list(&state, format, &params, "albumList").await,
        "getAlbumList2" => get_album_list(&state, format, &params, "albumList2").await,
        "search2" => search(&state, format, &params, "searchResult2").await,
        "search3" => search(&state, format, &params, "searchResult3").await,
        "getCoverArt" => get_cover_art(&state, &params).await,
        "stream" | "download" => stream(&state, format, &params, headers).await,
        "scrobble" => scrobble(&state, format, &params).await,
        "getPlaylists" => get_playlists(&state, format).await,
        "getPlaylist" => get_playlist(&state, format, &params).await,
        "createPlaylist" => create_playlist_ss(&state, format, &params, &all).await,
        "updatePlaylist" => update_playlist_ss(&state, format, &params, &all).await,
        "deletePlaylist" => delete_playlist_ss(&state, format, &params).await,
        "getInternetRadioStations" => get_internet_radio(&state, format).await,
        "createInternetRadioStation" => create_internet_radio(&state, format, &params).await,
        "updateInternetRadioStation" => update_internet_radio(&state, format, &params).await,
        "deleteInternetRadioStation" => delete_internet_radio(&state, format, &params).await,
        "getStarred" => get_starred(&state, format, "starred").await,
        "getStarred2" => get_starred(&state, format, "starred2").await,
        "star" => set_star(&state, format, &all, true).await,
        "unstar" => set_star(&state, format, &all, false).await,
        // Ratings aren't stored yet; accept the call so clients don't error.
        "setRating" => ok_response(format, Map::new()),
        "getUser" => ok_response(
            format,
            map1(
                "user",
                json!({
                    "username": state.subsonic.user,
                    "streamRole": true,
                    "downloadRole": true,
                    "scrobblingEnabled": true
                }),
            ),
        ),
        other => error_response(format, 0, &format!("Unsupported method: {other}")),
    }
}

// ---- Authentication -------------------------------------------------------

/// Returns `Ok(())` when the request is authorized, else a `(code, message)` Subsonic
/// error. Open mode (no configured password) accepts any credentials.
/// Verify the Subsonic password params (`p`, `p=enc:<hex>`, or salted `t`+`s` token) against an
/// `expected` secret. Shared by the single-user (legacy/setup) and per-account paths.
fn verify_credential(
    params: &HashMap<String, String>,
    expected: &str,
) -> Result<(), (u32, &'static str)> {
    if let Some(provided) = params.get("p") {
        let provided = match provided.strip_prefix("enc:") {
            Some(hex) => decode_hex(hex).ok_or((40, "Wrong username or password."))?,
            None => provided.clone(),
        };
        return (provided == expected)
            .then_some(())
            .ok_or((40, "Wrong username or password."));
    }
    if let (Some(token), Some(salt)) = (params.get("t"), params.get("s")) {
        let computed = md5_hex(&format!("{expected}{salt}"));
        return computed
            .eq_ignore_ascii_case(token)
            .then_some(())
            .ok_or((40, "Wrong username or password."));
    }
    Err((10, "Required parameter is missing."))
}

fn authenticate(
    auth: &SubsonicAuth,
    params: &HashMap<String, String>,
) -> Result<(), (u32, &'static str)> {
    let Some(expected) = auth.password.as_deref() else {
        return Ok(());
    };
    if params.get("u").map(String::as_str) != Some(auth.user.as_str()) {
        return Err((40, "Wrong username or password."));
    }
    verify_credential(params, expected)
}

/// Authenticate a Subsonic request. When user accounts exist (the normal case), authenticate
/// against them — `u` is the username and the credential is checked against that user's API
/// token (the cleartext secret the salted-token scheme needs). With no accounts yet (setup /
/// legacy), fall back to the single configured Subsonic user, or open mode when unset.
async fn authenticate_request(
    state: &AppState,
    params: &HashMap<String, String>,
) -> Result<(), (u32, &'static str)> {
    if state.database.count_users().await.unwrap_or(0) == 0 {
        return authenticate(&state.subsonic, params);
    }
    let Some(username) = params.get("u") else {
        return Err((10, "Required parameter is missing."));
    };
    let user = state
        .database
        .user_by_username(username)
        .await
        .ok()
        .flatten()
        .ok_or((40, "Wrong username or password."))?;
    verify_credential(params, &user.api_token)
}

fn md5_hex(input: &str) -> String {
    let digest = Md5::digest(input.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
    out
}

fn hex_digit(nibble: u8) -> char {
    char::from_digit(nibble as u32, 16).unwrap_or('0')
}

fn decode_hex(hex: &str) -> Option<String> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

// ---- Endpoints ------------------------------------------------------------

async fn get_genres(state: &AppState, format: Format) -> Response {
    let index = match state.database.browse_index().await {
        Ok(index) => index,
        Err(error) => return error_response(format, 0, &error.to_string()),
    };
    let genres: Vec<Value> = index
        .genres
        .into_iter()
        .map(|facet| json!({ "value": facet.value, "songCount": facet.track_count }))
        .collect();
    ok_response(format, map1("genres", json!({ "genre": genres })))
}

/// Shared by getArtists (`<artists>`, ID3) and getIndexes (`<indexes>`, legacy): both
/// return artists grouped into alphabetical `<index name="A">` buckets.
async fn artist_index(state: &AppState, format: Format, wrapper: &str) -> Response {
    let artists = match state.database.list_artists(None, -1, 0).await {
        Ok((artists, _)) => artists,
        Err(error) => return error_response(format, 0, &error.to_string()),
    };

    let favorites = load_favorites(state).await;
    let mut buckets: Vec<(String, Vec<Value>)> = Vec::new();
    for artist in &artists {
        let letter = index_letter(&artist.name);
        match buckets.iter_mut().find(|(name, _)| *name == letter) {
            Some((_, list)) => list.push(artist_value(artist, &favorites)),
            None => buckets.push((letter, vec![artist_value(artist, &favorites)])),
        }
    }
    let index: Vec<Value> = buckets
        .into_iter()
        .map(|(name, artist)| json!({ "name": name, "artist": artist }))
        .collect();

    ok_response(
        format,
        map1(
            wrapper,
            json!({ "ignoredArticles": IGNORED_ARTICLES, "index": index }),
        ),
    )
}

fn int_param(params: &HashMap<String, String>, key: &str, default: i64) -> i64 {
    params
        .get(key)
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(default)
}

/// Folder-style browsing: an artist id lists its albums as sub-directories; an album
/// id lists its songs. Older clients (and some new ones) use this instead of the ID3
/// endpoints.
async fn get_music_directory(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
) -> Response {
    let Some(id) = params.get("id") else {
        return error_response(format, 10, "Required parameter is missing.");
    };

    if let Ok(Some(album)) = state.database.album(id).await {
        let tracks = state
            .database
            .tracks_for_album(id)
            .await
            .unwrap_or_default();
        let children = songs_value(&tracks, &load_favorites(state).await);
        return ok_response(
            format,
            map1(
                "directory",
                json!({ "id": album.id, "name": album.title, "child": children }),
            ),
        );
    }

    if let Ok(Some(artist)) = state.database.artist(id).await {
        let albums = state
            .database
            .albums_for_artist(id)
            .await
            .unwrap_or_default();
        let children: Vec<Value> = albums
            .iter()
            .map(|album| {
                json!({
                    "id": album.id,
                    "name": album.title,
                    "title": album.title,
                    "isDir": true,
                    "artist": album.artist_name,
                    "artistId": album.artist_id,
                    "coverArt": album.id,
                })
            })
            .collect();
        return ok_response(
            format,
            map1(
                "directory",
                json!({ "id": artist.id, "name": artist.name, "child": children }),
            ),
        );
    }

    error_response(format, 70, "Directory not found.")
}

async fn get_random_songs(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
) -> Response {
    let size = int_param(params, "size", 10).clamp(1, 500);
    match state.database.random_tracks(size).await {
        Ok(tracks) => ok_response(
            format,
            map1(
                "randomSongs",
                json!({ "song": songs_value(&tracks, &load_favorites(state).await) }),
            ),
        ),
        Err(error) => error_response(format, 0, &error.to_string()),
    }
}

async fn get_songs_by_genre(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
) -> Response {
    let Some(genre) = params.get("genre") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    let count = int_param(params, "count", 10).clamp(1, 500);
    let offset = int_param(params, "offset", 0).max(0);
    let filter = BrowseFilter {
        genre: Some(genre.clone()),
        ..Default::default()
    };
    match state
        .database
        .list_tracks(&filter, None, count, offset)
        .await
    {
        Ok((tracks, _)) => ok_response(
            format,
            map1(
                "songsByGenre",
                json!({ "song": songs_value(&tracks, &load_favorites(state).await) }),
            ),
        ),
        Err(error) => error_response(format, 0, &error.to_string()),
    }
}

/// Legacy lyrics by artist/title. We match on title and return the first track's
/// stored lyrics (empty element if none).
async fn get_lyrics(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
) -> Response {
    let title = params.get("title").map(String::as_str).unwrap_or("").trim();
    let artist = params.get("artist").cloned().unwrap_or_default();
    let mut text = String::new();
    let mut matched_title = title.to_string();
    if !title.is_empty()
        && let Ok(results) = state.database.search(title, 5, 0).await
        && let Some(track) = results.tracks.first()
    {
        matched_title = track.title.clone();
        text = state
            .database
            .track_lyrics(&track.id)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
    }
    ok_response(
        format,
        map1(
            "lyrics",
            json!({ "artist": artist, "title": matched_title, "value": text }),
        ),
    )
}

/// OpenSubsonic structured lyrics by song id (unsynced — we store plain text).
async fn get_lyrics_by_song_id(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
) -> Response {
    let Some(id) = params.get("id") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    let track = match state.database.track(id).await {
        Ok(Some(track)) => track,
        Ok(None) => return error_response(format, 70, "Song not found."),
        Err(error) => return error_response(format, 0, &error.to_string()),
    };

    let structured = match state.database.track_lyrics(id).await.ok().flatten() {
        Some(text) => {
            let lines: Vec<Value> = text.lines().map(|line| json!({ "value": line })).collect();
            vec![json!({
                "displayArtist": track.artist_name,
                "displayTitle": track.title,
                "lang": "xxx",
                "offset": 0,
                "synced": false,
                "line": lines,
            })]
        }
        None => Vec::new(),
    };
    ok_response(
        format,
        map1("lyricsList", json!({ "structuredLyrics": structured })),
    )
}

async fn get_artist(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
) -> Response {
    let Some(id) = params.get("id") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    let artist = match state.database.artist(id).await {
        Ok(Some(artist)) => artist,
        Ok(None) => return error_response(format, 70, "Artist not found."),
        Err(error) => return error_response(format, 0, &error.to_string()),
    };
    let albums = match state.database.albums_for_artist(id).await {
        Ok(albums) => albums,
        Err(error) => return error_response(format, 0, &error.to_string()),
    };
    let favorites = load_favorites(state).await;
    let mut value = artist_value(&artist, &favorites);
    if let Value::Object(map) = &mut value {
        map.insert(
            "album".to_string(),
            Value::Array(albums_value(&albums, &favorites)),
        );
    }
    ok_response(format, map1("artist", value))
}

async fn get_album(state: &AppState, format: Format, params: &HashMap<String, String>) -> Response {
    let Some(id) = params.get("id") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    let album = match state.database.album(id).await {
        Ok(Some(album)) => album,
        Ok(None) => return error_response(format, 70, "Album not found."),
        Err(error) => return error_response(format, 0, &error.to_string()),
    };
    let tracks = match state.database.tracks_for_album(id).await {
        Ok(tracks) => tracks,
        Err(error) => return error_response(format, 0, &error.to_string()),
    };
    let favorites = load_favorites(state).await;
    let mut value = album_value(&album, &favorites);
    if let Value::Object(map) = &mut value {
        map.insert(
            "song".to_string(),
            Value::Array(songs_value(&tracks, &favorites)),
        );
    }
    ok_response(format, map1("album", value))
}

async fn get_song(state: &AppState, format: Format, params: &HashMap<String, String>) -> Response {
    let Some(id) = params.get("id") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    match state.database.track(id).await {
        Ok(Some(track)) => {
            let favorites = load_favorites(state).await;
            ok_response(format, map1("song", song_value(&track, &favorites)))
        }
        Ok(None) => error_response(format, 70, "Song not found."),
        Err(error) => error_response(format, 0, &error.to_string()),
    }
}

async fn get_album_list(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
    wrapper: &str,
) -> Response {
    let sort = match params.get("type").map(String::as_str) {
        Some("alphabeticalByName") => Some("title"),
        Some("newest") | Some("recent") | Some("byYear") => Some("year"),
        _ => None, // alphabeticalByArtist / random / frequent / starred -> default order
    };
    let size = params
        .get("size")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(10)
        .clamp(1, 500);
    let offset = params
        .get("offset")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);

    let albums = match state.database.list_albums(sort, size, offset).await {
        Ok((albums, _)) => albums,
        Err(error) => return error_response(format, 0, &error.to_string()),
    };
    let album = albums_value(&albums, &load_favorites(state).await);
    ok_response(format, map1(wrapper, json!({ "album": album })))
}

async fn search(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
    wrapper: &str,
) -> Response {
    let query = params
        .get("query")
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let results: SearchResults = if query.is_empty() {
        SearchResults {
            query,
            artists: Vec::new(),
            albums: Vec::new(),
            tracks: Vec::new(),
        }
    } else {
        match state.database.search(&query, 50, 0).await {
            Ok(results) => results,
            Err(error) => return error_response(format, 0, &error.to_string()),
        }
    };

    let favorites = load_favorites(state).await;
    ok_response(
        format,
        map1(
            wrapper,
            json!({
                "artist": artists_value(&results.artists, &favorites),
                "album": albums_value(&results.albums, &favorites),
                "song": songs_value(&results.tracks, &favorites),
            }),
        ),
    )
}

async fn get_cover_art(state: &AppState, params: &HashMap<String, String>) -> Response {
    let Some(id) = params.get("id") else {
        return (StatusCode::BAD_REQUEST, "missing id").into_response();
    };

    // The id may be an album id or a song id; resolve to the album's artwork path.
    let album_id = if let Ok(Some(track)) = state.database.track(id).await {
        track.album_id
    } else {
        id.clone()
    };

    let path = match state.database.album(&album_id).await {
        Ok(Some(album)) => album.artwork_path,
        _ => None,
    };
    let Some(path) = path else {
        return (StatusCode::NOT_FOUND, "no cover art").into_response();
    };

    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let extension = path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or_default();
            ([(CONTENT_TYPE, image_content_type(extension))], bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "cover art unavailable").into_response(),
    }
}

async fn stream(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
    headers: HeaderMap,
) -> Response {
    let Some(id) = params.get("id") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    let track = match state.database.track(id).await {
        Ok(Some(track)) => track,
        Ok(None) => return error_response(format, 70, "Song not found."),
        Err(error) => return error_response(format, 0, &error.to_string()),
    };
    let bytes = match tokio::fs::read(&track.path).await {
        Ok(bytes) => bytes,
        Err(error) => return error_response(format, 0, &error.to_string()),
    };
    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|value| value.to_str().ok());
    match ranged_response(bytes, range, audio_content_type(&track.extension)) {
        Ok(response) => response,
        Err(error) => error_response(format, 0, &error.message),
    }
}

async fn scrobble(state: &AppState, format: Format, params: &HashMap<String, String>) -> Response {
    let submission = params
        .get("submission")
        .map(|value| value != "false")
        .unwrap_or(true);
    if submission && let Some(id) = params.get("id") {
        // A confirmed external listen feeds the same history as local playback.
        let _ = state
            .database
            .record_listen(
                id,
                "subsonic",
                crate::now_unix_seconds(),
                musicata_storage::ListenKind::Played,
            )
            .await;
    }
    ok_response(format, Map::new())
}

// ---- Playlists ------------------------------------------------------------

fn playlist_value(playlist: &Playlist) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), json!(playlist.id));
    map.insert("name".into(), json!(playlist.name));
    map.insert("songCount".into(), json!(playlist.song_count));
    map.insert("owner".into(), json!("musicata"));
    map.insert("public".into(), json!(false));
    if let Some(comment) = &playlist.comment {
        map.insert("comment".into(), json!(comment));
    }
    Value::Object(map)
}

async fn get_playlists(state: &AppState, format: Format) -> Response {
    let playlists = state.database.list_playlists().await.unwrap_or_default();
    let list: Vec<Value> = playlists.iter().map(playlist_value).collect();
    ok_response(format, map1("playlists", json!({ "playlist": list })))
}

/// Build a single-playlist response with its song entries.
async fn playlist_response(state: &AppState, format: Format, id: &str) -> Response {
    match state.database.playlist(id).await {
        Ok(Some(playlist)) => {
            let tracks = state.database.playlist_tracks(id).await.unwrap_or_default();
            let favorites = load_favorites(state).await;
            let mut value = playlist_value(&playlist);
            if let Value::Object(map) = &mut value {
                map.insert(
                    "entry".into(),
                    Value::Array(songs_value(&tracks, &favorites)),
                );
            }
            ok_response(format, map1("playlist", value))
        }
        Ok(None) => error_response(format, 70, "Playlist not found."),
        Err(error) => error_response(format, 0, &error.to_string()),
    }
}

async fn get_playlist(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
) -> Response {
    let Some(id) = params.get("id") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    playlist_response(state, format, id).await
}

async fn create_playlist_ss(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
    all: &HashMap<String, Vec<String>>,
) -> Response {
    let now = crate::now_unix_seconds();
    let song_ids = multi(all, "songId").to_vec();

    // With a playlistId, createPlaylist replaces an existing playlist's tracks.
    if let Some(id) = params.get("playlistId") {
        if let Some(name) = params.get("name") {
            let _ = state
                .database
                .update_playlist_meta(id, Some(name), None, now)
                .await;
        }
        if let Err(error) = state.database.set_playlist_tracks(id, &song_ids, now).await {
            return error_response(format, 0, &error.to_string());
        }
        return playlist_response(state, format, id).await;
    }

    let Some(name) = params.get("name") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    match state
        .database
        .create_playlist(name, None, &song_ids, now)
        .await
    {
        Ok(id) => playlist_response(state, format, &id).await,
        Err(error) => error_response(format, 0, &error.to_string()),
    }
}

async fn update_playlist_ss(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
    all: &HashMap<String, Vec<String>>,
) -> Response {
    let Some(id) = params.get("playlistId") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    let now = crate::now_unix_seconds();

    if params.get("name").is_some() || params.get("comment").is_some() {
        let _ = state
            .database
            .update_playlist_meta(
                id,
                params.get("name").map(String::as_str),
                params.get("comment").map(String::as_str),
                now,
            )
            .await;
    }

    let removes = multi(all, "songIndexToRemove");
    let adds = multi(all, "songIdToAdd");
    if !removes.is_empty() || !adds.is_empty() {
        let mut ids = state
            .database
            .playlist_track_ids(id)
            .await
            .unwrap_or_default();
        let mut indices: Vec<usize> = removes
            .iter()
            .filter_map(|value| value.parse().ok())
            .collect();
        indices.sort_unstable();
        indices.dedup();
        for index in indices.into_iter().rev() {
            if index < ids.len() {
                ids.remove(index);
            }
        }
        ids.extend(adds.iter().cloned());
        if let Err(error) = state.database.set_playlist_tracks(id, &ids, now).await {
            return error_response(format, 0, &error.to_string());
        }
    }
    ok_response(format, Map::new())
}

async fn delete_playlist_ss(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
) -> Response {
    let Some(id) = params.get("id") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    match state.database.delete_playlist(id).await {
        Ok(()) => ok_response(format, Map::new()),
        Err(error) => error_response(format, 0, &error.to_string()),
    }
}

// ---- Internet radio -------------------------------------------------------

async fn get_internet_radio(state: &AppState, format: Format) -> Response {
    // Read through the radio provider so every surface (web, native, Subsonic) goes
    // via the same source abstraction. Browse entries carry the station id, name, and
    // stream URL — exactly the Subsonic internet-radio shape.
    let entries = match state
        .providers
        .read()
        .await
        .get(crate::providers::RADIO_PROVIDER_ID)
    {
        Some(handle) => handle.browse().await.unwrap_or_default(),
        None => Vec::new(),
    };
    let list: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let mut map = Map::new();
            map.insert("id".into(), json!(entry.id));
            map.insert("name".into(), json!(entry.title));
            if let Some(url) = &entry.stream_url {
                map.insert("streamUrl".into(), json!(url));
            }
            if let Some(home) = &entry.homepage_url {
                map.insert("homePageUrl".into(), json!(home));
            }
            Value::Object(map)
        })
        .collect();
    ok_response(
        format,
        map1(
            "internetRadioStations",
            json!({ "internetRadioStation": list }),
        ),
    )
}

async fn create_internet_radio(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
) -> Response {
    let (Some(name), Some(stream_url)) = (params.get("name"), params.get("streamUrl")) else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    match state
        .database
        .create_radio_station(
            name,
            stream_url,
            params.get("homepageUrl").map(String::as_str),
            crate::now_unix_seconds(),
        )
        .await
    {
        Ok(_) => ok_response(format, Map::new()),
        Err(error) => error_response(format, 0, &error.to_string()),
    }
}

async fn update_internet_radio(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
) -> Response {
    let Some(id) = params.get("id") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    match state
        .database
        .update_radio_station(
            id,
            params.get("name").map(String::as_str),
            params.get("streamUrl").map(String::as_str),
            params.get("homepageUrl").map(String::as_str),
        )
        .await
    {
        Ok(()) => ok_response(format, Map::new()),
        Err(error) => error_response(format, 0, &error.to_string()),
    }
}

async fn delete_internet_radio(
    state: &AppState,
    format: Format,
    params: &HashMap<String, String>,
) -> Response {
    let Some(id) = params.get("id") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    match state.database.delete_radio_station(id).await {
        Ok(()) => ok_response(format, Map::new()),
        Err(error) => error_response(format, 0, &error.to_string()),
    }
}

// ---- Favorites (starred state) --------------------------------------------

/// Favorited ids, loaded once per request so entity values can carry a `starred`
/// flag and `getStarred` can build its lists.
#[derive(Default)]
struct Favorites {
    tracks: HashSet<String>,
    albums: HashSet<String>,
    artists: HashSet<String>,
}

/// Subsonic `starred` is an ISO datetime; clients use its presence to show the star.
/// We don't surface the exact time per entity in browse, so emit a stable marker.
const STARRED_AT: &str = "2024-01-01T00:00:00Z";

async fn load_favorites(state: &AppState) -> Favorites {
    let collect = |ids: Vec<String>| ids.into_iter().collect::<HashSet<_>>();
    Favorites {
        tracks: collect(
            state
                .database
                .favorite_ids("track")
                .await
                .unwrap_or_default(),
        ),
        albums: collect(
            state
                .database
                .favorite_ids("album")
                .await
                .unwrap_or_default(),
        ),
        artists: collect(
            state
                .database
                .favorite_ids("artist")
                .await
                .unwrap_or_default(),
        ),
    }
}

async fn get_starred(state: &AppState, format: Format, wrapper: &str) -> Response {
    let favorites = load_favorites(state).await;
    let tracks = state.database.starred_tracks().await.unwrap_or_default();
    let albums = state.database.starred_albums().await.unwrap_or_default();
    let artists = state.database.starred_artists().await.unwrap_or_default();
    ok_response(
        format,
        map1(
            wrapper,
            json!({
                "artist": artists_value(&artists, &favorites),
                "album": albums_value(&albums, &favorites),
                "song": songs_value(&tracks, &favorites),
            }),
        ),
    )
}

/// star/unstar: `id` (resolved to track/album/artist), `albumId`, `artistId` — any
/// number of each.
async fn set_star(
    state: &AppState,
    format: Format,
    all: &HashMap<String, Vec<String>>,
    star: bool,
) -> Response {
    let now = crate::now_unix_seconds();
    let mut acted = false;
    for id in multi(all, "albumId") {
        toggle_favorite(state, "album", id, star, now).await;
        acted = true;
    }
    for id in multi(all, "artistId") {
        toggle_favorite(state, "artist", id, star, now).await;
        acted = true;
    }
    for id in multi(all, "id") {
        let kind = resolve_favorite_kind(state, id).await;
        toggle_favorite(state, kind, id, star, now).await;
        acted = true;
    }
    if !acted {
        return error_response(format, 10, "Required parameter is missing.");
    }
    ok_response(format, Map::new())
}

async fn toggle_favorite(state: &AppState, kind: &str, id: &str, star: bool, now: i64) {
    if star {
        let _ = state.database.set_favorite(kind, id, now).await;
    } else {
        let _ = state.database.clear_favorite(kind, id).await;
    }
}

/// A Subsonic `id` may be a song, album, or artist; resolve which by lookup.
async fn resolve_favorite_kind(state: &AppState, id: &str) -> &'static str {
    if matches!(state.database.track(id).await, Ok(Some(_))) {
        "track"
    } else if matches!(state.database.album(id).await, Ok(Some(_))) {
        "album"
    } else {
        "artist"
    }
}

// ---- Entity → Subsonic value ----------------------------------------------

fn songs_value(tracks: &[Track], favorites: &Favorites) -> Vec<Value> {
    tracks
        .iter()
        .map(|track| song_value(track, favorites))
        .collect()
}

fn albums_value(albums: &[Album], favorites: &Favorites) -> Vec<Value> {
    albums
        .iter()
        .map(|album| album_value(album, favorites))
        .collect()
}

fn artists_value(artists: &[Artist], favorites: &Favorites) -> Vec<Value> {
    artists
        .iter()
        .map(|artist| artist_value(artist, favorites))
        .collect()
}

fn artist_value(artist: &Artist, favorites: &Favorites) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), json!(artist.id));
    map.insert("name".into(), json!(artist.name));
    map.insert("albumCount".into(), json!(artist.album_count));
    if favorites.artists.contains(&artist.id) {
        map.insert("starred".into(), json!(STARRED_AT));
    }
    Value::Object(map)
}

fn album_value(album: &Album, favorites: &Favorites) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), json!(album.id));
    map.insert("name".into(), json!(album.title));
    map.insert("title".into(), json!(album.title));
    map.insert("artist".into(), json!(album.artist_name));
    map.insert("artistId".into(), json!(album.artist_id));
    map.insert("coverArt".into(), json!(album.id));
    map.insert("songCount".into(), json!(album.track_count));
    if let Some(year) = album.year {
        map.insert("year".into(), json!(year));
    }
    if favorites.albums.contains(&album.id) {
        map.insert("starred".into(), json!(STARRED_AT));
    }
    Value::Object(map)
}

fn song_value(track: &Track, favorites: &Favorites) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), json!(track.id));
    map.insert("parent".into(), json!(track.album_id));
    map.insert("isDir".into(), json!(false));
    map.insert("title".into(), json!(track.title));
    map.insert("album".into(), json!(track.album_title));
    map.insert("artist".into(), json!(track.artist_name));
    map.insert("albumId".into(), json!(track.album_id));
    map.insert("artistId".into(), json!(track.artist_id));
    map.insert("coverArt".into(), json!(track.album_id));
    map.insert("type".into(), json!("music"));
    map.insert("suffix".into(), json!(track.extension));
    map.insert(
        "contentType".into(),
        json!(audio_content_type(&track.extension)),
    );
    if let Some(track_number) = track.track_number {
        map.insert("track".into(), json!(track_number));
    }
    if let Some(disc_number) = track.disc_number {
        map.insert("discNumber".into(), json!(disc_number));
    }
    if let Some(year) = track.year {
        map.insert("year".into(), json!(year));
    }
    if let Some(size) = track.file_size_bytes {
        map.insert("size".into(), json!(size));
    }
    if let Some(duration) = track.duration_seconds {
        map.insert("duration".into(), json!(duration.round() as i64));
        // Approximate average bitrate (kbps) from size and duration.
        if let Some(size) = track.file_size_bytes
            && duration > 0.0
        {
            let kbps = (size as f64 * 8.0) / duration / 1000.0;
            map.insert("bitRate".into(), json!(kbps.round() as i64));
        }
    }
    if favorites.tracks.contains(&track.id) {
        map.insert("starred".into(), json!(STARRED_AT));
    }
    Value::Object(map)
}

fn index_letter(name: &str) -> String {
    name.trim()
        .chars()
        .next()
        .map(|character| {
            if character.is_ascii_alphabetic() {
                character.to_ascii_uppercase().to_string()
            } else {
                "#".to_string()
            }
        })
        .unwrap_or_else(|| "#".to_string())
}

// ---- Response rendering ----------------------------------------------------

fn map1(key: &str, value: Value) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(key.to_string(), value);
    map
}

fn ok_response(format: Format, body: Map<String, Value>) -> Response {
    render(format, "ok", body)
}

fn error_response(format: Format, code: u32, message: &str) -> Response {
    let mut body = Map::new();
    body.insert(
        "error".to_string(),
        json!({ "code": code, "message": message }),
    );
    render(format, "failed", body)
}

fn render(format: Format, status: &str, body: Map<String, Value>) -> Response {
    // Merge the standard envelope fields with the endpoint body.
    let mut root = Map::new();
    root.insert("status".into(), json!(status));
    root.insert("version".into(), json!(API_VERSION));
    root.insert("type".into(), json!("musicata"));
    root.insert("serverVersion".into(), json!(env!("CARGO_PKG_VERSION")));
    root.insert("openSubsonic".into(), json!(true));
    for (key, value) in body {
        root.insert(key, value);
    }

    match format {
        Format::Json => {
            let payload = json!({ "subsonic-response": Value::Object(root) });
            (
                [(CONTENT_TYPE, "application/json; charset=utf-8")],
                serde_json::to_string(&payload).unwrap_or_default(),
            )
                .into_response()
        }
        Format::Xml => {
            let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
            write_element(&mut xml, "subsonic-response", &Value::Object(root));
            ([(CONTENT_TYPE, "application/xml; charset=utf-8")], xml).into_response()
        }
    }
}

/// Render a `Value` as a Subsonic XML element: scalar fields become attributes,
/// object/array fields become child elements (arrays repeat the element).
fn write_element(out: &mut String, name: &str, value: &Value) {
    match value {
        Value::Object(map) => {
            let mut attributes = String::new();
            let mut children = String::new();
            let mut text = String::new();
            for (key, child) in map {
                match child {
                    Value::Null => {}
                    Value::Array(items) => {
                        for item in items {
                            write_element(&mut children, key, item);
                        }
                    }
                    Value::Object(_) => write_element(&mut children, key, child),
                    // A "value" scalar is the element's text content (Subsonic uses
                    // this for e.g. <genre>Rock</genre>); other scalars are attributes.
                    scalar if key == "value" => {
                        text.push_str(&escape_text(&scalar_to_string(scalar)));
                    }
                    scalar => {
                        attributes.push_str(&format!(
                            " {}=\"{}\"",
                            key,
                            escape_attr(&scalar_to_string(scalar))
                        ));
                    }
                }
            }
            if children.is_empty() && text.is_empty() {
                out.push_str(&format!("<{name}{attributes}/>"));
            } else {
                out.push_str(&format!("<{name}{attributes}>{text}{children}</{name}>"));
            }
        }
        Value::Null => {}
        scalar => out.push_str(&format!(
            "<{name}>{}</{name}>",
            escape_text(&scalar_to_string(scalar))
        )),
    }
}

fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

fn escape_attr(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn escape_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_known_vector() {
        // RFC 1321 test vector.
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn token_auth_accepts_correct_token_and_rejects_wrong() {
        let auth = SubsonicAuth {
            user: "u".into(),
            password: Some("sesame".into()),
        };
        let salt = "c19b2d";
        let token = md5_hex(&format!("sesame{salt}"));
        let mut params = HashMap::new();
        params.insert("u".into(), "u".into());
        params.insert("t".into(), token);
        params.insert("s".into(), salt.into());
        assert!(authenticate(&auth, &params).is_ok());

        params.insert("t".into(), "deadbeef".into());
        assert!(authenticate(&auth, &params).is_err());
    }

    #[test]
    fn plaintext_and_hex_password_auth() {
        let auth = SubsonicAuth {
            user: "u".into(),
            password: Some("sesame".into()),
        };
        let mut params = HashMap::new();
        params.insert("u".into(), "u".into());
        params.insert("p".into(), "sesame".into());
        assert!(authenticate(&auth, &params).is_ok());

        // enc: hex-encoded "sesame"
        params.insert("p".into(), "enc:736573616d65".into());
        assert!(authenticate(&auth, &params).is_ok());

        params.insert("p".into(), "wrong".into());
        assert!(authenticate(&auth, &params).is_err());
    }

    #[test]
    fn open_mode_accepts_anything() {
        let auth = SubsonicAuth {
            user: "u".into(),
            password: None,
        };
        assert!(authenticate(&auth, &HashMap::new()).is_ok());
    }

    #[test]
    fn xml_renders_attributes_and_child_elements() {
        let value = json!({
            "status": "ok",
            "artists": { "index": [ { "name": "A", "artist": [ { "id": "1", "name": "ABBA" } ] } ] }
        });
        let mut xml = String::new();
        write_element(&mut xml, "subsonic-response", &value);
        assert!(xml.contains("<subsonic-response status=\"ok\">"));
        assert!(xml.contains("<artists>"));
        assert!(xml.contains("<index name=\"A\">"));
        assert!(xml.contains("<artist id=\"1\" name=\"ABBA\"/>"));
    }

    #[test]
    fn xml_escapes_special_characters() {
        let value = json!({ "name": "Rock & <Roll>" });
        let mut xml = String::new();
        write_element(&mut xml, "item", &value);
        assert_eq!(xml, "<item name=\"Rock &amp; &lt;Roll&gt;\"/>");
    }
}
