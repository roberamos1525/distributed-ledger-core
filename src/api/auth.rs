//! API key authentication and role-based authorization.
//!
//! Keys are random secrets presented as `Authorization: Bearer <key>` or
//! `X-API-Key: <key>`. The server only stores their SHA-256 hashes; a
//! presented key is hashed and compared against every configured hash in
//! constant time.

use axum::extract::{FromRequestParts, Request, State};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use crate::api::AppState;
use crate::api::error::ApiError;
use crate::config::{ApiKeyConfig, Role};

/// Prefix of generated keys, which makes leaked keys easy to grep for.
pub const KEY_PREFIX: &str = "ldg_";

/// Authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub id: String,
    pub role: Role,
}

impl Principal {
    /// Whether the principal has at least `role`.
    pub fn has(&self, role: Role) -> bool {
        self.role >= role
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .ok_or_else(|| ApiError::unauthorized("authentication required"))
    }
}

/// Hex SHA-256 of a key, as stored in configuration.
pub fn hash_key(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// Generates a new random key (32 bytes of OS randomness).
pub fn generate_key() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("random generator failed: {e}"))?;
    Ok(format!("{KEY_PREFIX}{}", hex::encode(bytes)))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The configured key set.
#[derive(Debug, Clone, Default)]
pub struct ApiKeys {
    keys: Vec<(String, Role, Vec<u8>)>,
}

impl ApiKeys {
    pub fn from_config(keys: &[ApiKeyConfig]) -> Self {
        ApiKeys {
            keys: keys
                .iter()
                .filter_map(|k| {
                    hex::decode(&k.key_sha256)
                        .ok()
                        .map(|hash| (k.id.clone(), k.role, hash))
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Resolves a presented secret to a principal.
    pub fn authenticate(&self, secret: &str) -> Option<Principal> {
        let presented = Sha256::digest(secret.as_bytes());
        let mut found = None;
        // Compare against every key so timing does not reveal which one matched.
        for (id, role, hash) in &self.keys {
            if constant_time_eq(hash, &presented) && found.is_none() {
                found = Some(Principal {
                    id: id.clone(),
                    role: *role,
                });
            }
        }
        found
    }
}

/// Extracts the presented key from request headers.
pub fn presented_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(value) = headers.get(axum::http::header::AUTHORIZATION)
        && let Ok(text) = value.to_str()
        && let Some(token) = text.strip_prefix("Bearer ")
    {
        let token = token.trim();
        if !token.is_empty() {
            return Some(token);
        }
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty())
}

/// Paths reachable without credentials.
fn is_public(path: &str, metrics_public: bool) -> bool {
    matches!(path, "/health" | "/ready") || (metrics_public && path == "/metrics")
}

/// Middleware: authenticates the request and stores the [`Principal`].
pub async fn authenticate(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if is_public(&path, state.config().server.metrics_public) {
        return next.run(req).await;
    }
    let principal = match presented_key(req.headers()) {
        None => {
            state.metrics().auth_failures.with(&["missing"]).inc();
            return ApiError::unauthorized("missing API key (use Authorization: Bearer <key>)")
                .into_response();
        }
        Some(key) => match state.keys().authenticate(key) {
            Some(p) => p,
            None => {
                state.metrics().auth_failures.with(&["invalid"]).inc();
                tracing::warn!(path = %path, "rejected request with invalid API key");
                return ApiError::unauthorized("invalid API key").into_response();
            }
        },
    };
    req.extensions_mut().insert(principal);
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> (ApiKeys, String, String) {
        let reader = generate_key().unwrap();
        let admin = generate_key().unwrap();
        let config = vec![
            ApiKeyConfig {
                id: "reporting".into(),
                role: Role::Reader,
                key_sha256: hash_key(&reader),
            },
            ApiKeyConfig {
                id: "ops".into(),
                role: Role::Admin,
                key_sha256: hash_key(&admin),
            },
        ];
        (ApiKeys::from_config(&config), reader, admin)
    }

    #[test]
    fn generated_keys_are_unique_and_prefixed() {
        let a = generate_key().unwrap();
        let b = generate_key().unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with(KEY_PREFIX));
        assert_eq!(a.len(), KEY_PREFIX.len() + 64);
        assert_eq!(hash_key("password").len(), 64);
    }

    #[test]
    fn authenticates_by_hash() {
        let (keys, reader, admin) = keys();
        assert_eq!(keys.authenticate(&reader).unwrap().role, Role::Reader);
        let p = keys.authenticate(&admin).unwrap();
        assert_eq!(p.id, "ops");
        assert!(p.has(Role::Writer));
        assert!(keys.authenticate("ldg_wrong").is_none());
        assert!(!keys.is_empty());
    }

    #[test]
    fn role_checks() {
        let reader = Principal {
            id: "r".into(),
            role: Role::Reader,
        };
        assert!(reader.has(Role::Reader));
        assert!(!reader.has(Role::Writer));
    }

    #[test]
    fn header_extraction() {
        let mut h = HeaderMap::new();
        assert!(presented_key(&h).is_none());
        h.insert("x-api-key", "k1".parse().unwrap());
        assert_eq!(presented_key(&h), Some("k1"));
        h.insert("authorization", "Bearer k2".parse().unwrap());
        assert_eq!(presented_key(&h), Some("k2"));
        h.insert("authorization", "Basic abc".parse().unwrap());
        assert_eq!(presented_key(&h), Some("k1"));
    }

    #[test]
    fn public_paths() {
        assert!(is_public("/health", false));
        assert!(is_public("/ready", false));
        assert!(is_public("/metrics", true));
        assert!(!is_public("/metrics", false));
        assert!(!is_public("/v1/accounts", true));
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
