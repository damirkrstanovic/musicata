// SPDX-License-Identifier: AGPL-3.0-or-later
//! Minimal async client for the Music Player Daemon (MPD) control protocol.
//!
//! MPD speaks a line-based text protocol over TCP: the client writes a command
//! line, the server replies with `key: value` lines terminated by `OK` (or an
//! `ACK ...` error line). The [`idle`](MpdConnection::idle) command blocks until
//! a subsystem changes, which we use to push live state to controllers without
//! polling. Only the subset Musicata needs is implemented; responses are parsed
//! by the pure functions at the bottom of this module so they can be unit tested.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use musicata_core::{PlaybackState, PlaybackStatus, QueueItem, RepeatMode};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Cap how long a connect can block. An unreachable MPD (host that drops the SYN,
/// not a fast local refusal) would otherwise stall the caller for the OS default —
/// seconds to minutes — and command handlers connect on the request's hot path.
/// Two seconds is plenty for a reachable daemon and bounds the worst case.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const OUTPUT_PHASE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUTS: usize = 64;

/// One MPD-configured audio output. `id` is only meaningful for this MPD process: callers
/// persist the `(name, plugin)` identity and resolve its current id immediately before use.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct MpdOutput {
    pub id: u32,
    pub name: String,
    pub plugin: String,
    pub enabled: bool,
}

/// A single MPD connection. Not internally synchronized: callers serialize access
/// (one connection for commands, a separate one for the blocking `idle` loop).
pub struct MpdConnection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    addr: String,
    password: Option<String>,
    output_stream_tainted: bool,
}

