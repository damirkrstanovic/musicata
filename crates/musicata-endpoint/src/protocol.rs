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
                Action::Load {
                    stream_url: item.stream_url.clone(),
                    title: item.title.clone(),
                    play: want_playing,
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
        };
        assert_eq!(decide(&view, &state("playing", Some("t1"), "/s")), Action::Nothing);
    }

    #[test]
    fn stops_when_nothing_is_playing() {
        let view = EndpointView {
            track_id: Some("t1".into()),
            stream_url: Some("/s".into()),
            playing: true,
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
