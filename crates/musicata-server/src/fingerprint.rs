//! Audio fingerprinting via Chromaprint + AcoustID — identify a track from its audio
//! and resolve its MusicBrainz ids, so the id-exact artwork providers (Cover Art
//! Archive, fanart.tv) and metadata enrichment work even for untagged files.
//!
//! Pure Rust: `symphonia` decodes the audio, `rusty-chromaprint` computes the
//! fingerprint (resampling internally), and a small `ureq` client queries AcoustID.
//! Everything here is synchronous and CPU-heavy — the caller runs it on `spawn_blocking`.
//! Parsing is a pure function (canned-JSON tested); the base URL and client key are
//! injectable for tests.

use std::io::Cursor;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};
use serde_json::Value;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// AcoustID **application** API key — identifies *Musicata* to the service (like
/// MusicBrainz Picard ships its own). Free to register at <https://acoustid.org>. When
/// empty the fingerprint pass cleanly no-ops (it builds and CI stays green). Do not
/// borrow another project's key.
pub const ACOUSTID_CLIENT_KEY: &str = "Ez6XS6V6Z3";

/// How many seconds of audio to decode for the fingerprint — AcoustID's window (matches
/// `fpcalc -length 120`); decoding the whole track would be wasteful.
const FINGERPRINT_SECONDS: u32 = 120;
const ACOUSTID_TIMEOUT: Duration = Duration::from_secs(15);

/// Compute the Chromaprint fingerprint (AcoustID's base64 form) for an audio file's
/// bytes, plus its duration in whole seconds. Decodes up to [`FINGERPRINT_SECONDS`].
/// `extension` (e.g. `flac`) hints the demuxer. Synchronous + CPU-heavy.
pub fn compute_fingerprint(audio: &[u8], extension: &str) -> Result<(String, u32), String> {
    let (samples, sample_rate, channels) = decode_samples(audio, extension)?;
    if sample_rate == 0 || channels == 0 {
        return Err("decoded audio had no sample rate / channels".to_string());
    }
    let duration = (samples.len() as u32 / sample_rate.max(1) / channels.max(1)).max(1);

    let config = Configuration::preset_test2();
    let mut printer = Fingerprinter::new(&config);
    printer
        .start(sample_rate, channels)
        .map_err(|error| format!("fingerprinter start: {error:?}"))?;
    printer.consume(&samples);
    printer.finish();
    let raw = printer.fingerprint();
    if raw.is_empty() {
        return Err("empty fingerprint".to_string());
    }
    let compressed = FingerprintCompressor::from(&config).compress(raw);
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(compressed);
    Ok((encoded, duration))
}

/// Decode (up to [`FINGERPRINT_SECONDS`] of) an audio file's bytes into interleaved
/// `i16` samples, returning them with the sample rate and channel count.
fn decode_samples(audio: &[u8], extension: &str) -> Result<(Vec<i16>, u32, u32), String> {
    let source = Cursor::new(audio.to_vec());
    let stream = MediaSourceStream::new(Box::new(source), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(extension);

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| format!("probe: {error}"))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "no decodable audio track".to_string())?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|error| format!("decoder: {error}"))?;

    let mut samples: Vec<i16> = Vec::new();
    let mut sample_rate = 0u32;
    let mut channels = 0u32;
    let mut sample_buf: Option<SampleBuffer<i16>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            // End of stream (or an unrecoverable read) — stop with what we have.
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                if sample_buf.is_none() {
                    let spec = *decoded.spec();
                    sample_rate = spec.rate;
                    channels = spec.channels.count() as u32;
                    sample_buf = Some(SampleBuffer::<i16>::new(decoded.capacity() as u64, spec));
                }
                if let Some(buf) = sample_buf.as_mut() {
                    buf.copy_interleaved_ref(decoded);
                    samples.extend_from_slice(buf.samples());
                }
                if sample_rate > 0
                    && channels > 0
                    && samples.len() as u32 / sample_rate / channels >= FINGERPRINT_SECONDS
                {
                    break;
                }
            }
            // A single corrupt packet shouldn't abort the whole decode.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    if samples.is_empty() {
        return Err("no audio decoded".to_string());
    }
    Ok((samples, sample_rate, channels))
}

