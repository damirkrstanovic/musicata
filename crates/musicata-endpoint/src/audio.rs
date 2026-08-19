//! The audio side: fetch a stream and play it through the default output device with rodio,
//! tracking elapsed time and end-of-track. Single-threaded — owned by the main control loop.

use std::io::{Cursor, Read};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use rodio::{Decoder, OutputStream, Sink, Source};

/// A decoded next track, fetched ahead of the boundary but not yet handed to the sink.
struct Pending {
    track_id: String,
    source: Decoder<Cursor<Vec<u8>>>,
    duration: Option<f64>,
}

/// The next track, already appended to the sink and waiting for the current one to drain.
struct Appended {
    track_id: String,
    duration: Option<f64>,
}

pub struct AudioPlayer {
    // The stream handle must stay alive for audio to keep playing; `_stream` owns the device.
    _stream: OutputStream,
    // One long-lived sink for the player's lifetime: appending the next decoded track lets
    // rodio play it back-to-back with the current one (gapless). A per-track sink would gap.
    sink: Sink,
    agent: ureq::Agent,
    base_url: String,
    token: String,
    loaded: bool,
    duration: Option<f64>,
    // Elapsed accounting: time played before the current span, plus the running span.
    play_started: Option<Instant>,
    accumulated: Duration,
    reported_ended: bool,
    // Gapless prefetch: `pending` is decoded but not appended; `appended` is queued in the
    // sink behind the current track. `last_len` tracks the sink's source count so a drop
    // (current track drained) marks the boundary.
    pending: Option<Pending>,
    appended: Option<Appended>,
    last_len: usize,
}

/// Resolve a stream URL against the server base and decide whether the endpoint's
/// Bearer token may be attached. Only library streams — given as relative paths, which
/// we resolve against our own server — are trusted with the token. Absolute URLs
/// (radio/podcast, possibly external) are sent verbatim and never carry the token, so a
/// look-alike host cannot capture it via a shared textual prefix.
fn resolve_request(base_url: &str, stream_url: &str) -> (String, bool) {
    if stream_url.starts_with("http://") || stream_url.starts_with("https://") {
        (stream_url.to_string(), false)
    } else {
        (format!("{base_url}{stream_url}"), true)
    }
}

impl AudioPlayer {
    pub fn new(base_url: String, token: String) -> Result<Self> {
        let (stream, handle) =
            OutputStream::try_default().context("open the default audio output device")?;
        let sink = Sink::try_new(&handle).map_err(|error| anyhow!("create sink: {error}"))?;
        let agent = ureq::AgentBuilder::new()
            .user_agent(concat!("musicata-endpoint/", env!("CARGO_PKG_VERSION")))
            .build();
        Ok(Self {
            _stream: stream,
            sink,
            agent,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            loaded: false,
            duration: None,
            play_started: None,
            accumulated: Duration::ZERO,
            reported_ended: false,
            pending: None,
            appended: None,
            last_len: 0,
        })
    }

    /// Resolve a (relative or absolute) stream URL and download the whole track into memory.
    /// Library streams (under our server) carry the endpoint Bearer token; external streams
    /// (radio/podcast) don't.
    fn fetch(&self, stream_url: &str) -> Result<Vec<u8>> {
        let (url, send_token) = resolve_request(&self.base_url, stream_url);
        let mut request = self.agent.get(&url);
        if send_token {
            request = request.set("Authorization", &format!("Bearer {}", self.token));
        }
        let response = request
            .call()
            .map_err(|error| anyhow!("fetch stream: {error}"))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .context("read stream body")?;
        Ok(bytes)
    }

    /// Fetch and decode a stream into a held [`Decoder`], returning it with its duration.
    fn decode(&self, stream_url: &str) -> Result<(Decoder<Cursor<Vec<u8>>>, Option<f64>)> {
        let bytes = self.fetch(stream_url)?;
        let decoder =
            Decoder::new(Cursor::new(bytes)).map_err(|error| anyhow!("decode stream: {error}"))?;
        let duration = decoder.total_duration().map(|d| d.as_secs_f64());
        Ok((decoder, duration))
    }

    /// Load a stream and start it (or hold it paused when `play` is false). Clears the
    /// persistent sink first, so any prefetched/appended next track is dropped — used for an
    /// explicit (re)load (a new track, a seek), where a gap is acceptable.
    pub fn load(&mut self, stream_url: &str, play: bool) -> Result<()> {
        let (decoder, duration) = self.decode(stream_url)?;
        self.sink.clear(); // stops + empties the queue, leaving the sink paused but reusable
        self.sink.append(decoder);
        self.duration = duration;
        self.loaded = true;
        self.pending = None;
        self.appended = None;
        self.accumulated = Duration::ZERO;
        self.reported_ended = false;
        if play {
            self.sink.play();
            self.play_started = Some(Instant::now());
        } else {
            self.play_started = None;
        }
        self.last_len = self.sink.len();
        Ok(())
    }

