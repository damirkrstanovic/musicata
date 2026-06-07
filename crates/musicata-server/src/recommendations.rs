//! Track similarity + radio seeding for "Start radio from this" and continuous play.
//!
//! Two layers, first-hit-wins: **ListenBrainz Labs** `similar-recordings` (MBID-in/MBID-out,
//! CC0, no key — cached in `similarity_cache`) resolved to local tracks, then a **local
//! content fallback** (genre/artist overlap, no network) to fill. The external call is
//! rate-limited and cached, so refills/requests resolve off the DB, not the wire.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use musicata_storage::Database;

/// The ListenBrainz Labs similarity model (session-based collaborative filtering). The string
/// encodes the model's own knobs (`limit_50`, `skip_30`, `threshold_15`); it is versioned
/// upstream and may need refreshing — check labs.api.listenbrainz.org if results dry up.
pub const LB_SIMILAR_ALGORITHM: &str =
    "session_based_days_7500_session_300_contribution_5_threshold_15_limit_50_skip_30_top_n_listeners_1000";
const LB_TIMEOUT: Duration = Duration::from_secs(15);
/// ListenBrainz rate-limits via response headers; space request starts ~1/s to stay polite.
const LB_MIN_INTERVAL: Duration = Duration::from_millis(1100);
/// Re-query a seed's similar recordings at most this often.
const CACHE_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;
/// Recency-exclusion window for continuous play: don't re-queue something heard this recently.
const RECENCY_WINDOW_SECONDS: i64 = 7 * 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoredMbid {
    pub mbid: String,
    pub score: f64,
}

/// A throttled ListenBrainz Labs client. Sync (run on `spawn_blocking`); base URL injectable
/// for tests.
pub struct ListenBrainzClient {
    base_url: String,
    agent: ureq::Agent,
    next_slot: Mutex<Option<Instant>>,
}

impl Default for ListenBrainzClient {
    fn default() -> Self {
        Self::with_base_url("https://labs.api.listenbrainz.org")
    }
}

impl ListenBrainzClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        let user_agent = format!(
            "Musicata/{} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_REPOSITORY")
        );
        let agent = ureq::AgentBuilder::new()
            .user_agent(&user_agent)
            .timeout(LB_TIMEOUT)
            .build();
        Self {
            base_url: base_url.into(),
            agent,
            next_slot: Mutex::new(None),
        }
    }

    fn reserve_slot(&self) {
        let start = {
            let mut slot = self.next_slot.lock().expect("lb limiter poisoned");
            let now = Instant::now();
            let start = match *slot {
                Some(next) if next > now => next,
                _ => now,
            };
            *slot = Some(start + LB_MIN_INTERVAL);
            start
        };
        let now = Instant::now();
        if start > now {
            std::thread::sleep(start - now);
        }
    }

    /// Similar recordings for a recording MBID, highest score first. `Ok(vec)` (an empty vec
    /// means genuinely none), `Err` transient (HTTP/transport — retry, don't negative-cache).
    pub fn similar_recordings(&self, recording_mbid: &str) -> Result<Vec<ScoredMbid>, String> {
        self.reserve_slot();
        let url = format!("{}/similar-recordings/json", self.base_url);
        let value = self
            .agent
            .get(&url)
            .query("recording_mbids", recording_mbid)
            .query("algorithm", LB_SIMILAR_ALGORITHM)
            .call()
            .map_err(|error| match error {
                ureq::Error::Status(status, response) => {
                    let body = response.into_string().unwrap_or_else(|_| "<no body>".to_string());
                    format!("ListenBrainz HTTP {status}: {body}")
                }
                ureq::Error::Transport(error) => error.to_string(),
            })?
            .into_json::<Value>()
            .map_err(|error| error.to_string())?;
        Ok(parse_similar_recordings(&value))
    }
}

/// Parse a ListenBrainz Labs similar-recordings response into scored MBIDs. Tolerant of the
/// shapes the endpoint has used: a top-level array, a `{"data":[…]}` wrapper, or `[[…]]`.
fn parse_similar_recordings(value: &Value) -> Vec<ScoredMbid> {
    let arr = value
        .as_array()
        .cloned()
        .or_else(|| value.get("data").and_then(|d| d.as_array()).cloned())
        .unwrap_or_default();
    // Some labs endpoints wrap the rows in an extra array.
    let items = if arr.len() == 1 && arr[0].is_array() {
        arr[0].as_array().cloned().unwrap_or_default()
    } else {
        arr
    };
    items
        .iter()
        .filter_map(|item| {
            let mbid = item.get("recording_mbid")?.as_str()?.to_string();
            let score = item.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            Some(ScoredMbid { mbid, score })
        })
        .collect()
}

