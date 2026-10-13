//! REST API (axum): routing, shared state and request context.

pub mod auth;
pub mod dto;
pub mod error;
pub mod extract;
pub mod handlers;
pub mod idempotent;
pub mod middleware;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{DefaultBodyLimit, FromRequestParts};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use chrono::{DateTime, Utc};
use tower::ServiceBuilder;

use crate::audit::{AuditEntry, AuditLog};
use crate::config::{Config, Role};
use crate::engine::ledger::Ledger;
use crate::error::LedgerResult;
use crate::idempotency::IdempotencyStore;
use crate::metrics::LedgerMetrics;

use self::auth::{ApiKeys, Principal};
use self::error::ApiError;
use self::middleware::RequestId;

struct AppInner {
    ledger: Arc<Ledger>,
    idempotency: Arc<IdempotencyStore>,
    audit: Arc<AuditLog>,
    metrics: Arc<LedgerMetrics>,
    keys: ApiKeys,
    config: Config,
    started_at: DateTime<Utc>,
}

/// Shared state of the HTTP layer. Cheap to clone.
#[derive(Clone)]
pub struct AppState(Arc<AppInner>);

impl AppState {
    pub fn new(
        config: Config,
        ledger: Arc<Ledger>,
        idempotency: Arc<IdempotencyStore>,
        audit: Arc<AuditLog>,
        metrics: Arc<LedgerMetrics>,
    ) -> Self {
        AppState(Arc::new(AppInner {
            keys: ApiKeys::from_config(&config.auth.api_keys),
            ledger,
            idempotency,
            audit,
            metrics,
            config,
            started_at: Utc::now(),
        }))
    }

    pub fn ledger(&self) -> &Arc<Ledger> {
        &self.0.ledger
    }

    pub fn idempotency(&self) -> &Arc<IdempotencyStore> {
        &self.0.idempotency
    }

    pub fn audit_log(&self) -> &Arc<AuditLog> {
        &self.0.audit
    }

    pub fn metrics(&self) -> &Arc<LedgerMetrics> {
        &self.0.metrics
    }

    pub fn keys(&self) -> &ApiKeys {
        &self.0.keys
    }

    pub fn config(&self) -> &Config {
        &self.0.config
    }

    pub fn started_at(&self) -> DateTime<Utc> {
        self.0.started_at
    }

    /// Enforces a minimum role.
    pub fn require(&self, principal: &Principal, role: Role) -> Result<(), ApiError> {
        if principal.has(role) {
            Ok(())
        } else {
            self.metrics().auth_failures.with(&["forbidden"]).inc();
            Err(ApiError::forbidden(format!(
                "this operation requires the {} role",
                role.as_str()
            )))
        }
    }

    /// Runs a (blocking) engine operation on the blocking thread pool.
    pub async fn run<T, F>(&self, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&Ledger) -> LedgerResult<T> + Send + 'static,
    {
        let ledger = self.ledger().clone();
        tokio::task::spawn_blocking(move || f(&ledger))
            .await
            .map_err(|e| ApiError::internal(format!("engine task failed: {e}")))?
            .map_err(ApiError::from)
    }

    /// Appends an audit record describing the outcome of an operation.
    pub async fn audit<T>(
        &self,
        ctx: &Ctx,
        action: &str,
        resource: impl Into<String>,
        outcome: &Result<T, ApiError>,
        details: serde_json::Value,
    ) {
        let outcome = match outcome {
            Ok(_) => "success".to_string(),
            Err(e) => format!("rejected:{}", e.code),
        };
        let entry = AuditEntry::new(&ctx.principal.id, action, resource)
            .with_outcome(outcome)
            .with_request_id(ctx.request_id.clone())
            .with_details(details);
        let log = self.audit_log().clone();
        let metrics = self.metrics().clone();
        let result = tokio::task::spawn_blocking(move || log.append(entry)).await;
        match result {
            Ok(Ok(_)) => metrics.audit_records.inc(),
            Ok(Err(e)) => tracing::error!(error = %e, action, "failed to write audit record"),
            Err(e) => tracing::error!(error = %e, action, "audit task failed"),
        }
    }
}

/// Per-request context: authenticated principal and request id.
pub struct Ctx {
    pub principal: Principal,
    pub request_id: Option<String>,
}

impl<S: Send + Sync> FromRequestParts<S> for Ctx {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let principal = Principal::from_request_parts(parts, state).await?;
        let request_id = parts.extensions.get::<RequestId>().map(|r| r.0.clone());
        Ok(Ctx {
            principal,
            request_id,
        })
    }
}

async fn handle_middleware_error(err: tower::BoxError) -> Response {
    if err.is::<tower::timeout::error::Elapsed>() {
        ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            "request_timeout",
            "the request did not complete within the configured timeout",
        )
        .into_response()
    } else {
        ApiError::internal(format!("unhandled middleware error: {err}")).into_response()
    }
}

async fn not_found() -> ApiError {
    ApiError::not_found("no such route")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "method not allowed for this route",
    )
}

/// Builds the complete router with all middleware.
pub fn router(state: AppState) -> Router {
    use handlers::{accounts, audit, reconciliation, reports, system, transactions};

    let server = &state.config().server;
    let timeout = Duration::from_secs(server.request_timeout_secs);
    let body_limit = server.max_body_bytes;

    let routes = Router::new()
        .route("/health", get(system::health))
        .route("/ready", get(system::ready))
        .route("/metrics", get(system::metrics))
        .route("/v1/info", get(system::info))
        .route("/v1/admin/snapshots", post(system::snapshot))
        .route("/v1/admin/compactions", post(system::compact))
        .route("/v1/accounts", post(accounts::create).get(accounts::list))
        .route("/v1/accounts/{id}", get(accounts::get))
        .route("/v1/accounts/{id}/status", post(accounts::update_status))
        .route(
            "/v1/accounts/{id}/metadata",
            patch(accounts::update_metadata),
        )
        .route("/v1/accounts/{id}/policy", put(accounts::update_policy))
        .route("/v1/accounts/{id}/balance", get(accounts::balance))
        .route("/v1/accounts/{id}/history", get(accounts::history))
        .route(
            "/v1/transactions",
            post(transactions::create).get(transactions::list),
        )
        .route("/v1/transactions/{id}", get(transactions::get))
        .route(
            "/v1/transactions/{id}/post",
            post(transactions::post_pending),
        )
        .route(
            "/v1/transactions/{id}/void",
            post(transactions::void_pending),
        )
        .route(
            "/v1/transactions/{id}/reversals",
            post(transactions::reverse),
        )
        .route("/v1/reports/trial-balance", get(reports::trial_balance))
        .route("/v1/reports/invariants", get(reports::invariants))
        .route("/v1/reconciliations", post(reconciliation::run))
        .route("/v1/audit", get(audit::list))
        .route("/v1/audit/verify", get(audit::verify))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::observe,
        ));

    routes
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::authenticate,
        ))
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(handle_middleware_error))
                .layer(tower::timeout::TimeoutLayer::new(timeout)),
        )
        .layer(axum::middleware::from_fn(middleware::request_id))
        .with_state(state)
}
