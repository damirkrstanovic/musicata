// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Musicata — a local-first music server + web controller.
// Copyright (C) 2026 Damir Krstanović
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU Affero General Public License as published by the Free Software Foundation, either
// version 3 of the License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
// without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See the GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License along with this
// program. If not, see <https://www.gnu.org/licenses/>.

//! Musicata native playback endpoint (M10 prototype).
//!
//! A small program a device runs to become a Musicata player: it registers with the server,
//! connects to that player's control WebSocket, and plays whatever the **server-owned queue**
//! tells it to — the same protocol the browser player uses. It holds only a scoped per-player
//! token (not a user account), authenticating its WS channel and the audio streams it fetches.
//!
//! Usage:
//!   First run (bootstrap with your user API token; saved for next time):
//!     musicata-endpoint --server http://host:3030 --register \
//!         --user-token <YOUR_API_TOKEN> --name "Living Room"
//!   Later runs (reuse the saved credentials):
//!     musicata-endpoint --server http://host:3030
//!   Or pass credentials directly:
//!     musicata-endpoint --server http://host:3030 --id <player-id> --token <player-token>
//!
//! Only `ws://` (LAN) is supported; put it behind a TLS reverse proxy for `wss://`.

mod audio;
mod protocol;

use std::io::ErrorKind;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tungstenite::Message;

use crate::audio::AudioPlayer;
use crate::protocol::{Action, EndpointView, PlaybackState, decide, prefetch_target};

/// How long before a track ends to fetch+decode the next one into the buffer — early enough
/// that a slow (SMB) fetch finishes before the boundary, late enough to waste little on skips.
const PREFETCH_LEAD_SECS: f64 = 30.0;
/// How long before a track ends to append the buffered next track to the sink. Kept short so a
/// queue edit before this window simply drops the buffer; only an edit inside it costs a gap.
const APPEND_WINDOW_SECS: f64 = 12.0;

/// Saved endpoint credentials (one player registration).
#[derive(Debug, Serialize, Deserialize)]
struct Creds {
    server: String,
    id: String,
    token: String,
}

