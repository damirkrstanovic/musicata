// SPDX-License-Identifier: AGPL-3.0-or-later
//! Durable speaker membership; transport groups are an implementation of user-facing zones.
use crate::{
    AppError, AppState,
    players::PlayerManager,
    snapcast::{SnapClient, SnapControl},
};
use anyhow::{Result, anyhow};
use axum::{
    Json,
    extract::{Path, State},
};
use musicata_storage::Database;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

const MEMBERSHIP: &str = "snapcast.client_zones";
const LEGACY_ZONE: &str = "snapcast.legacy_zone_id";
static ROUTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
type Membership = BTreeMap<String, Option<String>>;

pub async fn lock_routing() -> tokio::sync::MutexGuard<'static, ()> {
    ROUTING.lock().await
}

async fn membership(database: &Database) -> Result<Membership> {
    database
        .get_setting(MEMBERSHIP)
        .await?
        .map(|s| serde_json::from_str(&s).map_err(Into::into))
        .unwrap_or_else(|| Ok(BTreeMap::new()))
}
async fn save(database: &Database, values: &Membership) -> Result<()> {
    database
        .set_setting(MEMBERSHIP, &serde_json::to_string(values)?)
        .await
}

pub async fn decorate(database: &Database, clients: &mut [SnapClient]) -> Result<()> {
    let values = membership(database).await?;
    for client in clients {
        client.zone_id = values.get(&client.id).cloned().flatten();
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct ClientUpdate {
    name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_zone")]
    zone_id: Option<Option<String>>,
}
fn deserialize_zone<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

pub async fn update_client(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(update): Json<ClientUpdate>,
) -> Result<Json<crate::SnapcastStatus>, AppError> {
    let _lock = ROUTING.lock().await;
    let manager = state
        .players
        .snapcast_manager()
        .await
        .ok_or_else(|| AppError::bad_request("Snapcast is not running"))?;
    let control = SnapControl::new(manager.control_addr());
    let observed = control.status().await.map_err(crate::db_error)?;
    if !observed.clients.iter().any(|client| client.id == id) {
        return Err(AppError::not_found("Unknown Snapcast player"));
    }
    if update.name.is_some() && update.zone_id.is_some() {
        return Err(AppError::bad_request(
            "Update the player name and zone separately",
        ));
    }
    if update.name.is_none() && update.zone_id.is_none() {
        return Err(AppError::bad_request("Choose a player name or zone"));
    }
    if let Some(name) = &update.name {
        if name.trim().is_empty() || name.trim().chars().count() > 64 {
            return Err(AppError::bad_request(
                "Player name must contain 1–64 characters",
            ));
        }
    }
    if let Some(Some(zone)) = &update.zone_id {
        if !state
            .players
            .zones()
            .await
            .map_err(crate::db_error)?
            .iter()
            .any(|z| z.id == *zone)
        {
            return Err(AppError::bad_request("Unknown zone"));
        }
        state
            .players
            .ensure_zone_snapcast(zone)
            .await
            .map_err(crate::db_error)?;
    }
    if let Some(zone) = update.zone_id {
        let before = membership(&state.database).await.map_err(crate::db_error)?;
        let mut desired = before.clone();
        desired.insert(id.clone(), zone.clone());
        // Apply before persisting. Failed routing leaves the saved assignment unchanged;
        // restore the prior routes where possible, and report the original error.
        if let Err(error) = apply(&state.players, &desired, Some(&zone)).await {
            let _ = restore_groups(&control, &observed.groups).await;
            return Err(crate::db_error(error));
        }
        if let Err(error) = save(&state.database, &desired).await {
            let _ = restore_groups(&control, &observed.groups).await;
            return Err(crate::db_error(error));
        }
    }
    if let Some(name) = update.name {
        control
            .set_client_name(&id, name.trim())
            .await
            .map_err(crate::db_error)?;
    }
    crate::snapcast_status_after_routing(state).await
}

fn desired_groups(
    values: &Membership,
    clients: &[SnapClient],
) -> BTreeMap<Option<String>, BTreeSet<String>> {
    let known: BTreeSet<_> = clients.iter().map(|c| &c.id).collect();
    let mut groups: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
    for (id, zone) in values {
        if known.contains(id) {
            groups.entry(zone.clone()).or_default().insert(id.clone());
        }
    }
    groups
}

async fn apply(
    players: &PlayerManager,
    values: &Membership,
    force_zone: Option<&Option<String>>,
) -> Result<()> {
    let Some(manager) = players.snapcast_manager().await else {
        return Ok(());
    };
    let control = SnapControl::new(manager.control_addr());
    let mut status = control.status().await?;
    for (zone, desired) in desired_groups(values, &status.clients) {
        let stream = if let Some(zone) = &zone {
            players.ensure_zone_snapcast(zone).await?;
            crate::snapcast::zone_stream_name(zone)
        } else {
            crate::snapcast::STREAM_NAME.to_string()
        };
        // An anchor always exists for a known client. SetClients splits unwanted members
        // into separate groups and moves requested clients atomically inside Snapserver.
        let group = status
            .groups
            .iter()
            .find(|g| g.clients.iter().any(|c| desired.contains(&c.id)))
            .ok_or_else(|| anyhow!("Snapcast player disappeared during zone assignment"))?;
        let id = group.id.clone();
        let current: BTreeSet<_> = group.clients.iter().map(|c| c.id.clone()).collect();
        let changed = current != desired;
        if changed {
            control
                .set_group_clients(&id, &desired.into_iter().collect::<Vec<_>>())
                .await?;
        }
        // Explicit external input selections remain valid until membership is changed.
        if (force_zone == Some(&zone) || group.stream_id.starts_with("Musicata"))
            && group.stream_id != stream
        {
            control.set_group_stream(&id, &stream).await?;
        }
        if changed {
            status = control.status().await?;
        }
    }
    Ok(())
}

// A failed control/database write must restore the actual prior groups, including external
// inputs. Group IDs can disappear while merging, so anchor each saved set by a live client.
async fn restore_groups(
    control: &SnapControl,
    groups: &[crate::snapcast::SnapGroup],
) -> Result<()> {
    for saved in groups {
        if saved.clients.is_empty() {
            continue;
        }
        let live = control.status().await?;
        let target = live
            .groups
            .iter()
            .find(|group| {
                group
                    .clients
                    .iter()
                    .any(|client| client.id == saved.clients[0].id)
            })
            .ok_or_else(|| anyhow!("Player disconnected while restoring zone assignment"))?;
        control
            .set_group_clients(
                &target.id,
                &saved
                    .clients
                    .iter()
                    .map(|client| client.id.clone())
                    .collect::<Vec<_>>(),
            )
            .await?;
        control
            .set_group_stream(&target.id, &saved.stream_id)
            .await?;
    }
    Ok(())
}

/// Retain the old zone assignment before the player manager migrates its shared output.
pub async fn remember_legacy_zone(database: &Database) -> Result<()> {
    if database.get_setting(LEGACY_ZONE).await?.is_none() {
        let zone = database
            .player_record(crate::players::SNAPCAST_PLAYER_ID)
            .await?
            .and_then(|p| p.zone_id)
            .unwrap_or_default();
        database.set_setting(LEGACY_ZONE, &zone).await?;
    }
    Ok(())
}

pub async fn run(database: Database, players: Arc<PlayerManager>) {
    loop {
        let result = async {
            let _lock = ROUTING.lock().await;
            let Some(manager) = players.snapcast_manager().await else {
                return Ok::<(), anyhow::Error>(());
            };
            let status = SnapControl::new(manager.control_addr()).status().await?;
            let mut values = membership(&database).await?;
            let zones: BTreeSet<_> = players.zones().await?.into_iter().map(|z| z.id).collect();
            let mut changed = false;
            for zone in values.values_mut() {
                if zone.as_ref().is_some_and(|z| !zones.contains(z)) {
                    *zone = None;
                    changed = true;
                }
            }
            let legacy = database
                .get_setting(LEGACY_ZONE)
                .await?
                .filter(|z| zones.contains(z));
            let migrating = legacy.is_some() && !status.clients.is_empty();
            if let Some(legacy) = legacy {
                for group in &status.groups {
                    for client in &group.clients {
                        if !values.contains_key(&client.id) {
                            values.insert(client.id.clone(), Some(legacy.clone()));
                            changed = true;
                        }
                    }
                }
                // Migration is a one-time operation. Newly discovered clients stay unassigned.
            }
            apply(&players, &values, None).await?;
            if changed {
                save(&database, &values).await?;
            }
            if migrating {
                database.set_setting(LEGACY_ZONE, "").await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::debug!(%error, "Snapcast zone routing deferred");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn null_zone_is_an_explicit_unassignment() {
        let absent: ClientUpdate = serde_json::from_str(r#"{"name":"Speaker"}"#).unwrap();
        let cleared: ClientUpdate = serde_json::from_str(r#"{"zone_id":null}"#).unwrap();
        assert_eq!(absent.zone_id, None);
        assert_eq!(cleared.zone_id, Some(None));
    }
    #[test]
    fn routing_groups_keep_zones_independent_and_ignore_absent_clients() {
        let client = |id: &str| SnapClient {
            id: id.into(),
            name: id.into(),
            hostname: String::new(),
            ip_address: String::new(),
            zone_id: None,
            connected: true,
            volume_percent: 100,
            muted: false,
        };
        let values = BTreeMap::from([
            ("a".into(), Some("one".into())),
            ("b".into(), Some("one".into())),
            ("c".into(), Some("two".into())),
            ("d".into(), None),
            ("missing".into(), Some("one".into())),
        ]);
        let grouped = desired_groups(
            &values,
            &[
                client("a"),
                client("b"),
                client("c"),
                client("d"),
                client("unmanaged"),
            ],
        );
        assert_eq!(
            grouped[&Some("one".into())],
            BTreeSet::from(["a".into(), "b".into()])
        );
        assert_eq!(grouped[&Some("two".into())], BTreeSet::from(["c".into()]));
        assert_eq!(grouped[&None], BTreeSet::from(["d".into()]));
    }
}
