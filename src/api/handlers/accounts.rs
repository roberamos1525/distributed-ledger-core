//! Account endpoints.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::api::dto::{
    BalanceQuery, CreateAccountRequest, HistoryQuery, ListAccountsQuery, ListResponse,
    UpdateMetadataRequest, UpdatePolicyRequest, UpdateStatusRequest, decode_cursor,
    decode_numeric_cursor, encode_cursor, page_limit,
};
use crate::api::error::ApiError;
use crate::api::extract::{ApiQuery, JsonBody};
use crate::api::handlers::{parse_account_id, to_json};
use crate::api::{AppState, Ctx, idempotent};
use crate::config::Role;
use crate::domain::ids::AccountId;
use crate::engine::commands::{ChangePolicy, ChangeStatus, OpenAccount, UpdateMetadata};
use crate::engine::queries::AsOf;

/// `POST /v1/accounts`
pub async fn create(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    body: JsonBody<CreateAccountRequest>,
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
        "/v1/accounts",
        &raw,
        |_| async move {
            let requested = value.id.as_ref().map(ToString::to_string);
            let cmd: OpenAccount = value.into();
            let result = st.run(move |l| l.open_account(cmd)).await;
            let resource = match &result {
                Ok(a) => a.id.to_string(),
                Err(_) => requested.unwrap_or_else(|| "-".into()),
            };
            st.audit(&ctx, "account.open", resource, &result, details)
                .await;
            let account = result?;
            let id = account.id.clone();
            let view = st.run(move |l| l.account(&id)).await?;
            Ok((StatusCode::CREATED, to_json(&view)?))
        },
    )
    .await
}

/// `GET /v1/accounts`
pub async fn list(
    State(state): State<AppState>,
    ctx: Ctx,
    ApiQuery(query): ApiQuery<ListAccountsQuery>,
) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Reader)?;
    let limit = page_limit(query.limit)?;
    let after = query
        .cursor
        .as_deref()
        .map(|c| decode_cursor("acct", c).and_then(|v| parse_account_id(&v)))
        .transpose()?;
    let filter = query.filter();
    let page = state
        .run(move |l| Ok(l.list_accounts(&filter, after.as_ref(), limit)))
        .await?;
    Ok(Json(to_json(&ListResponse {
        data: page.items,
        next_cursor: page
            .next_cursor
            .map(|id| encode_cursor("acct", id.as_str())),
    })?))
}

/// `GET /v1/accounts/{id}`
pub async fn get(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Reader)?;
    let id = parse_account_id(&id)?;
    let view = state.run(move |l| l.account(&id)).await?;
    Ok(Json(to_json(&view)?))
}

/// `POST /v1/accounts/{id}/status`
pub async fn update_status(
    State(state): State<AppState>,
    ctx: Ctx,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: JsonBody<UpdateStatusRequest>,
) -> Response {
    if let Err(e) = state.require(&ctx.principal, Role::Admin) {
        return e.into_response();
    }
    let account_id = match parse_account_id(&id) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let principal = ctx.principal.clone();
    let path = format!("/v1/accounts/{account_id}/status");
    let JsonBody { value, raw } = body;
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
            let cmd = ChangeStatus {
                account_id: account_id.clone(),
                status: value.status,
                expected_version: value.expected_version,
                reason: value.reason,
            };
            let result = st.run(move |l| l.change_status(cmd)).await;
            st.audit(
                &ctx,
                "account.status",
                account_id.to_string(),
                &result,
                details,
            )
            .await;
            respond_account(&st, result?.id).await
        },
    )
    .await
}

/// `PATCH /v1/accounts/{id}/metadata`
pub async fn update_metadata(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
    body: JsonBody<UpdateMetadataRequest>,
) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Writer)?;
    let account_id = parse_account_id(&id)?;
    let JsonBody { value, raw } = body;
    let cmd = UpdateMetadata {
        account_id: account_id.clone(),
        set: value.set,
        remove: value.remove,
        expected_version: value.expected_version,
    };
    let result = state.run(move |l| l.update_metadata(cmd)).await;
    state
        .audit(
            &ctx,
            "account.metadata",
            account_id.to_string(),
            &result,
            raw,
        )
        .await;
    let (_, body) = respond_account(&state, result?.id).await?;
    Ok(Json(body))
}

/// `PUT /v1/accounts/{id}/policy`
pub async fn update_policy(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
    body: JsonBody<UpdatePolicyRequest>,
) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Admin)?;
    let account_id = parse_account_id(&id)?;
    let JsonBody { value, raw } = body;
    let cmd = ChangePolicy {
        account_id: account_id.clone(),
        policy: value.policy,
        expected_version: value.expected_version,
    };
    let result = state.run(move |l| l.change_policy(cmd)).await;
    state
        .audit(&ctx, "account.policy", account_id.to_string(), &result, raw)
        .await;
    let (_, body) = respond_account(&state, result?.id).await?;
    Ok(Json(body))
}

/// `GET /v1/accounts/{id}/balance[?as_of_sequence=N|as_of_time=RFC3339]`
pub async fn balance(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<BalanceQuery>,
) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Reader)?;
    let id = parse_account_id(&id)?;
    let as_of = match (query.as_of_sequence, query.as_of_time) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request(
                "use either as_of_sequence or as_of_time, not both",
            ));
        }
        (Some(seq), None) => AsOf::Sequence(seq),
        (None, Some(t)) => AsOf::Time(t),
        (None, None) => AsOf::Latest,
    };
    let view = state.run(move |l| l.balance(&id, as_of)).await?;
    let currency = state
        .ledger()
        .currencies()
        .get(view.currency)
        .ok_or_else(|| ApiError::internal("account currency not registered"))?;
    let mut body = to_json(&view)?;
    body["posted_decimal"] = json!(currency.format_amount(view.posted));
    body["available_decimal"] = json!(currency.format_amount(view.available));
    Ok(Json(body))
}

/// `GET /v1/accounts/{id}/history`
pub async fn history(
    State(state): State<AppState>,
    ctx: Ctx,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    state.require(&ctx.principal, Role::Reader)?;
    let id = parse_account_id(&id)?;
    let limit = page_limit(query.limit)?;
    let cursor = query
        .cursor
        .as_deref()
        .map(|c| decode_numeric_cursor::<usize>("hist", c))
        .transpose()?;
    let order = query.order();
    let page = state
        .run(move |l| l.history(&id, cursor, limit, order))
        .await?;
    Ok(Json(to_json(&ListResponse {
        data: page.items,
        next_cursor: page
            .next_cursor
            .map(|pos| encode_cursor("hist", &pos.to_string())),
    })?))
}

async fn respond_account(state: &AppState, id: AccountId) -> Result<(StatusCode, Value), ApiError> {
    let view = state.run(move |l| l.account(&id)).await?;
    Ok((StatusCode::OK, to_json(&view)?))
}
