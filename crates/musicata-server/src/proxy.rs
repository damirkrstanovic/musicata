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

//! Fetch a remote URL on the browser's behalf and stream it back.
//!
//! **Why this exists:** the web app is served under a Content-Security-Policy that permits
//! `'self'` only — no origin the page can reach directly. Everything the browser needs from
//! the internet (internet-radio streams, podcast enclosures, Internet Archive files, AutoEq
//! presets) is fetched here, in Rust, and relayed. That way an XSS that gets past Svelte's
//! escaping still can't reach the network, and the browser never discloses the user's IP to a
//! third party the operator didn't configure.
//!
//! Nothing is cached to disk: this is a pass-through, so a Musicata instance never accumulates
//! a copy of anyone's corpus (see NOTICE on the AutoEq measurement terms).
//!
//! The chunked-relay shape is the same one `opensubsonic::read_range_stream` uses — `ureq` is
//! blocking, so the request and every read happen on `spawn_blocking` threads and reach axum
//! through an mpsc channel. Bodies stream; they are never buffered whole.
//!
//! **SSRF note:** callers must never pass a URL taken straight from a request. Every caller
//! here resolves the URL server-side from something already persisted (a radio station the
//! operator added, a feed enclosure, a fixed AutoEq base), so the client only ever names *what*
//! it wants, never *where* to fetch it from.

use std::io::Read;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Headers worth passing back to the browser: enough for `<audio>` to seek and to know what
/// it's decoding. Everything else (cookies, caching directives we can't vouch for, CORS
/// headers meant for a different origin) is deliberately dropped.
const FORWARDED: [&str; 4] = [
    "Content-Type",
    "Content-Length",
    "Content-Range",
    "Accept-Ranges",
];

/// A live radio stream never ends, so this bounds only how long we wait to *start* receiving.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Chunk size for the relay. Matches the OpenSubsonic path.
const CHUNK: usize = 64 * 1024;

/// How many chunks may sit in flight before the reader thread blocks. Backpressure matters for
/// radio: without it a fast upstream would buffer an unbounded live stream in memory.
const CHANNEL_DEPTH: usize = 4;

pub struct ProxiedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub stream: ReceiverStream<std::io::Result<Vec<u8>>>,
}

/// Fetch `url`, forwarding `range` upstream when present, and relay the body in chunks.
///
/// Only `http`/`https` are accepted — a `file:` URL here would turn a podcast feed into a local
/// file read.
pub async fn fetch(url: &str, range: Option<&str>) -> anyhow::Result<ProxiedResponse> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        anyhow::bail!("refusing to proxy a non-http(s) URL");
    }

    let url = url.to_string();
    let range = range.map(str::to_string);
    let response = tokio::task::spawn_blocking(move || {
        let agent = ureq::AgentBuilder::new()
            .user_agent(&format!("Musicata/{}", env!("CARGO_PKG_VERSION")))
            .timeout_connect(CONNECT_TIMEOUT)
            .build();
        let mut request = agent.get(&url);
        if let Some(range) = range {
            request = request.set("Range", &range);
        }
        request.call()
    })
    .await?
    // A 4xx/5xx is an `Err` in ureq; unwrap it back to a response so the status reaches the
    // browser instead of becoming an opaque 500.
    .or_else(|error| match error {
        ureq::Error::Status(_, response) => Ok(response),
        other => Err(anyhow::anyhow!("upstream fetch: {other}")),
    })?;

    let status = response.status();
    let headers = FORWARDED
        .iter()
        .filter_map(|name| {
            response
                .header(name)
                .map(|value| (name.to_string(), value.to_string()))
        })
        .collect();

    let (tx, rx) = mpsc::channel::<std::io::Result<Vec<u8>>>(CHANNEL_DEPTH);
    let mut reader = response.into_reader();
    tokio::task::spawn_blocking(move || {
        let mut buffer = vec![0u8; CHUNK];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    // A closed channel means the browser hung up — stop pulling from upstream
                    // rather than draining a live radio stream into nowhere.
                    if tx.blocking_send(Ok(buffer[..read].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tx.blocking_send(Err(error));
                    break;
                }
            }
        }
    });

    Ok(ProxiedResponse {
        status,
        headers,
        stream: ReceiverStream::new(rx),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refuses_non_http_schemes() {
        for url in ["file:///etc/passwd", "ftp://example.com/x", "/etc/passwd"] {
            let Err(error) = fetch(url, None).await else {
                panic!("should have refused {url}");
            };
            assert!(
                error.to_string().contains("non-http(s)"),
                "unexpected error for {url}: {error}"
            );
        }
    }
}
