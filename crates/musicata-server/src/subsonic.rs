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

use std::collections::HashMap;

use axum::{
    Router,
    body::to_bytes,
    extract::{Path, Request, State},
    http::{HeaderMap, Method, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};
use md5::{Digest, Md5};
use musicata_core::{Album, Artist, SearchResults, Track};
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

/// Parse `a=1&b=2` (query string or form body) into a map, percent-decoding both
/// keys and values. Repeated keys keep the first value.
fn parse_params(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        map.entry(percent_decode(key))
            .or_insert_with(|| percent_decode(value));
    }
    map
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
    // body (POST); accept both. Query wins on the rare duplicate.
    let mut params = parse_params(parts.uri.query().unwrap_or(""));
    if parts.method == Method::POST && is_form_body(&parts.headers) {
        if let Ok(bytes) = to_bytes(body, 64 * 1024).await {
            let text = String::from_utf8_lossy(&bytes);
            for (key, value) in parse_params(&text) {
                params.entry(key).or_insert(value);
            }
        }
    }

    let format = format_of(&params);
    if let Err((code, message)) = authenticate(&state.subsonic, &params) {
        return error_response(format, code, message);
    }

    let headers = parts.headers;
    let method = method.strip_suffix(".view").unwrap_or(&method);
    match method {
        "ping" => ok_response(format, Map::new()),
        "getLicense" => ok_response(format, map1("license", json!({ "valid": true }))),
        "getOpenSubsonicExtensions" => {
            ok_response(format, map1("openSubsonicExtensions", json!([])))
        }
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
        // Things clients probe on connect that we don't model yet: answer empty so
        // they don't treat it as an error.
        "getPlaylists" => ok_response(format, map1("playlists", json!({}))),
        "getStarred" => ok_response(format, map1("starred", json!({}))),
        "getStarred2" => ok_response(format, map1("starred2", json!({}))),
        "star" | "unstar" | "setRating" => ok_response(format, Map::new()),
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

    if let Some(provided) = params.get("p") {
        let provided = match provided.strip_prefix("enc:") {
            Some(hex) => decode_hex(hex).ok_or((40, "Wrong username or password."))?,
            None => provided.clone(),
        };
        if provided == expected {
            return Ok(());
        }
        return Err((40, "Wrong username or password."));
    }

    if let (Some(token), Some(salt)) = (params.get("t"), params.get("s")) {
        let computed = md5_hex(&format!("{expected}{salt}"));
        if computed.eq_ignore_ascii_case(token) {
            return Ok(());
        }
        return Err((40, "Wrong username or password."));
    }

    Err((10, "Required parameter is missing."))
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
        .map(|genre| json!({ "value": genre }))
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

    let mut buckets: Vec<(String, Vec<Value>)> = Vec::new();
    for artist in &artists {
        let letter = index_letter(&artist.name);
        match buckets.iter_mut().find(|(name, _)| *name == letter) {
            Some((_, list)) => list.push(artist_value(artist)),
            None => buckets.push((letter, vec![artist_value(artist)])),
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
    let mut value = artist_value(&artist);
    if let Value::Object(map) = &mut value {
        map.insert(
            "album".to_string(),
            Value::Array(albums.iter().map(album_value).collect()),
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
    let mut value = album_value(&album);
    if let Value::Object(map) = &mut value {
        map.insert(
            "song".to_string(),
            Value::Array(tracks.iter().map(song_value).collect()),
        );
    }
    ok_response(format, map1("album", value))
}

async fn get_song(state: &AppState, format: Format, params: &HashMap<String, String>) -> Response {
    let Some(id) = params.get("id") else {
        return error_response(format, 10, "Required parameter is missing.");
    };
    match state.database.track(id).await {
        Ok(Some(track)) => ok_response(format, map1("song", song_value(&track))),
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
    let album: Vec<Value> = albums.iter().map(album_value).collect();
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
        match state.database.search(&query, 50).await {
            Ok(results) => results,
            Err(error) => return error_response(format, 0, &error.to_string()),
        }
    };

    ok_response(
        format,
        map1(
            wrapper,
            json!({
                "artist": results.artists.iter().map(artist_value).collect::<Vec<_>>(),
                "album": results.albums.iter().map(album_value).collect::<Vec<_>>(),
                "song": results.tracks.iter().map(song_value).collect::<Vec<_>>(),
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
            .record_listen(id, "subsonic", crate::now_unix_seconds())
            .await;
    }
    ok_response(format, Map::new())
}

// ---- Entity → Subsonic value ----------------------------------------------

fn artist_value(artist: &Artist) -> Value {
    json!({
        "id": artist.id,
        "name": artist.name,
        "albumCount": artist.album_count,
    })
}

fn album_value(album: &Album) -> Value {
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
    Value::Object(map)
}

fn song_value(track: &Track) -> Value {
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
            for (key, child) in map {
                match child {
                    Value::Null => {}
                    Value::Array(items) => {
                        for item in items {
                            write_element(&mut children, key, item);
                        }
                    }
                    Value::Object(_) => write_element(&mut children, key, child),
                    scalar => {
                        attributes.push_str(&format!(
                            " {}=\"{}\"",
                            key,
                            escape_attr(&scalar_to_string(scalar))
                        ));
                    }
                }
            }
            if children.is_empty() {
                out.push_str(&format!("<{name}{attributes}/>"));
            } else {
                out.push_str(&format!("<{name}{attributes}>{children}</{name}>"));
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
