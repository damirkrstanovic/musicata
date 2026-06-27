//! Optional outbound scrobbling: submit confirmed `played` listens to ListenBrainz.
//!
//! A **decoupled, queue-draining** worker (see AGENTS.md): the per-player recorder enqueues a
//! confirmed listen into `scrobble_queue`; this loop drains it at its own pace, so a network
//! outage or a slow ListenBrainz never stalls playback or history recording. Off by default;
//! enabled with a user token in the `/admin` Settings panel.

use std::time::Duration;

use musicata_core::Track;
use musicata_storage::Database;
use serde_json::{Value, json};
use tokio::sync::watch;

/// Submit confirmed listens to ListenBrainz (default off).
pub const SETTING_SCROBBLE_ENABLED: &str = "scrobble_enabled";
/// The user's ListenBrainz token (from listenbrainz.org/settings). Empty = unset.
pub const SETTING_LISTENBRAINZ_TOKEN: &str = "listenbrainz_token";

const SUBMIT_URL: &str = "https://api.listenbrainz.org/1/submit-listens";
/// Submit at most this many queued listens per pass (ListenBrainz accepts an `import` batch).
const BATCH: i64 = 50;
const TIMEOUT: Duration = Duration::from_secs(15);
/// Idle poll when the queue is empty or scrobbling is off.
const IDLE_POLL: Duration = Duration::from_secs(30);

/// Whether outbound scrobbling is enabled (default **off**, unlike the default-on passes).
pub async fn enabled(database: &Database) -> bool {
    database
        .get_setting(SETTING_SCROBBLE_ENABLED)
        .await
        .ok()
        .flatten()
        .map(|value| value == "true" || value == "1")
        .unwrap_or(false)
}

async fn token(database: &Database) -> String {
    database
        .get_setting(SETTING_LISTENBRAINZ_TOKEN)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Drains the scrobble queue forever, submitting in batches and idling when empty.
pub async fn scrobble_loop(database: Database, mut ready: watch::Receiver<bool>) {
    let _ = ready.wait_for(|&r| r).await;
    loop {
        let did = scrobble_pass(&database).await;
        if did == 0 {
            tokio::time::sleep(IDLE_POLL).await;
        }
    }
}

/// One drain pass. Returns how many queued listens it resolved (submitted or dropped); `0`
/// means nothing to do (or a transient failure to retry after the idle poll).
async fn scrobble_pass(database: &Database) -> usize {
    if !enabled(database).await {
        return 0;
    }
    let token = token(database).await;
    if token.is_empty() {
        return 0;
    }
    let pending = match database.pending_scrobbles(BATCH).await {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(%error, "scrobble: reading the queue failed");
            return 0;
        }
    };
    if pending.is_empty() {
        return 0;
    }

    // Resolve each queued listen to its track metadata; drop rows whose track is gone.
    let mut to_submit: Vec<(i64, Value)> = Vec::new();
    let mut dropped = 0usize;
    for (id, track_id, listened_at) in &pending {
        match database.track(track_id).await {
            Ok(Some(track)) => to_submit.push((*id, listen_payload(&track, *listened_at))),
            Ok(None) => {
                let _ = database.delete_scrobble(*id).await;
                dropped += 1;
            }
            Err(error) => tracing::warn!(%track_id, %error, "scrobble: track lookup failed"),
        }
    }
    if to_submit.is_empty() {
        return dropped;
    }

    let body = json!({
        "listen_type": "import",
        "payload": to_submit.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>(),
    });
    let ids: Vec<i64> = to_submit.iter().map(|(id, _)| *id).collect();
    let count = ids.len();

    match tokio::task::spawn_blocking(move || submit_listens(&token, &body)).await {
        Ok(Ok(())) => {
            for id in ids {
                let _ = database.delete_scrobble(id).await;
            }
            tracing::info!(count, "scrobbled to ListenBrainz");
            dropped + count
        }
        Ok(Err(SubmitError::Retryable(error))) => {
            tracing::warn!(%error, "scrobble: submit failed; will retry");
            0 // leave this batch queued and back off via the idle poll
        }
        Ok(Err(SubmitError::Permanent(error))) => {
            // The batch will never be accepted (e.g. a malformed listen or a bad token).
            // Drop it so it can't wedge the queue and block every later listen behind it.
            tracing::warn!(%error, count, "scrobble: submit permanently rejected; dropping batch");
            for id in ids {
                let _ = database.delete_scrobble(id).await;
            }
            dropped + count
        }
        Err(error) => {
            tracing::warn!(%error, "scrobble: submit task panicked");
            0
        }
    }
}