impl MpdConnection {
    /// Connect and consume the `OK MPD <version>` greeting.
    pub async fn connect(addr: &str) -> Result<Self> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow!("MPD connect to {addr} timed out"))??;
        let (read, writer) = stream.into_split();
        let mut connection = Self {
            reader: BufReader::new(read),
            writer,
            addr: addr.to_string(),
            password: None,
            output_stream_tainted: false,
        };
        let greeting = tokio::time::timeout(CONNECT_TIMEOUT, connection.read_line())
            .await
            .map_err(|_| anyhow!("MPD greeting timed out"))??;
        if !greeting.starts_with("OK MPD") {
            bail!("unexpected MPD greeting: {greeting}");
        }
        Ok(connection)
    }

    /// Connect and authenticate before any status or control command. Authentication failures
    /// intentionally do not expose MPD's echoed command, which can include the password.
    pub async fn connect_with_password(addr: &str, password: Option<&str>) -> Result<Self> {
        if password.is_some_and(|password| password.contains('\r') || password.contains('\n')) {
            bail!("MPD password must not contain a line break");
        }
        let mut connection = Self::connect(addr).await?;
        if let Some(password) = password {
            tokio::time::timeout(
                CONNECT_TIMEOUT,
                connection.command(&format!("password {}", quote_arg(password))),
            )
            .await
            .map_err(|_| anyhow!("MPD authentication failed"))?
            .map_err(|_| anyhow!("MPD authentication failed"))?;
            connection.password = Some(password.to_string());
        }
        Ok(connection)
    }

    /// Verify the complete MPD setup path without retaining the connection. The single outer
    /// deadline covers greeting, optional authentication and status, so a peer that accepts TCP
    /// but never speaks cannot hold a wizard request open.
    pub async fn probe(addr: &str, password: Option<&str>) -> Result<()> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            let mut connection = Self::connect_with_password(addr, password).await?;
            connection.command("status").await?;
            Ok(())
        })
        .await
        .map_err(|_| anyhow!("MPD probe timed out"))?
    }

    /// List configured MPD outputs. This is MPD configuration, not a hardware discovery API.
    pub async fn outputs(&mut self) -> Result<Vec<MpdOutput>> {
        parse_outputs(&self.output_command("outputs").await?)
    }

    /// Select exactly one configured output, identified by its persistent `(name, plugin)` pair.
    ///
    /// MPD assigns output ids at runtime, so this resolves the id from a fresh `outputs` reply
    /// and never asks callers to retain it. All output commands (including recovery) have a
    /// deadline. Queue and transport commands are intentionally never issued here.
    pub async fn select_output(&mut self, name: &str, plugin: &str) -> Result<Vec<MpdOutput>> {
        if self.output_stream_tainted {
            bail!("MPD output connection is unusable after a timed-out command; reconnect first");
        }
        let before = self.outputs().await?;
        ensure_unique_output_identities(&before)?;
        let target = unique_output(&before, name, plugin)?.clone();

        let change = match tokio::time::timeout(OUTPUT_PHASE_TIMEOUT, async {
            // Disable existing outputs before enabling a disabled target. This avoids a period
            // where an old speaker and the requested one are both enabled.
            for output in before
                .iter()
                .filter(|output| output.enabled && output.id != target.id)
            {
                self.set_output_enabled(output.id, false).await?;
            }
            if !target.enabled {
                self.set_output_enabled(target.id, true).await?;
            }
            let after = self.outputs().await?;
            ensure_unique_output_identities(&after)?;
            if !is_only_enabled_output(&after, name, plugin) {
                bail!("MPD output selection verification failed");
            }
            Ok(after)
        })
        .await
        {
            Ok(change) => change,
            Err(_) => {
                self.output_stream_tainted = true;
                Err(anyhow!("MPD output selection timed out"))
            }
        };

        match change {
            Ok(outputs) => Ok(outputs),
            Err(change_error) => match self.rollback_outputs(&before).await {
                Ok(()) => Err(anyhow!(
                    "MPD output selection failed ({change_error}); prior outputs restored"
                )),
                Err(rollback_error) => Err(anyhow!(
                    "MPD output selection failed: {change_error}; rollback failed: {rollback_error}"
                )),
            },
        }
    }

    async fn output_command(&mut self, command: &str) -> Result<Vec<(String, String)>> {
        tokio::time::timeout(CONNECT_TIMEOUT, self.command(command))
            .await
            .map_err(|_| {
                self.output_stream_tainted = true;
                anyhow!("MPD output command timed out")
            })?
    }

    async fn set_output_enabled(&mut self, id: u32, enabled: bool) -> Result<()> {
        let command = if enabled {
            format!("enableoutput {id}")
        } else {
            format!("disableoutput {id}")
        };
        self.output_command(&command).await?;
        Ok(())
    }

    /// Restore exactly the enabled set observed before this operation. Fetching first resolves
    /// current ids rather than trusting a persisted id after an MPD reload.
    async fn restore_outputs(&mut self, before: &[MpdOutput]) -> Result<()> {
        let current = self.outputs().await?;
        ensure_unique_output_identities(&current)?;
        for output in &current {
            let wanted = output_enabled(before, output)?;
            if output.enabled && !wanted {
                self.set_output_enabled(output.id, false).await?;
            }
        }
        for output in &current {
            let wanted = output_enabled(before, output)?;
            if !output.enabled && wanted {
                self.set_output_enabled(output.id, true).await?;
            }
        }
        let restored = self.outputs().await?;
        ensure_unique_output_identities(&restored)?;
        if !same_enabled_outputs(before, &restored)? {
            bail!("MPD output rollback verification failed");
        }
        Ok(())
    }

    /// A timed-out MPD command can still produce an ACK/OK after its caller has given up.
    /// Never read that uncertain framing while restoring speakers: reconnect, authenticate, and
    /// resolve the output identities again. The caller receives an explicit unverified rollback
    /// error if that fresh recovery cannot complete.
    async fn rollback_outputs(&mut self, before: &[MpdOutput]) -> Result<()> {
        let rollback = async {
            if self.output_stream_tainted {
                let mut recovery =
                    Self::connect_with_password(&self.addr, self.password.as_deref())
                        .await
                        .context("fresh MPD recovery connection failed")?;
                recovery.restore_outputs(before).await
            } else {
                self.restore_outputs(before).await
            }
        };
        tokio::time::timeout(OUTPUT_PHASE_TIMEOUT, rollback)
            .await
            .map_err(|_| anyhow!("MPD output rollback unverified: timed out"))?
            .map_err(|error| anyhow!("MPD output rollback unverified: {error}"))
    }

    async fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).await?;
        if read == 0 {
            bail!("MPD connection closed");
        }
        Ok(line.trim_end().to_string())
    }

    /// Send one command and collect its `key: value` response pairs (order
    /// preserved), returning an error if MPD replies with `ACK`.
    pub async fn command(&mut self, command: &str) -> Result<Vec<(String, String)>> {
        // Write the command and its newline in one call so the request reaches the
        // server as a single line rather than two fragments.
        self.writer
            .write_all(format!("{command}\n").as_bytes())
            .await?;
        self.writer.flush().await?;
        self.read_response().await
    }

    async fn read_response(&mut self) -> Result<Vec<(String, String)>> {
        let mut pairs = Vec::new();
        loop {
            let line = self.read_line().await?;
            if line == "OK" {
                return Ok(pairs);
            }
            if let Some(error) = line.strip_prefix("ACK") {
                bail!("MPD error:{error}");
            }
            if let Some((key, value)) = line.split_once(": ") {
                pairs.push((key.to_string(), value.to_string()));
            }
        }
    }

    /// Block until one of the watched subsystems changes, returning their names.
    pub async fn idle(&mut self) -> Result<Vec<String>> {
        let pairs = self.command("idle player mixer playlist options").await?;
        Ok(pairs
            .into_iter()
            .filter(|(key, _)| key == "changed")
            .map(|(_, value)| value)
            .collect())
    }

    /// Fetch the current player state (`status` + `currentsong` + queue).
    pub async fn playback_state(&mut self) -> Result<PlaybackState> {
        let status = self.command("status").await?;
        let current = self.command("currentsong").await?;
        let queue = self.command("playlistinfo").await?;
        Ok(parse_playback_state(&status, &current, &queue))
    }

    pub async fn play(&mut self) -> Result<()> {
        self.command("play").await?;
        Ok(())
    }

    pub async fn play_index(&mut self, index: usize) -> Result<()> {
        self.command(&format!("play {index}")).await?;
        Ok(())
    }

    pub async fn pause(&mut self) -> Result<()> {
        self.command("pause 1").await?;
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        self.command("stop").await?;
        Ok(())
    }

    pub async fn next(&mut self) -> Result<()> {
        self.command("next").await?;
        Ok(())
    }

    pub async fn previous(&mut self) -> Result<()> {
        self.command("previous").await?;
        Ok(())
    }

    pub async fn seek(&mut self, position_seconds: f64) -> Result<()> {
        self.command(&format!("seekcur {}", seekcur_arg(position_seconds)))
            .await?;
        Ok(())
    }
}

/// Format an *absolute* `seekcur` argument. A leading `+`/`-` makes MPD seek relatively,
/// and `NaN`/`inf` would draw an `ACK`, so clamp to a finite, non-negative value.
fn seekcur_arg(position_seconds: f64) -> String {
    let seconds = if position_seconds.is_finite() {
        position_seconds.max(0.0)
    } else {
        0.0
    };
    format!("{seconds}")
}

