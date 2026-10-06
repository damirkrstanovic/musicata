// SPDX-License-Identifier: AGPL-3.0-or-later
use super::DiagnosticHealth;
use crate::{AppError, AppState, auth::CurrentUser, output_audio::EndpointIdentity};
use axum::{
    Extension, Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use musicata_core::diagnostics::{DiagnosticAction, DiagnosticEvent};
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DiagnosticExportStatus {
    pub running: bool,
    pub ready: bool,
    pub error: Option<String>,
}
#[derive(Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DiagnosticStatus {
    pub health: DiagnosticHealth,
    pub export: DiagnosticExportStatus,
}
fn admin(user: Option<Extension<CurrentUser>>) -> Result<(), AppError> {
    match user {
        Some(Extension(user)) if user.is_admin() => Ok(()),
        Some(_) => Err(AppError::forbidden("administrator access required")),
        None => Err(AppError::unauthorized("sign in to continue")),
    }
}
pub async fn status(
    State(state): State<AppState>,
    user: Option<Extension<CurrentUser>>,
) -> Result<Json<DiagnosticStatus>, AppError> {
    admin(user)?;
    Ok(Json(DiagnosticStatus {
        health: state.diagnostics.health(),
        export: state
            .diagnostic_export
            .lock()
            .expect("diagnostic export")
            .clone(),
    }))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetailRequest {
    enabled: bool,
}
pub async fn detail(
    State(state): State<AppState>,
    user: Option<Extension<CurrentUser>>,
    Json(request): Json<DetailRequest>,
) -> Result<Json<DiagnosticStatus>, AppError> {
    admin(user.clone())?;
    if request.enabled {
        state.diagnostics.enable_detail();
    } else {
        state.diagnostics.disable_detail();
    }
    status(State(state), user).await
}
fn validate_report(
    mut event: DiagnosticEvent,
    output: &str,
    native: bool,
) -> Result<DiagnosticEvent, AppError> {
    let categories = if native {
        &[
            "native.connection",
            "native.decode",
            "native.audio",
            "native.dsp",
            "native.buffering",
        ][..]
    } else {
        &[
            "browser.audio",
            "browser.buffering",
            "browser.graph",
            "browser.routing",
            "browser.dsp",
        ][..]
    };
    if !categories.contains(&event.category.as_str())
        || event.component != if native { "native" } else { "browser" }
        || !matches!(
            event.action,
            DiagnosticAction::Failure | DiagnosticAction::Recovery
        )
        || event.context.source_id.is_some()
        || event.context.track_id.is_some()
        || event.context.renderer_session_id.is_some()
        || event
            .context
            .output_id
            .as_deref()
            .is_some_and(|id| id != output)
        || event.message.len() > 1024
        || event
            .measurements
            .iter()
            .any(|(key, value)| key != "lost_reports" || *value > 1_000_000)
    {
        return Err(AppError::bad_request("invalid diagnostic report"));
    }
    event.context.output_id = Some(output.into());
    Ok(event)
}
fn record_report(
    state: &AppState,
    identity: &str,
    event: DiagnosticEvent,
) -> Result<StatusCode, AppError> {
    if !state
        .diagnostic_reports
        .lock()
        .expect("diagnostic reports")
        .allow(identity)
    {
        return Err(AppError {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "rate_limited",
            message: "too many diagnostic reports; try again later".into(),
        });
    }
    if let Some(count) = event.measurements.get("lost_reports") {
        state.diagnostics.reported_loss(*count);
    }
    state.diagnostics.record(event);
    Ok(StatusCode::ACCEPTED)
}
pub async fn browser_report(
    State(state): State<AppState>,
    user: Option<Extension<CurrentUser>>,
    headers: HeaderMap,
    Json(event): Json<DiagnosticEvent>,
) -> Result<StatusCode, AppError> {
    let Some(Extension(user)) = user else {
        return Err(AppError::unauthorized("sign in to continue"));
    };
    let token = headers
        .get("x-musicata-renderer")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !state.output_audio.owns_browser_reporter(token).await {
        return Err(AppError::forbidden("active browser renderer required"));
    }
    let mut event = validate_report(event, "browser-local", false)?;
    event.context.renderer_session_id = Some(token.to_owned());
    record_report(&state, &format!("user:{}", user.id), event)
}
pub async fn native_report(
    State(state): State<AppState>,
    Path(id): Path<String>,
    identity: Option<Extension<EndpointIdentity>>,
    Json(event): Json<DiagnosticEvent>,
) -> Result<StatusCode, AppError> {
    if !identity.is_some_and(|Extension(identity)| identity.0 == id) {
        return Err(AppError::unauthorized("output credentials required"));
    }
    let event = validate_report(event, &id, true)?;
    record_report(&state, &format!("output:{id}"), event)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    pub description: Option<String>,
    pub problem_time_unix_seconds: Option<i64>,
}
pub async fn prepare_export(
    State(state): State<AppState>,
    user: Option<Extension<CurrentUser>>,
    Json(request): Json<ExportRequest>,
) -> Result<Json<DiagnosticExportStatus>, AppError> {
    admin(user)?;
    let now = super::now();
    if request
        .description
        .as_ref()
        .is_some_and(|text| text.chars().count() > 1000)
        || request
            .problem_time_unix_seconds
            .is_some_and(|time| time < now - 14 * 86400 || time > now + 300)
    {
        return Err(AppError::bad_request(
            "description or problem time is outside the supported range",
        ));
    }
    {
        let mut export = state.diagnostic_export.lock().expect("diagnostic export");
        if export.running {
            return Ok(Json(export.clone()));
        }
        export.running = true;
        export.ready = false;
        export.error = None;
    }
    let job = state.clone();
    tokio::spawn(async move {
        let result = super::export::prepare(&job, request).await;
        let mut export = job.diagnostic_export.lock().expect("diagnostic export");
        export.running = false;
        export.ready = result.is_ok();
        export.error = result.err().map(|_| {
            "Could not prepare diagnostics. Try again when diagnostic storage is available.".into()
        });
    });
    Ok(Json(
        state
            .diagnostic_export
            .lock()
            .expect("diagnostic export")
            .clone(),
    ))
}
pub async fn download(
    State(state): State<AppState>,
    user: Option<Extension<CurrentUser>>,
) -> Result<Response, AppError> {
    admin(user)?;
    if !state
        .diagnostic_export
        .lock()
        .expect("diagnostic export")
        .ready
    {
        return Err(AppError::not_found("prepare a diagnostic download first"));
    }
    let bytes = tokio::fs::read(super::export::path(&state.db_path))
        .await
        .map_err(|_| AppError::not_found("diagnostic download is unavailable"))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"musicata-diagnostics.zip\"",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reports_cannot_forge_attribution_or_capture_arbitrary_fields() {
        let mut report = super::super::event(
            "native.audio",
            "native",
            DiagnosticAction::Failure,
            Some("other"),
            "failed",
        );
        assert!(validate_report(report.clone(), "mine", true).is_err());
        report.context.output_id = None;
        // Wire reporters cannot choose internal source locations.
        report.measurements.clear();
        assert_eq!(
            validate_report(report.clone(), "mine", true)
                .unwrap()
                .context
                .output_id
                .as_deref(),
            Some("mine")
        );
        report.category = "arbitrary.secret".into();
        assert!(validate_report(report, "mine", true).is_err());
    }
    #[test]
    fn rate_limits_bound_repeat_and_global_reports() {
        let mut limits = super::super::ReportLimits::default();
        for _ in 0..10 {
            assert!(limits.allow("one"));
        }
        assert!(!limits.allow("one"));
        for id in 0..50 {
            assert!(limits.allow(&format!("id-{id}")));
        }
        assert!(!limits.allow("extra"));
    }
}