/// One ListenBrainz `payload` entry for a played track.
fn listen_payload(track: &Track, listened_at: i64) -> Value {
    listen_payload_fields(
        &track.artist_name,
        &track.title,
        &track.album_title,
        listened_at,
    )
}

/// The payload shape, taking primitives so it's unit-testable without a full `Track`.
fn listen_payload_fields(artist: &str, title: &str, album: &str, listened_at: i64) -> Value {
    let mut metadata = json!({
        "artist_name": artist,
        "track_name": title,
        "additional_info": { "submission_client": "Musicata", "media_player": "Musicata" },
    });
    if !album.is_empty() {
        metadata["release_name"] = json!(album);
    }
    json!({ "listened_at": listened_at, "track_metadata": metadata })
}

/// Whether an HTTP status from ListenBrainz is worth retrying. Rate-limit (429) and
/// server errors (5xx) are transient; any other 4xx (a malformed batch, a bad token) is
/// permanent and must not be retried forever.
fn is_retryable_status(status: u16) -> bool {
    status == 429 || status >= 500
}

/// Why a submission failed, so the drain loop can retry transient errors but drop a batch
/// that will never be accepted (which would otherwise wedge the whole queue behind it).
enum SubmitError {
    Retryable(String),
    Permanent(String),
}

/// Sync POST to ListenBrainz (run on `spawn_blocking`). Token auth; any non-2xx is an error.
fn submit_listens(token: &str, body: &Value) -> Result<(), SubmitError> {
    let user_agent = format!(
        "Musicata/{} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    );
    let agent = ureq::AgentBuilder::new()
        .user_agent(&user_agent)
        .timeout(TIMEOUT)
        .build();
    agent
        .post(SUBMIT_URL)
        .set("Authorization", &format!("Token {token}"))
        .send_json(body.clone())
        .map_err(|error| match error {
            ureq::Error::Status(status, response) => {
                let body = response.into_string().unwrap_or_else(|_| "<no body>".into());
                let message = format!("ListenBrainz HTTP {status}: {body}");
                if is_retryable_status(status) {
                    SubmitError::Retryable(message)
                } else {
                    SubmitError::Permanent(message)
                }
            }
            ureq::Error::Transport(error) => SubmitError::Retryable(error.to_string()),
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_has_listenbrainz_shape() {
        let payload = listen_payload_fields("Skunk Anansie", "Weak", "Stoosh", 1_700_000_000);
        assert_eq!(payload["listened_at"], 1_700_000_000);
        let meta = &payload["track_metadata"];
        assert_eq!(meta["artist_name"], "Skunk Anansie");
        assert_eq!(meta["track_name"], "Weak");
        assert_eq!(meta["release_name"], "Stoosh");
        assert_eq!(meta["additional_info"]["submission_client"], "Musicata");
    }

    #[test]
    fn payload_omits_empty_album() {
        let payload = listen_payload_fields("Artist", "Title", "", 42);
        // No release_name key when the album is unknown (ListenBrainz treats it as absent).
        assert!(payload["track_metadata"].get("release_name").is_none());
    }

    #[test]
    fn http_status_retry_classification() {
        // BUG-12: a permanent 4xx (e.g. 400 malformed, 401 bad token) must NOT be retried
        // forever — only rate-limit (429) and server (5xx) errors are retryable.
        assert!(!is_retryable_status(400), "malformed batch is permanent");
        assert!(!is_retryable_status(401), "bad token is permanent");
        assert!(!is_retryable_status(404));
        assert!(is_retryable_status(429), "rate limit is retryable");
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
    }
}
