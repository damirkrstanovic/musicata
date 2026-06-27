//! `musicata-ml` — an optional, standalone audio-ML service (M7).
//!
//! It runs an ONNX audio model (PANNs CNN14, raw 16 kHz waveform in) and exposes it over HTTP:
//! `POST /analyze` with audio bytes → a 2048-d embedding (for "sounds-like" similarity) plus
//! the top AudioSet tags. The core `musicata-server` talks to it over this boundary and never
//! links the ML/ONNX stack — it stays optional and off the playback path.
//!
//! Config (env): `MUSICATA_ML_ADDR` (default `127.0.0.1:3091`), `MUSICATA_ML_MODEL` (path to the
//! `.onnx`), and optionally `MUSICATA_ML_MODEL_URL` / `MUSICATA_ML_DATA_URL` to fetch + cache the
//! model on first run.

mod decode;
mod model;

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, post},
};
use serde_json::json;

use crate::model::{Analysis, AudioModel};

/// The model's fixed input sample rate (PANNs CNN14 16 kHz variant).
const SAMPLE_RATE: u32 = 16_000;
const MODEL_ID: &str = "panns-cnn14-16k";

#[derive(Clone)]
struct AppState {
    model: Arc<Mutex<AudioModel>>,
    tag_count: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "musicata_ml=info".into()),
        )
        .init();

    let addr = std::env::var("MUSICATA_ML_ADDR").unwrap_or_else(|_| "127.0.0.1:3091".to_string());
    let model_path = std::env::var("MUSICATA_ML_MODEL")
        .context("set MUSICATA_ML_MODEL to the ONNX model path")?;
    if let Ok(url) = std::env::var("MUSICATA_ML_MODEL_URL") {
        let data_url = std::env::var("MUSICATA_ML_DATA_URL").ok();
        model::ensure_model(&model_path, &url, data_url.as_deref())?;
    }

    tracing::info!("loading model {model_path}");
    let model = AudioModel::load(&model_path)?;
    let tag_count = model.tag_count();
    let state = AppState {
        model: Arc::new(Mutex::new(model)),
        tag_count,
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/info", get(info))
        // Audio uploads are whole tracks; lift the default body cap.
        .route("/analyze", post(analyze))
        .layer(DefaultBodyLimit::disable())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!("musicata-ml listening on {addr} (model {MODEL_ID})");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "model": MODEL_ID }))
}

async fn info(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "model": MODEL_ID,
        "version": env!("CARGO_PKG_VERSION"),
        "embedding_dim": 2048,
        "sample_rate": SAMPLE_RATE,
        "tags": state.tag_count,
    }))
}

/// `POST /analyze` — body is the raw audio file; returns the embedding + top tags.
async fn analyze(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<Analysis>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || -> Result<Analysis, String> {
        let samples = decode::decode_to_mono(&body, SAMPLE_RATE).map_err(|e| e.to_string())?;
        let mut model = state.model.lock().map_err(|_| "model lock poisoned".to_string())?;
        model.analyze(&samples, 12).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    result
        .map(Json)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))
}