fn main() -> Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    let creds = resolve_creds(&args)?;
    eprintln!(
        "musicata-endpoint: player {} on {} — Ctrl-C to quit",
        creds.id, creds.server
    );
    // The audio player and our view of what's playing outlive individual sessions, so a
    // reconnect (server restart, network blip) keeps the current track playing rather than
    // tearing down the output and reloading from the start.
    let mut audio = AudioPlayer::new(creds.server.clone(), creds.token.clone())?;
    let mut view = EndpointView::default();
    // Reconnect forever; a dropped connection just reconnects.
    loop {
        match run_session(&creds, &mut audio, &mut view) {
            Ok(()) => eprintln!("connection closed; reconnecting in 2s…"),
            Err(error) => eprintln!("error: {error}; reconnecting in 2s…"),
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Obtain credentials: register (and save) with a user token, use ones passed inline, or load
/// the saved file.
fn resolve_creds(args: &Args) -> Result<Creds> {
    if args.register {
        let user_token = args
            .user_token
            .as_deref()
            .context("--register needs --user-token (your Musicata API token)")?;
        let name = args
            .name
            .clone()
            .unwrap_or_else(|| "Musicata Endpoint".to_string());
        let address = args.address.clone().unwrap_or_else(|| slug(&name));
        let creds = register(&args.server, user_token, &name, &address)?;
        save_creds(&args.state, &creds)?;
        eprintln!(
            "registered '{name}' as player {} — saved to {}",
            creds.id, args.state
        );
        return Ok(creds);
    }
    if let (Some(id), Some(token)) = (args.id.clone(), args.token.clone()) {
        return Ok(Creds {
            server: args.server.clone(),
            id,
            token,
        });
    }
    load_creds(&args.state).with_context(|| {
        format!(
            "no credentials: pass --register --user-token …, or --id/--token, or a saved {}",
            args.state
        )
    })
}

/// Register this endpoint as a `native` player and get its scoped token (shown once).
fn register(server: &str, user_token: &str, name: &str, address: &str) -> Result<Creds> {
    let url = format!("{}/api/players", server.trim_end_matches('/'));
    let response = ureq::AgentBuilder::new()
        .user_agent(concat!("musicata-endpoint/", env!("CARGO_PKG_VERSION")))
        .build()
        .post(&url)
        .set("Authorization", &format!("Bearer {user_token}"))
        .send_json(serde_json::json!({
            "kind": "native",
            "address": address,
            "name": name,
            "issue_token": true,
        }))
        .map_err(|error| anyhow!("register: {error}"))?;
    let body: serde_json::Value = response.into_json().context("decode registration")?;
    let id = body["id"]
        .as_str()
        .context("registration returned no player id")?;
    let token = body["auth_token"]
        .as_str()
        .context("registration returned no token (issue_token rejected?)")?;
    Ok(Creds {
        server: server.to_string(),
        id: id.to_string(),
        token: token.to_string(),
    })
}

/// One connected session: handshake, then the read/play/report loop until the socket drops.
/// `audio`/`view` are owned by the caller so they survive a reconnect — a transient blip
/// must not stop playback or reload the current track from the start.
fn run_session(creds: &Creds, audio: &mut AudioPlayer, view: &mut EndpointView) -> Result<()> {
    let (tcp_addr, ws_url) = ws_target(&creds.server, &creds.id, &creds.token)?;
    let stream = TcpStream::connect(&tcp_addr).with_context(|| format!("connect {tcp_addr}"))?;
    let (mut socket, _response) = tungstenite::client(ws_url.as_str(), stream)
        .map_err(|error| anyhow!("websocket handshake: {error}"))?;
    // A short read timeout turns the blocking read into a ~2 Hz tick for progress/ended reports.
    socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(500)))
        .context("set read timeout")?;
    eprintln!("connected.");

    let mut last_tick = Instant::now();
    // The library track to prefetch next (from the server's `next_up` hint), if any.
    let mut next_target: Option<(String, String)> = None;

    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let body = text.as_str();
                // The server also echoes `{type:"progress"}` frames; act only on state frames.
                let is_event = serde_json::from_str::<serde_json::Value>(body)
                    .ok()
                    .and_then(|value| value.get("type").cloned())
                    .is_some();
                if !is_event && let Ok(state) = serde_json::from_str::<PlaybackState>(body) {
                    apply(audio, view, &state);
                    next_target = prefetch_target(view, &state);
                }
            }
            Ok(Message::Ping(payload)) => {
                let _ = socket.send(Message::Pong(payload));
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                // Idle tick — fall through to the periodic reports below.
            }
            Err(error) => return Err(anyhow!("websocket read: {error}")),
        }

        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            if view.playing {
                let frame = serde_json::json!({
                    "type": "progress",
                    "elapsed_seconds": audio.elapsed_seconds(),
                    "duration_seconds": audio.duration_seconds(),
                });
                let _ = socket.send(Message::text(frame.to_string()));

                // Gapless prefetch: decode the next track ahead of the boundary, then append it
                // to the sink in the final window so rodio plays it back-to-back (no gap).
                if let Some(duration) = audio.duration_seconds() {
                    let remaining = duration - audio.elapsed_seconds();
                    if remaining <= PREFETCH_LEAD_SECS
                        && let Some((stream_url, track_id)) = &next_target
                    {
                        match audio.prefetch(stream_url, track_id) {
                            Ok(()) => view.prefetched = Some(track_id.clone()),
                            Err(error) => eprintln!("prefetch failed: {error}"),
                        }
                    }
                    if remaining <= APPEND_WINDOW_SECS {
                        audio.append_pending();
                    }
                }
            }
            if audio.take_ended() {
                let _ = socket.send(Message::text(r#"{"type":"ended"}"#.to_string()));
                // A gapless boundary keeps audio flowing (the appended track is now playing);
                // only a true end (the sink drained) stops playback.
                if !audio.is_active() {
                    view.playing = false;
                }
            }
        }
    }
}

/// Apply the decided action to the audio player and update our view of what we're doing.
fn apply(audio: &mut AudioPlayer, view: &mut EndpointView, next: &PlaybackState) {
    match decide(view, next) {
        Action::Load {
            stream_url,
            title,
            play,
        } => {
            if !title.is_empty() {
                eprintln!("▶ {title}");
            }
            if let Err(error) = audio.load(&stream_url, play) {
                eprintln!("load failed: {error}");
            }
            view.track_id = next
                .now_playing
                .as_ref()
                .and_then(|item| item.track_id.clone());
            view.stream_url = Some(stream_url);
            view.prefetched = None;
            view.playing = play;
        }
        Action::Advance {
            track_id,
            stream_url,
            title,
            play,
        } => {
            // The cursor reached the track we already appended — it's playing gaplessly. Adopt
            // it as current; only sync the pause state, since the audio already advanced.
            if !title.is_empty() {
                eprintln!("▶ {title}");
            }
            view.track_id = track_id;
            view.stream_url = Some(stream_url);
            view.prefetched = None;
            view.playing = play;
            if !play {
                audio.pause();
            }
        }
        Action::Resume => {
            audio.resume();
            view.playing = true;
        }
        Action::Pause => {
            audio.pause();
            view.playing = false;
        }
        Action::Stop => {
            audio.stop();
            *view = EndpointView::default();
        }
        Action::Nothing => {}
    }
}

// ---- Pure helpers (unit-tested) ----

