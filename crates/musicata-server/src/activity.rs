// SPDX-License-Identifier: AGPL-3.0-or-later
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
use tokio::sync::watch;

/// How many recent activities to retain.
const MAX_ACTIVITIES: usize = 60;

#[derive(Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Activity {
    // Wire ints are JSON numbers (well within JS safe-integer range); override ts-rs's
    // default i64/u64 → bigint so the generated TS matches the runtime `number`.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub id: u64,
    /// Coarse category, e.g. "scan".
    pub kind: String,
    /// Human-readable description, updated as the work progresses.
    pub label: String,
    /// "running" | "ok" | "error".
    pub status: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub started_at_unix_seconds: i64,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub finished_at_unix_seconds: Option<i64>,
    /// Result summary, or the failure with its root cause.
    pub message: Option<String>,
}

pub struct ActivityLog {
    items: Mutex<VecDeque<Activity>>,
    next_id: AtomicU64,
    // A version counter bumped on every change, so WebSocket subscribers can push
    // updates instead of clients polling.
    version: watch::Sender<u64>,
}

impl Default for ActivityLog {
    fn default() -> Self {
        Self {
            items: Mutex::new(VecDeque::new()),
            next_id: AtomicU64::new(0),
            version: watch::channel(0).0,
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

impl ActivityLog {
    /// Build a log seeded with persisted activities (newest first), continuing ids
    /// past the highest loaded one so new entries don't collide.
    pub fn seeded(initial: Vec<Activity>) -> Self {
        let next_id = initial
            .iter()
            .map(|activity| activity.id)
            .max()
            .unwrap_or(0);
        let log = Self::default();
        log.next_id.store(next_id, Ordering::Relaxed);
        *log.items.lock().expect("activity log poisoned") = initial.into_iter().collect();
        log
    }

    /// Subscribe to change notifications; the value is an opaque version counter.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    fn bump(&self) {
        self.version.send_modify(|version| *version += 1);
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
        {
            let mut items = self.items.lock().expect("activity log poisoned");
            items.push_front(activity);
            while items.len() > MAX_ACTIVITIES {
                items.pop_back();
            }
        }
        self.bump();
        id
    }

    /// Update the label of a running activity (e.g. which source is being scanned).
    pub fn update(&self, id: u64, label: impl Into<String>) {
        {
            let mut items = self.items.lock().expect("activity log poisoned");
            if let Some(activity) = items.iter_mut().find(|activity| activity.id == id) {
                activity.label = label.into();
            }
        }
        self.bump();
    }

    /// Mark an activity finished, ok or error, with a message.
    pub fn finish(&self, id: u64, ok: bool, message: Option<String>) {
        {
            let mut items = self.items.lock().expect("activity log poisoned");
            if let Some(activity) = items.iter_mut().find(|activity| activity.id == id) {
                activity.status = if ok { "ok" } else { "error" }.to_string();
                activity.finished_at_unix_seconds = Some(now());
                activity.message = message;
            }
        }
        self.bump();
    }

    /// Drop an activity entirely — used to avoid logging routine no-op rescans.
    pub fn remove(&self, id: u64) {
        {
            let mut items = self.items.lock().expect("activity log poisoned");
            items.retain(|activity| activity.id != id);
        }
        self.bump();
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
