//! The wire contract with the server's player WebSocket, and the pure decision logic that
//! turns an incoming `PlaybackState` into an audio action. Kept free of audio/IO so it can be
//! unit-tested without a sound device.

use serde::Deserialize;

/// The subset of the server's `PlaybackState` the endpoint reacts to. The server sends the full
/// struct; serde ignores the fields we don't model here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlaybackState {
    /// "stopped" | "playing" | "paused" (snake_case, matching the server enum).
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub now_playing: Option<QueueItem>,
    /// The track the server will play next (its prefetch hint). The endpoint loads this ahead
    /// of the boundary so playback is gapless. `None` when the next track can't be predicted.
    #[serde(default)]
    pub next_up: Option<QueueItem>,
}

/// The now-playing item — what to fetch and how to label it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QueueItem {
    #[serde(default)]
    pub track_id: Option<String>,
    #[serde(default)]
    pub title: String,
    /// What the endpoint actually fetches — a library stream URL (relative) or an external one.
    #[serde(default)]
    pub stream_url: String,
}

impl PlaybackState {
    pub fn is_playing(&self) -> bool {
        self.status == "playing"
    }
}

/// What the endpoint's audio player should do in response to a new state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Load this stream and start (or, if `play` is false, hold it paused).
    Load {
        stream_url: String,
        title: String,
        play: bool,
    },
    /// The server cursor moved to a track the endpoint already prefetched and appended — the
    /// audio is already playing it gaplessly, so just adopt it as current (no reload).
    Advance {
        track_id: Option<String>,
        stream_url: String,
        title: String,
        play: bool,
    },
    Resume,
    Pause,
    Stop,
    /// Nothing to do — already in the desired state.
    Nothing,
}

/// What the endpoint is currently doing, fed back into [`decide`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointView {
    /// The currently-loaded track id (when the item had one).
    pub track_id: Option<String>,
    /// The currently-loaded stream URL (the identity for items without a track id, e.g. radio).
    pub stream_url: Option<String>,
    pub playing: bool,
    /// The track id the endpoint has prefetched (and appended) for gapless advance. When the
    /// server cursor reaches this id, the audio is already playing it — adopt, don't reload.
    pub prefetched: Option<String>,
}

/// Decide the audio action for a new server state, given what the endpoint is doing now. Pure.
pub fn decide(view: &EndpointView, next: &PlaybackState) -> Action {
    let want_playing = next.is_playing();
    match &next.now_playing {
        // Nothing to play (or an item with no stream): stop if we were playing something.
        None => {
            if view.track_id.is_some() || view.stream_url.is_some() {
                Action::Stop
            } else {
                Action::Nothing
            }
        }
        Some(item) if item.stream_url.is_empty() => Action::Stop,
        Some(item) => {
            // Identity is the track id when present, else the stream URL (radio/podcast).
            let same = if item.track_id.is_some() {
                item.track_id == view.track_id
            } else {
                view.stream_url.as_deref() == Some(item.stream_url.as_str())
            };
            if !same {
                // Did the cursor land on the track we already prefetched and appended? Then the
                // audio is already playing it gaplessly — adopt it without a reload.
                let advanced = item.track_id.is_some() && item.track_id == view.prefetched;
                if advanced {
                    Action::Advance {
                        track_id: item.track_id.clone(),
                        stream_url: item.stream_url.clone(),
                        title: item.title.clone(),
                        play: want_playing,
                    }
                } else {
                    Action::Load {
                        stream_url: item.stream_url.clone(),
                        title: item.title.clone(),
                        play: want_playing,
                    }
                }
            } else if want_playing && !view.playing {
                Action::Resume
            } else if !want_playing && view.playing {
                Action::Pause
            } else {
                Action::Nothing
            }
        }
    }
}