fn parse_outputs(pairs: &[(String, String)]) -> Result<Vec<MpdOutput>> {
    let mut outputs = Vec::new();
    let mut current: Option<MpdOutput> = None;
    for (key, value) in pairs {
        match key.as_str() {
            "outputid" => {
                if let Some(output) = current.take() {
                    outputs.push(output);
                    if outputs.len() > MAX_OUTPUTS {
                        bail!("MPD reported too many outputs");
                    }
                }
                current = Some(MpdOutput {
                    id: value.parse().context("invalid MPD output id")?,
                    name: String::new(),
                    plugin: String::new(),
                    enabled: false,
                });
            }
            "outputname" => {
                current
                    .as_mut()
                    .ok_or_else(|| anyhow!("MPD output name without output id"))?
                    .name = value.clone()
            }
            "plugin" => {
                current
                    .as_mut()
                    .ok_or_else(|| anyhow!("MPD output plugin without output id"))?
                    .plugin = value.clone()
            }
            "outputenabled" => {
                let output = current
                    .as_mut()
                    .ok_or_else(|| anyhow!("MPD output enabled state without output id"))?;
                output.enabled = match value.as_str() {
                    "0" => false,
                    "1" => true,
                    _ => bail!("invalid MPD output enabled state"),
                };
            }
            _ => {}
        }
    }
    if let Some(output) = current {
        outputs.push(output);
    }
    if outputs.len() > MAX_OUTPUTS {
        bail!("MPD reported too many outputs");
    }
    if outputs
        .iter()
        .any(|output| output.name.is_empty() || output.plugin.is_empty())
    {
        bail!("incomplete MPD output response");
    }
    Ok(outputs)
}

fn unique_output<'a>(outputs: &'a [MpdOutput], name: &str, plugin: &str) -> Result<&'a MpdOutput> {
    let mut matched = outputs
        .iter()
        .filter(|output| output.name == name && output.plugin == plugin);
    let Some(output) = matched.next() else {
        bail!("MPD output {name:?} ({plugin}) is not configured");
    };
    if matched.next().is_some() {
        bail!("MPD output {name:?} ({plugin}) is ambiguous");
    }
    Ok(output)
}

fn ensure_unique_output_identities(outputs: &[MpdOutput]) -> Result<()> {
    for (index, output) in outputs.iter().enumerate() {
        if outputs[..index]
            .iter()
            .any(|other| other.name == output.name && other.plugin == output.plugin)
        {
            bail!(
                "MPD output {:?} ({}) is ambiguous",
                output.name,
                output.plugin
            );
        }
    }
    Ok(())
}

fn is_only_enabled_output(outputs: &[MpdOutput], name: &str, plugin: &str) -> bool {
    outputs
        .iter()
        .filter(|output| output.enabled)
        .all(|output| output.name == name && output.plugin == plugin)
        && outputs
            .iter()
            .find(|output| output.name == name && output.plugin == plugin)
            .is_some_and(|output| output.enabled)
}

fn output_enabled(before: &[MpdOutput], output: &MpdOutput) -> Result<bool> {
    Ok(unique_output(before, &output.name, &output.plugin)?.enabled)
}

fn same_enabled_outputs(before: &[MpdOutput], after: &[MpdOutput]) -> Result<bool> {
    if before.len() != after.len() {
        return Ok(false);
    }
    for output in after {
        if output_enabled(before, output)? != output.enabled {
            return Ok(false);
        }
    }
    Ok(true)
}

impl MpdConnection {
    pub async fn set_volume(&mut self, volume: u8) -> Result<()> {
        self.command(&format!("setvol {}", volume.min(100))).await?;
        Ok(())
    }

    pub async fn set_repeat(&mut self, mode: RepeatMode) -> Result<()> {
        let (repeat, single) = match mode {
            RepeatMode::Off => (0, 0),
            RepeatMode::All => (1, 0),
            RepeatMode::One => (1, 1),
        };
        self.command(&format!("repeat {repeat}")).await?;
        self.command(&format!("single {single}")).await?;
        Ok(())
    }

    pub async fn set_shuffle(&mut self, enabled: bool) -> Result<()> {
        self.command(&format!("random {}", u8::from(enabled)))
            .await?;
        Ok(())
    }

    pub async fn clear(&mut self) -> Result<()> {
        self.command("clear").await?;
        Ok(())
    }

    pub async fn add(&mut self, uri: &str) -> Result<()> {
        self.command(&format!("add {}", quote_arg(uri))).await?;
        Ok(())
    }

    /// Add a queue item with MPD's highest priority so it wins over random playback too.
    pub async fn add_next(&mut self, uri: &str) -> Result<()> {
        let response = self.command(&format!("addid {}", quote_arg(uri))).await?;
        let id = find(&response, "Id").context("MPD addid did not return an id")?;
        self.command(&format!("prioid 255 {id}")).await?;
        Ok(())
    }

    pub async fn delete_index(&mut self, index: usize) -> Result<()> {
        self.command(&format!("delete {index}")).await?;
        Ok(())
    }

    pub async fn move_item(&mut self, from: usize, to: usize) -> Result<()> {
        self.command(&format!("move {from} {to}")).await?;
        Ok(())
    }

    /// Replace the queue with `uris` and start playing from the first.
    pub async fn replace_queue(&mut self, uris: &[String]) -> Result<()> {
        self.clear().await?;
        for uri in uris {
            self.add(uri).await?;
        }
        if !uris.is_empty() {
            self.play_index(0).await?;
        }
        Ok(())
    }

    /// Replace the queue with `uris` but do **not** start playing — used to restore a
    /// persisted queue paused at startup. The saved position is applied on the first
    /// `play` so we never emit an audible blip at boot.
    pub async fn load_queue(&mut self, uris: &[String]) -> Result<()> {
        self.clear().await?;
        for uri in uris {
            self.add(uri).await?;
        }
        Ok(())
    }

