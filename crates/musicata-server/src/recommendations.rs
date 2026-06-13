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
/// similar-artists uses a *different* algorithm enum than similar-recordings (no
/// `top_n_listeners`). Artist similarity has far better coverage, so it's the primary source.
pub const LB_ARTISTS_ALGORITHM: &str =
    "session_based_days_9000_session_300_contribution_5_threshold_15_limit_50_skip_30";
const LB_TIMEOUT: Duration = Duration::from_secs(15);
/// ListenBrainz rate-limits via response headers; space request starts ~1/s to stay polite.
const LB_MIN_INTERVAL: Duration = Duration::from_millis(1100);
/// Re-query a seed's similar recordings at most this often.
const CACHE_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;
/// Recency-exclusion window for continuous play (autoplay only): don't re-queue something heard
/// this recently. Explicit radio passes `None` so a station isn't shrunk by what you've played.
pub const RECENCY_WINDOW_SECONDS: i64 = 7 * 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoredMbid {
    pub mbid: String,
    pub score: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoredArtist {
    pub name: String,
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

    /// Similar artists for an artist MBID, highest score first. The radio workhorse — better
    /// coverage than similar-recordings.
    pub fn similar_artists(&self, artist_mbid: &str) -> Result<Vec<ScoredArtist>, String> {
        self.reserve_slot();
        let url = format!("{}/similar-artists/json", self.base_url);
        let value = self
            .agent
            .get(&url)
            .query("artist_mbids", artist_mbid)
            .query("algorithm", LB_ARTISTS_ALGORITHM)
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
        Ok(parse_similar_artists(&value))
    }
}

/// Parse a similar-artists response (a flat array of `{name, artist_mbid, score}`) into names.
fn parse_similar_artists(value: &Value) -> Vec<ScoredArtist> {
    let arr = value
        .as_array()
        .cloned()
        .or_else(|| value.get("data").and_then(|d| d.as_array()).cloned())
        .unwrap_or_default();
    let items = if arr.len() == 1 && arr[0].is_array() {
        arr[0].as_array().cloned().unwrap_or_default()
    } else {
        arr
    };
    items
        .iter()
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?.to_string();
            let score = item.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            Some(ScoredArtist { name, score })
        })
        .collect()
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

/// Cached similar artists for a seed artist MBID (same cache/fetch shape as recordings).
async fn cached_similar_artists(
    database: &Database,
    client: &Arc<ListenBrainzClient>,
    artist_mbid: &str,
    now_unix: i64,
) -> Vec<ScoredArtist> {
    if let Ok(Some((json, fetched))) = database.get_similarity_cache("artist", artist_mbid).await {
        if now_unix - fetched < CACHE_TTL_SECONDS {
            return serde_json::from_str(&json).unwrap_or_default();
        }
    }
    let client = client.clone();
    let mbid = artist_mbid.to_string();
    match tokio::task::spawn_blocking(move || client.similar_artists(&mbid)).await {
        Ok(Ok(artists)) => {
            if let Ok(json) = serde_json::to_string(&artists) {
                let _ = database.set_similarity_cache("artist", artist_mbid, &json, now_unix).await;
            }
            artists
        }
        _ => Vec::new(),
    }
}

/// How many tracks any one artist may contribute to a radio batch (variety cap).
const PER_ARTIST: usize = 2;

/// The seed's MusicBrainz artist id: from the track's tags, else resolved from the artist
/// NAME via a cached MusicBrainz search. This is what makes radio work for libraries whose
/// files carry no MBIDs (the common case) — without it, ListenBrainz can't be seeded at all.
async fn seed_artist_mbid(
    database: &Database,
    mb: &crate::musicbrainz::MusicBrainzClient,
    seed_track_id: &str,
    now_unix: i64,
) -> Option<String> {
    if let Ok(Some(mbid)) = database.track_artist_mbid(seed_track_id).await {
        return Some(mbid);
    }
    let artist_name = database.track(seed_track_id).await.ok().flatten()?.artist_name;
    let key = artist_name.trim().to_lowercase();
    if key.is_empty() {
        return None;
    }
    if let Ok(Some((cached, fetched))) = database.get_similarity_cache("artist_name", &key).await {
        if now_unix - fetched < CACHE_TTL_SECONDS {
            return (!cached.is_empty()).then_some(cached);
        }
    }
    let mb = mb.clone();
    let name = artist_name.clone();
    let mbid = match tokio::task::spawn_blocking(move || mb.search_artist_mbid(&name)).await {
        Ok(Ok(mbid)) => mbid,
        _ => return None, // transient: don't cache a failure
    };
    let _ = database
        .set_similarity_cache("artist_name", &key, mbid.as_deref().unwrap_or(""), now_unix)
        .await;
    mbid
}

/// SplitMix64 — a tiny, dependency-free PRNG. Deterministic given the seed, so a single radio
/// "press" is reproducible and tests are stable; we seed it with the wall-clock + the seed
/// track so each session shuffles differently.
fn split_mix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Next pseudo-random f64 in `[0, 1)` (53-bit mantissa).
fn next_f64(state: &mut u64) -> f64 {
    (split_mix64(state) >> 11) as f64 / (1u64 << 53) as f64
}

