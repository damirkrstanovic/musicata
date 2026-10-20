// SPDX-License-Identifier: AGPL-3.0-or-later
use musicata_storage::Database;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const HISTORY_LIMIT: usize = 2880; // 24 hours at 30 seconds.
const EVENT_LIMIT: usize = 256;
const CHECKPOINT_LIMIT: u64 = 4 * 1024 * 1024;
const WORKERS: &[&str] = &[
    "scan",
    "fingerprint",
    "loudness",
    "musicbrainz",
    "artwork",
    "prewarm",
    "ml",
];

#[derive(Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ResourceSample {
    pub at: usize,
    pub session: String,
    pub uptime_seconds: usize,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub revision: String,
    pub active_workers: Vec<String>,
    pub backlog: BTreeMap<String, usize>,
    pub cpu_percent: Option<f64>,
    pub rss_bytes: Option<usize>,
    pub swap_bytes: Option<usize>,
    pub threads: Option<usize>,
    pub read_bytes: Option<usize>,
    pub write_bytes: Option<usize>,
    pub container_bytes: Option<usize>,
    pub container_limit_bytes: Option<usize>,
    pub allocated_bytes: Option<usize>,
    pub allocator_resident_bytes: Option<usize>,
    pub allocator_retained_bytes: Option<usize>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct WorkerHealth {
    pub kind: String,
    pub work_unit: String,
    pub state: String,
    pub queued: Option<usize>,
    pub oldest_queued_at: Option<usize>,
    pub queue_observed_at: Option<usize>,
    pub active: usize,
    pub attempts: usize,
    pub completed: usize,
    pub failed: usize,
    pub retryable_failures: usize,
    pub consecutive_failures: usize,
    pub last_progress_at: Option<usize>,
    pub operation_started_at: Option<usize>,
    pub last_failure: Option<String>,
    pub next_retry_at: Option<usize>,
    pub starts: usize,
    pub exits: usize,
    pub panics: usize,
    pub restarts: usize,
    pub stalled: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct WorkerEvent {
    pub at: usize,
    #[serde(default)]
    pub session: String,
    pub kind: String,
    pub event: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RuntimeSnapshot {
    pub session: String,
    pub version: String,
    pub revision: String,
    pub counter_scope: String,
    pub latest: Option<ResourceSample>,
    pub history: Vec<ResourceSample>,
    pub workers: Vec<WorkerHealth>,
    pub events: Vec<WorkerEvent>,
    pub checkpoint_error: bool,
}
#[derive(Default)]
struct Data {
    history: VecDeque<ResourceSample>,
    workers: BTreeMap<String, WorkerHealth>,
    events: VecDeque<WorkerEvent>,
    checkpoint_error: bool,
}
pub struct Runtime {
    data: Mutex<Data>,
    started: Instant,
    session: String,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            data: Mutex::new(Data::default()),
            started: Instant::now(),
            session: format!("{}-{}", super::now(), std::process::id()),
        }
    }
}
impl Runtime {
    pub fn snapshot(&self, history: bool) -> RuntimeSnapshot {
        let d = self.data.lock().unwrap();
        RuntimeSnapshot {
            session: self.session.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            revision: option_env!("MUSICATA_GIT_SHA").unwrap_or("unknown").into(),
            counter_scope: "worker counters: current server session; history: up to 24 hours"
                .into(),
            latest: d.history.back().cloned(),
            history: if history {
                d.history.iter().cloned().collect()
            } else {
                Vec::new()
            },
            workers: d.workers.values().cloned().collect(),
            events: d.events.iter().cloned().collect(),
            checkpoint_error: d.checkpoint_error,
        }
    }
    fn change(&self, kind: &str, f: impl FnOnce(&mut WorkerHealth)) {
        if !WORKERS.contains(&kind) {
            return;
        }
        let mut d = self.data.lock().unwrap();
        let w = d
            .workers
            .entry(kind.into())
            .or_insert_with(|| WorkerHealth {
                kind: kind.into(),
                work_unit: if matches!(kind, "fingerprint" | "loudness" | "musicbrainz" | "ml") {
                    "tracks"
                } else if matches!(kind, "prewarm" | "artwork") {
                    "covers"
                } else {
                    "job passes"
                }
                .into(),
                state: "idle".into(),
                ..Default::default()
            });
        f(w);
    }
    fn event(&self, kind: &str, event: &str) {
        if kind != "server" && !WORKERS.contains(&kind) {
            return;
        }
        let mut d = self.data.lock().unwrap();
        d.events.push_back(WorkerEvent {
            session: self.session.clone(),
            at: super::now() as usize,
            kind: kind.into(),
            event: event.into(),
        });
        while d.events.len() > EVENT_LIMIT {
            d.events.pop_front();
        }
    }
    pub fn begin(&self, kind: &str) {
        self.event(kind, "job_started");
        self.change(kind, |w| {
            w.active += 1;
            w.attempts += 1;
            w.state = "running".into();
            w.operation_started_at.get_or_insert(super::now() as usize);
            w.next_retry_at = None;
        });
    }
    pub fn finish(&self, kind: &str, success: bool, retry: bool, category: &'static str) {
        let now = super::now() as usize;
        self.event(
            kind,
            if success {
                "job_completed"
            } else if retry {
                "retryable_failure"
            } else {
                "job_failed"
            },
        );
        self.change(kind, |w| {
            w.active = w.active.saturating_sub(1);
            if success {
                w.completed += 1;
                w.last_progress_at = Some(now);
                w.consecutive_failures = 0;
                w.stalled = false;
            } else {
                w.consecutive_failures += 1;
                w.last_failure = Some(category.into());
                if retry {
                    w.retryable_failures += 1;
                } else {
                    w.failed += 1;
                }
            }
            if w.active == 0 {
                w.operation_started_at = None;
                w.state = if retry { "backing_off" } else { "idle" }.into();
            }
            w.stalled = w.queued.unwrap_or(0) > 0
                && w.consecutive_failures > 0
                && now.saturating_sub(
                    w.last_progress_at
                        .unwrap_or(now.saturating_sub(self.started.elapsed().as_secs() as usize)),
                ) >= 300;
        });
    }
    pub fn state(&self, kind: &str, state: &'static str) {
        self.change(kind, |w| {
            if w.active == 0 {
                w.state = state.into();
            }
        });
    }
    pub fn pending_retry(&self, kind: &str, count: usize, next: Option<usize>) {
        self.queue(kind, count, None);
        self.change(kind, |w| {
            w.next_retry_at = next;
            if next.is_some_and(|deadline| deadline > super::now() as usize) {
                w.stalled = false;
            }
            if w.active == 0 && w.state != "disabled" {
                w.state = if count > 0 { "backing_off" } else { "idle" }.into();
            }
        });
    }
    pub fn backoff(&self, kind: &str) -> Duration {
        let mut seconds = 20;
        self.change(kind, |w| {
            if w.consecutive_failures > 0 && w.state != "disabled" {
                seconds = retry_seconds(
                    w.consecutive_failures as u64,
                    super::now() as u64 ^ std::process::id() as u64,
                );
                w.state = if w.last_failure.as_deref() == Some("source_read") {
                    "waiting_for_source"
                } else {
                    "backing_off"
                }
                .into();
                w.next_retry_at = Some(super::now() as usize + seconds as usize);
            }
        });
        Duration::from_secs(seconds)
    }
    fn push_sample(&self, mut sample: ResourceSample) {
        let mut d = self.data.lock().unwrap();
        if sample.session == self.session {
            sample.active_workers = d
                .workers
                .values()
                .filter(|w| w.active > 0)
                .map(|w| w.kind.clone())
                .collect();
            sample.backlog = d
                .workers
                .values()
                .filter_map(|w| w.queued.map(|n| (w.kind.clone(), n)))
                .collect();
        }
        d.history.push_back(sample);
        while d.history.len() > HISTORY_LIMIT {
            d.history.pop_front();
        }
    }
    fn queue(&self, kind: &str, queued: usize, oldest: Option<usize>) {
        self.change(kind, |w| {
            w.queued = Some(queued);
            w.queue_observed_at = Some(super::now() as usize);
            w.oldest_queued_at = oldest;
            w.stalled = queued > 0
                && w.state != "disabled"
                && w.state != "waiting_for_schedule"
                && (w.consecutive_failures > 0 || w.state == "stopped")
                && (super::now() as usize).saturating_sub(
                    w.last_progress_at.unwrap_or(
                        (super::now() as usize)
                            .saturating_sub(self.started.elapsed().as_secs() as usize),
                    ),
                ) >= 300;
        });
    }
}

