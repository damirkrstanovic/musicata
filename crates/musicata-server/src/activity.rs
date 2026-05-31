//! A small in-memory log of long-running activities (library scans, rescans, …)
//! so the admin page can show what's happening now and why something failed.
//!
//! It's a bounded ring buffer behind a sync mutex — entries are tiny and the
//! critical sections never await, so it's cheap to touch from async scan code.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// How many recent activities to retain.
const MAX_ACTIVITIES: usize = 60;

#[derive(Clone, Serialize)]
pub struct Activity {
    pub id: u64,
    /// Coarse category, e.g. "scan".
    pub kind: String,
    /// Human-readable description, updated as the work progresses.
    pub label: String,
    /// "running" | "ok" | "error".
    pub status: String,
    pub started_at_unix_seconds: i64,
    pub finished_at_unix_seconds: Option<i64>,
    /// Result summary, or the failure with its root cause.
    pub message: Option<String>,
}

#[derive(Default)]
pub struct ActivityLog {
    items: Mutex<VecDeque<Activity>>,
    next_id: AtomicU64,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

impl ActivityLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the start of a running activity and return its id.
    pub fn start(&self, kind: impl Into<String>, label: impl Into<String>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let activity = Activity {
            id,
            kind: kind.into(),
            label: label.into(),
            status: "running".to_string(),
            started_at_unix_seconds: now(),
            finished_at_unix_seconds: None,
            message: None,
        };
        let mut items = self.items.lock().expect("activity log poisoned");
        items.push_front(activity);
        while items.len() > MAX_ACTIVITIES {
            items.pop_back();
        }
        id
    }

    /// Update the label of a running activity (e.g. which source is being scanned).
    pub fn update(&self, id: u64, label: impl Into<String>) {
        let mut items = self.items.lock().expect("activity log poisoned");
        if let Some(activity) = items.iter_mut().find(|activity| activity.id == id) {
            activity.label = label.into();
        }
    }

    /// Mark an activity finished, ok or error, with a message.
    pub fn finish(&self, id: u64, ok: bool, message: Option<String>) {
        let mut items = self.items.lock().expect("activity log poisoned");
        if let Some(activity) = items.iter_mut().find(|activity| activity.id == id) {
            activity.status = if ok { "ok" } else { "error" }.to_string();
            activity.finished_at_unix_seconds = Some(now());
            activity.message = message;
        }
    }

    /// Drop an activity entirely — used to avoid logging routine no-op rescans.
    pub fn remove(&self, id: u64) {
        let mut items = self.items.lock().expect("activity log poisoned");
        items.retain(|activity| activity.id != id);
    }

    /// All activities, newest first.
    pub fn list(&self) -> Vec<Activity> {
        self.items
            .lock()
            .expect("activity log poisoned")
            .iter()
            .cloned()
            .collect()
    }
}
