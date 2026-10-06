// SPDX-License-Identifier: AGPL-3.0-or-later
use musicata_core::diagnostics::{
    DiagnosticAction, DiagnosticContext, DiagnosticEvent, PerformanceSummary, RecordedEvent,
};
use musicata_storage::diagnostics::{DiagnosticBatch, DiagnosticSnapshot, DiagnosticStore};
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::{RwLock, mpsc};

static GLOBAL: OnceLock<Diagnostics> = OnceLock::new();
pub fn global() -> Option<&'static Diagnostics> {
    GLOBAL.get()
}
pub fn detail_filter<S: tracing::Subscriber>(
    handle: Option<Diagnostics>,
) -> impl tracing_subscriber::layer::Filter<S> {
    tracing_subscriber::filter::dynamic_filter_fn(move |metadata, _context| {
        *metadata.level() <= tracing::Level::WARN
            || handle
                .as_ref()
                .or_else(|| global())
                .is_some_and(|d| d.detail_enabled())
    })
}
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}
pub fn data_path(path: &std::path::Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".diagnostics.db");
    PathBuf::from(value)
}

#[derive(Clone, Default, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DiagnosticHealth {
    pub available: bool,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub database_bytes: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub wal_bytes: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub dropped_events: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub rejected_events: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub detail_remaining_seconds: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub storage_gaps: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub reported_lost_events: u64,
}
#[derive(Clone)]
pub struct Diagnostics(Arc<Inner>);
struct Inner {
    tx: mpsc::Sender<Input>,
    dropped: AtomicU64,
    rejected: AtomicU64,
    gaps: AtomicU64,
    reported: AtomicU64,
    available: AtomicBool,
    database_bytes: AtomicU64,
    wal_bytes: AtomicU64,
    start: Instant,
    detail_until_ms: AtomicU64,
    pub store: RwLock<Option<DiagnosticStore>>,
    session: String,
    pending: std::sync::Mutex<DiagnosticBatch>,
}
#[derive(Debug)]
enum Input {
    Event(DiagnosticEvent, i64),
    Measurement(Operation, u64, DiagnosticContext, i64),
}
#[derive(Clone, Copy, Debug)]
pub enum Operation {
    Control,
    DatabaseRead,
    DatabaseWrite,
    SourceFirstByte,
    Decode,
    BackgroundJob,
}
impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::DatabaseRead => "database_read",
            Self::DatabaseWrite => "database_write",
            Self::SourceFirstByte => "source_first_byte",
            Self::Decode => "decode",
            Self::BackgroundJob => "background_job",
        }
    }
    fn threshold(self) -> Option<u64> {
        match self {
            Self::Control => Some(250),
            Self::DatabaseRead => Some(50),
            Self::DatabaseWrite => Some(250),
            Self::SourceFirstByte => Some(2000),
            _ => None,
        }
    }
}
impl Diagnostics {
    fn channel() -> (Self, mpsc::Receiver<Input>) {
        let (tx, rx) = mpsc::channel(512);
        let session = format!("{}-{}", now(), std::process::id());
        (
            Self(Arc::new(Inner {
                tx,
                dropped: AtomicU64::new(0),
                rejected: AtomicU64::new(0),
                gaps: AtomicU64::new(0),
                reported: AtomicU64::new(0),
                available: AtomicBool::new(false),
                database_bytes: AtomicU64::new(0),
                wal_bytes: AtomicU64::new(0),
                start: Instant::now(),
                detail_until_ms: AtomicU64::new(0),
                store: RwLock::new(None),
                session,
                pending: std::sync::Mutex::new(DiagnosticBatch::default()),
            })),
            rx,
        )
    }
    pub fn start(path: PathBuf) -> Self {
        let (handle, rx) = Self::channel();
        #[cfg(not(test))]
        let _ = GLOBAL.set(handle.clone());
        tokio::spawn(worker(handle.clone(), rx, path));
        handle
    }
    pub fn record(&self, event: DiagnosticEvent) {
        if event.action == DiagnosticAction::Detail && !self.detail_enabled() {
            return;
        }
        if self.0.tx.capacity() == 0 {
            self.0.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(event) = sanitize(event) else {
            self.0.rejected.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if self.0.tx.try_send(Input::Event(event, now())).is_err() {
            self.0.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn observe(&self, operation: Operation, elapsed: Duration, context: DiagnosticContext) {
        if [
            &context.output_id,
            &context.source_id,
            &context.track_id,
            &context.renderer_session_id,
        ]
        .iter()
        .any(|value| value.as_ref().is_some_and(|id| id.len() > 256))
        {
            self.0.rejected.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self
            .0
            .tx
            .try_send(Input::Measurement(
                operation,
                elapsed.as_millis().min(u64::MAX as u128) as u64,
                context,
                now(),
            ))
            .is_err()
        {
            self.0.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn reported_loss(&self, count: u64) {
        self.0
            .reported
            .fetch_add(count.min(1_000_000), Ordering::Relaxed);
    }
    pub fn health(&self) -> DiagnosticHealth {
        let remaining = self
            .0
            .detail_until_ms
            .load(Ordering::Relaxed)
            .saturating_sub(self.0.start.elapsed().as_millis() as u64);
        DiagnosticHealth {
            available: self.0.available.load(Ordering::Relaxed),
            database_bytes: self.0.database_bytes.load(Ordering::Relaxed),
            wal_bytes: self.0.wal_bytes.load(Ordering::Relaxed),
            dropped_events: self.0.dropped.load(Ordering::Relaxed),
            rejected_events: self.0.rejected.load(Ordering::Relaxed),
            detail_remaining_seconds: remaining.div_ceil(1000),
            storage_gaps: self.0.gaps.load(Ordering::Relaxed),
            reported_lost_events: self.0.reported.load(Ordering::Relaxed),
        }
    }
    pub fn enable_detail(&self) {
        self.0.detail_until_ms.store(
            self.0.start.elapsed().as_millis() as u64 + 900_000,
            Ordering::Relaxed,
        );
    }
    pub fn disable_detail(&self) {
        self.0.detail_until_ms.store(0, Ordering::Relaxed);
    }
    pub fn detail_enabled(&self) -> bool {
        self.0.detail_until_ms.load(Ordering::Relaxed) > self.0.start.elapsed().as_millis() as u64
    }
    /// The writer owns this bounded mirror; producers never acquire this lock.
    pub fn memory_snapshot(&self) -> DiagnosticSnapshot {
        let health = self.health();
        let mut snapshot = DiagnosticSnapshot {
            dropped: health.dropped_events,
            storage_gaps: health.storage_gaps,
            reported_lost: health.reported_lost_events,
            ..Default::default()
        };
        let Ok(pending) = self.0.pending.try_lock() else {
            return snapshot;
        };
        for record in &pending.events {
            if record.event.action == DiagnosticAction::Failure {
                let key = (
                    &record.event.category,
                    &record.event.component,
                    &record.event.context,
                );
                let existing = snapshot.incidents.iter_mut().find(|row| {
                    row["category"] == key.0.as_str()
                        && row["component"] == key.1.as_str()
                        && row["context"] == serde_json::to_value(key.2).unwrap_or_default()
                        && row["recovered_at"].is_null()
                });
                if let Some(row) = existing {
                    row["count"] = serde_json::json!(row["count"].as_u64().unwrap_or(0) + 1);
                    row["last"] = serde_json::json!(record.timestamp);
                } else {
                    snapshot.incidents.push(serde_json::json!({"category":key.0,"component":key.1,"context":key.2,"message":record.event.message,"measurements":record.event.measurements,"version":record.version,"session":record.session,"first":record.timestamp,"last":record.timestamp,"count":1,"recovered_at":null}));
                }
            } else if record.event.action == DiagnosticAction::Recovery {
                for row in &mut snapshot.incidents {
                    if row["category"] == record.event.category
                        && row["component"] == record.event.component
                        && row["context"]
                            == serde_json::to_value(&record.event.context).unwrap_or_default()
                        && row["recovered_at"].is_null()
                    {
                        row["recovered_at"] = serde_json::json!(record.timestamp);
                    }
                }
            } else if let Ok(row) = serde_json::to_value(record) {
                snapshot.transitions.push(row);
            }
        }
        snapshot.performance = pending
            .summaries
            .iter()
            .filter_map(|row| serde_json::to_value(row).ok())
            .collect();
        snapshot
    }
    pub async fn store(&self) -> Option<DiagnosticStore> {
        self.0.store.read().await.clone()
    }
}
/// Never persist unrestricted third-party error strings. Codes retain the failure's meaning.
pub fn safe_cause(message: &str) -> &'static str {
    musicata_core::diagnostics::safe_cause(message)
}

pub fn identity(value: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("ref-{:x}", Sha256::digest(value.as_bytes()))
}
fn sanitize(mut event: DiagnosticEvent) -> Option<DiagnosticEvent> {
    if serde_json::to_vec(&event).ok()?.len() > 3500 {
        return None;
    }
    for value in [&event.category, &event.component] {
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return None;
        }
    }
    for value in [
        &mut event.context.output_id,
        &mut event.context.source_id,
        &mut event.context.track_id,
        &mut event.context.renderer_session_id,
    ] {
        if let Some(id) = value {
            if id.len() > 256 {
                return None;
            }
            *id = identity(id);
        }
    }
    event.measurements.retain(|key, _| {
        matches!(
            key.as_str(),
            "elapsed_ms" | "status" | "duration_ms" | "method" | "source_line"
        )
    });
    event.message = match event.action {
        DiagnosticAction::Failure => safe_cause(&event.message).to_owned(),
        DiagnosticAction::Recovery => "operation recovered".into(),
        DiagnosticAction::Transition => "playback or processing state changed".into(),
        DiagnosticAction::Detail => "detailed operation recorded".into(),
    };
    Some(event)
}
#[track_caller]
pub fn event(
    category: &str,
    component: &str,
    action: DiagnosticAction,
    output: Option<&str>,
    message: &str,
) -> DiagnosticEvent {
    DiagnosticEvent {
        category: category.to_owned(),
        component: component.to_owned(),
        action,
        context: DiagnosticContext {
            output_id: output.map(str::to_owned),
            ..Default::default()
        },
        message: message.chars().take(1024).collect(),
        measurements: [(
            "source_line".into(),
            std::panic::Location::caller().line() as u64,
        )]
        .into(),
    }
}
#[track_caller]
pub fn failure(category: &str, component: &str, output: Option<&str>, cause: &str) {
    if let Some(d) = global() {
        d.record(event(
            category,
            component,
            DiagnosticAction::Failure,
            output,
            cause,
        ));
    }
}
pub fn recovery(category: &str, component: &str, output: Option<&str>) {
    if let Some(d) = global() {
        d.record(event(
            category,
            component,
            DiagnosticAction::Recovery,
            output,
            "",
        ));
    }
}
pub fn transition(category: &str, component: &str, output: Option<&str>) {
    if let Some(d) = global() {
        d.record(event(
            category,
            component,
            DiagnosticAction::Transition,
            output,
            "",
        ));
    }
}

#[track_caller]
pub fn record_context(
    category: &str,
    component: &str,
    action: DiagnosticAction,
    context: DiagnosticContext,
    cause: &str,
) {
    if let Some(d) = global() {
        let mut e = event(category, component, action, None, cause);
        e.context = context;
        d.record(e);
    }
}

async fn worker(d: Diagnostics, mut rx: mpsc::Receiver<Input>, path: PathBuf) {
    let mut pending = DiagnosticBatch::default();
    let mut written_drops = 0;
    let mut written_gaps = 0;
    let mut written_reported = 0;
    let mut unavailable = false;
    let mut last_prune: Option<Instant> = None;
    let mut wal_path = path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let wal_path = PathBuf::from(wal_path);
    loop {
        {
            let first = if pending.events.is_empty() && pending.summaries.is_empty() {
                tokio::time::timeout(Duration::from_secs(1), rx.recv()).await
            } else {
                Ok(rx.try_recv().ok())
            };
            if let Ok(None) = first
                && rx.is_closed()
            {
                break;
            }
            let mut inputs = Vec::with_capacity(128);
            if let Ok(Some(input)) = first {
                inputs.push(input);
            }
            while inputs.len() < 128 {
                match rx.try_recv() {
                    Ok(input) => inputs.push(input),
                    Err(_) => break,
                }
            }
            for input in inputs {
                match input {
                    Input::Event(event, received) => pending.events.push(RecordedEvent {
                        event,
                        timestamp: received,
                        version: env!("CARGO_PKG_VERSION").into(),
                        session: d.0.session.clone(),
                    }),
                    Input::Measurement(operation, ms, context, received) => {
                        if operation
                            .threshold()
                            .is_some_and(|threshold| ms >= threshold)
                        {
                            let mut sample = event(
                                &format!("performance.{}", operation.name()),
                                "performance",
                                DiagnosticAction::Transition,
                                None,
                                "",
                            );
                            sample.context = context;
                            sample.measurements.insert("duration_ms".into(), ms);
                            if let Some(event) = sanitize(sample) {
                                pending.events.push(RecordedEvent {
                                    event,
                                    timestamp: received,
                                    version: env!("CARGO_PKG_VERSION").into(),
                                    session: d.0.session.clone(),
                                });
                            }
                        }
                        let minute = (received / 300) * 5;
                        let name = operation.name();
                        let index = pending
                            .summaries
                            .iter()
                            .position(|s| s.operation == name && s.minute == minute);
                        let summary = if let Some(index) = index {
                            &mut pending.summaries[index]
                        } else {
                            pending.summaries.push(PerformanceSummary {
                                operation: name.into(),
                                minute,
                                ..Default::default()
                            });
                            pending.summaries.last_mut().unwrap()
                        };
                        summary.count = summary.count.saturating_add(1);
                        summary.total_ms = summary.total_ms.saturating_add(ms);
                        summary.max_ms = summary.max_ms.max(ms);
                        if operation
                            .threshold()
                            .is_some_and(|threshold| ms >= threshold)
                        {
                            summary.slow_count = summary.slow_count.saturating_add(1);
                        }
                        let bucket = [10, 50, 250, 1000, 2000]
                            .iter()
                            .position(|limit| ms < *limit)
                            .unwrap_or(5);
                        summary.buckets[bucket] += 1;
                    }
                }
            }
        }
        // Keep draining during outages: prioritize incident evidence over routine transitions.
        while pending.events.len() > 128 {
            let index = pending
                .events
                .iter()
                .position(|record| {
                    matches!(
                        record.event.action,
                        DiagnosticAction::Transition | DiagnosticAction::Detail
                    )
                })
                .unwrap_or(0);
            pending.events.remove(index);
            d.0.dropped.fetch_add(1, Ordering::Relaxed);
        }
        while pending.summaries.len() > 256 {
            let row = pending.summaries.remove(0);
            d.0.dropped.fetch_add(row.count, Ordering::Relaxed);
        }
        *d.0.pending.lock().expect("diagnostic pending") = pending.clone();
        let mut store = d.store().await;
        if store.is_none() {
            match DiagnosticStore::open(&path).await {
                Ok(opened) => {
                    *d.0.store.write().await = Some(opened.clone());
                    store = Some(opened);
                }
                Err(_) => {
                    if !unavailable {
                        d.0.gaps.fetch_add(1, Ordering::Relaxed);
                        unavailable = true;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            }
        }
        let store = store.unwrap();
        let drops = d.0.dropped.load(Ordering::Relaxed);
        pending.dropped = drops.saturating_sub(written_drops);
        let gaps = d.0.gaps.load(Ordering::Relaxed);
        let reported = d.0.reported.load(Ordering::Relaxed);
        pending.storage_gaps = gaps.saturating_sub(written_gaps);
        pending.reported_lost = reported.saturating_sub(written_reported);
        let result = if last_prune.is_none_or(|last| last.elapsed() >= Duration::from_secs(60)) {
            let result = store.prune(now()).await;
            if result.is_ok() {
                last_prune = Some(Instant::now());
            }
            result
        } else {
            Ok(())
        };
        let written = result.is_ok() && store.write_batch(&pending).await.is_ok();
        d.0.database_bytes.store(
            tokio::fs::metadata(&path)
                .await
                .map(|m| m.len())
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        d.0.wal_bytes.store(
            tokio::fs::metadata(&wal_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        if written {
            pending = DiagnosticBatch::default();
            *d.0.pending.lock().expect("diagnostic pending") = DiagnosticBatch::default();
            written_drops = drops;
            written_gaps = gaps;
            written_reported = reported;
            unavailable = false;
            d.0.available.store(true, Ordering::Relaxed);
        } else {
            d.0.available.store(false, Ordering::Relaxed);
            if !unavailable {
                d.0.gaps.fetch_add(1, Ordering::Relaxed);
                unavailable = true;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn failure() -> DiagnosticEvent {
        DiagnosticEvent {
            category: "mpd.command".into(),
            component: "mpd".into(),
            action: DiagnosticAction::Failure,
            context: DiagnosticContext::default(),
            message: "connection refused".into(),
            measurements: Default::default(),
        }
    }
    #[test]
    fn tracing_request_failure_has_bounded_attribution_without_private_path() {
        use tracing_subscriber::prelude::*;
        let (d, mut rx) = Diagnostics::channel();
        let subscriber = tracing_subscriber::registry().with(DiagnosticLayer(Some(d)));
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(
                diagnostic_category = "server.internal",
                error = "permission denied /private/password",
                method = "POST",
                path = "/api/players/private-id/commands?token=secret",
                status = 500u64,
                "request failed"
            );
        });
        let Input::Event(e, _) = rx.try_recv().unwrap() else {
            panic!("expected incident")
        };
        assert_eq!(e.category, "server.internal.players.post");
        assert!(e.measurements.contains_key("source_line"));
        assert!(!serde_json::to_string(&e).unwrap().contains("secret"));
    }
    #[test]
    fn temporary_filter_rechecks_the_same_callsite_live() {
        use tracing_subscriber::prelude::*;
        let (d, mut rx) = Diagnostics::channel();
        let filter = detail_filter(Some(d.clone()));
        let subscriber = tracing_subscriber::registry()
            .with(DiagnosticLayer(Some(d.clone())).with_filter(filter));
        fn success() {
            tracing::debug!(
                method = "GET",
                path = "/api/health",
                status = 200u64,
                "HTTP request"
            );
        }
        tracing::subscriber::with_default(subscriber, || {
            success();
            assert!(rx.try_recv().is_err());
            d.enable_detail();
            success();
            assert!(
                rx.try_recv().is_ok(),
                "live detail must enable existing callsite"
            );
            d.disable_detail();
            success();
            assert!(rx.try_recv().is_err());
        });
    }
    #[tokio::test]
    async fn delayed_writer_preserves_received_time_and_performance_bucket() {
        let path = std::env::temp_dir().join(format!(
            "musicata-receipt-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (d, rx) = Diagnostics::channel();
        let received = now() - 600;
        d.0.tx.try_send(Input::Event(failure(), received)).unwrap();
        d.0.tx
            .try_send(Input::Measurement(
                Operation::Control,
                10,
                Default::default(),
                received,
            ))
            .unwrap();
        tokio::spawn(worker(d.clone(), rx, path));
        tokio::time::timeout(Duration::from_secs(3), async {
            while !d.health().available {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let snapshot = d.store().await.unwrap().snapshot(0).await.unwrap();
        assert_eq!(snapshot.incidents[0]["first"], received);
        assert_eq!(snapshot.performance[0]["minute"], (received / 300) * 5);
    }
    #[test]
    fn arbitrary_credentials_never_reach_persistence() {
        let mut event = failure();
        event.message =
            "https://alice:secret@example.test/audio?token=secret /home/alice/music Bearer secret"
                .into();
        let safe = sanitize(event).unwrap();
        assert!(!safe.message.contains("secret"));
        assert!(!safe.message.contains("alice"));
    }
    #[test]
    fn oversized_unicode_input_is_rejected() {
        let mut event = failure();
        event.message = "密".repeat(5000);
        assert!(sanitize(event).is_none());
    }
    #[test]
    fn detailed_recording_can_be_enabled_and_stopped() {
        let (d, _rx) = Diagnostics::channel();
        d.enable_detail();
        assert!(d.detail_enabled());
        d.disable_detail();
        assert!(!d.detail_enabled());
    }
    #[test]
    fn blocked_consumer_drops_evidence_without_waiting() {
        let (d, _rx) = Diagnostics::channel();
        let started = Instant::now();
        for _ in 0..10_000 {
            d.record(failure());
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(d.health().dropped_events, 9488);
    }
    #[test]
    fn detail_expiry_uses_elapsed_time_and_restart_is_disabled() {
        let (mut d, _rx) = Diagnostics::channel();
        assert!(!d.detail_enabled());
        d.enable_detail();
        Arc::get_mut(&mut d.0).unwrap().start = Instant::now() - Duration::from_secs(901);
        assert!(!d.detail_enabled());
        assert_eq!(d.health().detail_remaining_seconds, 0);
        let (fresh, _rx) = Diagnostics::channel();
        assert!(!fresh.detail_enabled());
    }
    #[tokio::test]
    async fn unavailable_directory_recovers_with_retained_evidence() {
        let parent = std::env::temp_dir().join(format!(
            "musicata-diag-blocked-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&parent, b"not a directory").unwrap();
        let d = Diagnostics::start(parent.join("diagnostics.db"));
        d.record(failure());
        tokio::time::timeout(Duration::from_secs(3), async {
            while d.health().storage_gaps == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!d.health().available);
        std::fs::remove_file(&parent).unwrap();
        std::fs::create_dir(&parent).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !d.health().available {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let snapshot = d.store().await.unwrap().snapshot(0).await.unwrap();
        assert_eq!(snapshot.incidents.len(), 1);
        assert_eq!(snapshot.incidents[0]["message"], "connection refused");
    }

    #[test]
    fn tracing_failure_preserves_safe_cause_and_attribution() {
        use tracing_subscriber::prelude::*;
        let (d, mut rx) = Diagnostics::channel();
        let subscriber = tracing_subscriber::registry().with(DiagnosticLayer(Some(d)));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                diagnostic_category = "queue.persist",
                source_line = 42u64,
                output = "private-player",
                error = "permission denied /home/alice/private",
                "queue write failed"
            );
        });
        let Input::Event(e, _) = rx.try_recv().unwrap() else {
            panic!("expected incident")
        };
        assert_eq!(e.category, "queue.persist");
        assert_eq!(e.measurements["source_line"], 42);
        assert_eq!(e.message, "permission denied");
        assert_eq!(e.context.output_id, Some(identity("private-player")));
    }

    #[tokio::test]
    async fn performance_thresholds_are_specific_and_summaries_are_bounded() {
        let path = std::env::temp_dir().join(format!(
            "musicata-perf-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let d = Diagnostics::start(path);
        for ms in [249, 250, 251] {
            d.observe(
                Operation::Control,
                Duration::from_millis(ms),
                Default::default(),
            );
        }
        d.observe(
            Operation::BackgroundJob,
            Duration::from_secs(600),
            Default::default(),
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            while !d.health().available {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let snapshot = d.store().await.unwrap().snapshot(0).await.unwrap();
        let control = snapshot
            .performance
            .iter()
            .find(|row| row["operation"] == "control")
            .unwrap();
        assert_eq!(control["count"], 3);
        assert_eq!(control["total_ms"], 750);
        assert_eq!(control["slow_count"], 2);
        let background = snapshot
            .performance
            .iter()
            .find(|row| row["operation"] == "background_job")
            .unwrap();
        assert_eq!(background["slow_count"], 0);
        assert!(
            snapshot
                .transitions
                .iter()
                .any(|row| row["event"]["category"] == "performance.control")
        );
    }
}

/// Structured tracing fallback. No unrestricted message/field dump reaches local storage.
pub struct DiagnosticLayer(pub Option<Diagnostics>);
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for DiagnosticLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let Some(d) = self.0.as_ref().or_else(|| global()) else {
            return;
        };
        let mut fields = TraceFields::default();
        event.record(&mut fields);
        if fields.recorded {
            return;
        }
        let detail = *event.metadata().level() > tracing::Level::WARN;
        if detail && !d.detail_enabled() {
            return;
        }
        let target = event.metadata().target();
        let component = fields
            .component
            .as_deref()
            .unwrap_or_else(|| target.rsplit("::").next().unwrap_or("server"));
        let fallback = if fields.http {
            format!(
                "http.{}.{}",
                fields.route.unwrap_or("other"),
                fields.method.unwrap_or("other")
            )
        } else {
            "server.operation".into()
        };
        let category = fields.category.as_deref().unwrap_or(&fallback);
        let category = if category == "server.internal" && fields.http {
            format!(
                "server.internal.{}.{}",
                fields.route.unwrap_or("other"),
                fields.method.unwrap_or("other")
            )
        } else {
            category.to_owned()
        };
        let mut e = crate::diagnostics::event(
            &category,
            component,
            if detail {
                DiagnosticAction::Detail
            } else {
                DiagnosticAction::Failure
            },
            fields.output.as_deref(),
            &fields.cause,
        );
        e.measurements = fields.measurements;
        if let Some(line) = event.metadata().line() {
            e.measurements
                .entry("source_line".into())
                .or_insert(line as u64);
        }
        d.record(e);
    }
}
#[derive(Default)]
struct TraceFields {
    cause: String,
    recorded: bool,
    http: bool,
    method: Option<&'static str>,
    route: Option<&'static str>,
    output: Option<String>,
    category: Option<String>,
    component: Option<String>,
    measurements: std::collections::BTreeMap<String, u64>,
}
fn bounded_debug(value: &dyn std::fmt::Debug, limit: usize) -> String {
    struct Buffer {
        text: String,
        limit: usize,
    }
    impl std::fmt::Write for Buffer {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            for c in text.chars() {
                if self.text.len() + c.len_utf8() > self.limit {
                    return Err(std::fmt::Error);
                }
                self.text.push(c);
            }
            Ok(())
        }
    }
    let mut buffer = Buffer {
        text: String::with_capacity(limit),
        limit,
    };
    let _ = std::fmt::write(&mut buffer, format_args!("{value:?}"));
    buffer.text
}
impl tracing::field::Visit for TraceFields {
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        if field.name() == "diagnostic_recorded" {
            self.recorded = value;
        }
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if matches!(field.name(), "elapsed_ms" | "status" | "source_line") {
            self.measurements.insert(field.name().into(), value);
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "diagnostic_category" => self.category = Some(value.chars().take(64).collect()),
            "diagnostic_component" => self.component = Some(value.chars().take(64).collect()),
            "output" | "output_id" | "player" | "player_id" => {
                self.output = Some(value.chars().take(256).collect())
            }
            _ => self.record_debug(field, &value),
        }
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "error" => self.cause = bounded_debug(value, 1024),
            "output" | "output_id" | "player" | "player_id" => {
                self.output = Some(bounded_debug(value, 256))
            }
            "method" => {
                self.http = true;
                self.method = Some(match bounded_debug(value, 16).trim_matches('"') {
                    "GET" => "get",
                    "POST" => "post",
                    "PATCH" => "patch",
                    "DELETE" => "delete",
                    "PUT" => "put",
                    _ => "other",
                });
            }
            "path" => {
                let path = bounded_debug(value, 256);
                self.route = Some(
                    match path.trim_matches('"').split('/').nth(2).unwrap_or("") {
                        "tracks" => "tracks",
                        "albums" => "albums",
                        "artists" => "artists",
                        "players" => "players",
                        "zones" => "zones",
                        "radio" => "radio",
                        "sources" => "sources",
                        "diagnostics" => "diagnostics",
                        "settings" => "settings",
                        "auth" => "auth",
                        _ => "other",
                    },
                );
            }
            _ => {}
        }
    }
}

pub struct Timing {
    operation: Operation,
    started: Instant,
}
impl Timing {
    pub fn new(operation: Operation) -> Self {
        Self {
            operation,
            started: Instant::now(),
        }
    }
}
impl Drop for Timing {
    fn drop(&mut self) {
        if let Some(d) = global() {
            d.observe(self.operation, self.started.elapsed(), Default::default());
        }
    }
}

#[derive(Default)]
pub struct ReportLimits {
    global: Option<(Instant, u32)>,
    identities: std::collections::BTreeMap<String, (Instant, u32)>,
}
impl ReportLimits {
    pub fn allow(&mut self, identity: &str) -> bool {
        let now = Instant::now();
        self.identities
            .retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(60));
        let global = self.global.get_or_insert((now, 0));
        if now.duration_since(global.0) >= Duration::from_secs(60) {
            *global = (now, 0);
        }
        if global.1 >= 60 {
            return false;
        }
        if !self.identities.contains_key(identity) && self.identities.len() >= 256 {
            return false;
        }
        let entry = self.identities.entry(identity.into()).or_insert((now, 0));
        if entry.1 >= 10 {
            return false;
        }
        entry.1 += 1;
        global.1 += 1;
        true
    }
}

/// Observe an intentionally best-effort background write without changing its caller's flow.
#[track_caller]
pub fn background_result<T, E: std::fmt::Display>(
    result: Result<T, E>,
    category: &'static str,
) -> Option<T> {
    match result {
        Ok(value) => {
            recovery(category, "background", None);
            Some(value)
        }
        Err(error) => {
            tracing::warn!(diagnostic_category=category,diagnostic_component="background",source_line=std::panic::Location::caller().line() as u64,error=%error,"background persistence failed");
            None
        }
    }
}