    /// Fetch + decode the next track into the pending buffer (no append yet), so the network
    /// cost is paid before the boundary. Only library streams should be passed here (callers
    /// gate on `prefetch_target`); the buffer is dropped cheaply if the queue changes.
    pub fn prefetch(&mut self, stream_url: &str, track_id: &str) -> Result<()> {
        let already = self
            .pending
            .as_ref()
            .is_some_and(|p| p.track_id == track_id)
            || self
                .appended
                .as_ref()
                .is_some_and(|a| a.track_id == track_id);
        if already {
            return Ok(());
        }
        let (source, duration) = self.decode(stream_url)?;
        self.pending = Some(Pending {
            track_id: track_id.to_string(),
            source,
            duration,
        });
        Ok(())
    }

    /// Append the pending track to the sink so rodio plays it back-to-back when the current
    /// track drains (gapless). Called in the final window of the current track.
    pub fn append_pending(&mut self) {
        if self.appended.is_some() {
            return;
        }
        if let Some(pending) = self.pending.take() {
            self.sink.append(pending.source);
            self.appended = Some(Appended {
                track_id: pending.track_id,
                duration: pending.duration,
            });
            self.last_len = self.sink.len();
        }
    }

    pub fn resume(&mut self) {
        if self.loaded {
            self.sink.play();
            if self.play_started.is_none() {
                self.play_started = Some(Instant::now());
            }
        }
    }

    pub fn pause(&mut self) {
        if self.loaded {
            self.sink.pause();
            if let Some(started) = self.play_started.take() {
                self.accumulated += started.elapsed();
            }
        }
    }

    pub fn stop(&mut self) {
        self.sink.clear();
        self.loaded = false;
        self.duration = None;
        self.play_started = None;
        self.accumulated = Duration::ZERO;
        self.reported_ended = false;
        self.pending = None;
        self.appended = None;
        self.last_len = 0;
    }

    /// Whether audio is still flowing — a loaded track with samples left in the sink. Used to
    /// tell a gapless boundary (keep playing) from a true end (the sink drained → stop).
    pub fn is_active(&self) -> bool {
        self.loaded && !self.sink.empty()
    }

    pub fn elapsed_seconds(&self) -> f64 {
        let mut total = self.accumulated;
        if let Some(started) = self.play_started {
            total += started.elapsed();
        }
        total.as_secs_f64()
    }

    pub fn duration_seconds(&self) -> Option<f64> {
        self.duration
    }

    /// Returns `true` exactly once when the current track finishes, driving the `ended` report
    /// that advances the server-owned queue. With a prefetched track appended, the boundary is
    /// a drop in the sink's source count — the appended track is already playing (gapless), so
    /// this promotes it (adopts its duration, resets the elapsed clock) and reports the end.
    /// Without one, it's the plain case: the sink drained while playing.
    pub fn take_ended(&mut self) -> bool {
        if !self.loaded {
            return false;
        }
        let len = self.sink.len();
        if self.appended.is_some() {
            if len < self.last_len {
                let next = self.appended.take().expect("appended present");
                self.duration = next.duration;
                self.accumulated = Duration::ZERO;
                self.play_started = Some(Instant::now());
                self.reported_ended = false;
                self.last_len = len;
                return true;
            }
            self.last_len = len;
            return false;
        }
        self.last_len = len;
        if self.reported_ended {
            return false;
        }
        let finished = self.play_started.is_some() && self.sink.empty();
        if finished {
            self.reported_ended = true;
        }
        finished
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_request;

    #[test]
    fn library_relative_url_resolves_and_carries_token() {
        let (url, send_token) = resolve_request("http://127.0.0.1:3030", "/api/tracks/t1/stream");
        assert_eq!(url, "http://127.0.0.1:3030/api/tracks/t1/stream");
        assert!(send_token);
    }

    #[test]
    fn lookalike_host_does_not_receive_token() {
        // BUG-04: a prefix check sent the token to any URL textually starting with
        // base_url, e.g. an attacker host that merely shares the prefix.
        let (url, send_token) = resolve_request(
            "http://127.0.0.1:3030",
            "http://127.0.0.1:3030.example.com/track",
        );
        assert_eq!(url, "http://127.0.0.1:3030.example.com/track");
        assert!(!send_token, "token must not leak to a look-alike host");
    }

    #[test]
    fn external_stream_does_not_receive_token() {
        let (_, send_token) = resolve_request("http://127.0.0.1:3030", "http://radio.example/s");
        assert!(!send_token);
    }
}
