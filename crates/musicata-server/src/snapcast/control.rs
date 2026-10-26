// SPDX-License-Identifier: AGPL-3.0-or-later
//! snapserver JSON-RPC control client (TCP, newline-delimited messages on port 1705).
//!
//! Used to enumerate the connected snapclients (rooms), set their per-output volume, and
//! point every group at the Musicata stream so all clients hear our audio in sync. We open
//! a bounded short-lived connection per call and read until the matching response. The
//! independent health worker observes status; HTTP status reads use its cached snapshot.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// A snapclient as snapserver sees it — one synchronized output (a room/speaker).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SnapClient {
    pub id: String,
    pub name: String,
    pub connected: bool,
    pub volume_percent: u8,
    pub muted: bool,
}

/// A real Snapserver group: its member speakers hear its selected stream together. This is
/// transport membership, deliberately separate from a Musicata playback zone and its queue.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SnapGroup {
    pub id: String,
    pub name: String,
    pub stream_id: String,
    pub clients: Vec<SnapClient>,
}

/// A snapserver stream (the Musicata library stream, or an AirPlay/Spotify input).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SnapStream {
    pub id: String,
    /// snapserver stream status: usually `"playing"` or `"idle"`.
    pub status: String,
    /// "Artist — Title" from the stream's metadata, when it carries any (AirPlay/Spotify do).
    pub now_playing: Option<String>,
}

/// A control client bound to one snapserver's JSON-RPC endpoint (`host:port`).
pub struct SnapControl {
    addr: String,
}

impl SnapControl {
    pub fn new(addr: String) -> Self {
        Self { addr }
    }