    /// Queue identity and order only; tags can change without the URLs changing.
    pub async fn queue_uris(&mut self) -> Result<Vec<String>> {
        Ok(self
            .command("playlistinfo")
            .await?
            .into_iter()
            .filter_map(|(key, value)| (key == "file").then_some(value))
            .collect())
    }

    /// Read the playback cursor (status + current song) **without** the queue — the
    /// server owns the queue, so it only reads which index is playing, the elapsed
    /// position, and the options back from MPD. Also returns MPD's queue version,
    /// which increments on content edits and stream metadata updates. Compare queue
    /// URLs before treating a version bump as an external content/order edit.
    pub async fn read_status(&mut self) -> Result<MpdStatus> {
        let status = self.command("status").await?;
        let current = self.command("currentsong").await?;
        let state = parse_playback_state(&status, &current, &[]);
        let playlist_version = find(&status, "playlist")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        Ok(MpdStatus {
            state,
            playlist_version,
        })
    }
}

/// MPD's playback cursor plus its queue version. `state.queue` is empty — the server
/// owns the queue and reads only the cursor (position/elapsed/options/now-playing).
pub struct MpdStatus {
    pub state: PlaybackState,
    /// Increments on queue edits and metadata updates; not proof of a content change.
    pub playlist_version: u64,
}

