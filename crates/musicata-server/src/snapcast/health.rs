// SPDX-License-Identifier: AGPL-3.0-or-later
//! Independent, bounded observations. No network or database work in snapshot reads.
use super::control::SnapGroup;
use super::{SnapClient, SnapControl, SnapStream};
use crate::{activity::ActivityLog, players::PlayerManager};
use serde::Serialize;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SnapcastHealth {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub observed_at: i64,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub last_success_at: Option<i64>,
    pub process: String,
    pub control: String,
    pub recovery: String,
    pub attempts: u8,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub next_retry_at: Option<i64>,
    pub failure: Option<String>,
    pub physical_output: String,
    pub server_version: Option<String>,
    pub clients: Vec<SnapClient>,
    pub groups: Vec<SnapGroup>,
    pub streams: Vec<SnapStream>,
}
impl Default for SnapcastHealth {
    fn default() -> Self {
        Self {
            observed_at: 0,
            last_success_at: None,
            process: "unknown".into(),
            control: "unknown".into(),
            recovery: "idle".into(),
            attempts: 0,
            next_retry_at: None,
            failure: None,
            physical_output: "unknown".into(),
            server_version: None,
            clients: Vec::new(),
            groups: Vec::new(),
            streams: Vec::new(),
        }
    }
}
pub struct HealthMonitor {
    updates: watch::Sender<SnapcastHealth>,
    retry: AtomicBool,
    wake: Notify,
}
impl Default for HealthMonitor {
    fn default() -> Self {
        Self {
            updates: watch::channel(SnapcastHealth::default()).0,
            retry: AtomicBool::new(false),
            wake: Notify::new(),
        }
    }
}
impl HealthMonitor {
    pub fn snapshot(&self) -> SnapcastHealth {
        let mut value = self.updates.borrow().clone();
        // A wedged worker must not leave a green connection badge indefinitely.
        if value.control == "connected" && crate::diagnostics::now() - value.observed_at > 15 {
            value.control = "stale".into();
        }
        value
    }
    pub fn export_summary(&self) -> serde_json::Value {
        let value = self.snapshot();
        // Version core only: never export arbitrary strings from an external server.
        let version = value.server_version.as_deref().and_then(|value| {
            let core = value.split('-').next()?.trim_start_matches('v');
            (core.len() <= 32
                && core.bytes().any(|b| b.is_ascii_digit())
                && core.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
            .then_some(core)
        });
        serde_json::json!({"observed_at": value.observed_at, "last_success_at": value.last_success_at,
            "process": value.process, "control": value.control, "recovery": value.recovery,
            "attempts": value.attempts, "next_retry_at": value.next_retry_at,
            "server_version": version, "physical_output": "unknown", "client_device_health": "unknown",
            "clients_observed": value.clients.len(), "clients_connected": value.clients.iter().filter(|c| c.connected).count(),
            "streams_observed": value.streams.len()})
    }

    pub fn subscribe(&self) -> watch::Receiver<SnapcastHealth> {
        self.updates.subscribe()
    }
    pub fn request_retry(&self) {
        // Reserve the reset under the watch lock so concurrent clicks cannot enqueue
        // another reset while the worker is awaiting settings or process startup.
        if self.updates.send_if_modified(|value| {
            if value.recovery != "exhausted" {
                return false;
            }
            value.recovery = "waiting".into();
            self.retry.store(true, Ordering::Release);
            true
        }) {
            self.wake.notify_one();
        }
    }
    fn publish(&self, next: SnapcastHealth, activity: &ActivityLog) {
        let previous = self.updates.borrow().clone();
        if (
            previous.process.as_str(),
            previous.control.as_str(),
            previous.recovery.as_str(),
            &previous.failure,
        ) != (
            next.process.as_str(),
            next.control.as_str(),
            next.recovery.as_str(),
            &next.failure,
        ) {
            let message = format!(
                "Process: {}; control: {}; recovery: {}. {}",
                next.process,
                next.control,
                next.recovery,
                next.failure.as_deref().unwrap_or("")
            );
            let id = activity.start("playback", "Multi-room playback health");
            activity.finish(id, next.failure.is_none(), Some(message.clone()));
            if previous.process != next.process {
                if matches!(next.process.as_str(), "exited" | "stopped" | "unknown") {
                    let mut event = crate::diagnostics::event(
                        "snapcast.process",
                        "snapcast",
                        musicata_core::diagnostics::DiagnosticAction::Failure,
                        None,
                        &message,
                    );
                    if let Some(code) = next
                        .failure
                        .as_deref()
                        .and_then(|v| v.split_whitespace().last())
                        .and_then(|v| v.parse::<u64>().ok())
                    {
                        event.measurements.insert("status".into(), code);
                    }
                    if let Some(diagnostics) = crate::diagnostics::global() {
                        diagnostics.record(event);
                    }
                } else {
                    crate::diagnostics::recovery("snapcast.process", "snapcast", None);
                }
            }
            if previous.control != next.control {
                if next.control == "unavailable" {
                    crate::diagnostics::failure(
                        "snapcast.control",
                        "snapcast",
                        None,
                        "control connection unavailable",
                    );
                } else if next.control == "connected" {
                    crate::diagnostics::recovery("snapcast.control", "snapcast", None);
                }
            }
            if previous.recovery != next.recovery {
                let category = match next.recovery.as_str() {
                    "retrying" => "snapcast.retry_attempt",
                    "exhausted" => "snapcast.retry_exhausted",
                    "waiting" => "snapcast.retry_waiting",
                    _ => "snapcast.retry_idle",
                };
                crate::diagnostics::transition(category, "snapcast", None);
            }
        }
        // Compare only fresh client observations. A control outage is not a mass disconnect.
        if next.control == "connected" {
            for client in &next.clients {
                let old = previous.clients.iter().find(|old| old.id == client.id);
                if old.map(|old| old.connected) != Some(client.connected) {
                    if client.connected {
                        crate::diagnostics::recovery(
                            "snapcast.client",
                            "snapcast",
                            Some(&client.id),
                        );
                    } else {
                        crate::diagnostics::failure(
                            "snapcast.client",
                            "snapcast",
                            Some(&client.id),
                            "Snapclient disconnected; check the playback host and audio device",
                        );
                    }
                }
            }
        }
        self.updates.send_replace(next);
    }
}

#[derive(Default)]
struct RetryBudget {
    attempts: u8,
    next: Option<u64>,
    healthy_since: Option<u64>,
}
impl RetryBudget {
    fn failed(&mut self, now: u64) {
        self.healthy_since = None;
        if self.next.is_none() && self.attempts < 3 {
            self.next = Some(now.saturating_add([1, 5, 30][self.attempts as usize]));
        }
    }
    fn take_due(&mut self, now: u64) -> bool {
        if self.next.is_some_and(|deadline| now >= deadline) {
            self.next = None;
            self.attempts += 1;
            true
        } else {
            false
        }
    }
    fn healthy(&mut self, now: u64) {
        self.next = None;
        if now.saturating_sub(*self.healthy_since.get_or_insert(now)) >= 60 {
            *self = Self::default();
        }
    }
    fn apply(&self, value: &mut SnapcastHealth, elapsed: u64) {
        value.attempts = self.attempts;
        value.next_retry_at = self
            .next
            .map(|next| crate::diagnostics::now() + next.saturating_sub(elapsed) as i64);
        value.recovery = if self.next.is_some() {
            "waiting"
        } else if self.attempts >= 3
            && self.healthy_since.is_none()
            && matches!(value.process.as_str(), "stopped" | "exited")
        {
            "exhausted"
        } else {
            "idle"
        }
        .into();
    }
}

pub async fn run(
    database: musicata_storage::Database,
    players: Arc<PlayerManager>,
    activity: Arc<ActivityLog>,
) {
    let start = Instant::now();
    let mut budget = RetryBudget::default();
    loop {
        let elapsed = start.elapsed().as_secs();
        let monitor = &players.snapcast_health;
        if monitor.retry.swap(false, Ordering::AcqRel) {
            budget = RetryBudget::default();
        }
        let mut value = monitor.snapshot();
        // A database outage must not be interpreted as an instruction to restart audio.
        let enabled = match tokio::time::timeout(
            Duration::from_secs(2),
            database.get_bool_setting(crate::SETTING_SNAPCAST_ENABLED),
        )
        .await
        {
            Ok(Ok(value)) => value
                .unwrap_or_else(|| crate::bool_setting_default(crate::SETTING_SNAPCAST_ENABLED)),
            _ => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let manager = players.snapcast_manager().await;
        if !enabled && manager.is_none() {
            budget = RetryBudget::default();
            value = SnapcastHealth {
                process: "disabled".into(),
                control: "unknown".into(),
                ..Default::default()
            };
        } else {
            let (process, failure) = manager.as_ref().map(|m| m.process_status())
                .unwrap_or(("stopped", Some("Multi-room startup failed; check that Snapserver is installed and its ports are available".into())));
            value.process = process.into();
            value.failure = failure;
            if matches!(process, "stopped" | "exited") {
                value.control = "unavailable".into();
                if enabled {
                    budget.failed(elapsed);
                } else {
                    budget = RetryBudget::default();
                }
                if enabled && budget.take_due(elapsed) {
                    value.recovery = "retrying".into();
                    value.attempts = budget.attempts;
                    value.next_retry_at = None;
                    value.observed_at = crate::diagnostics::now();
                    monitor.publish(value.clone(), &activity);
                    let recovered = match manager {
                        Some(manager) => manager.recover(&database).await,
                        None => crate::maybe_enable_snapcast(&database, &players)
                            .await
                            .map(|_| true),
                    };
                    if matches!(recovered, Ok(true)) {
                        // Observe the replacement before publishing: the pre-restart process
                        // state would otherwise falsely exhaust a successful third attempt.
                        continue;
                    }
                    if matches!(recovered, Ok(false)) {
                        budget = RetryBudget::default();
                    }
                    if recovered.is_err() {
                        value.failure = Some("Snapserver restart failed; check installation, ports and service permissions".into());
                        budget.failed(start.elapsed().as_secs());
                    }
                }
            } else if let Some(manager) = manager {
                let control = SnapControl::new(manager.control_addr());
                let observation = control.status().await;
                match observation {
                    Ok(status) => {
                        value.groups = status.groups;
                        value.clients = status.clients;
                        value.streams = status.streams;
                        value.server_version = status.server_version;
                        value.last_success_at = Some(crate::diagnostics::now());
                        value.control = "connected".into();
                        // Process-inspection failures remain failures even if control works.
                        if process != "unknown" {
                            value.failure = None;
                        }
                        budget.healthy(start.elapsed().as_secs());
                    }
                    _ => {
                        value.control = "unavailable".into();
                        value.failure = Some("Cannot reach Snapserver control; check the service and network. Client observations may be stale".into());
                        // Do not spend restart attempts on a live or externally owned process.
                        budget.healthy_since = None;
                    }
                }
            }
        }
        value.observed_at = crate::diagnostics::now();
        budget.apply(&mut value, start.elapsed().as_secs());
        monitor.publish(value, &activity);
        tokio::select! { _ = tokio::time::sleep(Duration::from_secs(2)) => {}, _ = monitor.wake.notified() => {} }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn export_omits_private_client_and_stream_metadata() {
        let monitor = HealthMonitor::default();
        monitor.updates.send_replace(SnapcastHealth {
            server_version: Some("0.26.0-private-build".into()),
            clients: vec![SnapClient {
                id: "secret-id".into(),
                name: "private-room".into(),
                connected: true,
                volume_percent: 50,
                muted: false,
            }],
            streams: vec![SnapStream {
                id: "private-stream".into(),
                status: "playing".into(),
                now_playing: Some("private-track".into()),
            }],
            ..Default::default()
        });
        let summary = monitor.export_summary();
        assert_eq!(summary["server_version"], "0.26.0");
        assert_eq!(summary["clients_connected"], 1);
        assert!(!summary.to_string().contains("private"));
        assert!(!summary.to_string().contains("secret"));
    }

    #[test]
    fn retry_requests_coalesce_and_do_not_override_backoff() {
        let monitor = HealthMonitor::default();
        monitor.request_retry();
        assert!(!monitor.retry.load(Ordering::Acquire));
        monitor.updates.send_replace(SnapcastHealth {
            recovery: "exhausted".into(),
            ..Default::default()
        });
        for _ in 0..100 {
            monitor.request_retry();
        }
        assert!(monitor.retry.swap(false, Ordering::AcqRel));
        // Worker has consumed the request but is still awaiting settings/startup.
        monitor.request_retry();
        assert!(!monitor.retry.swap(false, Ordering::AcqRel));
    }

    #[test]
    fn retry_budget_exhausts_and_requires_sustained_recovery() {
        let mut budget = RetryBudget::default();
        for (failure, before, due) in [(0, 0, 1), (1, 5, 6), (6, 35, 36)] {
            budget.failed(failure);
            assert!(!budget.take_due(before));
            assert!(budget.take_due(due));
        }
        budget.failed(36);
        assert!(!budget.take_due(1_000));
        budget.healthy(40);
        budget.failed(41); // Flapping does not reset the retry budget.
        assert!(!budget.take_due(1_000));
        budget.healthy(50);
        budget.healthy(110);
        budget.failed(111);
        assert!(budget.take_due(112));
    }
    #[test]
    fn live_process_control_outage_is_not_restart_exhaustion() {
        let budget = RetryBudget {
            attempts: 3,
            ..Default::default()
        };
        let mut value = SnapcastHealth {
            process: "running".into(),
            control: "unavailable".into(),
            ..Default::default()
        };
        budget.apply(&mut value, 40);
        assert_eq!(value.recovery, "idle");
        value.process = "exited".into();
        budget.apply(&mut value, 40);
        assert_eq!(value.recovery, "exhausted");
    }

    #[test]
    fn stale_snapshot_never_claims_current_connectivity() {
        let monitor = HealthMonitor::default();
        monitor.updates.send_replace(SnapcastHealth {
            control: "connected".into(),
            observed_at: crate::diagnostics::now() - 20,
            last_success_at: Some(123),
            ..Default::default()
        });
        assert_eq!(monitor.snapshot().control, "stale");
        assert_eq!(monitor.snapshot().last_success_at, Some(123));
        assert_eq!(monitor.snapshot().physical_output, "unknown");
    }
    #[test]
    fn repeated_observation_does_not_grow_activity_history() {
        let monitor = HealthMonitor::default();
        let activity = ActivityLog::default();
        let mut value = SnapcastHealth {
            process: "running".into(),
            control: "unavailable".into(),
            failure: Some("Control unavailable".into()),
            ..Default::default()
        };
        for time in 1..1000 {
            value.observed_at = time;
            monitor.publish(value.clone(), &activity);
        }
        assert_eq!(activity.list().len(), 1);
        let mut receiver = monitor.subscribe();
        for time in 1000..2000 {
            value.observed_at = time;
            monitor.publish(value.clone(), &activity);
        }
        assert_eq!(receiver.borrow_and_update().observed_at, 1999);
        assert_eq!(activity.list().len(), 1);
    }
}