    /// Issue one JSON-RPC call and return its `result`, skipping any interleaved
    /// notifications. Every step is deadline-bounded so a wedged snapserver can't block us.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        tokio::time::timeout(Duration::from_secs(6), self.call_inner(method, params))
            .await
            .context("snapcast control request timeout")?
    }

    async fn call_inner(&self, method: &str, params: Value) -> Result<Value> {
        let connect = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&self.addr))
            .await
            .context("snapcast control connect timeout")?
            .with_context(|| format!("connect snapserver control {}", self.addr))?;
        let (read, mut write) = connect.into_split();

        let mut request = json!({ "id": 1, "jsonrpc": "2.0", "method": method });
        if !params.is_null() {
            request["params"] = params;
        }
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        tokio::time::timeout(Duration::from_secs(3), write.write_all(line.as_bytes()))
            .await
            .context("snapcast control write timeout")??;

        let mut reader = BufReader::new(read);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let mut buf = Vec::new();
            let read = tokio::time::timeout_at(
                deadline,
                (&mut reader)
                    .take(1024 * 1024 + 1)
                    .read_until(b'\n', &mut buf),
            )
            .await
            .context("snapcast control read timeout")??;
            if read == 0 {
                return Err(anyhow!("snapserver closed the control connection"));
            }
            if buf.len() > 1024 * 1024 || buf.last() != Some(&b'\n') {
                return Err(anyhow!("invalid snapcast control frame"));
            }
            let message: Value = match serde_json::from_slice(&buf) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if message.get("id") == Some(&json!(1)) {
                if let Some(error) = message.get("error") {
                    return Err(anyhow!("snapserver error: {error}"));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    /// One coherent observation; malformed telemetry is never a healthy empty server.
    pub async fn status(&self) -> Result<SnapStatus> {
        let status = self.call("Server.GetStatus", Value::Null).await?;
        parse_status(status)
    }

    pub async fn groups(&self) -> Result<Vec<SnapGroup>> {
        Ok(self.status().await?.groups)
    }

    /// Set one client's output volume (0–100%), unmuting it.
    pub async fn set_volume(&self, client_id: &str, percent: u8) -> Result<()> {
        self.call(
            "Client.SetVolume",
            json!({ "id": client_id, "volume": { "muted": false, "percent": percent.min(100) } }),
        )
        .await
        .map(|_| ())
    }

    /// The streams snapserver currently has defined (Musicata + any enabled AirPlay/Spotify
    /// inputs), with their status and now-playing metadata — for the input picker.
    pub async fn streams(&self) -> Result<Vec<SnapStream>> {
        Ok(self.status().await?.streams)
    }

    /// Point every group at `stream` so all connected clients play it in sync. A no-op for a
    /// stream snapserver doesn't have (e.g. an input enabled but not yet live after a restart).
    pub async fn assign_all_to_stream(&self, stream: &str) -> Result<()> {
        let status = self.status().await?;
        self.assign_observed_to_stream(&status, stream).await
    }

    pub async fn assign_observed_to_stream(&self, status: &SnapStatus, stream: &str) -> Result<()> {
        if !status.streams.iter().any(|item| item.id == stream) {
            return Ok(());
        }
        for group in &status.groups {
            if group.stream_id != stream {
                self.call(
                    "Group.SetStream",
                    json!({ "id": group.id, "stream_id": stream }),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Replace the member list of one existing Snapserver group.
    pub async fn set_group_clients(&self, group_id: &str, client_ids: &[String]) -> Result<()> {
        self.call(
            "Group.SetClients",
            json!({ "id": group_id, "clients": client_ids }),
        )
        .await
        .map(|_| ())
    }

    /// Route one existing Snapserver group to one existing stream.
    pub async fn set_group_stream(&self, group_id: &str, stream_id: &str) -> Result<()> {
        self.call(
            "Group.SetStream",
            json!({ "id": group_id, "stream_id": stream_id }),
        )
        .await
        .map(|_| ())
    }
}

pub struct SnapStatus {
    pub clients: Vec<SnapClient>,
    pub streams: Vec<SnapStream>,
    pub groups: Vec<SnapGroup>,
    pub server_version: Option<String>,
}

fn parse_status(status: Value) -> Result<SnapStatus> {
    let groups = status
        .pointer("/server/groups")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("invalid snapcast groups observation"))?;
    let streams = status
        .pointer("/server/streams")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("invalid snapcast streams observation"))?;
    let mut clients = Vec::new();
    let mut assignments = Vec::new();
    for group in groups {
        let _id = group
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("invalid snapcast group identity"))?;
        let _stream = group
            .get("stream_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("invalid snapcast group stream"))?;
        assignments.push(parse_group(group));
        for client in group
            .get("clients")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("invalid snapcast clients observation"))?
        {
            if client.get("id").and_then(Value::as_str).is_none()
                || client.get("connected").and_then(Value::as_bool).is_none()
            {
                return Err(anyhow!("invalid snapcast client observation"));
            }
            clients.push(parse_client(client));
        }
    }
    for stream in streams {
        if stream.get("id").and_then(Value::as_str).is_none()
            || stream.get("status").and_then(Value::as_str).is_none()
        {
            return Err(anyhow!("invalid snapcast stream observation"));
        }
    }
    Ok(SnapStatus {
        clients,
        streams: streams.iter().map(parse_stream).collect(),
        groups: assignments,
        server_version: status
            .pointer("/server/server/snapserver/version")
            .and_then(Value::as_str)
            .map(|v| v.chars().take(64).collect()),
    })
}

fn parse_stream(stream: &Value) -> SnapStream {
    // Metadata lives under properties.metadata for active inputs (airplay/librespot).
    let meta = stream.pointer("/properties/metadata");
    let title = meta
        .and_then(|m| m.get("title"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let artist = meta
        .and_then(|m| m.get("artist"))
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .or_else(|| meta.and_then(|m| m.get("artist")).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty());
    let now_playing = match (artist, title) {
        (Some(a), Some(t)) => Some(format!("{a} — {t}")),
        (None, Some(t)) => Some(t.to_string()),
        _ => None,
    };
    SnapStream {
        id: stream
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        status: stream
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        now_playing,
    }
}

fn parse_client(client: &Value) -> SnapClient {
    let name = client
        .pointer("/config/name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| client.pointer("/host/name").and_then(|v| v.as_str()))
        .unwrap_or("client")
        .to_string();
    SnapClient {
        id: client
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        name,
        connected: client
            .get("connected")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        volume_percent: client
            .pointer("/config/volume/percent")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u8,
        muted: client
            .pointer("/config/volume/muted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

fn parse_group(group: &Value) -> SnapGroup {
    let id = group
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let name = group
        .pointer("/config/name")
        .and_then(|v| v.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&id)
        .to_string();
    let clients = group
        .get("clients")
        .and_then(|clients| clients.as_array())
        .map(|clients| clients.iter().map(parse_client).collect())
        .unwrap_or_default();
    SnapGroup {
        id,
        name,
        stream_id: group
            .get("stream_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        clients,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_control_releases_its_socket_after_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control = SnapControl::new(listener.local_addr().unwrap().to_string());
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        });
        assert!(
            tokio::time::timeout(Duration::from_secs(7), control.status())
                .await
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(1), peer)
            .await
            .unwrap()
            .unwrap();
    }

    async fn reply(response: String) -> (SnapControl, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control = SnapControl::new(listener.local_addr().unwrap().to_string());
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            stream.read_line(&mut request).await.unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], "Server.GetStatus");
            // Rejection of oversized replies may close the socket before writing finishes.
            let _ = stream.get_mut().write_all(response.as_bytes()).await;
        });
        (control, task)
    }

    #[tokio::test]
    async fn malformed_status_is_not_a_successful_empty_observation() {
        for result in [
            Value::Null,
            json!({}),
            json!({"server":{"groups":{},"streams":[]}}),
        ] {
            let (control, task) = reply(format!("{}\n", json!({"id":1,"result":result}))).await;
            assert!(
                control.status().await.is_err(),
                "accepted malformed status: {result}"
            );
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn empty_server_and_interleaved_notifications_are_valid() {
        let (control, task) = reply(
            concat!(
                "{\"method\":\"Server.OnUpdate\",\"params\":{}}\n",
                "{\"id\":1,\"result\":{\"server\":{\"groups\":[],\"streams\":[]}}}\n"
            )
            .into(),
        )
        .await;
        assert!(control.status().await.unwrap().clients.is_empty());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_control_response_is_rejected() {
        let result = json!({"id":1,"result":{"server":{"groups":[],"streams":[]}},"padding":"x".repeat(1024*1024)});
        let (control, task) = reply(format!("{result}\n")).await;
        assert!(control.streams().await.is_err());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn unterminated_control_response_is_rejected() {
        let (control, task) =
            reply("{\"id\":1,\"result\":{\"server\":{\"groups\":[],\"streams\":[]}}}".into()).await;
        assert!(control.streams().await.is_err());
        task.await.unwrap();
    }
    #[test]
    fn preserves_group_membership_and_stream_from_snapserver_status() {
        let group = parse_group(&json!({
            "id": "kitchen-group",
            "config": { "name": "Kitchen" },
            "stream_id": "AirPlay",
            "clients": [{
                "id": "kitchen-speaker",
                "connected": true,
                "config": { "name": "Kitchen speaker", "volume": { "percent": 42, "muted": false } }
            }]
        }));
        assert_eq!(group.id, "kitchen-group");
        assert_eq!(group.name, "Kitchen");
        assert_eq!(group.stream_id, "AirPlay");
        assert_eq!(group.clients.len(), 1);
        assert_eq!(group.clients[0].id, "kitchen-speaker");
        assert_eq!(group.clients[0].volume_percent, 42);
    }
}