/// Quote an MPD command argument: wrap in double quotes, escaping `"` and `\`.
fn quote_arg(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn find<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// Extract the library track id from a Musicata stream URL of the form
/// `.../api/tracks/{id}/stream`, if present.
fn track_id_from_uri(uri: &str) -> Option<String> {
    let rest = uri.split("/api/tracks/").nth(1)?;
    let rest = rest.split('?').next().unwrap_or(rest);
    let id = rest.strip_suffix("/stream").unwrap_or(rest);
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn queue_item(pairs: &[(String, String)]) -> QueueItem {
    let uri = find(pairs, "file").unwrap_or_default().to_string();
    QueueItem {
        track_id: track_id_from_uri(&uri),
        title: find(pairs, "Title").unwrap_or_default().to_string(),
        artist: find(pairs, "Artist").unwrap_or_default().to_string(),
        album: find(pairs, "Album").unwrap_or_default().to_string(),
        stream_url: uri,
        // Filled in from the library during enrichment; MPD has no artwork URL.
        artwork_url: None,
        ..Default::default()
    }
}

/// Split a `playlistinfo` response (repeating objects, each starting at `file:`)
/// into per-song pair groups.
fn split_objects(pairs: &[(String, String)]) -> Vec<Vec<(String, String)>> {
    let mut objects: Vec<Vec<(String, String)>> = Vec::new();
    for pair in pairs {
        if pair.0 == "file" {
            objects.push(Vec::new());
        }
        if let Some(current) = objects.last_mut() {
            current.push(pair.clone());
        }
    }
    objects
}

/// Build a [`PlaybackState`] from parsed `status`, `currentsong`, and
/// `playlistinfo` responses.
fn parse_playback_state(
    status: &[(String, String)],
    current: &[(String, String)],
    queue: &[(String, String)],
) -> PlaybackState {
    let repeat = find(status, "repeat") == Some("1");
    let single = find(status, "single") == Some("1");
    let repeat_mode = match (repeat, single) {
        (true, true) => RepeatMode::One,
        (true, false) => RepeatMode::All,
        _ => RepeatMode::Off,
    };
    let now_playing = if current.is_empty() {
        None
    } else {
        Some(queue_item(current))
    };
    let queue_items: Vec<QueueItem> = split_objects(queue)
        .iter()
        .map(|object| queue_item(object))
        .collect();
    // MPD reports the next song's queue position directly; surface it as the prefetch hint.
    let next_up = find(status, "nextsong")
        .and_then(|value| value.parse::<usize>().ok())
        .and_then(|index| queue_items.get(index).cloned());

    PlaybackState {
        status: match find(status, "state") {
            Some("play") => PlaybackStatus::Playing,
            Some("pause") => PlaybackStatus::Paused,
            _ => PlaybackStatus::Stopped,
        },
        now_playing,
        elapsed_seconds: find(status, "elapsed").and_then(|value| value.parse().ok()),
        duration_seconds: find(status, "duration").and_then(|value| value.parse().ok()),
        // MPD reports volume -1 when no mixer is available.
        volume: find(status, "volume")
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|volume| *volume >= 0)
            .map(|volume| volume as u8),
        repeat: repeat_mode,
        shuffle: find(status, "random") == Some("1"),
        queue: queue_items,
        queue_position: find(status, "song").and_then(|value| value.parse().ok()),
        queue_activity: None,
        next_up,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    fn pairs(text: &str) -> Vec<(String, String)> {
        text.lines()
            .filter_map(|line| line.split_once(": "))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn seekcur_arg_is_absolute_finite_and_non_negative() {
        // ISS-13: a negative arg makes MPD seek *relatively*, and NaN/inf draw an ACK.
        assert_eq!(seekcur_arg(42.5), "42.5");
        assert_eq!(seekcur_arg(-5.0), "0"); // not "-5" (which would be a relative seek)
        assert_eq!(seekcur_arg(f64::NAN), "0");
        assert_eq!(seekcur_arg(f64::INFINITY), "0");
    }

    #[test]
    fn parses_status_current_and_queue() {
        let status = pairs(
            "volume: 80\nrepeat: 1\nsingle: 1\nrandom: 0\nstate: play\nsong: 0\nelapsed: 12.500\nduration: 200.000",
        );
        let current = pairs(
            "file: http://host/api/tracks/track_abc/stream\nTitle: Brzi Vavilon\nArtist: Darkwood Dub\nAlbum: Paramparcad",
        );
        let queue = pairs(
            "file: http://host/api/tracks/track_abc/stream\nTitle: Brzi Vavilon\nArtist: Darkwood Dub\nfile: http://host/api/tracks/track_def/stream\nTitle: Spori Vavilon\nArtist: Darkwood Dub",
        );

        let state = parse_playback_state(&status, &current, &queue);

        assert_eq!(state.status, PlaybackStatus::Playing);
        assert_eq!(state.repeat, RepeatMode::One);
        assert!(!state.shuffle);
        assert_eq!(state.volume, Some(80));
        assert_eq!(state.elapsed_seconds, Some(12.5));
        assert_eq!(state.duration_seconds, Some(200.0));
        assert_eq!(state.queue_position, Some(0));
        let now = state.now_playing.expect("now playing");
        assert_eq!(now.title, "Brzi Vavilon");
        assert_eq!(now.track_id.as_deref(), Some("track_abc"));
        assert_eq!(state.queue.len(), 2);
        assert_eq!(state.queue[1].track_id.as_deref(), Some("track_def"));
    }

    #[test]
    fn missing_mixer_yields_no_volume() {
        let status = pairs("volume: -1\nstate: stop");
        let state = parse_playback_state(&status, &[], &[]);
        assert_eq!(state.volume, None);
        assert_eq!(state.status, PlaybackStatus::Stopped);
        assert!(state.now_playing.is_none());
    }

    #[test]
    fn quotes_arguments() {
        assert_eq!(quote_arg("http://h/a/b"), "\"http://h/a/b\"");
        assert_eq!(quote_arg("a \"b\" \\c"), "\"a \\\"b\\\" \\\\c\"");
    }

    #[test]
    fn extracts_track_id_from_stream_url() {
        assert_eq!(
            track_id_from_uri("http://host:3030/api/tracks/track_9/stream"),
            Some("track_9".to_string())
        );
        assert_eq!(track_id_from_uri("file:///music/song.mp3"), None);
    }

    #[test]
    fn extracts_track_id_from_authenticated_stream_url() {
        assert_eq!(
            track_id_from_uri("http://host:3030/api/tracks/track_9/stream?token=secret"),
            Some("track_9".to_string())
        );
    }

    /// A tiny fake MPD server that replays canned responses, used to exercise the
    /// real TCP connect/command/parse path offline.
    async fn fake_mpd(script: Vec<(&'static str, &'static str)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"OK MPD 0.23.0\n").await.unwrap();
            let mut buffer = vec![0_u8; 1024];
            let mut pending = String::new();
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                pending.push_str(&String::from_utf8_lossy(&buffer[..read]));
                // Process only complete (newline-terminated) command lines.
                while let Some(newline) = pending.find('\n') {
                    let command: String = pending.drain(..=newline).collect();
                    let command = command.trim_end();
                    let reply = script
                        .iter()
                        .find(|(name, _)| command == *name)
                        .map(|(_, reply)| *reply)
                        .unwrap_or("");
                    socket.write_all(reply.as_bytes()).await.unwrap();
                    socket.write_all(b"OK\n").await.unwrap();
                }
            }
        });
        addr
    }

    /// A stricter scripted server: every received command must match the next expected command.
    /// The recorded sequence also proves output selection never touches queue or transport state.
    async fn scripted_mpd(
        script: Vec<(&'static str, &'static str)>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let recorded = commands.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"OK MPD 0.23.0\n").await.unwrap();
            let mut script: VecDeque<_> = script.into();
            let mut buffer = vec![0_u8; 1024];
            let mut pending = String::new();
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                pending.push_str(&String::from_utf8_lossy(&buffer[..read]));
                while let Some(newline) = pending.find('\n') {
                    let command: String = pending.drain(..=newline).collect();
                    let command = command.trim_end().to_string();
                    recorded.lock().await.push(command.clone());
                    let Some((expected, reply)) = script.pop_front() else {
                        socket
                            .write_all(b"ACK [5@0] {} unexpected command\n")
                            .await
                            .unwrap();
                        continue;
                    };
                    assert_eq!(command, expected, "unexpected MPD command");
                    socket.write_all(reply.as_bytes()).await.unwrap();
                    if !reply.starts_with("ACK") {
                        socket.write_all(b"OK\n").await.unwrap();
                    }
                }
            }
        });
        (addr, commands)
    }

    /// Serve one script per TCP connection. `<silent>` simulates a peer which accepts a command
    /// but never frames a reply, so recovery must open a new connection rather than consume a
    /// delayed ACK/OK from the first one.
    async fn scripted_mpd_connections(
        scripts: Vec<Vec<(&'static str, &'static str)>>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let commands = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn({
            let commands = commands.clone();
            async move {
                for script in scripts {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let commands = commands.clone();
                    tokio::spawn(async move {
                        socket.write_all(b"OK MPD 0.23.0\n").await.unwrap();
                        let mut script: VecDeque<_> = script.into();
                        let mut buffer = vec![0_u8; 1024];
                        let mut pending = String::new();
                        loop {
                            let read = socket.read(&mut buffer).await.unwrap();
                            if read == 0 {
                                break;
                            }
                            pending.push_str(&String::from_utf8_lossy(&buffer[..read]));
                            while let Some(newline) = pending.find('\n') {
                                let command: String = pending.drain(..=newline).collect();
                                let command = command.trim_end().to_string();
                                commands.lock().await.push(command.clone());
                                let (expected, reply) = script.pop_front().expect("script command");
                                assert_eq!(command, expected, "unexpected MPD command");
                                if reply == "<silent>" {
                                    tokio::time::sleep(CONNECT_TIMEOUT + Duration::from_secs(1))
                                        .await;
                                } else {
                                    socket.write_all(reply.as_bytes()).await.unwrap();
                                    if !reply.starts_with("ACK") {
                                        socket.write_all(b"OK\n").await.unwrap();
                                    }
                                }
                            }
                        }
                    });
                }
            }
        });
        (addr, commands)
    }

    const INITIAL_OUTPUTS: &str = "outputid: 1\noutputname: Living room\nplugin: alsa\noutputenabled: 1\noutputid: 2\noutputname: Kitchen\nplugin: pulse\noutputenabled: 0\n";
    const KITCHEN_ONLY_RENUMBERED: &str = "outputid: 19\noutputname: Living room\nplugin: alsa\noutputenabled: 0\noutputid: 20\noutputname: Kitchen\nplugin: pulse\noutputenabled: 1\n";
    const NOTHING_ENABLED: &str = "outputid: 1\noutputname: Living room\nplugin: alsa\noutputenabled: 0\noutputid: 2\noutputname: Kitchen\nplugin: pulse\noutputenabled: 0\n";
    const NOTHING_ENABLED_RENUMBERED: &str = "outputid: 19\noutputname: Living room\nplugin: alsa\noutputenabled: 0\noutputid: 20\noutputname: Kitchen\nplugin: pulse\noutputenabled: 0\n";
    const INITIAL_OUTPUTS_RENUMBERED: &str = "outputid: 19\noutputname: Living room\nplugin: alsa\noutputenabled: 1\noutputid: 20\noutputname: Kitchen\nplugin: pulse\noutputenabled: 0\n";

    #[tokio::test]
    async fn select_output_disables_other_outputs_and_rechecks_the_current_ids() {
        let (addr, commands) = scripted_mpd(vec![
            ("outputs", INITIAL_OUTPUTS),
            ("disableoutput 1", ""),
            ("enableoutput 2", ""),
            ("outputs", KITCHEN_ONLY_RENUMBERED),
        ])
        .await;
        let mut connection = MpdConnection::connect(&addr).await.unwrap();

        let outputs = connection.select_output("Kitchen", "pulse").await.unwrap();

        assert_eq!(outputs.iter().filter(|output| output.enabled).count(), 1);
        assert_eq!(outputs.iter().find(|output| output.enabled).unwrap().id, 20);
        assert_eq!(
            *commands.lock().await,
            vec![
                "outputs".to_string(),
                "disableoutput 1".to_string(),
                "enableoutput 2".to_string(),
                "outputs".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn select_output_rejects_an_ambiguous_name_and_plugin_without_changing_outputs() {
        let duplicate = "outputid: 1\noutputname: Kitchen\nplugin: alsa\noutputenabled: 1\noutputid: 2\noutputname: Kitchen\nplugin: alsa\noutputenabled: 0\n";
        let (addr, commands) = scripted_mpd(vec![("outputs", duplicate)]).await;
        let mut connection = MpdConnection::connect(&addr).await.unwrap();

        let error = connection
            .select_output("Kitchen", "alsa")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("ambiguous"));
        assert_eq!(*commands.lock().await, vec!["outputs".to_string()]);
    }

    #[tokio::test]
    async fn select_output_rejects_an_unknown_output_without_changing_outputs() {
        let (addr, commands) = scripted_mpd(vec![("outputs", INITIAL_OUTPUTS)]).await;
        let mut connection = MpdConnection::connect(&addr).await.unwrap();

        let error = connection
            .select_output("Office", "alsa")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("not configured"));
        assert_eq!(*commands.lock().await, vec!["outputs".to_string()]);
    }

    #[tokio::test]
    async fn select_output_rolls_back_after_a_partial_apply_failure_with_renumbered_ids() {
        let (addr, commands) = scripted_mpd(vec![
            ("outputs", INITIAL_OUTPUTS),
            ("disableoutput 1", ""),
            ("enableoutput 2", "ACK [5@0] {enableoutput} denied\n"),
            ("outputs", NOTHING_ENABLED_RENUMBERED),
            ("enableoutput 19", ""),
            ("outputs", INITIAL_OUTPUTS_RENUMBERED),
        ])
        .await;
        let mut connection = MpdConnection::connect(&addr).await.unwrap();

        let error = connection
            .select_output("Kitchen", "pulse")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("enableoutput"));
        assert_eq!(
            *commands.lock().await,
            vec![
                "outputs".to_string(),
                "disableoutput 1".to_string(),
                "enableoutput 2".to_string(),
                "outputs".to_string(),
                "enableoutput 19".to_string(),
                "outputs".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn select_output_rolls_back_when_readback_does_not_select_only_the_target() {
        let (addr, commands) = scripted_mpd(vec![
            ("outputs", INITIAL_OUTPUTS),
            ("disableoutput 1", ""),
            ("enableoutput 2", ""),
            ("outputs", NOTHING_ENABLED),
            ("outputs", NOTHING_ENABLED),
            ("enableoutput 1", ""),
            ("outputs", INITIAL_OUTPUTS),
        ])
        .await;
        let mut connection = MpdConnection::connect(&addr).await.unwrap();

        let error = connection
            .select_output("Kitchen", "pulse")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("verification"));
        assert_eq!(
            *commands.lock().await,
            vec![
                "outputs".to_string(),
                "disableoutput 1".to_string(),
                "enableoutput 2".to_string(),
                "outputs".to_string(),
                "outputs".to_string(),
                "enableoutput 1".to_string(),
                "outputs".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn select_output_reconnects_before_rollback_after_a_silent_command() {
        let (addr, commands) = scripted_mpd_connections(vec![
            vec![
                ("outputs", INITIAL_OUTPUTS),
                ("disableoutput 1", "<silent>"),
            ],
            vec![
                ("outputs", NOTHING_ENABLED),
                ("enableoutput 1", ""),
                ("outputs", INITIAL_OUTPUTS),
            ],
        ])
        .await;
        let mut connection = MpdConnection::connect(&addr).await.unwrap();

        let error = connection
            .select_output("Kitchen", "pulse")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("timed out"));
        assert_eq!(
            *commands.lock().await,
            vec![
                "outputs".to_string(),
                "disableoutput 1".to_string(),
                "outputs".to_string(),
                "enableoutput 1".to_string(),
                "outputs".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn round_trips_commands_over_tcp() {
        let addr = fake_mpd(vec![
            ("clear", ""),
            ("add \"http://host/api/tracks/track_1/stream\"", ""),
            ("play 0", ""),
            (
                "status",
                "volume: 50\nstate: play\nsong: 0\nrepeat: 0\nsingle: 0\nrandom: 0\n",
            ),
            (
                "currentsong",
                "file: http://host/api/tracks/track_1/stream\nTitle: Song\nArtist: Artist\n",
            ),
            (
                "playlistinfo",
                "file: http://host/api/tracks/track_1/stream\nTitle: Song\n",
            ),
        ])
        .await;

        let mut connection = MpdConnection::connect(&addr).await.expect("connect");
        connection
            .replace_queue(&["http://host/api/tracks/track_1/stream".to_string()])
            .await
            .expect("replace queue");
        let state = connection.playback_state().await.expect("state");

        assert_eq!(state.status, PlaybackStatus::Playing);
        assert_eq!(state.volume, Some(50));
        assert_eq!(
            state.now_playing.expect("now playing").track_id.as_deref(),
            Some("track_1")
        );
        assert_eq!(state.queue.len(), 1);
    }

    #[tokio::test]
    async fn load_queue_adds_without_playing() {
        // load_queue must issue clear + add but NOT play (restore stays paused).
        let addr = fake_mpd(vec![
            ("clear", ""),
            ("add \"http://host/api/tracks/track_1/stream\"", ""),
            ("add \"http://host/api/tracks/track_2/stream\"", ""),
        ])
        .await;

        let mut connection = MpdConnection::connect(&addr).await.expect("connect");
        connection
            .load_queue(&[
                "http://host/api/tracks/track_1/stream".to_string(),
                "http://host/api/tracks/track_2/stream".to_string(),
            ])
            .await
            .expect("load queue");
        // No `play`/`play 0` was scripted; the fake server replies OK to anything, so
        // the assertion that matters is exercised by the live test. Here we just
        // confirm the call sequence completes without a play command erroring.
    }

    #[tokio::test]
    async fn add_next_assigns_mpd_priority_for_random_playback() {
        // Physical queue order alone cannot promise "next" when MPD random is enabled.
        // MPD's priority queue is the mechanism that wins over random selection.
        let addr = fake_mpd(vec![
            (
                "addid \"http://host/api/tracks/track_2/stream\"",
                "Id: 42\n",
            ),
            ("prioid 255 42", ""),
        ])
        .await;
        let mut connection = MpdConnection::connect(&addr).await.expect("connect");

        connection
            .add_next("http://host/api/tracks/track_2/stream")
            .await
            .expect("enqueue next");
    }

    #[tokio::test]
    async fn read_status_reports_cursor_and_playlist_version() {
        let addr = fake_mpd(vec![
            (
                "status",
                "volume: 60\nstate: pause\nsong: 2\nelapsed: 5.0\nrepeat: 0\nsingle: 0\nrandom: 0\nplaylist: 7\nplaylistlength: 4\n",
            ),
            (
                "currentsong",
                "file: http://host/api/tracks/track_3/stream\nTitle: Three\n",
            ),
        ])
        .await;

        let mut connection = MpdConnection::connect(&addr).await.expect("connect");
        let status = connection.read_status().await.expect("read status");

        assert_eq!(status.playlist_version, 7);
        assert_eq!(status.state.queue_position, Some(2));
        assert_eq!(status.state.status, PlaybackStatus::Paused);
        // The server owns the queue, so read_status never fetches it.
        assert!(status.state.queue.is_empty());
    }

    #[tokio::test]
    async fn password_authenticates_before_status() {
        let addr = fake_mpd(vec![
            ("password \"secret\"", ""),
            ("status", "state: stop\n"),
            ("currentsong", ""),
        ])
        .await;
        let mut connection = MpdConnection::connect_with_password(&addr, Some("secret"))
            .await
            .expect("authenticated connect");
        connection
            .read_status()
            .await
            .expect("authenticated status");
    }

    #[tokio::test]
    async fn password_failure_does_not_echo_the_secret() {
        let addr = fake_mpd(vec![(
            "password \"secret\"",
            "ACK [3@0] {password \"secret\"} incorrect password\n",
        )])
        .await;
        let error = MpdConnection::connect_with_password(&addr, Some("secret"))
            .await
            .err()
            .expect("authentication must fail");
        assert_eq!(error.to_string(), "MPD authentication failed");
        assert!(!error.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn password_with_line_break_is_rejected_before_connecting() {
        let error = MpdConnection::connect_with_password("127.0.0.1:1", Some("secret\nnext"))
            .await
            .err()
            .expect("line break must be rejected");
        assert_eq!(
            error.to_string(),
            "MPD password must not contain a line break"
        );
        assert!(!error.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn password_response_is_bounded_and_generic_when_mpd_goes_silent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"OK MPD 0.23.0\n").await.unwrap();
            let mut request = [0_u8; 128];
            let _ = socket.read(&mut request).await;
            tokio::time::sleep(CONNECT_TIMEOUT + Duration::from_secs(1)).await;
        });
        let error = MpdConnection::connect_with_password(&addr, Some("secret"))
            .await
            .err()
            .expect("silent authentication must time out");
        assert_eq!(error.to_string(), "MPD authentication failed");
        assert!(!error.to_string().contains("secret"));
    }

    // ---- Live test against a real `mpd` process -------------------------------
    //
    // Ignored by default because it needs the `mpd` binary installed. Run with:
    //   cargo test -p musicata-server -- --ignored live_mpd
    // Override the binary path with MUSICATA_MPD_BIN if mpd is not on PATH.

    use std::io::Write as _;
    use std::path::PathBuf;
    use std::process::{Child, Command};
    use std::time::Duration;

    /// A spawned MPD process that is killed and cleaned up on drop.
    struct LiveMpd {
        child: Child,
        dir: PathBuf,
        port: u16,
    }

    impl Drop for LiveMpd {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A minimal 0.3s silent PCM WAV that MPD can decode natively.
    fn silent_wav() -> Vec<u8> {
        let sample_rate: u32 = 8000;
        let data_len: u32 = sample_rate * 2 / 3; // ~0.3s of 16-bit mono
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend(vec![0_u8; data_len as usize]);
        wav
    }

    /// Serve `body` over HTTP for any request at a Musicata-shaped stream path,
    /// returning the URL MPD should fetch.
    async fn serve_stream(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut scratch = [0_u8; 1024];
                let _ = socket.read(&mut scratch).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            }
        });
        format!("http://{addr}/api/tracks/live_track/stream")
    }

    /// Spawn `mpd` with a null audio output on a free port. Returns `None` if the
    /// binary is unavailable so the test can skip rather than fail.
    fn start_mpd() -> Option<LiveMpd> {
        let binary = std::env::var("MUSICATA_MPD_BIN").unwrap_or_else(|_| "mpd".to_string());
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .ok()?
            .local_addr()
            .ok()?
            .port();
        let dir = std::env::temp_dir().join(format!("musicata-mpd-live-{port}"));
        std::fs::create_dir_all(dir.join("music")).ok()?;
        let conf_path = dir.join("mpd.conf");
        let conf = format!(
            "music_directory \"{music}\"\n\
             db_file \"{base}/db\"\n\
             state_file \"{base}/state\"\n\
             pid_file \"{base}/pid\"\n\
             log_file \"{base}/log\"\n\
             bind_to_address \"127.0.0.1\"\n\
             port \"{port}\"\n\
             audio_output {{\n\ttype \"null\"\n\tname \"null\"\n\tmixer_type \"software\"\n}}\n",
            music = dir.join("music").display(),
            base = dir.display(),
        );
        std::fs::File::create(&conf_path)
            .ok()?
            .write_all(conf.as_bytes())
            .ok()?;
        let child = Command::new(binary)
            .arg("--no-daemon")
            .arg("--stderr")
            .arg(&conf_path)
            .spawn()
            .ok()?;
        Some(LiveMpd { child, dir, port })
    }

    async fn wait_for_mpd(port: u16) -> bool {
        for _ in 0..50 {
            if MpdConnection::connect(&format!("127.0.0.1:{port}"))
                .await
                .is_ok()
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    #[tokio::test]
    #[ignore = "requires a local mpd binary; run with: cargo test -- --ignored"]
    async fn live_mpd_controls_playback() {
        let Some(mpd) = start_mpd() else {
            eprintln!("skipping: `mpd` binary not found (set MUSICATA_MPD_BIN)");
            return;
        };
        assert!(wait_for_mpd(mpd.port).await, "mpd did not start listening");

        let mut connection = MpdConnection::connect(&format!("127.0.0.1:{}", mpd.port))
            .await
            .expect("connect to live mpd");

        // Options and transport round-trip deterministically against real MPD.
        connection.set_volume(42).await.expect("setvol");
        connection
            .set_repeat(RepeatMode::All)
            .await
            .expect("repeat");
        connection.set_shuffle(true).await.expect("random");
        connection.clear().await.expect("clear");

        let state = connection.playback_state().await.expect("state");
        assert_eq!(state.repeat, RepeatMode::All);
        assert!(state.shuffle);
        assert_eq!(state.volume, Some(42));
        assert!(state.queue.is_empty());
        assert_eq!(state.status, PlaybackStatus::Stopped);

        // Enqueue and play a Musicata-shaped stream URL served over HTTP.
        let url = serve_stream(silent_wav()).await;
        connection
            .replace_queue(std::slice::from_ref(&url))
            .await
            .expect("replace queue");

        let state = connection.playback_state().await.expect("state after play");
        assert_eq!(state.queue.len(), 1);
        assert_eq!(state.queue[0].stream_url, url);
        let now = state.now_playing.expect("now playing after play");
        assert_eq!(now.track_id.as_deref(), Some("live_track"));

        // MPD should reach the play state shortly after fetching the stream.
        let mut playing = false;
        for _ in 0..50 {
            if connection.playback_state().await.expect("poll").status == PlaybackStatus::Playing {
                playing = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(playing, "mpd did not reach the playing state");

        // read_status reads the cursor + the queue version without fetching the queue.
        let status = connection.read_status().await.expect("read status");
        assert!(status.state.queue.is_empty(), "read_status omits the queue");
        assert_eq!(status.state.queue_position, Some(0));
        let version_after_play = status.playlist_version;

        // load_queue replaces the queue WITHOUT auto-playing (restore stays paused),
        // and bumps the queue version (so the server can spot external edits).
        connection
            .load_queue(std::slice::from_ref(&url))
            .await
            .expect("load queue");
        let status = connection.read_status().await.expect("read status");
        assert_ne!(
            status.state.status,
            PlaybackStatus::Playing,
            "load_queue must not start playback"
        );
        assert!(
            status.playlist_version > version_after_play,
            "a queue edit bumps MPD's playlist version"
        );
    }
}