/// The MusicBrainz ids AcoustID resolved for a track.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AcoustIdMatch {
    pub recording_mbid: Option<String>,
    pub release_mbid: Option<String>,
    pub release_group_mbid: Option<String>,
}

impl AcoustIdMatch {
    fn is_empty(&self) -> bool {
        self.recording_mbid.is_none()
            && self.release_mbid.is_none()
            && self.release_group_mbid.is_none()
    }
}

/// A throttled AcoustID lookup client. No-ops (returns `Ok(None)`) when no application
/// key is configured, so the feature builds and runs without one.
pub struct AcoustIdClient {
    base_url: String,
    client_key: String,
    agent: ureq::Agent,
    /// AcoustID asks for no more than 3 requests/second.
    last_request: Mutex<Option<Instant>>,
}

impl AcoustIdClient {
    pub fn new(client_key: impl Into<String>) -> Self {
        Self::with_base_url("https://api.acoustid.org", client_key)
    }

    pub fn with_base_url(base_url: impl Into<String>, client_key: impl Into<String>) -> Self {
        let user_agent = format!(
            "Musicata/{} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_REPOSITORY")
        );
        let agent = ureq::AgentBuilder::new()
            .user_agent(&user_agent)
            .timeout(ACOUSTID_TIMEOUT)
            .build();
        Self {
            base_url: base_url.into(),
            client_key: client_key.into(),
            agent,
            last_request: Mutex::new(None),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.client_key.is_empty()
    }

    /// Look up the MusicBrainz ids for a fingerprint + duration. `Ok(None)` = no key,
    /// no match, or a transport error treated as "not found" by the caller.
    pub fn lookup(
        &self,
        fingerprint: &str,
        duration: u32,
    ) -> Result<Option<AcoustIdMatch>, String> {
        if !self.is_configured() {
            return Ok(None);
        }
        // Throttle to <= 3 req/s, holding the lock across the request to serialize.
        let mut last = self.last_request.lock().expect("acoustid limiter poisoned");
        if let Some(previous) = *last {
            let min_gap = Duration::from_millis(350);
            let elapsed = previous.elapsed();
            if elapsed < min_gap {
                std::thread::sleep(min_gap - elapsed);
            }
        }
        let url = format!("{}/v2/lookup", self.base_url);
        let value = self
            .agent
            .get(&url)
            .query("client", &self.client_key)
            .query("format", "json")
            .query("meta", "recordingids releaseids releasegroupids")
            .query("duration", &duration.to_string())
            .query("fingerprint", fingerprint)
            .call()
            .map_err(|error| match error {
                ureq::Error::Status(status, response) => {
                    let body = response
                        .into_string()
                        .unwrap_or_else(|_| "<no body>".to_string());
                    format!("AcoustID HTTP {status}: {body}")
                }
                ureq::Error::Transport(error) => error.to_string(),
            })?
            .into_json::<Value>()
            .map_err(|error| error.to_string())?;
        *last = Some(Instant::now());
        Ok(parse_acoustid(&value))
    }
}