/// A work-item attempt is distinct from the long-lived worker task.
pub struct Job {
    kind: &'static str,
    finished: bool,
}
impl Job {
    pub fn new(kind: &'static str) -> Self {
        begin(kind);
        Self {
            kind,
            finished: false,
        }
    }
    pub fn success(&mut self) {
        if !self.finished {
            finish(self.kind, true, false, "");
            self.finished = true;
        }
    }
    pub fn fail(&mut self, retry: bool, category: &'static str) {
        if !self.finished {
            finish(self.kind, false, retry, category);
            self.finished = true;
        }
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        if !self.finished {
            self.fail(
                true,
                if std::thread::panicking() {
                    "item_panicked"
                } else {
                    "interrupted_or_failed"
                },
            );
        }
    }
}

pub struct WorkerGuard {
    runtime: Option<Arc<Runtime>>,
    kind: &'static str,
}
pub fn worker(kind: &'static str) -> WorkerGuard {
    let runtime = super::global().map(|d| d.runtime());
    if let Some(r) = &runtime {
        r.change(kind, |w| {
            if w.starts > 0 {
                w.restarts += 1;
            }
            w.starts += 1;
            w.state = "idle".into();
        });
        r.event(kind, "worker_started");
    }
    WorkerGuard { runtime, kind }
}
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if let Some(r) = &self.runtime {
            let panic = std::thread::panicking();
            r.change(self.kind, |w| {
                w.exits += 1;
                w.active = 0;
                w.operation_started_at = None;
                w.panics += usize::from(panic);
                w.state = "stopped".into();
            });
            r.event(
                self.kind,
                if panic {
                    "worker_panicked"
                } else {
                    "worker_exited"
                },
            );
        }
    }
}
pub fn begin(kind: &str) {
    if let Some(d) = super::global() {
        d.runtime().begin(kind);
    }
}
pub fn finish(kind: &str, success: bool, retry: bool, category: &'static str) {
    if let Some(d) = super::global() {
        d.runtime().finish(kind, success, retry, category);
    }
}
pub fn state(kind: &str, state: &'static str) {
    if let Some(d) = super::global() {
        d.runtime().state(kind, state);
    }
}
pub fn pending_retry(kind: &str, count: usize, next: Option<usize>) {
    if let Some(d) = super::global() {
        d.runtime().pending_retry(kind, count, next);
    }
}
pub fn idle_delay(kind: &str) -> Duration {
    super::global()
        .map(|d| d.runtime().backoff(kind))
        .unwrap_or(Duration::from_secs(20))
}

