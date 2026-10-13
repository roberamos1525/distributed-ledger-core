//! Reconciliation endpoint.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::api::dto::ReconcileRequestBody;
use crate::api::error::ApiError;
use crate::api::extract::JsonBody;
use crate::api::handlers::to_json;
use crate::api::{AppState, Ctx};
use crate::config::Role;
use crate::reconciliation::{ReconcileRequest, Tolerance, reconcile};

/// `POST /v1/reconciliations`
pub async fn run(
    State(state): State<AppState>,
    ctx: Ctx,
    body: JsonBody<ReconcileRequestBody>,
) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Writer)?;
    let JsonBody { value, .. } = body;
    let defaults = &state.config().reconciliation;
    let view = state.ledger().account(&value.account_id)?;
    let currency = state
        .ledger()
        .currencies()
        .get(view.account.currency)
        .ok_or_else(|| ApiError::internal("account currency not registered"))?;
    let amount_tolerance = match &value.amount_tolerance {
        Some(input) => input.resolve(&currency)?,
        None => defaults.amount_tolerance,
    };
    if amount_tolerance < 0 {
        return Err(ApiError::bad_request("amount_tolerance must be >= 0"));
    }
    let date_days = value
        .date_tolerance_days
        .unwrap_or(defaults.date_tolerance_days);
    if date_days > 31 {
        return Err(ApiError::bad_request("date_tolerance_days must be <= 31"));
    }
    let request = ReconcileRequest {
        account_id: value.account_id.clone(),
        format: value.format,
        data: value.data,
        tolerance: Tolerance {
            date_days,
            amount: amount_tolerance,
        },
        from: value.from,
        to: value.to,
        max_lines: defaults.max_statement_lines,
    };
    let result = state.run(move |l| reconcile(l, &request)).await;
    let details = match &result {
        Ok(r) => json!({"status": r.status, "summary": r.summary}),
        Err(_) => Value::Null,
    };
    state
        .audit(
            &ctx,
            "reconciliation.run",
            value.account_id.to_string(),
            &result,
            details,
        )
        .await;
    let report = result?;
    state.metrics().reconciliations.with(&[report.status]).inc();
    Ok(Json(to_json(&report)?))
}
