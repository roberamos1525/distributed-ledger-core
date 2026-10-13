//! Reporting endpoints: trial balance and invariant check.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;

use crate::api::error::ApiError;
use crate::api::handlers::to_json;
use crate::api::{AppState, Ctx};
use crate::config::Role;

/// `GET /v1/reports/trial-balance`
pub async fn trial_balance(
    State(state): State<AppState>,
    ctx: Ctx,
) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Reader)?;
    let report = state.run(|l| Ok(l.trial_balance())).await?;
    Ok(Json(to_json(&report)?))
}

/// `GET /v1/reports/invariants` — 200 when all invariants hold, 500 with
/// the violation list otherwise (so monitors can alert on the status code).
pub async fn invariants(
    State(state): State<AppState>,
    ctx: Ctx,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    state.require(&ctx.principal, Role::Reader)?;
    let report = state.run(|l| Ok(l.check_invariants())).await?;
    let status = if report.ok {
        StatusCode::OK
    } else {
        tracing::error!(violations = ?report.violations, "ledger invariant violation");
        StatusCode::INTERNAL_SERVER_ERROR
    };
    Ok((status, Json(to_json(&report)?)))
}
