use anyhow::{Context, Result, anyhow};
use axum::{
    Json, Router,
    body::Body,
    extract::Request,
    extract::{Path, Query, State},
    http::{
        HeaderMap, StatusCode,
        header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE},
    },
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use musicata_core::{Library, LocalDiskProvider, MusicProvider};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Instant};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    library: Arc<Library>,
}

#[derive(Debug)]
struct Config {
    library: PathBuf,
    addr: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let config = Config::from_args()?;
    let provider = LocalDiskProvider::new(&config.library);
    let library = Arc::new(
        provider
            .scan()
            .with_context(|| format!("failed to scan {}", provider.root().display()))?,
    );

    tracing::info!(
        artists = library.artists.len(),
        albums = library.albums.len(),
        tracks = library.tracks.len(),
        root = %library.source_root,
        "library ready"
    );

    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("failed to bind {}", config.addr))?;

    tracing::info!("listening on http://{}", config.addr);
    axum::serve(listener, app(library))
        .await
        .context("server failed")?;

    Ok(())
}

fn app(library: Arc<Library>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/styles.css", get(styles_css))
        .route("/manifest.webmanifest", get(manifest))
        .route("/sw.js", get(service_worker))
        .route("/api/health", get(health))
        .route("/api/library/summary", get(library_summary))
        .route("/api/artists", get(artists))
        .route("/api/albums", get(albums))
        .route("/api/tracks", get(tracks))
        .route("/api/search", get(search))
        .route("/api/tracks/{id}/stream", get(stream_track))
        .route("/api/albums/{id}/artwork", get(album_artwork))
        .fallback(fallback)
        .layer(middleware::from_fn(log_request))
        .with_state(AppState { library })
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

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "provider": state.library.provider_id,
        "tracks": state.library.tracks.len(),
    }))
}

async fn library_summary(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.library.summary())
}

async fn artists(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.library.artists.clone())
}

async fn albums(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.library.albums.clone())
}

async fn tracks(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.library.tracks.clone())
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

async fn search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    let q = query.q.unwrap_or_default();
    Json(state.library.search(&q))
}

async fn stream_track(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let track = state
        .library
        .track(&id)
        .ok_or_else(|| AppError::not_found(format!("unknown track: {id}")))?;
    let bytes = tokio::fs::read(&track.path).await?;
    let content_type = audio_content_type(&track.extension);

    ranged_response(
        bytes,
        headers.get(RANGE).and_then(|value| value.to_str().ok()),
        content_type,
    )
}

async fn album_artwork(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let album = state
        .library
        .album(&id)
        .ok_or_else(|| AppError::not_found(format!("unknown album: {id}")))?;
    let path = album
        .artwork_path
        .as_ref()
        .ok_or_else(|| AppError::not_found(format!("album has no artwork: {id}")))?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let bytes = tokio::fs::read(path).await?;

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, image_content_type(extension))
        .header(CONTENT_LENGTH, bytes.len().to_string())
        .body(Body::from(bytes))
        .map_err(AppError::from)
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
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: error.to_string(),
        }
    }
}

impl From<axum::http::Error> for AppError {
    fn from(error: axum::http::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: error.to_string(),
        }
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
        let mut library = PathBuf::from("testdata");
        let mut addr: SocketAddr = "127.0.0.1:3030".parse()?;
        let mut args = std::env::args().skip(1);

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--library" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--library requires a path"))?;
                    library = PathBuf::from(value);
                }
                "--addr" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--addr requires host:port"))?;
                    addr = value
                        .parse()
                        .with_context(|| format!("invalid --addr value: {value}"))?;
                }
                "--help" | "-h" => {
                    println!(
                        "Usage: musicata-server [--library PATH] [--addr HOST:PORT]\n\nDefaults: --library testdata --addr 127.0.0.1:3030"
                    );
                    std::process::exit(0);
                }
                value => return Err(anyhow!("unknown argument: {value}")),
            }
        }

        Ok(Self { library, addr })
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("musicata_server=info,musicata_core=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::{app, parse_range};
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header::RANGE},
    };
    use musicata_core::{Library, scan_local_library};
    use std::{fs, path::PathBuf, sync::Arc, time::SystemTime};
    use tower::ServiceExt;

    #[test]
    fn parses_common_byte_ranges() {
        assert_eq!(parse_range("bytes=0-", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=150-", 100), None);
    }

    #[tokio::test]
    async fn serves_library_summary_json() {
        let fixture = TestFixture::new("summary");
        let app = app(Arc::new(fixture.library()));
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
    async fn serves_search_results_json() {
        let fixture = TestFixture::new("search");
        let app = app(Arc::new(fixture.library()));
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
    async fn serves_track_stream_ranges() {
        let fixture = TestFixture::new("stream");
        let library = Arc::new(fixture.library());
        let track_id = library.tracks.first().expect("track").id.clone();
        let app = app(library);
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
    async fn serves_stable_error_envelopes() {
        let fixture = TestFixture::new("missing");
        let app = app(Arc::new(fixture.library()));
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
            fixture.write("1996 - U Nedogled/Darkwood Dub - U Nedogled.mp3");
            fixture
        }

        fn library(&self) -> Library {
            scan_local_library(&self.root).expect("scan fixture")
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
}
