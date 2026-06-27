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
/// Analyze a centered excerpt of at most this many seconds. PANNs is trained on ~10 s AudioSet
/// clips, so a short window is both representative *and* far faster than a whole multi-minute
/// track (inference cost scales with length). The centered window skips quiet intros/outros.
const MAX_ANALYZE_SECONDS: usize = 15;

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

/// A centered window of at most `max` samples (the whole signal if it's already shorter).
fn center_excerpt(samples: &[f32], max: usize) -> &[f32] {
    if samples.len() <= max {
        return samples;
    }
    let start = (samples.len() - max) / 2;
    &samples[start..start + max]
}

#[cfg(test)]
mod tests {
    use super::{center_excerpt, lock_recovered};
    use std::sync::{Arc, Mutex};

    #[test]
    fn center_excerpt_caps_and_centers() {
        let signal: Vec<f32> = (0..100).map(|n| n as f32).collect();
        // Shorter than the cap → unchanged.
        assert_eq!(center_excerpt(&signal, 200).len(), 100);
        // Longer → a centered window of exactly `max`.
        let excerpt = center_excerpt(&signal, 40);
        assert_eq!(excerpt.len(), 40);
        assert_eq!(excerpt[0], 30.0); // (100-40)/2 = 30
    }

    #[test]
    fn lock_recovered_survives_a_poisoned_mutex() {
        // BUG-13: a panic while holding the model lock used to poison it, so every later
        // request failed with "model lock poisoned". Locking must recover the guard so a
        // single bad request can't brick the service.
        let mutex = Arc::new(Mutex::new(7));
        let poisoner = {
            let mutex = Arc::clone(&mutex);
            std::thread::spawn(move || {
                let _guard = mutex.lock().unwrap();
                panic!("boom while holding the lock");
            })
        };
        assert!(poisoner.join().is_err(), "the thread panicked, poisoning the lock");
        assert!(mutex.lock().is_err(), "the mutex is now poisoned");

        // Recovery still yields the guard and the value.
        let guard = lock_recovered(&mutex);
        assert_eq!(*guard, 7);
    }
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

/// Lock a mutex, recovering the guard even if a previous holder panicked. A single bad
/// request must not poison the model and brick every later request.
fn lock_recovered<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `POST /analyze` — body is the raw audio file; returns the embedding + top tags.
async fn analyze(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<Analysis>, (StatusCode, String)> {
    let result = tokio::task::spawn_blocking(move || -> Result<Analysis, String> {
        let samples = decode::decode_to_mono(&body, SAMPLE_RATE).map_err(|e| e.to_string())?;
        let excerpt = center_excerpt(&samples, MAX_ANALYZE_SECONDS * SAMPLE_RATE as usize);
        let mut model = lock_recovered(&state.model);
        model.analyze(excerpt, 12).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    result
        .map(Json)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))
}