/// Pick the top-scoring AcoustID result and pull its recording / release-group / release
/// MusicBrainz ids. `None` when nothing matched.
fn parse_acoustid(value: &Value) -> Option<AcoustIdMatch> {
    if value.get("status").and_then(Value::as_str) != Some("ok") {
        return None;
    }
    let results = value.get("results")?.as_array()?;
    // Highest score first.
    let best = results.iter().max_by(|a, b| {
        let score = |r: &Value| r.get("score").and_then(Value::as_f64).unwrap_or(0.0);
        score(a)
            .partial_cmp(&score(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;

    let recording = best
        .get("recordings")
        .and_then(Value::as_array)
        .and_then(|recordings| recordings.first());
    let recording_mbid = recording
        .and_then(|r| r.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let first_id = |recording: Option<&Value>, key: &str| {
        recording?
            .get(key)?
            .as_array()?
            .first()?
            .get("id")?
            .as_str()
            .map(str::to_string)
    };
    let release_group_mbid = first_id(recording, "releasegroups");
    let release_mbid = first_id(recording, "releases");

    let found = AcoustIdMatch {
        recording_mbid,
        release_mbid,
        release_group_mbid,
    };
    (!found.is_empty()).then_some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short silent PCM WAV (decodable by symphonia's wav/pcm).
    fn silent_wav(seconds: u32) -> Vec<u8> {
        let sample_rate: u32 = 44100;
        let channels: u16 = 2;
        let frames = sample_rate * seconds;
        let data_len = frames * channels as u32 * 2; // 16-bit
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * channels as u32 * 2).to_le_bytes());
        wav.extend_from_slice(&(channels * 2).to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend(vec![0u8; data_len as usize]);
        wav
    }

    #[test]
    fn decodes_and_fingerprints_a_wav() {
        // 10 s is enough for Chromaprint to emit sub-fingerprints.
        let (fingerprint, duration) =
            compute_fingerprint(&silent_wav(10), "wav").expect("fingerprint");
        assert!(!fingerprint.is_empty(), "produced a base64 fingerprint");
        assert!(
            fingerprint
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "url-safe base64"
        );
        assert_eq!(duration, 10);
    }

    #[test]
    fn parses_acoustid_top_match() {
        let value = serde_json::json!({
            "status": "ok",
            "results": [
                { "id": "low", "score": 0.4,
                  "recordings": [{ "id": "wrong-rec" }] },
                { "id": "high", "score": 0.97,
                  "recordings": [{
                      "id": "rec-mbid",
                      "releasegroups": [{ "id": "rg-mbid" }],
                      "releases": [{ "id": "rel-mbid" }]
                  }] }
            ]
        });
        let m = parse_acoustid(&value).expect("match");
        assert_eq!(m.recording_mbid.as_deref(), Some("rec-mbid"));
        assert_eq!(m.release_group_mbid.as_deref(), Some("rg-mbid"));
        assert_eq!(m.release_mbid.as_deref(), Some("rel-mbid"));
    }

    #[test]
    fn parses_acoustid_no_results() {
        let value = serde_json::json!({ "status": "ok", "results": [] });
        assert!(parse_acoustid(&value).is_none());
    }

    // Decode + fingerprint a real MP3 from the repo's fixture library (validates the mp3
    // decoder path, not just synthetic PCM). Ignored: depends on testdata being present.
    #[test]
    #[ignore = "reads testdata; run with: cargo test -- --ignored real_mp3"]
    fn fingerprints_a_real_mp3() {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("../../testdata");
        let mp3 = walk_first_mp3(&path).expect("a testdata .mp3");
        let bytes = std::fs::read(&mp3).expect("read mp3");
        let (fingerprint, duration) = compute_fingerprint(&bytes, "mp3").expect("fingerprint");
        assert!(!fingerprint.is_empty());
        assert!(duration > 0);
        eprintln!(
            "{}: {duration}s, fp[..16]={}",
            mp3.display(),
            &fingerprint[..16.min(fingerprint.len())]
        );
    }

    #[test]
    #[ignore = "live network; run with: cargo test -- --ignored live_acoustid"]
    fn live_acoustid_lookup() {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("../../testdata");
        let mp3 = walk_first_mp3(&path).expect("a testdata .mp3");
        let bytes = std::fs::read(&mp3).expect("read mp3");
        let (fingerprint, duration) = compute_fingerprint(&bytes, "mp3").expect("fingerprint");
        let client = AcoustIdClient::new(ACOUSTID_CLIENT_KEY);
        assert!(client.is_configured(), "compiled-in key should be set");
        let result = client.lookup(&fingerprint, duration).expect("lookup ok");
        eprintln!("{}: {duration}s -> {result:?}", mp3.display());
    }

    #[cfg(test)]
    fn walk_first_mp3(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = walk_first_mp3(&path) {
                    return Some(found);
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("mp3") {
                return Some(path);
            }
        }
        None
    }

    #[test]
    fn unconfigured_client_is_a_noop() {
        let client = AcoustIdClient::new("");
        assert!(!client.is_configured());
        assert_eq!(client.lookup("AQAAAA", 100).expect("noop"), None);
    }
}
