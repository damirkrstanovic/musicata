// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fixed pending state per category; the dedicated worker owns network retries.
use crate::Creds;
use musicata_core::diagnostics::{DiagnosticAction, DiagnosticEvent};
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
    mpsc::{SyncSender, sync_channel},
};
static WAKE: OnceLock<SyncSender<()>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);
const CATEGORIES: [&str; 5] = [
    "native.connection",
    "native.decode",
    "native.audio",
    "native.dsp",
    "native.buffering",
];
const CAUSES: [&str; 14] = [
    "operation failed",
    "permission denied",
    "connection refused",
    "operation timed out",
    "resource unavailable",
    "resource busy",
    "storage full",
    "audio decode failed",
    "connection closed",
    "playback interrupted",
    "worker failed",
    "model inference failed",
    "database constraint failed",
    "data corrupted",
];
static STATES: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
const FAILED: u64 = 1 << 6;
const NEED_FAILURE: u64 = 1 << 7;
const NEED_RECOVERY: u64 = 1 << 8;
fn notify() {
    if let Some(tx) = WAKE.get() {
        let _ = tx.try_send(());
    }
}
pub fn failure(category: &str, cause: &str) {
    let Some(index) = CATEGORIES.iter().position(|value| *value == category) else {
        return;
    };
    let cause = musicata_core::diagnostics::safe_cause(cause);
    let code = CAUSES.iter().position(|value| *value == cause).unwrap_or(0) as u64;
    let old = STATES[index]
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
            let needs = if old & FAILED == 0 || old & NEED_FAILURE != 0 {
                NEED_FAILURE
            } else {
                0
            };
            Some(((old >> 9).wrapping_add(1) << 9) | FAILED | needs | code)
        })
        .unwrap();
    if old & FAILED != 0 || old & NEED_RECOVERY != 0 {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    notify();
}
pub fn recovery(category: &str) {
    let Some(index) = CATEGORIES.iter().position(|value| *value == category) else {
        return;
    };
    let changed = STATES[index]
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
            if old & FAILED == 0 {
                return None;
            }
            Some(
                ((old >> 9).wrapping_add(1) << 9)
                    | (old & 63)
                    | (old & NEED_FAILURE)
                    | NEED_RECOVERY,
            )
        })
        .is_ok();
    if changed {
        notify();
    }
}
fn pending_report() -> Option<(usize, u64, DiagnosticEvent)> {
    for (index, slot) in STATES.iter().enumerate() {
        let state = slot.load(Ordering::Acquire);
        let action = if state & NEED_FAILURE != 0 {
            DiagnosticAction::Failure
        } else if state & NEED_RECOVERY != 0 {
            DiagnosticAction::Recovery
        } else {
            continue;
        };
        let message = if action == DiagnosticAction::Failure {
            CAUSES[(state & 63) as usize]
        } else {
            ""
        };
        return Some((
            index,
            state,
            DiagnosticEvent {
                category: CATEGORIES[index].into(),
                component: "native".into(),
                action,
                context: Default::default(),
                message: message.into(),
                measurements: Default::default(),
            },
        ));
    }
    None
}
fn delivered(index: usize, state: u64, action: &DiagnosticAction) {
    let bit = if *action == DiagnosticAction::Failure {
        NEED_FAILURE
    } else {
        NEED_RECOVERY
    };
    let _ =
        STATES[index].compare_exchange(state, state & !bit, Ordering::AcqRel, Ordering::Acquire);
}
pub fn start(creds: &Creds) {
    let (tx, rx) = sync_channel(1);
    if WAKE.set(tx).is_err() {
        return;
    }
    let url = format!(
        "{}/api/players/{}/diagnostics",
        creds.server.trim_end_matches('/'),
        creds.id
    );
    let token = creds.token.clone();
    std::thread::spawn(move || {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(3))
            .build();
        loop {
            if pending_report().is_none() {
                let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
            }
            let Some((index, state, mut event)) = pending_report() else {
                continue;
            };
            let all_lost = DROPPED.swap(0, Ordering::Relaxed);
            let lost = all_lost.min(1_000_000);
            DROPPED.fetch_add(all_lost - lost, Ordering::Relaxed);
            if lost > 0 {
                event.measurements.insert("lost_reports".into(), lost);
            }
            match agent
                .post(&url)
                .set("authorization", &format!("Bearer {token}"))
                .send_json(&event)
            {
                Ok(_) => {
                    delivered(index, state, &event.action);
                    std::thread::sleep(std::time::Duration::from_secs(6));
                }
                Err(error) => {
                    DROPPED.fetch_add(lost, Ordering::Relaxed);
                    let seconds = if matches!(error, ureq::Error::Status(429, _)) {
                        60
                    } else {
                        2
                    };
                    std::thread::sleep(std::time::Duration::from_secs(seconds));
                }
            }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_report_state_is_bounded_and_recovery_survives_failed_delivery() {
        for state in &STATES {
            state.store(0, Ordering::Relaxed);
        }
        DROPPED.store(0, Ordering::Relaxed);
        for _ in 0..100 {
            failure("native.audio", "permission denied /home/alice?token=secret");
        }
        assert_eq!(DROPPED.load(Ordering::Relaxed), 99);
        let (index, state, event) = pending_report().unwrap();
        assert_eq!(event.message, "permission denied");
        assert!(!serde_json::to_string(&event).unwrap().contains("secret"));
        delivered(index, state, &event.action);
        recovery("native.audio");
        let (index, state, event) = pending_report().unwrap();
        assert_eq!(event.action, DiagnosticAction::Recovery);
        // Failed/offline delivery leaves the exact recovery pending without another callback.
        assert_eq!(
            pending_report().unwrap().2.action,
            DiagnosticAction::Recovery
        );
        delivered(index, state, &event.action);
        assert!(pending_report().is_none());
        recovery("native.audio");
        assert!(pending_report().is_none());
        failure("native.audio", "connection closed");
        delivered(
            index,
            STATES[index].load(Ordering::Acquire),
            &DiagnosticAction::Failure,
        );
        recovery("native.audio");
        let (_, old, _) = pending_report().unwrap();
        failure("native.audio", "connection refused");
        delivered(index, old, &DiagnosticAction::Recovery);
        assert_eq!(
            pending_report().unwrap().2.action,
            DiagnosticAction::Failure,
            "old acknowledgement cannot erase a newer failure"
        );
    }
}
