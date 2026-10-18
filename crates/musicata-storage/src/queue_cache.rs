// SPDX-License-Identifier: AGPL-3.0-or-later
//! Cache only empty background queues and completed diagnostic refreshes. A dedicated
//! read-only connection observes commits from every library writer, including other processes.
use anyhow::Result;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_AGE: Duration = Duration::from_secs(300);
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Token {
    generation: u64,
    version: i64,
    retry_bucket: i64,
}
struct Entry {
    token: Token,
    observed: Instant,
}
#[derive(Clone)]
pub(super) struct QueueCache {
    observer: SqlitePool,
    generation: Arc<AtomicU64>,
    entries: Arc<Mutex<HashMap<&'static str, Entry>>>,
}
impl QueueCache {
    pub async fn connect(options: SqliteConnectOptions) -> Result<Self> {
        let generation = Arc::new(AtomicU64::new(0));
        let next = generation.clone();
        // data_version values are comparable only on the SAME connection. Never use the
        // ordinary read pool; a replacement observer also gets a new generation.
        let observer = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .after_connect(move |_, _| {
                next.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            })
            .connect_with(options.read_only(true).create_if_missing(false))
            .await?;
        Ok(Self {
            observer,
            generation,
            entries: Arc::new(Mutex::new(HashMap::new())),
        })
    }
    pub async fn begin(&self, key: &'static str, retry_before: i64) -> Result<Option<Token>> {
        let mut connection = self.observer.acquire().await?;
        let token = Token {
            generation: self.generation.load(Ordering::SeqCst),
            version: sqlx::query_scalar("PRAGMA data_version")
                .fetch_one(&mut *connection)
                .await?,
            // Time-based negative-cache expiry is checked at least every five minutes.
            retry_bucket: retry_before.div_euclid(300),
        };
        let entries = self.entries.lock().unwrap();
        Ok(
            if entries
                .get(key)
                .is_some_and(|entry| entry.token == token && entry.observed.elapsed() < MAX_AGE)
            {
                None
            } else {
                Some(token)
            },
        )
    }
    pub fn remember(&self, key: &'static str, token: Token) {
        // Capture the token BEFORE the query, so a commit during it invalidates this result.
        self.entries.lock().unwrap().insert(
            key,
            Entry {
                token,
                observed: Instant::now(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn commits_reconnects_and_expiry_invalidate_queue_cache() {
        let path = std::env::temp_dir().join(format!(
            "queue-cache-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let writer = SqlitePool::connect_with(options.clone()).await.unwrap();
        sqlx::query("CREATE TABLE changes (value INTEGER)")
            .execute(&writer)
            .await
            .unwrap();
        let cache = QueueCache::connect(options).await.unwrap();
        let token = cache.begin("test", 1000).await.unwrap().unwrap();
        cache.remember("test", token);
        assert!(cache.begin("test", 1001).await.unwrap().is_none());
        assert!(cache.begin("test", 2000).await.unwrap().is_some());
        // An external writer invalidates even though the observer never writes.
        sqlx::query("INSERT INTO changes VALUES (1)")
            .execute(&writer)
            .await
            .unwrap();
        assert!(cache.begin("test", 1000).await.unwrap().is_some());
        // A commit between begin and remember must not be hidden by the remembered token.
        cache.remember("test", token);
        assert!(cache.begin("test", 1000).await.unwrap().is_some());
        let token = cache.begin("test", 1000).await.unwrap().unwrap();
        cache.remember("test", token);
        cache
            .observer
            .acquire()
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
        let replacement = cache.begin("test", 1000).await.unwrap().unwrap();
        assert_ne!(token.generation, replacement.generation);
        cache.remember("test", replacement);
        cache
            .entries
            .lock()
            .unwrap()
            .get_mut("test")
            .unwrap()
            .observed -= MAX_AGE;
        assert!(cache.begin("test", 1000).await.unwrap().is_some());
    }
}