/// Failed passes must back off even if their queue still has targets.
pub async fn retry_if_failed(kind: &str) {
    let failed = super::global().is_some_and(|d| {
        d.runtime()
            .snapshot(false)
            .workers
            .iter()
            .any(|w| w.kind == kind && w.consecutive_failures > 0)
    });
    if failed {
        tokio::time::sleep(idle_delay(kind)).await;
    }
}

fn kib_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name == key)
            .then(|| {
                value
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()?
                    .checked_mul(1024)
            })
            .flatten()
    })
}
fn retry_seconds(failures: u64, jitter: u64) -> u64 {
    (20u64.saturating_mul(1u64 << failures.saturating_sub(1).min(4))).min(300) + jitter % 6
}
fn counter(text: &str, key: &str) -> Option<usize> {
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()?.trim_end_matches(':') == key)
            .then(|| words.next()?.parse().ok())
            .flatten()
    })
}
fn sample(session: &str, uptime: usize) -> (ResourceSample, Option<f64>) {
    let mut s = ResourceSample {
        at: super::now() as usize,
        session: session.into(),
        uptime_seconds: uptime,
        version: env!("CARGO_PKG_VERSION").into(),
        revision: env!("MUSICATA_GIT_SHA").into(),
        ..Default::default()
    };
    let mut cpu = None;
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        s.rss_bytes = kib_field(&status, "VmRSS").map(|v| v as usize);
        s.swap_bytes = kib_field(&status, "VmSwap").map(|v| v as usize);
        s.threads = counter(&status, "Threads");
        let io = std::fs::read_to_string("/proc/self/io").unwrap_or_default();
        s.read_bytes = counter(&io, "read_bytes");
        s.write_bytes = counter(&io, "write_bytes");
        if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
            if let Some((_, rest)) = stat.rsplit_once(')') {
                let fields: Vec<_> = rest.split_whitespace().collect();
                let ticks = fields
                    .get(11)
                    .and_then(|x| x.parse::<u64>().ok())
                    .zip(fields.get(12).and_then(|x| x.parse::<u64>().ok()));
                let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
                if hz > 0 {
                    cpu = ticks.map(|(a, b)| (a + b) as f64 / hz as f64);
                }
            }
        }
        // cgroup v2 path differs between a host process and a container namespace.
        if let Ok(cgroup) = std::fs::read_to_string("/proc/self/cgroup") {
            if let Some(relative) = cgroup.lines().find_map(|l| l.strip_prefix("0::")) {
                if !relative.split('/').any(|part| part == "..") {
                    let root = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
                    s.container_bytes = std::fs::read_to_string(root.join("memory.current"))
                        .ok()
                        .and_then(|v| v.trim().parse().ok());
                    s.container_limit_bytes = std::fs::read_to_string(root.join("memory.max"))
                        .ok()
                        .and_then(|v| v.trim().parse().ok());
                }
            }
        }
        if tikv_jemalloc_ctl::epoch::advance().is_ok() {
            s.allocated_bytes = tikv_jemalloc_ctl::stats::allocated::read().ok();
            s.allocator_resident_bytes = tikv_jemalloc_ctl::stats::resident::read().ok();
            s.allocator_retained_bytes = tikv_jemalloc_ctl::stats::retained::read().ok();
        }
    }
    (s, cpu)
}
fn checkpoint_path(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(".runtime.json");
    p.into()
}
fn checkpoint(runtime: &Runtime, db: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let bytes = serde_json::to_vec(&runtime.snapshot(true))?;
    if bytes.len() as u64 > CHECKPOINT_LIMIT {
        return Err(std::io::Error::other("runtime history exceeds limit"));
    }
    let path = checkpoint_path(db);
    let tmp = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(&bytes)?;
    std::fs::rename(tmp, path)
}
pub fn start(runtime: Arc<Runtime>, database: Database, db_path: PathBuf) {
    tokio::spawn(async move {
        let path = checkpoint_path(&db_path);
        if tokio::fs::metadata(&path)
            .await
            .is_ok_and(|m| m.len() <= CHECKPOINT_LIMIT)
        {
            if let Ok(bytes) = tokio::fs::read(path).await {
                if let Ok(old) = serde_json::from_slice::<RuntimeSnapshot>(&bytes) {
                    for event in old
                        .events
                        .into_iter()
                        .filter(|e| e.at > (super::now() as usize).saturating_sub(86400))
                        .rev()
                        .take(EVENT_LIMIT)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                    {
                        runtime.data.lock().unwrap().events.push_back(event);
                    }
                    for sample in old
                        .history
                        .into_iter()
                        .filter(|s| s.at > (super::now() as usize).saturating_sub(86400))
                        .take(HISTORY_LIMIT)
                    {
                        runtime.push_sample(sample);
                    }
                }
            }
        }
        runtime.event("server", "server_started");
        let mut previous: Option<(Instant, f64)> = None;
        let mut ticks = 0;
        loop {
            let session = runtime.session.clone();
            let uptime = runtime.started.elapsed().as_secs() as usize;
            if let Ok((mut s, cpu)) =
                tokio::task::spawn_blocking(move || sample(&session, uptime)).await
            {
                let now = Instant::now();
                if let (Some(cpu), Some((then, old))) = (cpu, previous) {
                    s.cpu_percent = Some(
                        ((cpu - old).max(0.0) / now.duration_since(then).as_secs_f64()) * 100.0,
                    );
                }
                previous = cpu.map(|cpu| (now, cpu));
                runtime.push_sample(s);
            }
            if let Ok(Ok(Some(queues))) = tokio::time::timeout(
                Duration::from_secs(2),
                database.poll_diagnostic_queues(super::now() as i64 - 7 * 24 * 60 * 60),
            )
            .await
            {
                for (kind, count, oldest) in queues {
                    runtime.queue(kind, count, oldest);
                }
            }
            ticks += 1;
            if ticks % 10 == 0 {
                let r = runtime.clone();
                let path = db_path.clone();
                let error = tokio::task::spawn_blocking(move || checkpoint(&r, &path))
                    .await
                    .map_or(true, |r| r.is_err());
                runtime.data.lock().unwrap().checkpoint_error = error;
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prewarm_retry_snapshot_is_pending_work_not_attempt_totals() {
        let r = Runtime {
            started: Instant::now() - Duration::from_secs(600),
            ..Default::default()
        };
        r.begin("prewarm");
        r.finish("prewarm", false, true, "cover_unavailable");
        let next = super::super::now() as usize + 300;
        r.pending_retry("prewarm", 2, Some(next));
        let w = r.snapshot(false).workers.remove(0);
        assert_eq!(w.queued, Some(2));
        assert_eq!(w.retryable_failures, 1);
        assert_eq!(w.next_retry_at, Some(next));
        assert_eq!(w.state, "backing_off");
        assert!(!w.stalled, "scheduled retries are not stalled work");
        r.pending_retry("prewarm", 0, None);
        let w = r.snapshot(false).workers.remove(0);
        assert_eq!(w.queued, Some(0));
        assert_eq!(w.next_retry_at, None);
        assert_eq!(w.state, "idle");
        r.state("prewarm", "disabled");
        r.pending_retry("prewarm", 2, Some(next));
        assert_eq!(r.snapshot(false).workers[0].state, "disabled");
    }
    #[test]
    fn health_distinguishes_attempts_failures_and_worker_restarts() {
        let r = Runtime::default();
        r.begin("loudness");
        r.finish("loudness", false, true, "source_read");
        let w = r.snapshot(false).workers.remove(0);
        assert_eq!(w.attempts, 1);
        assert_eq!(w.retryable_failures, 1);
        assert_eq!(w.completed, 0);
        assert_eq!(w.restarts, 0);
        r.begin("loudness");
        r.finish("loudness", true, false, "");
        let w = r.snapshot(false).workers.remove(0);
        assert_eq!(w.attempts, 2);
        assert_eq!(w.completed, 1);
        assert_eq!(w.consecutive_failures, 0);
        assert_eq!(w.active, 0);
    }
    #[test]
    fn queue_refresh_detects_stopped_workers_without_new_job_events() {
        let r = Runtime {
            started: Instant::now() - Duration::from_secs(600),
            ..Default::default()
        };
        r.state("loudness", "stopped");
        r.queue("loudness", 325, Some(1));
        assert!(r.snapshot(false).workers[0].stalled);
        r.queue("loudness", 0, None);
        assert!(!r.snapshot(false).workers[0].stalled);
        r.state("loudness", "disabled");
        r.queue("loudness", 325, Some(1));
        assert!(!r.snapshot(false).workers[0].stalled);
    }
    #[test]
    fn resource_history_and_worker_events_remain_bounded() {
        let r = Runtime::default();
        for _ in 0..HISTORY_LIMIT + 10 {
            r.push_sample(ResourceSample::default());
        }
        for _ in 0..EVENT_LIMIT + 10 {
            r.event("scan", "job_started");
        }
        r.begin("untrusted arbitrary identifier");
        let s = r.snapshot(true);
        assert_eq!(s.history.len(), HISTORY_LIMIT);
        assert_eq!(s.events.len(), EVENT_LIMIT);
        assert!(s.workers.is_empty());
    }

    #[test]
    fn proc_memory_fields_are_bytes_and_missing_is_unknown() {
        let status = "VmRSS:\t123 kB\nVmSwap:\t0 kB\n";
        assert_eq!(kib_field(status, "VmRSS"), Some(125952));
        assert_eq!(kib_field(status, "VmSwap"), Some(0));
        assert_eq!(kib_field(status, "missing"), None);
        assert_eq!(kib_field("VmRSS: nonsense", "VmRSS"), None);
    }
    #[test]
    fn retries_back_off_with_bounded_jitter() {
        assert_eq!(retry_seconds(1, 0), 20);
        assert!(retry_seconds(2, 0) > retry_seconds(1, 0));
        assert!(retry_seconds(u64::MAX, u64::MAX) <= 305);
        assert_ne!(retry_seconds(3, 1), retry_seconds(3, 2));
    }
}
