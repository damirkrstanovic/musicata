// SPDX-License-Identifier: AGPL-3.0-or-later
//! Separate diagnostic persistence: never migrates or locks the library database.
use anyhow::Result;
use musicata_core::diagnostics::{PerformanceSummary, RecordedEvent};
use std::path::Path;

#[derive(Clone, Default)]
pub struct DiagnosticBatch {
    pub events: Vec<RecordedEvent>,
    pub summaries: Vec<PerformanceSummary>,
    pub dropped: u64,
    pub storage_gaps: u64,
    pub reported_lost: u64,
}
#[derive(Default)]
pub struct DiagnosticSnapshot {
    pub evicted: u64,
    pub incidents: Vec<serde_json::Value>,
    pub transitions: Vec<serde_json::Value>,
    pub performance: Vec<serde_json::Value>,
    pub dropped: u64,
    pub storage_gaps: u64,
    pub reported_lost: u64,
}
#[derive(Clone)]
pub struct DiagnosticStore {
    pool: sqlx::SqlitePool,
    path: std::path::PathBuf,
    clock: std::sync::Arc<std::sync::Mutex<(i64, std::time::Instant)>>,
}
impl DiagnosticStore {
    pub async fn open(path: &Path) -> Result<Self> {
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file_options = std::fs::OpenOptions::new();
            file_options.write(true).create(true).mode(0o600);
            tokio::fs::OpenOptions::from(file_options)
                .open(path)
                .await?;
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_millis(100))
            .pragma("auto_vacuum", "INCREMENTAL")
            .pragma("max_page_count", "16384")
            .pragma("journal_size_limit", "1048576")
            .pragma("wal_autocheckpoint", "128");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let schema: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await?;
        anyhow::ensure!(schema <= 1, "diagnostic schema is newer than this server");
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS totals (key TEXT PRIMARY KEY,value INTEGER NOT NULL);
            INSERT OR IGNORE INTO totals VALUES('storage_gaps',0),('reported_lost',0),('evicted',0),('clock_high_water',0);
            CREATE TABLE IF NOT EXISTS incidents (
            id INTEGER PRIMARY KEY, key TEXT NOT NULL, first INTEGER NOT NULL,
            last INTEGER NOT NULL, count INTEGER NOT NULL, recovered_at INTEGER,
            data TEXT NOT NULL);
            CREATE UNIQUE INDEX IF NOT EXISTS incident_active ON incidents(key) WHERE recovered_at IS NULL;
            CREATE TABLE IF NOT EXISTS transitions (id INTEGER PRIMARY KEY, time INTEGER NOT NULL, output TEXT, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS performance (operation TEXT NOT NULL, minute INTEGER NOT NULL, data TEXT NOT NULL, PRIMARY KEY(operation, minute));
            CREATE TABLE IF NOT EXISTS health (id INTEGER PRIMARY KEY CHECK(id=1), dropped INTEGER NOT NULL);
            INSERT OR IGNORE INTO health VALUES(1,0); PRAGMA user_version=1;")
            .execute(&pool).await?;
        let high_water: i64 =
            sqlx::query_scalar("SELECT value FROM totals WHERE key='clock_high_water'")
                .fetch_one(&pool)
                .await?;
        Ok(Self {
            pool,
            path: path.to_owned(),
            clock: std::sync::Arc::new(std::sync::Mutex::new((
                high_water,
                std::time::Instant::now(),
            ))),
        })
    }
    async fn checkpoint(&self) -> Result<()> {
        use sqlx::Row;
        let result = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&self.pool)
            .await?;
        anyhow::ensure!(result.get::<i64, _>(0) == 0, "diagnostic checkpoint busy");
        Ok(())
    }
    async fn ensure_wal_room(&self) -> Result<()> {
        let mut wal = self.path.as_os_str().to_os_string();
        wal.push("-wal");
        let wal = std::path::PathBuf::from(wal);
        if tokio::fs::metadata(&wal)
            .await
            .map(|m| m.len())
            .unwrap_or(0)
            > 4 * 1024 * 1024
        {
            self.checkpoint().await?;
            anyhow::ensure!(
                tokio::fs::metadata(wal).await.map(|m| m.len()).unwrap_or(0) <= 4 * 1024 * 1024,
                "diagnostic WAL busy"
            );
        }
        Ok(())
    }
    pub async fn write_batch(&self, batch: &DiagnosticBatch) -> Result<()> {
        use musicata_core::diagnostics::DiagnosticAction;
        anyhow::ensure!(batch.events.len() <= 128, "diagnostic batch exceeds limit");
        self.ensure_wal_room().await?;
        anyhow::ensure!(
            batch.summaries.len() <= 256,
            "diagnostic summaries exceed limit"
        );
        for summary in &batch.summaries {
            anyhow::ensure!(
                summary.operation.len() <= 32 && serde_json::to_vec(summary)?.len() <= 4096,
                "diagnostic summary exceeds limit"
            );
        }
        let mut tx = self.pool.begin().await?;
        let mut extra_dropped = 0u64;
        let mut evicted = 0u64;
        for record in &batch.events {
            let event = &record.event;
            let data = serde_json::to_string(record)?;
            anyhow::ensure!(data.len() <= 4096, "diagnostic event exceeds limit");
            let key = serde_json::to_string(&(&event.category, &event.component, &event.context))?;
            match event.action {
                DiagnosticAction::Failure => {
                    let exists: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM incidents WHERE key=? AND recovered_at IS NULL",
                    )
                    .bind(&key)
                    .fetch_one(&mut *tx)
                    .await?;
                    if exists == 0 {
                        let active: i64 = sqlx::query_scalar(
                            "SELECT COUNT(*) FROM incidents WHERE recovered_at IS NULL",
                        )
                        .fetch_one(&mut *tx)
                        .await?;
                        if active >= 256 {
                            extra_dropped += 1;
                            continue;
                        }
                    }
                    sqlx::query("INSERT INTO incidents(key,first,last,count,data) VALUES(?,?,?,1,?) ON CONFLICT(key) WHERE recovered_at IS NULL DO UPDATE SET last=MAX(last,excluded.last),count=count+1,data=excluded.data")
                        .bind(&key).bind(record.timestamp).bind(record.timestamp).bind(&data).execute(&mut *tx).await?;
                }
                DiagnosticAction::Recovery => {
                    sqlx::query("UPDATE incidents SET recovered_at=MAX(last,?) WHERE key=? AND recovered_at IS NULL")
                        .bind(record.timestamp).bind(&key).execute(&mut *tx).await?;
                }
                DiagnosticAction::Transition | DiagnosticAction::Detail => {
                    sqlx::query("INSERT INTO transitions(time,output,data) VALUES(?,?,?)")
                        .bind(record.timestamp)
                        .bind(&event.context.output_id)
                        .bind(&data)
                        .execute(&mut *tx)
                        .await?;
                    evicted+=sqlx::query("DELETE FROM transitions WHERE id IN (SELECT id FROM transitions WHERE output IS ? ORDER BY id DESC LIMIT -1 OFFSET 32)")
                        .bind(&event.context.output_id).execute(&mut *tx).await?.rows_affected();
                }
            }
        }
        for summary in &batch.summaries {
            let old: Option<String> =
                sqlx::query_scalar("SELECT data FROM performance WHERE operation=? AND minute=?")
                    .bind(&summary.operation)
                    .bind(summary.minute)
                    .fetch_optional(&mut *tx)
                    .await?;
            let mut merged = summary.clone();
            if let Some(old) = old {
                let old: PerformanceSummary = serde_json::from_str(&old)?;
                merged.count = merged.count.saturating_add(old.count);
                merged.total_ms = merged.total_ms.saturating_add(old.total_ms);
                merged.max_ms = merged.max_ms.max(old.max_ms);
                merged.slow_count = merged.slow_count.saturating_add(old.slow_count);
                for (a, b) in merged.buckets.iter_mut().zip(old.buckets) {
                    *a = a.saturating_add(b);
                }
            }
            sqlx::query("INSERT OR REPLACE INTO performance VALUES(?,?,?)")
                .bind(&merged.operation)
                .bind(merged.minute)
                .bind(serde_json::to_string(&merged)?)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE health SET dropped=dropped+? WHERE id=1")
            .bind(
                batch
                    .dropped
                    .saturating_add(extra_dropped)
                    .min(i64::MAX as u64) as i64,
            )
            .execute(&mut *tx)
            .await?;
        evicted+=sqlx::query("DELETE FROM transitions WHERE id IN (SELECT id FROM transitions ORDER BY id DESC LIMIT -1 OFFSET 512)").execute(&mut *tx).await?.rows_affected();
        evicted+=sqlx::query("DELETE FROM incidents WHERE id IN (SELECT id FROM incidents ORDER BY last DESC,id DESC LIMIT -1 OFFSET 4096)").execute(&mut *tx).await?.rows_affected();
        evicted+=sqlx::query("DELETE FROM performance WHERE rowid IN (SELECT rowid FROM performance ORDER BY minute DESC LIMIT -1 OFFSET 32768)").execute(&mut *tx).await?.rows_affected();
        for (key, value) in [
            ("storage_gaps", batch.storage_gaps),
            ("reported_lost", batch.reported_lost),
            ("evicted", evicted),
        ] {
            sqlx::query("UPDATE totals SET value=value+? WHERE key=?")
                .bind(value.min(i64::MAX as u64) as i64)
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
        let observed = batch
            .events
            .iter()
            .map(|record| record.timestamp)
            .chain(
                batch
                    .summaries
                    .iter()
                    .map(|summary| summary.minute.saturating_mul(60)),
            )
            .max()
            .unwrap_or(0);
        sqlx::query("UPDATE totals SET value=MAX(value,?) WHERE key='clock_high_water'")
            .bind(observed)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn snapshot(&self, since: i64) -> Result<DiagnosticSnapshot> {
        use sqlx::Row;
        let mut snapshot = DiagnosticSnapshot::default();
        let mut tx = self.pool.begin().await?;
        for row in sqlx::query("SELECT * FROM incidents WHERE last>=? OR recovered_at>=? ORDER BY last DESC,id DESC LIMIT 4096")
            .bind(since).bind(since).fetch_all(&mut *tx).await? {
            let record: RecordedEvent = serde_json::from_str(row.get("data"))?;
            snapshot.incidents.push(serde_json::json!({"id":row.get::<i64,_>("id"),"category":record.event.category,
                "component":record.event.component,"context":record.event.context,"message":record.event.message,"measurements":record.event.measurements,
                "severity":"error","version":record.version,"session":record.session,
                "first":row.get::<i64,_>("first"),"last":row.get::<i64,_>("last"),"count":row.get::<i64,_>("count"),
                "recovered_at":row.get::<Option<i64>,_>("recovered_at")}));
        }
        for data in sqlx::query_scalar::<_, String>(
            "SELECT data FROM transitions WHERE time>=? ORDER BY id DESC LIMIT 512",
        )
        .bind(since)
        .fetch_all(&mut *tx)
        .await?
        {
            snapshot.transitions.push(serde_json::from_str(&data)?);
        }
        for data in sqlx::query_scalar::<_, String>(
            "SELECT data FROM performance WHERE minute>=? ORDER BY minute DESC LIMIT 32768",
        )
        .bind((since / 300) * 5)
        .fetch_all(&mut *tx)
        .await?
        {
            snapshot.performance.push(serde_json::from_str(&data)?);
        }
        snapshot.dropped = sqlx::query_scalar::<_, i64>("SELECT dropped FROM health WHERE id=1")
            .fetch_one(&mut *tx)
            .await? as u64;
        for (key, value) in [
            ("storage_gaps", &mut snapshot.storage_gaps),
            ("reported_lost", &mut snapshot.reported_lost),
            ("evicted", &mut snapshot.evicted),
        ] {
            *value = sqlx::query_scalar::<_, i64>("SELECT value FROM totals WHERE key=?")
                .bind(key)
                .fetch_one(&mut *tx)
                .await? as u64;
        }
        tx.commit().await?;
        Ok(snapshot)
    }
    pub async fn prune(&self, now: i64) -> Result<()> {
        self.ensure_wal_room().await?;
        let persisted: i64 =
            sqlx::query_scalar("SELECT value FROM totals WHERE key='clock_high_water'")
                .fetch_one(&self.pool)
                .await?;
        let retained_now = {
            let mut clock = self.clock.lock().expect("diagnostic retention clock");
            let value = now.max(persisted).max(
                clock
                    .0
                    .saturating_add(clock.1.elapsed().as_secs().min(i64::MAX as u64) as i64),
            );
            *clock = (value, std::time::Instant::now());
            value
        };
        let cutoff = retained_now.saturating_sub(14 * 86400);
        for (sql, value) in [
            (
                "DELETE FROM incidents WHERE id IN (SELECT id FROM incidents WHERE MAX(last,COALESCE(recovered_at,last))<? LIMIT 128)",
                cutoff,
            ),
            (
                "DELETE FROM transitions WHERE id IN (SELECT id FROM transitions WHERE time<? LIMIT 128)",
                cutoff,
            ),
            (
                "DELETE FROM performance WHERE rowid IN (SELECT rowid FROM performance WHERE minute<? LIMIT 128)",
                cutoff / 60,
            ),
        ] {
            loop {
                self.ensure_wal_room().await?;
                let mut tx = self.pool.begin().await?;
                sqlx::query("UPDATE totals SET value=MAX(value,?) WHERE key='clock_high_water'")
                    .bind(retained_now)
                    .execute(&mut *tx)
                    .await?;
                let removed = sqlx::query(sql)
                    .bind(value)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                sqlx::query("UPDATE totals SET value=value+? WHERE key='evicted'")
                    .bind(removed as i64)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                self.checkpoint().await?;
                if removed < 128 {
                    break;
                }
            }
        }
        self.ensure_wal_room().await?;
        sqlx::query("PRAGMA incremental_vacuum(128)")
            .execute(&self.pool)
            .await?;
        self.checkpoint().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use musicata_core::diagnostics::{DiagnosticAction, DiagnosticContext, DiagnosticEvent};
    fn path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "musicata-diagnostics-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    fn batch(action: DiagnosticAction, time: i64) -> DiagnosticBatch {
        DiagnosticBatch {
            events: vec![RecordedEvent {
                event: DiagnosticEvent {
                    category: "mpd.command".into(),
                    component: "mpd".into(),
                    action,
                    context: DiagnosticContext::default(),
                    message: "connection refused".into(),
                    measurements: Default::default(),
                },
                timestamp: time,
                version: "test".into(),
                session: "session-1".into(),
            }],
            ..Default::default()
        }
    }
    #[tokio::test]
    async fn incidents_survive_restart_and_aggregate_with_recovery() {
        let path = path();
        let store = DiagnosticStore::open(&path).await.unwrap();
        store
            .write_batch(&batch(DiagnosticAction::Failure, 100))
            .await
            .unwrap();
        store
            .write_batch(&batch(DiagnosticAction::Failure, 101))
            .await
            .unwrap();
        store
            .write_batch(&batch(DiagnosticAction::Recovery, 102))
            .await
            .unwrap();
        store
            .write_batch(&batch(DiagnosticAction::Recovery, 103))
            .await
            .unwrap();
        drop(store);
        let store = DiagnosticStore::open(&path).await.unwrap();
        let snapshot = store.snapshot(0).await.unwrap();
        assert_eq!(snapshot.incidents.len(), 1);
        assert_eq!(snapshot.incidents[0]["count"], 2);
        assert_eq!(snapshot.incidents[0]["first"], 100);
        assert_eq!(snapshot.incidents[0]["last"], 101);
        assert_eq!(snapshot.incidents[0]["recovered_at"], 102);
        store
            .write_batch(&batch(DiagnosticAction::Failure, 104))
            .await
            .unwrap();
        assert_eq!(store.snapshot(0).await.unwrap().incidents.len(), 2);
    }
    #[tokio::test]
    async fn retention_removes_expired_evidence_but_preserves_recent() {
        let store = DiagnosticStore::open(&path()).await.unwrap();
        store
            .write_batch(&batch(DiagnosticAction::Failure, 1))
            .await
            .unwrap();
        store
            .write_batch(&batch(DiagnosticAction::Recovery, 2))
            .await
            .unwrap();
        store
            .write_batch(&batch(DiagnosticAction::Failure, 2_000_000))
            .await
            .unwrap();
        store.prune(2_000_001).await.unwrap();
        let snapshot = store.snapshot(0).await.unwrap();
        assert_eq!(snapshot.incidents.len(), 1);
        assert_eq!(snapshot.incidents[0]["first"], 2_000_000);
    }
    #[tokio::test]
    async fn separate_schema_and_bounded_sidecars() {
        let path = path();
        let store = DiagnosticStore::open(&path).await.unwrap();
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table'")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert!(!tables.iter().any(|name| name == "tracks"));
        for time in 0..200 {
            store
                .write_batch(&batch(DiagnosticAction::Failure, time))
                .await
                .unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() <= 64 * 1024 * 1024);
        let wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
        assert!(std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0) <= 8 * 1024 * 1024);
        let limit: i64 = sqlx::query_scalar("PRAGMA max_page_count")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(limit, 16384);
    }
    #[tokio::test]
    async fn oversized_batch_rolls_back_without_partial_evidence() {
        let store = DiagnosticStore::open(&path()).await.unwrap();
        let mut b = batch(DiagnosticAction::Failure, 100);
        let mut huge = b.events[0].clone();
        huge.event.message = "x".repeat(5000);
        b.events.push(huge);
        assert!(store.write_batch(&b).await.is_err());
        assert!(store.snapshot(0).await.unwrap().incidents.is_empty());
    }
    #[tokio::test]
    async fn transition_history_is_bounded_per_output() {
        let store = DiagnosticStore::open(&path()).await.unwrap();
        for time in 0..80 {
            store
                .write_batch(&batch(DiagnosticAction::Transition, time))
                .await
                .unwrap();
        }
        let snapshot = store.snapshot(0).await.unwrap();
        assert_eq!(snapshot.transitions.len(), 32);
        assert_eq!(snapshot.evicted, 48);
    }
    #[tokio::test]
    async fn backward_clock_does_not_move_incident_last_backwards() {
        let store = DiagnosticStore::open(&path()).await.unwrap();
        store
            .write_batch(&batch(DiagnosticAction::Failure, 200))
            .await
            .unwrap();
        store
            .write_batch(&batch(DiagnosticAction::Failure, 100))
            .await
            .unwrap();
        assert_eq!(store.snapshot(0).await.unwrap().incidents[0]["last"], 200);
    }
    #[tokio::test]
    async fn performance_input_cannot_bypass_record_bounds() {
        let store = DiagnosticStore::open(&path()).await.unwrap();
        let b = DiagnosticBatch {
            summaries: vec![PerformanceSummary {
                operation: "x".repeat(5000),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(store.write_batch(&b).await.is_err());
    }

    #[tokio::test]
    async fn full_store_rolls_back_then_reclaims_expired_rows() {
        let store = DiagnosticStore::open(&path()).await.unwrap();
        let current: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        sqlx::query(&format!("PRAGMA max_page_count={}", current + 4))
            .execute(&store.pool)
            .await
            .unwrap();
        let mut written = 0;
        for index in 0..256 {
            let mut b = batch(DiagnosticAction::Failure, 1);
            b.events[0].event.context.output_id = Some(format!("output-{index}"));
            b.events[0].event.message = "x".repeat(3000);
            if store.write_batch(&b).await.is_err() {
                break;
            }
            written += 1;
        }
        assert!(written > 0 && written < 256);
        assert_eq!(store.snapshot(0).await.unwrap().incidents.len(), written);
        store.prune(2_000_000).await.unwrap();
        store
            .write_batch(&batch(DiagnosticAction::Failure, 2_000_000))
            .await
            .unwrap();
        assert_eq!(store.snapshot(0).await.unwrap().incidents.len(), 1);
    }

    #[tokio::test]
    async fn maintenance_stops_when_reader_blocks_checkpoint() {
        let path = path();
        let store = DiagnosticStore::open(&path).await.unwrap();
        for index in 0..64 {
            let mut b = batch(DiagnosticAction::Failure, 1);
            b.events[0].event.context.output_id = Some(format!("output-{index}"));
            b.events[0].event.message = "x".repeat(3000);
            store.write_batch(&b).await.unwrap();
        }
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&store.pool)
            .await
            .unwrap();
        let other = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        let mut reader = other.begin().await.unwrap();
        let _: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM incidents")
            .fetch_one(&mut *reader)
            .await
            .unwrap();
        assert!(
            store.prune(2_000_000).await.is_err(),
            "busy checkpoint must stop maintenance"
        );
        let wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
        assert!(std::fs::metadata(wal).unwrap().len() <= 8 * 1024 * 1024);
        reader.rollback().await.unwrap();
        store.prune(2_000_000).await.unwrap();
    }
    #[tokio::test]
    async fn rollback_retention_uses_elapsed_time_after_restart() {
        let path = path();
        let store = DiagnosticStore::open(&path).await.unwrap();
        store
            .write_batch(&batch(DiagnosticAction::Failure, 2_000_000))
            .await
            .unwrap();
        store.prune(2_000_010).await.unwrap();
        drop(store);
        let store = DiagnosticStore::open(&path).await.unwrap();
        *store.clock.lock().unwrap() = (
            2_000_010,
            std::time::Instant::now() - std::time::Duration::from_secs(15 * 86400),
        );
        store.prune(100).await.unwrap();
        assert!(store.snapshot(0).await.unwrap().incidents.is_empty());
    }
    #[tokio::test]
    async fn snapshots_retain_root_cause_measurements() {
        let store = DiagnosticStore::open(&path()).await.unwrap();
        let mut b = batch(DiagnosticAction::Failure, 100);
        b.events[0]
            .event
            .measurements
            .insert("source_line".into(), 42);
        store.write_batch(&b).await.unwrap();
        assert_eq!(
            store.snapshot(0).await.unwrap().incidents[0]["measurements"]["source_line"],
            42
        );
    }
    #[tokio::test]
    async fn changing_failure_keys_cannot_grow_active_incidents_unboundedly() {
        let store = DiagnosticStore::open(&path()).await.unwrap();
        for index in 0..300 {
            let mut b = batch(DiagnosticAction::Failure, 100);
            b.events[0].event.context.output_id = Some(format!("output-{index}"));
            store.write_batch(&b).await.unwrap();
        }
        let snapshot = store.snapshot(0).await.unwrap();
        assert_eq!(snapshot.incidents.len(), 256);
        assert_eq!(snapshot.dropped, 44);
    }
}
