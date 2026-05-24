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
use musicata_storage::Database;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{fs, net::SocketAddr, path::PathBuf, sync::Arc, time::Instant};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    library: Arc<Library>,
}

#[derive(Debug)]
struct Config {
    library: PathBuf,
    database: PathBuf,
    addr: SocketAddr,
    rescan: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let config = Config::from_args()?;
    let provider = LocalDiskProvider::new(&config.library);
    let database = Database::connect(&config.database)
        .await
        .with_context(|| format!("failed to open database {}", config.database.display()))?;
    let library = Arc::new(load_or_scan_library(&database, &provider, config.rescan).await?);

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

async fn load_or_scan_library(
    database: &Database,
    provider: &LocalDiskProvider,
    rescan: bool,
) -> Result<Library> {
    if !rescan && let Some(library) = database.load_library().await? {
        tracing::info!(
            artists = library.artists.len(),
            albums = library.albums.len(),
            tracks = library.tracks.len(),
            "loaded library from database"
        );
        return Ok(library);
    }

    let library = provider
        .scan()
        .with_context(|| format!("failed to scan {}", provider.root().display()))?;
    database.save_library(&library).await?;

    Ok(library)
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
                "--help" | "-h" => {
                    println!(
                        "Usage: musicata-server [--config PATH] [--library PATH] [--database PATH] [--addr HOST:PORT] [--rescan]\n\nConfig precedence: defaults < config file < environment < CLI\nEnvironment: MUSICATA_CONFIG, MUSICATA_LIBRARY, MUSICATA_DATABASE, MUSICATA_ADDR, MUSICATA_RESCAN\nConfig file keys: library, database, addr, rescan\nDefaults: --library testdata --database .musicata/musicata.db --addr 127.0.0.1:3030"
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
    }
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
    use super::{Config, app, parse_range};
    use axum::{
        body::{Body, to_bytes},
        http::{
            Request, StatusCode,
            header::{CONTENT_TYPE, RANGE},
        },
    };
    use musicata_core::{Library, scan_local_library};
    use std::{collections::HashMap, fs, path::PathBuf, sync::Arc, time::SystemTime};
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
            ],
            |name| env.get(name).cloned(),
        )
        .expect("config");

        assert_eq!(config.library, PathBuf::from("/from/cli"));
        assert_eq!(config.database, PathBuf::from("/from/cli.db"));
        assert_eq!(config.addr.to_string(), "127.0.0.1:6000");
        assert!(config.rescan);
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
    async fn serves_health_json() {
        let fixture = TestFixture::new("health");
        let app = app(Arc::new(fixture.library()));
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
    async fn serves_album_artwork() {
        let fixture = TestFixture::new("artwork");
        let library = Arc::new(fixture.library());
        let album_id = library
            .albums
            .iter()
            .find(|album| album.artwork_url.is_some())
            .expect("album artwork")
            .id
            .clone();
        let app = app(library);
        let response = app
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
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "image/jpeg");
        assert!(!body.is_empty());
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