/// Cached similar recordings for a seed MBID: serve the cache if fresh, else fetch (once,
/// rate-limited) and cache the result (an empty result is cached too, as a `not_found` marker).
async fn cached_similar_recordings(
    database: &Database,
    client: &Arc<ListenBrainzClient>,
    recording_mbid: &str,
    now_unix: i64,
) -> Vec<ScoredMbid> {
    if let Ok(Some((json, fetched))) = database.get_similarity_cache("recording", recording_mbid).await
    {
        if now_unix - fetched < CACHE_TTL_SECONDS {
            return serde_json::from_str(&json).unwrap_or_default();
        }
    }
    let client = client.clone();
    let mbid = recording_mbid.to_string();
    let fetched = tokio::task::spawn_blocking(move || client.similar_recordings(&mbid)).await;
    match fetched {
        Ok(Ok(scored)) => {
            if let Ok(json) = serde_json::to_string(&scored) {
                let _ = database
                    .set_similarity_cache("recording", recording_mbid, &json, now_unix)
                    .await;
            }
            scored
        }
        // Transient failure (network) or task panic: don't cache, return nothing for now.
        _ => Vec::new(),
    }
}

/// Up to `limit` local track ids similar to `seed_track_id`: cached ListenBrainz similar
/// recordings resolved to local tracks, topped up with local content similarity, excluding the
/// seed, the `exclude` set, and anything played recently.
pub async fn similar_track_ids(
    database: &Database,
    client: &Arc<ListenBrainzClient>,
    seed_track_id: &str,
    exclude: &HashSet<String>,
    limit: usize,
    now_unix: i64,
) -> Vec<String> {
    let mut seen = exclude.clone();
    seen.insert(seed_track_id.to_string());
    if let Ok(recent) = database.recently_played_track_ids(now_unix - RECENCY_WINDOW_SECONDS).await {
        seen.extend(recent);
    }

    let mut out: Vec<String> = Vec::new();

    // 1. External similarity (ListenBrainz, cached) resolved to local tracks.
    if let Ok(Some(mbid)) = database.track_recording_mbid(seed_track_id).await {
        let scored = cached_similar_recordings(database, client, &mbid, now_unix).await;
        if !scored.is_empty() {
            let mbids: Vec<String> = scored.into_iter().map(|s| s.mbid).collect();
            if let Ok(local) = database.tracks_for_recording_mbids(&mbids).await {
                for id in local {
                    if seen.insert(id.clone()) {
                        out.push(id);
                        if out.len() >= limit {
                            return out;
                        }
                    }
                }
            }
        }
    }

    // 2. Local content fallback to fill the rest (always available, no network).
    if out.len() < limit {
        let want = ((limit - out.len()) * 4).max(limit) as i64;
        if let Ok(local) = database.similar_local_track_ids(seed_track_id, want).await {
            for id in local {
                if seen.insert(id.clone()) {
                    out.push(id);
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_top_level_array() {
        let value = serde_json::json!([
            { "recording_mbid": "a", "recording_name": "A", "score": 458 },
            { "recording_mbid": "b", "score": 312 }
        ]);
        let parsed = parse_similar_recordings(&value);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], ScoredMbid { mbid: "a".into(), score: 458.0 });
        assert_eq!(parsed[1].mbid, "b");
    }

    #[test]
    fn parses_data_wrapper_and_skips_invalid() {
        let value = serde_json::json!({
            "data": [
                { "recording_mbid": "x", "score": 10 },
                { "no_mbid": true }
            ]
        });
        let parsed = parse_similar_recordings(&value);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].mbid, "x");
    }

    #[test]
    fn parses_nested_array() {
        let value = serde_json::json!([[{ "recording_mbid": "n", "score": 1 }]]);
        let parsed = parse_similar_recordings(&value);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].mbid, "n");
    }

    #[test]
    fn empty_response_is_empty() {
        assert!(parse_similar_recordings(&serde_json::json!([])).is_empty());
        assert!(parse_similar_recordings(&serde_json::json!({})).is_empty());
    }
}
