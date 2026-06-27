//! The audio-ML worker (M7). A **scheduled** background task (unlike the always-draining
//! workers): off by default, it runs at a user-set daily local time (default 02:00) — sending
//! un-analyzed tracks to the optional `musicata-ml` service over HTTP and storing the returned
//! embedding + tags. The embedding goes into the `vec0` index for "sounds-like" similarity.
//!
//! It coordinates only through the database + settings, like every other worker. The model runs
//! out-of-process (the HTTP boundary), so the server never links the ML/ONNX stack.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, RwLock, watch};

use musicata_storage::Database;

use crate::activity::ActivityLog;
use crate::providers::ProviderRegistry;

/// Settings keys (DB-backed, edited in `/admin` — not flags).
pub const SETTING_ML_ENABLED: &str = "ml_enabled";
pub const SETTING_ML_SERVICE_URL: &str = "ml_service_url";
/// Daily run time, "HH:MM" local. Default 02:00.
pub const SETTING_ML_SCHEDULE: &str = "ml_schedule";

const MODEL_ID: &str = "panns-cnn14-16k";
const BATCH: i64 = 100;
const ANALYZE_TIMEOUT: Duration = Duration::from_secs(120);
/// Re-check interval while disabled (so a runtime enable is noticed without a restart).
const DISABLED_POLL: Duration = Duration::from_secs(3600);

#[derive(Debug, Deserialize)]
struct ServiceAnalysis {
    embedding: Vec<f32>,
    #[serde(default)]
    tags: Vec<ServiceTag>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ServiceTag {
    label: String,
    score: f32,
}

/// Whether ML analysis is enabled (default **off** — opt-in).
pub async fn enabled(database: &Database) -> bool {
    matches!(database.get_setting(SETTING_ML_ENABLED).await, Ok(Some(v)) if v == "true")
}

async fn service_url(database: &Database) -> Option<String> {
    database
        .get_setting(SETTING_ML_SERVICE_URL)
        .await
        .ok()
        .flatten()
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
}

async fn schedule_seconds(database: &Database) -> i64 {
    let raw = database
        .get_setting(SETTING_ML_SCHEDULE)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "02:00".to_string());
    parse_schedule(&raw)
}

/// The scheduled worker loop. Sleeps until the next daily run (or a manual trigger), then drains
/// the analysis backlog. While disabled it just re-checks hourly.
pub async fn ml_loop(
    database: Database,
    providers: Arc<RwLock<ProviderRegistry>>,
    activity: Arc<ActivityLog>,
    trigger: Arc<Notify>,
    mut ready: watch::Receiver<bool>,
) {
    let _ = ready.wait_for(|&r| r).await;
    loop {
        let delay = if enabled(&database).await && service_url(&database).await.is_some() {
            let target = schedule_seconds(&database).await;
            Duration::from_secs(seconds_until(local_seconds_of_day(now_unix()), target) as u64)
        } else {
            DISABLED_POLL
        };

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = trigger.notified() => {} // manual "run now"
        }

        let Some(url) = service_url(&database).await else {
            continue;
        };
        if !enabled(&database).await {
            continue;
        }
        // Drain the whole backlog in this run (it's the scheduled window).
        while ml_pass(&database, &providers, &activity, &url).await > 0 {}
    }
}