/// FNV-1a over a string — a stable, dependency-free hash to fold the seed track id into the RNG
/// seed (so different seeds shuffle differently even within the same wall-clock second).
fn fnv1a(s: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Weighted-by-score sampling of similar artists into an ordered track list. Each artist is
/// drawn with probability proportional to its ListenBrainz similarity score (closer artists
/// appear earlier and more often, but the long tail still gets a chance — variety across radio
/// sessions, vs. a strict score-ordered round-robin), contributing at most `per_artist` of its
/// local tracks. `by_artist` maps a lowercased artist name to its local track ids (already in
/// random order from SQL). Deterministic given `seed`.
fn weighted_artist_track_order(
    artists: &[ScoredArtist],
    by_artist: &std::collections::HashMap<String, Vec<String>>,
    seed: u64,
    per_artist: usize,
) -> Vec<String> {
    // Pool entry per artist that has local tracks: (weight, its tracks, cursor, picks taken).
    // The +1 floors the weight so a zero-score artist still gets a (uniform) chance and the
    // total is never zero.
    let mut pool: Vec<(f64, &Vec<String>, usize, usize)> = artists
        .iter()
        .filter_map(|a| {
            let tracks = by_artist.get(&a.name.to_lowercase())?;
            (!tracks.is_empty()).then_some((a.score.max(0.0) + 1.0, tracks, 0usize, 0usize))
        })
        .collect();

    let mut state = seed ^ 0x9E3779B97F4A7C15;
    let mut out = Vec::new();
    loop {
        let active: Vec<usize> = (0..pool.len())
            .filter(|&i| pool[i].3 < per_artist && pool[i].2 < pool[i].1.len())
            .collect();
        if active.is_empty() {
            break;
        }
        let total: f64 = active.iter().map(|&i| pool[i].0).sum();
        let target = next_f64(&mut state) * total;
        let mut acc = 0.0;
        let mut chosen = *active.last().expect("active is non-empty");
        for &i in &active {
            acc += pool[i].0;
            if target < acc {
                chosen = i;
                break;
            }
        }
        let cursor = pool[chosen].2;
        out.push(pool[chosen].1[cursor].clone());
        pool[chosen].2 += 1;
        pool[chosen].3 += 1;
    }
    out
}

/// Up to `limit` local track ids to play after `seed_track_id`, highest-relevance first:
/// **(1)** ListenBrainz **similar-artists** of the seed's artist → those artists' local tracks
/// (the radio workhorse, **weighted-by-score sampling** so closer artists lead but the tail
/// still surfaces, capped per artist so none dominates); **(2)** ListenBrainz similar-recordings
/// (a bonus — sparse coverage); **(3)** local genre/artist content similarity. Excludes the seed
/// and the `exclude` set; `recency_window_secs` (autoplay only) additionally excludes
/// recently-played tracks. **Frequently-skipped** tracks (a skip penalty) are deprioritized —
/// held back and used only to reach `limit` — so radio leans away from what you keep skipping
/// without banning it outright (which would starve a small library).
pub async fn similar_track_ids(
    database: &Database,
    client: &Arc<ListenBrainzClient>,
    mb: &crate::musicbrainz::MusicBrainzClient,
    seed_track_id: &str,
    exclude: &HashSet<String>,
    limit: usize,
    now_unix: i64,
    recency_window_secs: Option<i64>,
) -> Vec<String> {
    let mut seen = exclude.clone();
    seen.insert(seed_track_id.to_string());
    if let Some(window) = recency_window_secs {
        if let Ok(recent) = database.recently_played_track_ids(now_unix - window).await {
            seen.extend(recent);
        }
    }
    // Skip penalty: tracks the listener keeps skipping are held back, not banned (see below).
    let penalized = database.frequently_skipped_track_ids().await.unwrap_or_default();

    let mut out: Vec<String> = Vec::new();
    let mut deferred: Vec<String> = Vec::new();
    // Route a candidate to `out` (or `deferred` if it's a frequent skip), de-duplicating via
    // `seen`. Returns true once `out` has reached `limit`.
    macro_rules! consider {
        ($id:expr) => {{
            let id = $id;
            if seen.insert(id.clone()) {
                if penalized.contains(&id) {
                    deferred.push(id);
                } else {
                    out.push(id);
                }
            }
            out.len() >= limit
        }};
    }

    // 1. Similar artists (best coverage) → their local tracks, weighted-by-score sampling.
    if let Some(artist_mbid) = seed_artist_mbid(database, mb, seed_track_id, now_unix).await {
        let artists = cached_similar_artists(database, client, &artist_mbid, now_unix).await;
        if !artists.is_empty() {
            let names: Vec<String> = artists.iter().take(40).map(|a| a.name.clone()).collect();
            if let Ok(pairs) = database.tracks_for_artist_names(&names).await {
                let mut by_artist: std::collections::HashMap<String, Vec<String>> =
                    std::collections::HashMap::new();
                for (name, track_id) in pairs {
                    by_artist.entry(name).or_default().push(track_id);
                }
                let seed = (now_unix as u64) ^ fnv1a(seed_track_id);
                for id in weighted_artist_track_order(&artists, &by_artist, seed, PER_ARTIST) {
                    if consider!(id) {
                        break;
                    }
                }
            }
        }
    }

    // 2. Similar recordings (bonus — often empty for a given recording MBID).
    if out.len() < limit {
        if let Ok(Some(mbid)) = database.track_recording_mbid(seed_track_id).await {
            let scored = cached_similar_recordings(database, client, &mbid, now_unix).await;
            if !scored.is_empty() {
                let mbids: Vec<String> = scored.into_iter().map(|s| s.mbid).collect();
                if let Ok(local) = database.tracks_for_recording_mbids(&mbids).await {
                    for id in local {
                        if consider!(id) {
                            break;
                        }
                    }
                }
            }
        }
    }

    // 3. Local content fallback (always available, no network).
    if out.len() < limit {
        let want = ((limit - out.len()) * 4).max(limit) as i64;
        if let Ok(local) = database.similar_local_track_ids(seed_track_id, want).await {
            for id in local {
                if consider!(id) {
                    break;
                }
            }
        }
    }

    // Pull in the held-back frequent-skips only if we still owe tracks to reach `limit`.
    for id in deferred {
        if out.len() >= limit {
            break;
        }
        out.push(id);
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

    #[test]
    fn parses_similar_artists() {
        // The real similar-artists shape (a flat array of {name, artist_mbid, score}).
        let value = serde_json::json!([
            { "name": "Nile Rodgers", "artist_mbid": "c6d571dd", "score": 10614 },
            { "name": "Gorillaz", "artist_mbid": "e21857d5", "score": 8663 }
        ]);
        let parsed = parse_similar_artists(&value);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], ScoredArtist { name: "Nile Rodgers".into(), score: 10614.0 });
        assert_eq!(parsed[1].name, "Gorillaz");
    }

    fn artist(name: &str, score: f64) -> ScoredArtist {
        ScoredArtist { name: name.into(), score }
    }

    fn pool(entries: &[(&str, &[&str])]) -> std::collections::HashMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(name, tracks)| {
                (name.to_lowercase(), tracks.iter().map(|t| t.to_string()).collect())
            })
            .collect()
    }

    #[test]
    fn weighted_order_is_deterministic_for_a_seed() {
        let artists = [artist("A", 100.0), artist("B", 50.0), artist("C", 10.0)];
        let by_artist = pool(&[("A", &["a1", "a2"]), ("B", &["b1"]), ("C", &["c1", "c2"])]);
        let first = weighted_artist_track_order(&artists, &by_artist, 42, 2);
        let again = weighted_artist_track_order(&artists, &by_artist, 42, 2);
        assert_eq!(first, again, "same seed must reproduce the same order");
        // A different seed should (almost surely) reshuffle.
        let other = weighted_artist_track_order(&artists, &by_artist, 7, 2);
        assert_ne!(first, other);
    }

    #[test]
    fn weighted_order_respects_per_artist_cap_and_track_supply() {
        let artists = [artist("A", 100.0), artist("B", 50.0)];
        let by_artist = pool(&[("A", &["a1", "a2", "a3"]), ("B", &["b1"])]);
        let order = weighted_artist_track_order(&artists, &by_artist, 99, 2);
        // A is capped at 2 of its 3 tracks; B contributes its only track. 3 total.
        assert_eq!(order.len(), 3);
        let a_count = order.iter().filter(|t| t.starts_with('a')).count();
        assert_eq!(a_count, 2, "per-artist cap of 2");
        assert!(order.contains(&"b1".to_string()));
    }

    #[test]
    fn weighted_order_skips_artists_with_no_local_tracks() {
        let artists = [artist("A", 100.0), artist("Ghost", 90.0), artist("B", 10.0)];
        let by_artist = pool(&[("A", &["a1"]), ("B", &["b1"])]);
        let order = weighted_artist_track_order(&artists, &by_artist, 5, 2);
        assert_eq!(order.len(), 2);
        assert!(order.iter().all(|t| t == "a1" || t == "b1"));
    }

    #[test]
    fn weighted_order_favors_higher_scores() {
        // One track each so first-pick reflects the artist draw. Over many seeds the
        // far-higher-scored artist should lead the great majority of the time.
        let artists = [artist("High", 1000.0), artist("Low", 1.0)];
        let by_artist = pool(&[("High", &["h"]), ("Low", &["l"])]);
        let high_first = (0..400u64)
            .filter(|&seed| weighted_artist_track_order(&artists, &by_artist, seed, 1).first()
                == Some(&"h".to_string()))
            .count();
        assert!(high_first > 320, "expected High to lead most draws, got {high_first}/400");
    }
}