/// Derive the TCP target and ws:// URL for a player's control channel from the server URL.
fn ws_target(server: &str, id: &str, token: &str) -> Result<(String, String)> {
    let (scheme, rest) = server
        .split_once("://")
        .ok_or_else(|| anyhow!("--server must be a URL like http://host:3030"))?;
    let ws_scheme = match scheme {
        "http" => "ws",
        "https" | "wss" => {
            return Err(anyhow!(
                "wss is not supported by this prototype — use ws:// on the LAN or a TLS reverse proxy"
            ));
        }
        other => return Err(anyhow!("unsupported scheme: {other}")),
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let tcp_addr = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    let ws_url = format!("{ws_scheme}://{authority}/api/players/{id}/ws?token={token}");
    Ok((tcp_addr, ws_url))
}

/// A filesystem/id-friendly slug from a display name (the default registration address).
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "endpoint".to_string()
    } else {
        s
    }
}

fn save_creds(path: &str, creds: &Creds) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(creds)?)
        .with_context(|| format!("write credentials to {path}"))
}

fn load_creds(path: &str) -> Result<Creds> {
    let text = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

// ---- Arg parsing (hand-rolled `--key value`, matching the server's flag style) ----

struct Args {
    server: String,
    register: bool,
    user_token: Option<String>,
    name: Option<String>,
    address: Option<String>,
    id: Option<String>,
    token: Option<String>,
    state: String,
}

impl Args {
    fn parse(mut argv: impl Iterator<Item = String>) -> Result<Self> {
        // Env vars seed the defaults; CLI flags below override them. This lets a systemd unit
        // (packaging/musicata-endpoint.service) configure the endpoint with `Environment=`
        // lines instead of a long command line.
        let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        let mut server = env("MUSICATA_ENDPOINT_SERVER");
        let mut register = false;
        let mut user_token = env("MUSICATA_ENDPOINT_USER_TOKEN");
        let mut name = env("MUSICATA_ENDPOINT_NAME");
        let mut address = None;
        let mut id = None;
        let mut token = None;
        let mut state =
            env("MUSICATA_ENDPOINT_STATE").unwrap_or_else(|| "musicata-endpoint.json".to_string());
        while let Some(arg) = argv.next() {
            let mut value = || argv.next().ok_or_else(|| anyhow!("{arg} needs a value"));
            match arg.as_str() {
                "--server" => server = Some(value()?),
                "--register" => register = true,
                "--user-token" => user_token = Some(value()?),
                "--name" => name = Some(value()?),
                "--address" => address = Some(value()?),
                "--id" => id = Some(value()?),
                "--token" => token = Some(value()?),
                "--state" => state = value()?,
                "-h" | "--help" => {
                    println!("{}", HELP);
                    std::process::exit(0);
                }
                other => return Err(anyhow!("unknown argument: {other}")),
            }
        }
        Ok(Self {
            server: server.context("--server is required")?,
            register,
            user_token,
            name,
            address,
            id,
            token,
            state,
        })
    }
}

const HELP: &str = "musicata-endpoint — a native Musicata playback endpoint\n\n\
  --server <url>        Musicata server, e.g. http://host:3030 (required)\n\
  --register            register this device as a player (needs --user-token)\n\
  --user-token <tok>    your Musicata API token (for --register)\n\
  --name <name>         display name when registering\n\
  --address <id>        stable address for the registration (default: slug of --name)\n\
  --id <id>             use an existing player id (instead of --register)\n\
  --token <tok>         use an existing player token\n\
  --state <path>        credentials file (default: musicata-endpoint.json)\n\
\n\
Env vars (CLI flags override): MUSICATA_ENDPOINT_SERVER, MUSICATA_ENDPOINT_USER_TOKEN,\n\
  MUSICATA_ENDPOINT_NAME, MUSICATA_ENDPOINT_STATE.\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_target_builds_url_and_addr() {
        let (addr, url) = ws_target("http://host:3030", "p1", "tok").unwrap();
        assert_eq!(addr, "host:3030");
        assert_eq!(url, "ws://host:3030/api/players/p1/ws?token=tok");
    }

    #[test]
    fn ws_target_defaults_port_and_strips_path() {
        let (addr, _) = ws_target("http://musicata.local/ignored", "p", "t").unwrap();
        assert_eq!(addr, "musicata.local:80");
    }

    #[test]
    fn ws_target_rejects_wss() {
        assert!(ws_target("https://host", "p", "t").is_err());
        assert!(ws_target("ftp://host", "p", "t").is_err());
    }

    #[test]
    fn slug_is_id_friendly() {
        assert_eq!(slug("Living Room"), "living-room");
        assert_eq!(slug("  Kitchen!! "), "kitchen");
        assert_eq!(slug("???"), "endpoint");
    }

    #[test]
    fn args_require_server() {
        assert!(Args::parse(["--register"].into_iter().map(String::from)).is_err());
        let args = Args::parse(
            [
                "--server",
                "http://h:3030",
                "--register",
                "--user-token",
                "x",
            ]
            .into_iter()
            .map(String::from),
        )
        .unwrap();
        assert!(args.register);
        assert_eq!(args.server, "http://h:3030");
    }
}
