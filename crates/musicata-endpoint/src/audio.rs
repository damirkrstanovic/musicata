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
        let url = if stream_url.starts_with("http://") || stream_url.starts_with("https://") {
            stream_url.to_string()
        } else {
            format!("{}{}", self.base_url, stream_url)
        };
        let mut request = self.agent.get(&url);
        if url.starts_with(&self.base_url) {
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
