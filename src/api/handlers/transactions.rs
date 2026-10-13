//! Transaction endpoints: create, list, inspect, two-phase post/void and
//! reversals.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::api::dto::{
    CreateTransactionRequest, ListResponse, ListTransactionsQuery, ReverseRequest, VoidRequest,
    decode_numeric_cursor, encode_cursor, page_limit,
};
use crate::api::error::ApiError;
use crate::api::extract::{ApiQuery, JsonBody};
use crate::api::handlers::{parse_tx_id, to_json};
use crate::api::{AppState, Ctx, idempotent};
use crate::config::Role;
use crate::domain::money::Currency;

fn currency(
    state: &AppState,
    code: crate::domain::money::CurrencyCode,
) -> Result<Currency, ApiError> {
    state
        .ledger()
        .currencies()
        .get(code)
        .ok_or_else(|| ApiError::internal(format!("currency {code} is not registered")))
}

/// `POST /v1/transactions`
pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    body: JsonBody<CreateTransactionRequest>,
) -> Response {
    if let Err(e) = state.require(&ctx.principal, Role::Writer) {
        return e.into_response();
    }
    let principal = ctx.principal.clone();
    let JsonBody { value, raw } = body;
    let details = raw.clone();
    let st = state.clone();
    idempotent::run(
        &state,
        &principal,
        &headers,
        "POST",
        "/v1/transactions",
        &raw,
        |tag| async move {
            let mut cmd = value.into_command(|id| {
                let view = st.ledger().account(id)?;
                currency(&st, view.account.currency)
            })?;
            cmd.idempotency = tag;
            let result = st.run(move |l| l.create_transaction(cmd)).await;
            let resource = result
                .as_ref()
                .map(|o| o.transaction.id.to_string())
                .unwrap_or_else(|_| "-".into());
            st.audit(&ctx, "transaction.create", resource, &result, details)
                .await;
            let outcome = result?;
            let status = if outcome.replayed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            Ok((status, to_json(&outcome.transaction)?))
        },
    )
    .await
}

/// `GET /v1/transactions`
pub async fn list(
    State(state): State<AppState>,
    ctx: Ctx,
    ApiQuery(query): ApiQuery<ListTransactionsQuery>,
) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Reader)?;
    let limit = page_limit(query.limit)?;
    let after = query
        .cursor
        .as_deref()
        .map(|c| decode_numeric_cursor::<u64>("tx", c))
        .transpose()?;
    let filter = query.filter();
    let page = state
        .run(move |l| Ok(l.list_transactions(&filter, after, limit)))
        .await?;
    Ok(Json(to_json(&ListResponse {
        data: page.items,
        next_cursor: page
            .next_cursor
            .map(|seq| encode_cursor("tx", &seq.to_string())),
    })?))
}

/// `GET /v1/transactions/{id}`
pub async fn get(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Reader)?;
    let id = parse_tx_id(&id)?;
    let tx = state.run(move |l| l.transaction(&id)).await?;
    Ok(Json(to_json(&tx)?))
}

/// `POST /v1/transactions/{id}/post`
pub async fn post_pending(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = state.require(&ctx.principal, Role::Writer) {
        return e.into_response();
    }
    let id = match parse_tx_id(&id) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let principal = ctx.principal.clone();
    let path = format!("/v1/transactions/{id}/post");
    let st = state.clone();
    idempotent::run(
        &state,
        &principal,
        &headers,
        "POST",
        &path,
        &Value::Null,
        |_| async move {
            let result = st.run(move |l| l.post_pending(id)).await;
            st.audit(
                &ctx,
                "transaction.post",
                id.to_string(),
                &result,
                Value::Null,
            )
            .await;
            Ok((StatusCode::OK, to_json(&result?)?))
        },
    )
    .await
}

/// `POST /v1/transactions/{id}/void`
pub async fn void_pending(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<JsonBody<VoidRequest>>,
) -> Response {
    if let Err(e) = state.require(&ctx.principal, Role::Writer) {
        return e.into_response();
    }
    let id = match parse_tx_id(&id) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let (value, raw) = match body {
        Some(JsonBody { value, raw }) => (value, raw),
        None => (VoidRequest::default(), Value::Null),
    };
    let principal = ctx.principal.clone();
    let path = format!("/v1/transactions/{id}/void");
    let details = raw.clone();
    let st = state.clone();
    idempotent::run(
        &state,
        &principal,
        &headers,
        "POST",
        &path,
        &raw,
        |_| async move {
            let result = st.run(move |l| l.void_pending(id, value.reason)).await;
            st.audit(&ctx, "transaction.void", id.to_string(), &result, details)
                .await;
            Ok((StatusCode::OK, to_json(&result?)?))
        },
    )
    .await
}

/// `POST /v1/transactions/{id}/reversals`
pub async fn reverse(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<JsonBody<ReverseRequest>>,
) -> Response {
    if let Err(e) = state.require(&ctx.principal, Role::Writer) {
        return e.into_response();
    }
    let original = match parse_tx_id(&id) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let (value, raw) = match body {
        Some(JsonBody { value, raw }) => (value, raw),
        None => (ReverseRequest::default(), Value::Null),
    };
    let principal = ctx.principal.clone();
    let path = format!("/v1/transactions/{original}/reversals");
    let details = raw.clone();
    let st = state.clone();
    idempotent::run(
        &state,
        &principal,
        &headers,
        "POST",
        &path,
        &raw,
        |_| async move {
            let original_tx = st.ledger().transaction(&original)?;
            let cmd = value.into_command(original, |index| {
                let posting = original_tx.postings.get(index).ok_or_else(|| {
                    ApiError::bad_request(format!("posting index {index} out of range"))
                })?;
                currency(&st, posting.currency)
            })?;
            let result = st.run(move |l| l.reverse(cmd)).await;
            st.audit(
                &ctx,
                "transaction.reverse",
                original.to_string(),
                &result,
                details,
            )
            .await;
            Ok((StatusCode::CREATED, to_json(&result?)?))
        },
    )
    .await
}