/// Analyze one batch of un-embedded tracks via the service. Returns how many were stored.
async fn ml_pass(
    database: &Database,
    providers: &Arc<RwLock<ProviderRegistry>>,
    activity: &Arc<ActivityLog>,
    service_url: &str,
) -> usize {
    let targets = match database.tracks_missing_embedding(BATCH).await {
        Ok(targets) => targets,
        Err(error) => {
            tracing::warn!(%error, "ml: failed to list tracks");
            return 0;
        }
    };
    if targets.is_empty() {
        return 0;
    }

    let total = targets.len();
    let task = activity.start("ml", format!("Audio analysis (0/{total})"));
    let mut done = 0;
    let mut stored = 0;
    for target in targets {
        // An unreadable file is skipped (not marked), so it's retried next run.
        let Ok(audio) = crate::read_track_source_file(providers, &target).await else {
            done += 1;
            continue;
        };
        let url = service_url.to_string();
        let analysis = tokio::task::spawn_blocking(move || analyze_via_service(&url, &audio))
            .await
            .unwrap_or_else(|_| Err("analysis task panicked".to_string()));
        match analysis {
            Ok(analysis) => {
                let tags_json = serde_json::to_string(&analysis.tags).unwrap_or_else(|_| "[]".into());
                if let Err(error) = database
                    .upsert_track_embedding(
                        &target.track_id,
                        &analysis.embedding,
                        MODEL_ID,
                        Some(env!("CARGO_PKG_VERSION")),
                        &tags_json,
                        now_unix(),
                    )
                    .await
                {
                    tracing::warn!(track = %target.track_id, %error, "ml: store failed");
                } else {
                    stored += 1;
                }
            }
            // Service down / track undecodable → skip (retried next run); don't poison the batch.
            Err(error) => {
                tracing::debug!(track = %target.track_id, %error, "ml: analyze failed");
            }
        }
        done += 1;
        activity.update(task, format!("Audio analysis ({done}/{total})"));
    }
    if stored > 0 {
        activity.finish(task, true, Some(format!("Analyzed {stored} tracks")));
    } else {
        activity.remove(task);
    }
    stored
}

/// POST the audio bytes to the service's `/analyze` and parse the embedding + tags.
fn analyze_via_service(service_url: &str, audio: &[u8]) -> Result<ServiceAnalysis, String> {
    let endpoint = format!("{}/analyze", service_url.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new().timeout(ANALYZE_TIMEOUT).build();
    let response = agent
        .post(&endpoint)
        .set("content-type", "application/octet-stream")
        .send_bytes(audio)
        .map_err(|error| error.to_string())?;
    response.into_json().map_err(|error| error.to_string())
}

// ---- Scheduling helpers (pure, unit-tested) ----

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Local wall-clock seconds-since-midnight for a Unix timestamp, via libc (no datetime dep,
/// and DST-correct because it's recomputed each cycle).
fn local_seconds_of_day(unix: i64) -> i64 {
    let time = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&time, &mut tm);
    }
    (tm.tm_hour as i64) * 3600 + (tm.tm_min as i64) * 60 + tm.tm_sec as i64
}

/// Seconds from now until the next local occurrence of `target_secs_of_day`.
fn seconds_until(now_secs_of_day: i64, target_secs_of_day: i64) -> i64 {
    let delta = target_secs_of_day - now_secs_of_day;
    if delta <= 0 { delta + 86_400 } else { delta }
}

/// Parse an "HH:MM" daily schedule to seconds-of-day (defaults/clamps to a valid time; 02:00).
fn parse_schedule(raw: &str) -> i64 {
    let mut parts = raw.split(':');
    let hour: i64 = parts.next().and_then(|h| h.trim().parse().ok()).unwrap_or(2).clamp(0, 23);
    let minute: i64 = parts.next().and_then(|m| m.trim().parse().ok()).unwrap_or(0).clamp(0, 59);
    hour * 3600 + minute * 60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_schedule_with_defaults() {
        assert_eq!(parse_schedule("02:00"), 2 * 3600);
        assert_eq!(parse_schedule("23:30"), 23 * 3600 + 30 * 60);
        assert_eq!(parse_schedule("7"), 7 * 3600); // missing minutes → :00
        assert_eq!(parse_schedule("bogus"), 2 * 3600); // garbage → default 02:00
        assert_eq!(parse_schedule("99:99"), 23 * 3600 + 59 * 60); // clamped
    }

    #[test]
    fn seconds_until_wraps_to_next_day() {
        // Target later today.
        assert_eq!(seconds_until(1 * 3600, 2 * 3600), 3600);
        // Target already passed → tomorrow.
        assert_eq!(seconds_until(3 * 3600, 2 * 3600), 23 * 3600);
        // Exactly now → a full day away (don't fire twice).
        assert_eq!(seconds_until(2 * 3600, 2 * 3600), 86_400);
    }
}
