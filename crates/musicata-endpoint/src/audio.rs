//! The audio side: fetch a stream and play it through the default output device with rodio,
//! tracking elapsed time and end-of-track. Single-threaded — owned by the main control loop.

use std::io::{Cursor, Read};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};

pub struct AudioPlayer {
    // The stream handle must stay alive for audio to keep playing; `_stream` owns the device.
    _stream: OutputStream,
    handle: OutputStreamHandle,
    agent: ureq::Agent,
    base_url: String,
    token: String,
    sink: Option<Sink>,
    duration: Option<f64>,
    // Elapsed accounting: time played before the current span, plus the running span.
    play_started: Option<Instant>,
    accumulated: Duration,
    reported_ended: bool,
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
        let agent = ureq::AgentBuilder::new()
            .user_agent(concat!("musicata-endpoint/", env!("CARGO_PKG_VERSION")))
            .build();
        Ok(Self {
            _stream: stream,
            handle,
            agent,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            sink: None,
            duration: None,
            play_started: None,
            accumulated: Duration::ZERO,
            reported_ended: false,
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
        let response = request.call().map_err(|error| anyhow!("fetch stream: {error}"))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .context("read stream body")?;
        Ok(bytes)
    }

    /// Load a stream and start it (or hold it paused when `play` is false).
    pub fn load(&mut self, stream_url: &str, play: bool) -> Result<()> {
        let bytes = self.fetch(stream_url)?;
        let decoder =
            Decoder::new(Cursor::new(bytes)).map_err(|error| anyhow!("decode stream: {error}"))?;
        self.duration = decoder.total_duration().map(|d| d.as_secs_f64());
        let sink = Sink::try_new(&self.handle).map_err(|error| anyhow!("create sink: {error}"))?;
        sink.append(decoder);
        self.accumulated = Duration::ZERO;
        self.reported_ended = false;
        if play {
            sink.play();
            self.play_started = Some(Instant::now());
        } else {
            sink.pause();
            self.play_started = None;
        }
        self.sink = Some(sink);
        Ok(())
    }

    pub fn resume(&mut self) {
        if let Some(sink) = &self.sink {
            sink.play();
            if self.play_started.is_none() {
                self.play_started = Some(Instant::now());
            }
        }
    }

    pub fn pause(&mut self) {
        if let Some(sink) = &self.sink {
            sink.pause();
            if let Some(started) = self.play_started.take() {
                self.accumulated += started.elapsed();
            }
        }
    }

    pub fn stop(&mut self) {
        self.sink = None;
        self.duration = None;
        self.play_started = None;
        self.accumulated = Duration::ZERO;
        self.reported_ended = false;
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

    /// Returns `true` exactly once, when the current track finishes (the sink drains while it
    /// was playing). Drives the `ended` report that advances the server-owned queue.
    pub fn take_ended(&mut self) -> bool {
        if self.reported_ended {
            return false;
        }
        let finished =
            self.play_started.is_some() && self.sink.as_ref().is_some_and(|sink| sink.empty());
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
        let (url, send_token) =
            resolve_request("http://127.0.0.1:3030", "/api/tracks/t1/stream");
        assert_eq!(url, "http://127.0.0.1:3030/api/tracks/t1/stream");
        assert!(send_token);
    }

    #[test]
    fn lookalike_host_does_not_receive_token() {
        // BUG-04: a prefix check sent the token to any URL textually starting with
        // base_url, e.g. an attacker host that merely shares the prefix.
        let (url, send_token) =
            resolve_request("http://127.0.0.1:3030", "http://127.0.0.1:3030.example.com/track");
        assert_eq!(url, "http://127.0.0.1:3030.example.com/track");
        assert!(!send_token, "token must not leak to a look-alike host");
    }

    #[test]
    fn external_stream_does_not_receive_token() {
        let (_, send_token) = resolve_request("http://127.0.0.1:3030", "http://radio.example/s");
        assert!(!send_token);
    }
}
