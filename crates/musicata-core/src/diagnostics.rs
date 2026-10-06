// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bounded diagnostic wire types. Recording and persistence belong to other crates.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticContext {
    pub output_id: Option<String>,
    pub source_id: Option<String>,
    pub track_id: Option<String>,
    pub renderer_session_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticAction {
    Failure,
    Recovery,
    Transition,
    Detail,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticEvent {
    pub category: String,
    pub component: String,
    pub action: DiagnosticAction,
    pub context: DiagnosticContext,
    pub message: String,
    #[serde(default)]
    pub measurements: std::collections::BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedEvent {
    pub event: DiagnosticEvent,
    pub timestamp: i64,
    pub version: String,
    pub session: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PerformanceSummary {
    pub operation: String,
    pub minute: i64,
    pub count: u64,
    pub total_ms: u64,
    pub max_ms: u64,
    pub slow_count: u64,
    pub buckets: [u64; 6],
}

pub fn safe_cause(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("model inference failed") {
        "model inference failed"
    } else if lower.contains("constraint") {
        "database constraint failed"
    } else if lower.contains("malformed") || lower.contains("corrupt") {
        "data corrupted"
    } else if lower.contains("permission denied") || lower.contains("access denied") {
        "permission denied"
    } else if lower.contains("connection refused") {
        "connection refused"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "operation timed out"
    } else if lower.contains("no such file")
        || lower.contains("not found")
        || lower.contains("unavailable")
    {
        "resource unavailable"
    } else if lower.contains("locked") || lower.contains("busy") {
        "resource busy"
    } else if lower == "storage full" || (lower.contains("disk") && lower.contains("full")) {
        "storage full"
    } else if lower.contains("decode") || lower.contains("invalid data") {
        "audio decode failed"
    } else if lower.contains("disconnect") || lower.contains("closed") {
        "connection closed"
    } else if lower.contains("buffer")
        || lower.contains("underrun")
        || lower.contains("playback interrupted")
    {
        "playback interrupted"
    } else if lower.contains("panic") || lower.contains("worker failed") {
        "worker failed"
    } else {
        "operation failed"
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn safe_cause_remains_meaningful_after_endpoint_and_server_sanitization() {
        for cause in [
            "permission denied",
            "resource unavailable",
            "playback interrupted",
            "worker failed",
            "model inference failed",
        ] {
            assert_eq!(super::safe_cause(cause), cause);
        }
    }
}
