// SPDX-License-Identifier: AGPL-3.0-or-later
//! Queue checkpoints never block playback or its WebSocket. Each output has one
//! writer and one coalesced pending update, so a busy DB cannot accumulate tasks.
use musicata_core::QueueItem;
use musicata_storage::{Database, PlayerPlayback};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub enum QueuePersist {
    Playback(PlayerPlayback),
    Queue(PlayerPlayback, Vec<QueueItem>),
}

impl QueuePersist {
    // A newer cursor must not discard a queue replacement still waiting to save.
    fn merge(self, newer: Self) -> Self {
        match (self, newer) {
            (Self::Queue(_, items), Self::Playback(playback)) => Self::Queue(playback, items),
            (_, newer) => newer,
        }
    }
}

pub enum QueueOwner {
    Player(String),
    Zone(String),
}

#[derive(Default)]
struct Pending {
    update: Option<QueuePersist>,
    stopped: bool,
}

pub struct QueuePersistence {
    pending: Arc<Mutex<Pending>>,
    wake: mpsc::Sender<()>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl QueuePersistence {
    pub fn new(database: Database, owner: QueueOwner) -> Self {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (wake, mut receiver) = mpsc::channel(1);
        let work = pending.clone();
        let worker = tokio::spawn(async move {
            while receiver.recv().await.is_some() {
                loop {
                    let update = {
                        let mut pending = work.lock().expect("queue checkpoint");
                        if pending.stopped {
                            return;
                        }
                        pending.update.take()
                    };
                    let Some(update) = update else { break };
                    let result = match (&owner, &update) {
                        (QueueOwner::Player(id), QueuePersist::Playback(p)) => {
                            database.save_player_playback(id, p).await
                        }
                        (QueueOwner::Player(id), QueuePersist::Queue(p, items)) => {
                            database.save_player_queue(id, p, items).await
                        }
                        (QueueOwner::Zone(id), QueuePersist::Playback(p)) => {
                            database.save_zone_playback(id, p).await
                        }
                        (QueueOwner::Zone(id), QueuePersist::Queue(p, items)) => {
                            database.save_zone_queue(id, p, items).await
                        }
                    };
                    if let Err(error) = result {
                        let id = match &owner {
                            QueueOwner::Player(id) | QueueOwner::Zone(id) => id,
                        };
                        tracing::warn!(output = %id, %error, "queue checkpoint failed; live playback continues");
                        // Keep the latest state, including any unsaved queue change, and
                        // retry without holding a DB connection or an output state lock.
                        {
                            let mut pending = work.lock().expect("queue checkpoint");
                            if pending.stopped {
                                return;
                            }
                            pending.update = Some(match pending.update.take() {
                                Some(newer) => update.merge(newer),
                                None => update,
                            });
                        }
                        if receiver.is_closed() {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        });
        Self {
            pending,
            wake,
            worker: Mutex::new(Some(worker)),
        }
    }

    /// Removal is an administrative operation: wait for an in-flight write before
    /// deleting rows, then reject late submissions from surviving controller handles.
    pub async fn stop(&self) {
        {
            let mut pending = self.pending.lock().expect("queue checkpoint");
            pending.stopped = true;
            pending.update = None;
        }
        let _ = self.wake.try_send(());
        let worker = self.worker.lock().expect("queue writer").take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }
    }

    pub fn submit(&self, update: QueuePersist) {
        let mut pending = self.pending.lock().expect("queue checkpoint");
        if pending.stopped {
            return;
        }
        pending.update = Some(match pending.update.take() {
            Some(older) => older.merge(update),
            None => update,
        });
        // A full channel already contains the wakeup. No unbounded command backlog.
        let _ = self.wake.try_send(());
    }
}
