//! Audit trail endpoints (admin only).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;

use crate::api::dto::{AuditQuery, ListResponse, encode_cursor, page_limit};
use crate::api::error::ApiError;
use crate::api::extract::ApiQuery;
use crate::api::handlers::to_json;
use crate::api::{AppState, Ctx};
use crate::config::Role;

/// `GET /v1/audit?after=<sequence>&limit=<n>`
pub async fn list(
    State(state): State<AppState>,
    ctx: Ctx,
    ApiQuery(query): ApiQuery<AuditQuery>,
) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Admin)?;
    let limit = page_limit(query.limit)?;
    let after = query.after.unwrap_or(0);
    let log = state.audit_log().clone();
    let records = tokio::task::spawn_blocking(move || log.list(after, limit))
        .await
        .map_err(|e| ApiError::internal(format!("audit task failed: {e}")))?
        .map_err(|e| ApiError::internal(format!("cannot read audit log: {e}")))?;
    let next_cursor = (records.len() == limit)
        .then(|| {
            records
                .last()
                .map(|r| encode_cursor("audit", &r.sequence.to_string()))
        })
        .flatten();
    Ok(Json(to_json(&ListResponse {
        data: records,
        next_cursor,
    })?))
}

/// `GET /v1/audit/verify` — re-reads the audit file and verifies the hash
/// chain. Returns 200 when intact and 409 when tampering is detected.
pub async fn verify(
    State(state): State<AppState>,
    ctx: Ctx,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    state.require(&ctx.principal, Role::Admin)?;
    let log = state.audit_log().clone();
    let report = tokio::task::spawn_blocking(move || log.verify())
        .await
        .map_err(|e| ApiError::internal(format!("audit task failed: {e}")))?
        .map_err(|e| ApiError::internal(format!("cannot read audit log: {e}")))?;
    let status = if report.ok {
        StatusCode::OK
    } else {
        tracing::error!(first_invalid = ?report.first_invalid_sequence, "audit chain verification failed");
        StatusCode::CONFLICT
    };
    Ok((status, Json(to_json(&report)?)))
}