/// The next library track the endpoint should prefetch for gapless playback, as
/// `(stream_url, track_id)`, or `None` when there's nothing to prefetch. Only **library**
/// tracks — those with a track id and a relative stream URL — are prefetched; radio/external
/// streams (absolute URLs) are never prefetched, so the endpoint's bearer token never leaves
/// the library host (the BUG-04 rule). Returns `None` when the hint is already prefetched.
pub fn prefetch_target(view: &EndpointView, next: &PlaybackState) -> Option<(String, String)> {
    let item = next.next_up.as_ref()?;
    let track_id = item.track_id.as_ref()?;
    if item.stream_url.is_empty()
        || item.stream_url.starts_with("http://")
        || item.stream_url.starts_with("https://")
    {
        return None;
    }
    if view.prefetched.as_deref() == Some(track_id.as_str()) {
        return None;
    }
    Some((item.stream_url.clone(), track_id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(status: &str, track: Option<&str>, url: &str) -> PlaybackState {
        PlaybackState {
            status: status.to_string(),
            now_playing: track.map(|id| QueueItem {
                track_id: Some(id.to_string()),
                stream_url: url.to_string(),
                ..Default::default()
            }),
            next_up: None,
        }
    }

    fn item(track: Option<&str>, url: &str) -> QueueItem {
        QueueItem {
            track_id: track.map(|id| id.to_string()),
            stream_url: url.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn loads_a_new_track_and_plays() {
        let view = EndpointView::default();
        let next = state("playing", Some("t1"), "/api/tracks/t1/stream");
        assert_eq!(
            decide(&view, &next),
            Action::Load {
                stream_url: "/api/tracks/t1/stream".into(),
                title: String::new(),
                play: true,
            }
        );
    }

    #[test]
    fn loads_paused_when_status_is_paused() {
        let next = state("paused", Some("t1"), "/s");
        match decide(&EndpointView::default(), &next) {
            Action::Load { play, .. } => assert!(!play),
            other => panic!("expected Load, got {other:?}"),
        }
    }

    #[test]
    fn pauses_and_resumes_the_same_track() {
        let view = EndpointView {
            track_id: Some("t1".into()),
            stream_url: Some("/s".into()),
            playing: true,
            prefetched: None,
        };
        assert_eq!(decide(&view, &state("paused", Some("t1"), "/s")), Action::Pause);

        let paused = EndpointView { playing: false, ..view.clone() };
        assert_eq!(decide(&paused, &state("playing", Some("t1"), "/s")), Action::Resume);
    }

    #[test]
    fn does_nothing_when_already_in_the_desired_state() {
        let view = EndpointView {
            track_id: Some("t1".into()),
            stream_url: Some("/s".into()),
            playing: true,
            prefetched: None,
        };
        assert_eq!(decide(&view, &state("playing", Some("t1"), "/s")), Action::Nothing);
    }

    #[test]
    fn stops_when_nothing_is_playing() {
        let view = EndpointView {
            track_id: Some("t1".into()),
            stream_url: Some("/s".into()),
            playing: true,
            prefetched: None,
        };
        assert_eq!(decide(&view, &state("stopped", None, "")), Action::Stop);
        assert_eq!(decide(&EndpointView::default(), &state("stopped", None, "")), Action::Nothing);
    }

    #[test]
    fn radio_without_track_id_keys_on_stream_url() {
        // Two radio items (no track id) with different URLs → a load on change.
        let view = EndpointView {
            track_id: None,
            stream_url: Some("http://a/stream".into()),
            playing: true,
            prefetched: None,
        };
        let mut next = state("playing", None, "");
        next.now_playing = Some(QueueItem {
            track_id: None,
            stream_url: "http://b/stream".into(),
            ..Default::default()
        });
        match decide(&view, &next) {
            Action::Load { stream_url, .. } => assert_eq!(stream_url, "http://b/stream"),
            other => panic!("expected Load, got {other:?}"),
        }
    }

    #[test]
    fn prefetches_only_library_next_up() {
        let view = EndpointView::default();
        // A library next_up (track id + relative URL) is the prefetch target.
        let mut next = state("playing", Some("t1"), "/api/tracks/t1/stream");
        next.next_up = Some(item(Some("t2"), "/api/tracks/t2/stream"));
        assert_eq!(
            prefetch_target(&view, &next),
            Some(("/api/tracks/t2/stream".into(), "t2".into()))
        );
        // Radio/external next_up (absolute URL) is never prefetched — token must not leak.
        next.next_up = Some(item(None, "http://radio.example/s"));
        assert_eq!(prefetch_target(&view, &next), None);
        next.next_up = Some(item(Some("t2"), "https://cdn.example/t2"));
        assert_eq!(prefetch_target(&view, &next), None);
        // No hint → nothing to prefetch.
        next.next_up = None;
        assert_eq!(prefetch_target(&view, &next), None);
        // Already prefetched → no repeat.
        next.next_up = Some(item(Some("t2"), "/api/tracks/t2/stream"));
        let prefetched = EndpointView { prefetched: Some("t2".into()), ..view };
        assert_eq!(prefetch_target(&prefetched, &next), None);
    }

    #[test]
    fn advances_to_a_prefetched_track_without_reloading() {
        // Currently playing t1, having prefetched t2; the cursor moves to t2.
        let view = EndpointView {
            track_id: Some("t1".into()),
            stream_url: Some("/api/tracks/t1/stream".into()),
            playing: true,
            prefetched: Some("t2".into()),
        };
        let next = state("playing", Some("t2"), "/api/tracks/t2/stream");
        assert_eq!(
            decide(&view, &next),
            Action::Advance {
                track_id: Some("t2".into()),
                stream_url: "/api/tracks/t2/stream".into(),
                title: String::new(),
                play: true,
            }
        );
        // A cursor move to an *un*prefetched track still reloads.
        let other = state("playing", Some("t3"), "/api/tracks/t3/stream");
        match decide(&view, &other) {
            Action::Load { stream_url, .. } => assert_eq!(stream_url, "/api/tracks/t3/stream"),
            other => panic!("expected Load, got {other:?}"),
        }
    }

    #[test]
    fn ignores_unknown_fields_in_the_state_frame() {
        // The server sends a much larger struct; we must still parse our subset.
        let json = r#"{"status":"playing","now_playing":{"track_id":"t1","title":"Song",
            "artist":"A","album":"Al","stream_url":"/api/tracks/t1/stream","artwork_url":null,
            "integrated_loudness_lufs":-14.0},"volume":80,"queue":[],"repeat":"off"}"#;
        let parsed: PlaybackState = serde_json::from_str(json).expect("parse");
        assert!(parsed.is_playing());
        assert_eq!(parsed.now_playing.unwrap().title, "Song");
    }
}
