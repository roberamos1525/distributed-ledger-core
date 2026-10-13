//! Health, readiness, metrics, service info and admin maintenance endpoints.

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{Value, json};

use crate::api::auth::Principal;
use crate::api::error::ApiError;
use crate::api::handlers::to_json;
use crate::api::{AppState, Ctx};
use crate::config::Role;

/// `GET /health` — liveness: the process is up and serving.
pub async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

/// `GET /ready` — readiness: recovery finished and the ledger accepts writes.
pub async fn ready(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let ledger = state.ledger();
    match ledger.poisoned() {
        None => (
            StatusCode::OK,
            Json(json!({
                "status": "ready",
                "last_sequence": ledger.last_sequence(),
            })),
        ),
        Some(reason) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "read_only", "reason": reason})),
        ),
    }
}

/// `GET /metrics` — Prometheus text exposition.
pub async fn metrics(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
) -> Response {
    if !state.config().server.metrics_public {
        match principal {
            Some(Extension(p)) => {
                if let Err(e) = state.require(&p, Role::Reader) {
                    return e.into_response();
                }
            }
            None => return ApiError::unauthorized("authentication required").into_response(),
        }
    }
    let m = state.metrics();
    m.idempotency_entries.set(state.idempotency().len() as f64);
    let stats = state.ledger().stats();
    m.accounts.set(stats.accounts as f64);
    m.transactions.set(stats.transactions as f64);
    m.pending_transactions
        .set(stats.pending_transactions as f64);
    m.last_sequence.set(stats.last_sequence as f64);
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        m.render(),
    )
        .into_response()
}

/// `GET /v1/info` — ledger statistics and the last recovery report.
pub async fn info(State(state): State<AppState>, ctx: Ctx) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Reader)?;
    let ledger = state.ledger();
    Ok(Json(json!({
        "service": "distributed-ledger-core",
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": state.started_at(),
        "principal": {"id": ctx.principal.id, "role": ctx.principal.role},
        "stats": to_json(&ledger.stats())?,
        "recovery": to_json(ledger.recovery_report())?,
        "idempotency_entries": state.idempotency().len(),
        "audit_last_sequence": state.audit_log().last_sequence(),
    })))
}

/// `POST /v1/admin/snapshots` — writes a snapshot now.
pub async fn snapshot(
    State(state): State<AppState>,
    ctx: Ctx,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    state.require(&ctx.principal, Role::Admin)?;
    let result = state.run(|l| l.snapshot()).await;
    let resource = result
        .as_ref()
        .map(|m| m.sequence.to_string())
        .unwrap_or_else(|_| "-".into());
    state
        .audit(&ctx, "admin.snapshot", resource, &result, Value::Null)
        .await;
    Ok((StatusCode::CREATED, Json(to_json(&result?)?)))
}

/// `POST /v1/admin/compactions` — snapshot, then drop covered WAL segments.
pub async fn compact(State(state): State<AppState>, ctx: Ctx) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Admin)?;
    let result = state.run(|l| l.compact()).await;
    let resource = result
        .as_ref()
        .map(|r| r.snapshot.sequence.to_string())
        .unwrap_or_else(|_| "-".into());
    state
        .audit(&ctx, "admin.compact", resource, &result, Value::Null)
        .await;
    let report = result?;
    if let Err(e) = state.idempotency().purge_expired(chrono::Utc::now()) {
        tracing::warn!(error = %e, "idempotency purge during compaction failed");
    }
    Ok(Json(to_json(&report)?))
}
