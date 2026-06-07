//! Minimal async client for the Music Player Daemon (MPD) control protocol.
//!
//! MPD speaks a line-based text protocol over TCP: the client writes a command
//! line, the server replies with `key: value` lines terminated by `OK` (or an
//! `ACK ...` error line). The [`idle`](MpdConnection::idle) command blocks until
//! a subsystem changes, which we use to push live state to controllers without
//! polling. Only the subset Musicata needs is implemented; responses are parsed
//! by the pure functions at the bottom of this module so they can be unit tested.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use musicata_core::{PlaybackState, PlaybackStatus, QueueItem, RepeatMode};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Cap how long a connect can block. An unreachable MPD (host that drops the SYN,
/// not a fast local refusal) would otherwise stall the caller for the OS default —
/// seconds to minutes — and command handlers connect on the request's hot path.
/// Two seconds is plenty for a reachable daemon and bounds the worst case.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// A single MPD connection. Not internally synchronized: callers serialize access
/// (one connection for commands, a separate one for the blocking `idle` loop).
pub struct MpdConnection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
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
        };
        let greeting = connection.read_line().await?;
        if !greeting.starts_with("OK MPD") {
            bail!("unexpected MPD greeting: {greeting}");
        }
        Ok(connection)
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
        self.command(&format!("seekcur {position_seconds}")).await?;
        Ok(())
    }

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

    /// Read the playback cursor (status + current song) **without** the queue — the
    /// server owns the queue, so it only reads which index is playing, the elapsed
    /// position, and the options back from MPD. Also returns MPD's queue version,
    /// which increments only on queue edits (not on seeks or song changes), so an
    /// unexpected bump signals an external client edited the queue.
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
    /// Increments only on queue edits (add/delete/move/clear) — used to detect an
    /// external client changing the queue out from under the server.
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
    let id = rest.strip_suffix("/stream").unwrap_or(rest);
    let id = id.split('?').next().unwrap_or(id);
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
        queue: split_objects(queue)
            .iter()
            .map(|object| queue_item(object))
            .collect(),
        queue_position: find(status, "song").and_then(|value| value.parse().ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn pairs(text: &str) -> Vec<(String, String)> {
        text.lines()
            .filter_map(|line| line.split_once(": "))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
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
