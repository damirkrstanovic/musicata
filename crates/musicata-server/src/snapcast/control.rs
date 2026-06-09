//! snapserver JSON-RPC control client (TCP, newline-delimited messages on port 1705).
//!
//! Used to enumerate the connected snapclients (rooms), set their per-output volume, and
//! point every group at the Musicata stream so all clients hear our audio in sync. We open
//! a short-lived connection per call and read until the matching response — the admin page
//! polls occasionally, so a persistent subscription isn't worth the complexity yet (a live
//! `Server.OnUpdate` subscription is a future refinement noted in docs/snapcast.md).

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// A snapclient as snapserver sees it — one synchronized output (a room/speaker).
#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SnapClient {
    pub id: String,
    pub name: String,
    pub connected: bool,
    pub volume_percent: u8,
    pub muted: bool,
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
        write.write_all(line.as_bytes()).await?;

        let mut reader = BufReader::new(read);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let mut buf = String::new();
            let read = tokio::time::timeout_at(deadline, reader.read_line(&mut buf))
                .await
                .context("snapcast control read timeout")??;
            if read == 0 {
                return Err(anyhow!("snapserver closed the control connection"));
            }
            let message: Value = match serde_json::from_str(buf.trim()) {
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

    /// Every client across all groups.
    pub async fn clients(&self) -> Result<Vec<SnapClient>> {
        let status = self.call("Server.GetStatus", Value::Null).await?;
        let mut clients = Vec::new();
        for group in groups(&status) {
            if let Some(array) = group.get("clients").and_then(|c| c.as_array()) {
                clients.extend(array.iter().map(parse_client));
            }
        }
        Ok(clients)
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

    /// Point every group at `stream` so all connected clients play the Musicata audio.
    pub async fn assign_all_to_stream(&self, stream: &str) -> Result<()> {
        let status = self.call("Server.GetStatus", Value::Null).await?;
        for group in groups(&status) {
            let Some(id) = group.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            if group.get("stream_id").and_then(|v| v.as_str()) != Some(stream) {
                let _ = self
                    .call("Group.SetStream", json!({ "id": id, "stream_id": stream }))
                    .await;
            }
        }
        Ok(())
    }
}

/// The `server.groups` array from a `Server.GetStatus` result.
fn groups(status: &Value) -> Vec<Value> {
    status
        .pointer("/server/groups")
        .and_then(|g| g.as_array())
        .cloned()
        .unwrap_or_default()
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
