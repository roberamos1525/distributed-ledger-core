//! Configuration: defaults, optional TOML file, environment overrides, and
//! validation. Precedence (lowest to highest): built-in defaults, the TOML
//! file, `LEDGER_*` environment variables.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::account::{AccountType, BalancePolicy};
use crate::domain::money::{Currency, CurrencyRegistry};
use crate::engine::commands::{DefaultPolicies, EngineLimits};
use crate::engine::ledger::LedgerOptions;
use crate::idempotency::IdempotencyConfig;
use crate::store::recovery::RecoveryOptions;
use crate::store::wal::SyncMode;

/// Environment variable naming the config file.
pub const CONFIG_ENV: &str = "LEDGER_CONFIG";

/// Configuration errors (all problems are reported at once).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config file {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid environment variable {name}: {message}")]
    Env { name: String, message: String },
    #[error("invalid configuration:\n  - {}", .0.join("\n  - "))]
    Invalid(Vec<String>),
}

/// Access role attached to an API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Reader,
    Writer,
    Admin,
}

impl Role {
    pub fn parse(raw: &str) -> Option<Role> {
        match raw {
            "reader" => Some(Role::Reader),
            "writer" => Some(Role::Writer),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Reader => "reader",
            Role::Writer => "writer",
            Role::Admin => "admin",
        }
    }
}

/// HTTP server settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub max_body_bytes: usize,
    pub request_timeout_secs: u64,
    pub shutdown_grace_secs: u64,
    /// Serve `/metrics` without authentication (typical for scrapers on a
    /// private network). When false, a reader key is required.
    pub metrics_public: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind: "127.0.0.1:8080".parse().expect("valid default address"),
            max_body_bytes: 1 << 20,
            request_timeout_secs: 30,
            shutdown_grace_secs: 10,
            metrics_public: true,
        }
    }
}

/// Durability mode name used in config files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsyncMode {
    Always,
    Batch,
    Never,
}

/// Persistence settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub data_dir: PathBuf,
    pub fsync: FsyncMode,
    pub fsync_batch: u32,
    pub segment_max_bytes: u64,
    pub max_record_bytes: usize,
    /// Take a snapshot after this many events (0 disables).
    pub snapshot_every_events: u64,
    pub snapshot_retain: usize,
    pub snapshot_check_interval_secs: u64,
    pub repair_torn_tail: bool,
    pub truncate_corruption: bool,
    pub audit_fsync: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            data_dir: PathBuf::from("./data"),
            fsync: FsyncMode::Always,
            fsync_batch: 32,
            segment_max_bytes: 64 << 20,
            max_record_bytes: 4 << 20,
            snapshot_every_events: 10_000,
            snapshot_retain: 3,
            snapshot_check_interval_secs: 30,
            repair_torn_tail: true,
            truncate_corruption: false,
            audit_fsync: true,
        }
    }
}

/// An extra currency registered in addition to the ISO 4217 defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrencyConfig {
    pub code: String,
    pub minor_units: u8,
}

/// Ledger rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LedgerConfig {
    pub max_postings: usize,
    pub max_amount: i64,
    pub max_pending_ttl_secs: u64,
    pub pending_sweep_interval_secs: u64,
    /// Overrides of the default balance policy per account type.
    pub default_policies: BTreeMap<AccountType, BalancePolicy>,
    pub extra_currencies: Vec<CurrencyConfig>,
}

impl Default for LedgerConfig {
    fn default() -> Self {
        let limits = EngineLimits::default();
        LedgerConfig {
            max_postings: limits.max_postings,
            max_amount: limits.max_amount,
            max_pending_ttl_secs: limits.max_pending_ttl.as_secs(),
            pending_sweep_interval_secs: 5,
            default_policies: BTreeMap::new(),
            extra_currencies: Vec::new(),
        }
    }
}

/// Idempotency store settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IdempotencySettings {
    pub ttl_secs: u64,
    pub max_entries: usize,
    pub purge_interval_secs: u64,
}

impl Default for IdempotencySettings {
    fn default() -> Self {
        IdempotencySettings {
            ttl_secs: 24 * 3600,
            max_entries: 100_000,
            purge_interval_secs: 60,
        }
    }
}

/// One API key: only the SHA-256 hash of the secret is configured.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyConfig {
    pub id: String,
    pub role: Role,
    pub key_sha256: String,
}

/// Authentication settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub api_keys: Vec<ApiKeyConfig>,
}

/// Log output settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// `tracing` filter directive, e.g. `info` or `ledger_core=debug,info`.
    pub level: String,
    pub format: LogFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    Json,
    Pretty,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        LoggingConfig {
            level: "info".into(),
            format: LogFormat::Json,
        }
    }
}

/// Reconciliation defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReconciliationConfig {
    pub date_tolerance_days: u32,
    pub amount_tolerance: i64,
    pub max_statement_lines: usize,
}

impl Default for ReconciliationConfig {
    fn default() -> Self {
        ReconciliationConfig {
            date_tolerance_days: 2,
            amount_tolerance: 0,
            max_statement_lines: 10_000,
        }
    }
}

/// Complete service configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub ledger: LedgerConfig,
    pub idempotency: IdempotencySettings,
    pub auth: AuthConfig,
    pub logging: LoggingConfig,
    pub reconciliation: ReconciliationConfig,
}

fn env_parse<T: std::str::FromStr>(name: &str, raw: &str) -> Result<T, ConfigError>
where
    T::Err: std::fmt::Display,
{
    raw.trim().parse::<T>().map_err(|e| ConfigError::Env {
        name: name.to_string(),
        message: e.to_string(),
    })
}

fn env_bool(name: &str, raw: &str) -> Result<bool, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(ConfigError::Env {
            name: name.to_string(),
            message: format!("expected a boolean, got {other:?}"),
        }),
    }
}

/// Parses `LEDGER_API_KEYS`: `id:role:sha256hex` entries separated by `,`.
pub fn parse_api_keys(raw: &str) -> Result<Vec<ApiKeyConfig>, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            let mut parts = entry.splitn(3, ':');
            let (Some(id), Some(role), Some(hash)) = (parts.next(), parts.next(), parts.next())
            else {
                return Err(format!("entry {entry:?} must be id:role:sha256hex"));
            };
            let role = Role::parse(role).ok_or_else(|| format!("unknown role {role:?}"))?;
            Ok(ApiKeyConfig {
                id: id.to_string(),
                role,
                key_sha256: hash.to_ascii_lowercase(),
            })
        })
        .collect()
}

impl Config {
    /// Loads configuration from an optional file (or `$LEDGER_CONFIG`) and
    /// the process environment, then validates it.
    pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
        let from_env = std::env::var_os(CONFIG_ENV).map(PathBuf::from);
        let path = path.map(Path::to_path_buf).or(from_env);
        let mut config = match path {
            Some(p) => Config::from_file(&p)?,
            None => Config::default(),
        };
        config.apply_env(std::env::vars())?;
        config.validate()?;
        Ok(config)
    }

    /// Parses a TOML file (without env overrides or validation).
    pub fn from_file(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Config::from_toml(&text).map_err(|message| ConfigError::Parse {
            path: path.to_path_buf(),
            message,
        })
    }

    pub fn from_toml(text: &str) -> Result<Config, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// Applies `LEDGER_*` overrides from the given variables.
    pub fn apply_env(
        &mut self,
        vars: impl IntoIterator<Item = (String, String)>,
    ) -> Result<(), ConfigError> {
        for (name, value) in vars {
            let n = name.as_str();
            match n {
                "LEDGER_BIND" => self.server.bind = env_parse(n, &value)?,
                "LEDGER_MAX_BODY_BYTES" => self.server.max_body_bytes = env_parse(n, &value)?,
                "LEDGER_REQUEST_TIMEOUT_SECS" => {
                    self.server.request_timeout_secs = env_parse(n, &value)?
                }
                "LEDGER_METRICS_PUBLIC" => self.server.metrics_public = env_bool(n, &value)?,
                "LEDGER_DATA_DIR" => self.storage.data_dir = PathBuf::from(value),
                "LEDGER_FSYNC" => {
                    self.storage.fsync = match value.trim() {
                        "always" => FsyncMode::Always,
                        "batch" => FsyncMode::Batch,
                        "never" => FsyncMode::Never,
                        other => {
                            return Err(ConfigError::Env {
                                name: name.clone(),
                                message: format!("expected always|batch|never, got {other:?}"),
                            });
                        }
                    }
                }
                "LEDGER_SNAPSHOT_EVERY" => {
                    self.storage.snapshot_every_events = env_parse(n, &value)?
                }
                "LEDGER_IDEMPOTENCY_TTL_SECS" => self.idempotency.ttl_secs = env_parse(n, &value)?,
                "LEDGER_LOG_LEVEL" => self.logging.level = value,
                "LEDGER_LOG_FORMAT" => {
                    self.logging.format = match value.trim() {
                        "json" => LogFormat::Json,
                        "pretty" => LogFormat::Pretty,
                        other => {
                            return Err(ConfigError::Env {
                                name: name.clone(),
                                message: format!("expected json|pretty, got {other:?}"),
                            });
                        }
                    }
                }
                "LEDGER_API_KEYS" => {
                    self.auth.api_keys =
                        parse_api_keys(&value).map_err(|message| ConfigError::Env {
                            name: name.clone(),
                            message,
                        })?
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Checks every setting, collecting all problems.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();
        let s = &self.server;
        if !(1024..=64 << 20).contains(&s.max_body_bytes) {
            errors.push("server.max_body_bytes must be between 1 KiB and 64 MiB".into());
        }
        if !(1..=600).contains(&s.request_timeout_secs) {
            errors.push("server.request_timeout_secs must be between 1 and 600".into());
        }
        if s.shutdown_grace_secs > 300 {
            errors.push("server.shutdown_grace_secs must be <= 300".into());
        }
        let st = &self.storage;
        if st.data_dir.as_os_str().is_empty() {
            errors.push("storage.data_dir must not be empty".into());
        }
        if st.segment_max_bytes < 4096 {
            errors.push("storage.segment_max_bytes must be at least 4096".into());
        }
        if !(1024..=256 << 20).contains(&st.max_record_bytes) {
            errors.push("storage.max_record_bytes must be between 1 KiB and 256 MiB".into());
        }
        if st.max_record_bytes as u64 > st.segment_max_bytes {
            errors
                .push("storage.max_record_bytes must not exceed storage.segment_max_bytes".into());
        }
        if st.fsync == FsyncMode::Batch && st.fsync_batch == 0 {
            errors.push("storage.fsync_batch must be >= 1 in batch mode".into());
        }
        if st.snapshot_retain == 0 {
            errors.push("storage.snapshot_retain must be >= 1".into());
        }
        if st.snapshot_check_interval_secs == 0 {
            errors.push("storage.snapshot_check_interval_secs must be >= 1".into());
        }
        let l = &self.ledger;
        if !(2..=1000).contains(&l.max_postings) {
            errors.push("ledger.max_postings must be between 2 and 1000".into());
        }
        if l.max_amount <= 0 {
            errors.push("ledger.max_amount must be positive".into());
        }
        if l.max_pending_ttl_secs == 0 {
            errors.push("ledger.max_pending_ttl_secs must be positive".into());
        }
        if l.pending_sweep_interval_secs == 0 {
            errors.push("ledger.pending_sweep_interval_secs must be positive".into());
        }
        for (t, p) in &l.default_policies {
            if let Err(e) = p.validate() {
                errors.push(format!("ledger.default_policies.{t}: {e}"));
            }
        }
        for c in &l.extra_currencies {
            if let Err(e) = Currency::new(&c.code, c.minor_units) {
                errors.push(format!("ledger.extra_currencies: {e}"));
            }
        }
        let i = &self.idempotency;
        if i.ttl_secs == 0 || i.max_entries == 0 || i.purge_interval_secs == 0 {
            errors.push(
                "idempotency ttl_secs, max_entries and purge_interval_secs must be positive".into(),
            );
        }
        let mut ids = std::collections::BTreeSet::new();
        for key in &self.auth.api_keys {
            if key.id.is_empty() || key.id.len() > 64 {
                errors.push(format!(
                    "auth.api_keys: id {:?} must be 1..=64 characters",
                    key.id
                ));
            }
            if !ids.insert(key.id.clone()) {
                errors.push(format!("auth.api_keys: duplicate id {:?}", key.id));
            }
            if key.key_sha256.len() != 64 || hex::decode(&key.key_sha256).is_err() {
                errors.push(format!(
                    "auth.api_keys: key {:?} must have a 64 character hex sha256",
                    key.id
                ));
            }
        }
        if self.logging.level.trim().is_empty() {
            errors.push("logging.level must not be empty".into());
        }
        let r = &self.reconciliation;
        if r.amount_tolerance < 0 {
            errors.push("reconciliation.amount_tolerance must be >= 0".into());
        }
        if r.date_tolerance_days > 31 {
            errors.push("reconciliation.date_tolerance_days must be <= 31".into());
        }
        if r.max_statement_lines == 0 {
            errors.push("reconciliation.max_statement_lines must be positive".into());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(errors))
        }
    }

    /// Engine options derived from this configuration.
    pub fn ledger_options(&self) -> LedgerOptions {
        let st = &self.storage;
        let mut currencies = CurrencyRegistry::iso4217();
        for c in &self.ledger.extra_currencies {
            if let Ok(currency) = Currency::new(&c.code, c.minor_units) {
                currencies.register(currency);
            }
        }
        let mut policies = DefaultPolicies::default();
        policies
            .0
            .extend(self.ledger.default_policies.iter().map(|(k, v)| (*k, *v)));
        LedgerOptions {
            data_dir: st.data_dir.clone(),
            sync: match st.fsync {
                FsyncMode::Always => SyncMode::Always,
                FsyncMode::Batch => SyncMode::Batch {
                    every: st.fsync_batch,
                },
                FsyncMode::Never => SyncMode::Never,
            },
            segment_max_bytes: st.segment_max_bytes,
            max_record_bytes: st.max_record_bytes,
            snapshot_retain: st.snapshot_retain,
            recovery: RecoveryOptions {
                repair_torn_tail: st.repair_torn_tail,
                truncate_corruption: st.truncate_corruption,
                max_record_bytes: st.max_record_bytes,
                up_to_sequence: None,
                ignore_snapshots: false,
            },
            limits: EngineLimits {
                max_postings: self.ledger.max_postings,
                max_amount: self.ledger.max_amount,
                max_pending_ttl: Duration::from_secs(self.ledger.max_pending_ttl_secs),
            },
            default_policies: policies,
            currencies,
        }
    }

    pub fn idempotency_config(&self) -> IdempotencyConfig {
        IdempotencyConfig {
            ttl: Duration::from_secs(self.idempotency.ttl_secs),
            max_entries: self.idempotency.max_entries,
            sync: self.storage.fsync != FsyncMode::Never,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "5e884898da28047151d0e56f8dc6292773603d0d6aabbdd62a11ef721d1542d8";

    #[test]
    fn defaults_are_valid() {
        let c = Config::default();
        c.validate().unwrap();
        assert_eq!(c.server.bind.port(), 8080);
        let opts = c.ledger_options();
        assert_eq!(opts.sync, SyncMode::Always);
        assert_eq!(
            opts.default_policies.for_type(AccountType::Asset),
            BalancePolicy::NoOverdraft
        );
    }

    #[test]
    fn parses_full_toml() {
        let text = format!(
            r#"
            [server]
            bind = "0.0.0.0:9000"
            max_body_bytes = 65536
            [storage]
            data_dir = "/var/lib/ledger"
            fsync = "batch"
            fsync_batch = 8
            [ledger]
            max_postings = 10
            extra_currencies = [{{ code = "XTS", minor_units = 4 }}]
            [ledger.default_policies]
            asset = {{ kind = "overdraft", limit = 1000 }}
            [[auth.api_keys]]
            id = "ops"
            role = "admin"
            key_sha256 = "{HASH}"
            [logging]
            level = "debug"
            format = "pretty"
            "#
        );
        let c = Config::from_toml(&text).unwrap();
        c.validate().unwrap();
        assert_eq!(c.server.bind.port(), 9000);
        assert_eq!(c.storage.fsync, FsyncMode::Batch);
        assert_eq!(c.auth.api_keys[0].role, Role::Admin);
        let opts = c.ledger_options();
        assert_eq!(opts.sync, SyncMode::Batch { every: 8 });
        assert_eq!(opts.currencies.resolve("XTS").unwrap().minor_units, 4);
        assert_eq!(
            opts.default_policies.for_type(AccountType::Asset),
            BalancePolicy::Overdraft { limit: 1000 }
        );
        assert_eq!(opts.limits.max_postings, 10);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(Config::from_toml("[server]\nport = 1").is_err());
        assert!(Config::from_toml("[nope]\nx = 1").is_err());
    }

    #[test]
    fn env_overrides() {
        let mut c = Config::default();
        c.apply_env(vec![
            ("LEDGER_BIND".to_string(), "127.0.0.1:9999".to_string()),
            ("LEDGER_DATA_DIR".to_string(), "/tmp/l".to_string()),
            ("LEDGER_FSYNC".to_string(), "never".to_string()),
            ("LEDGER_METRICS_PUBLIC".to_string(), "false".to_string()),
            ("LEDGER_LOG_FORMAT".to_string(), "pretty".to_string()),
            (
                "LEDGER_API_KEYS".to_string(),
                format!("a:reader:{HASH}, b:writer:{HASH}"),
            ),
            ("UNRELATED".to_string(), "x".to_string()),
        ])
        .unwrap();
        assert_eq!(c.server.bind.port(), 9999);
        assert_eq!(c.storage.fsync, FsyncMode::Never);
        assert!(!c.server.metrics_public);
        assert_eq!(c.auth.api_keys.len(), 2);
        assert_eq!(c.auth.api_keys[1].role, Role::Writer);
        c.validate().unwrap();
    }

    #[test]
    fn bad_env_values_fail() {
        let mut c = Config::default();
        for (k, v) in [
            ("LEDGER_BIND", "nope"),
            ("LEDGER_FSYNC", "sometimes"),
            ("LEDGER_METRICS_PUBLIC", "maybe"),
            ("LEDGER_API_KEYS", "only-two:parts"),
            ("LEDGER_API_KEYS", "x:superuser:abc"),
            ("LEDGER_LOG_FORMAT", "xml"),
        ] {
            assert!(
                c.apply_env(vec![(k.to_string(), v.to_string())]).is_err(),
                "{k}={v}"
            );
        }
    }

    #[test]
    fn validation_collects_all_errors() {
        let mut c = Config::default();
        c.server.max_body_bytes = 1;
        c.storage.snapshot_retain = 0;
        c.ledger.max_postings = 1;
        c.auth.api_keys = vec![
            ApiKeyConfig {
                id: "dup".into(),
                role: Role::Reader,
                key_sha256: "short".into(),
            },
            ApiKeyConfig {
                id: "dup".into(),
                role: Role::Reader,
                key_sha256: HASH.into(),
            },
        ];
        c.ledger.extra_currencies.push(CurrencyConfig {
            code: "bad".into(),
            minor_units: 2,
        });
        match c.validate() {
            Err(ConfigError::Invalid(errors)) => {
                assert!(errors.len() >= 6, "{errors:?}");
                assert!(errors.iter().any(|e| e.contains("duplicate id")));
            }
            other => panic!("expected invalid config, got {other:?}"),
        }
    }

    #[test]
    fn load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.toml");
        std::fs::write(&path, "[server]\nrequest_timeout_secs = 5\n").unwrap();
        let c = Config::from_file(&path).unwrap();
        assert_eq!(c.server.request_timeout_secs, 5);
        assert!(matches!(
            Config::from_file(&dir.path().join("missing.toml")),
            Err(ConfigError::Read { .. })
        ));
        std::fs::write(&path, "[server\n").unwrap();
        assert!(matches!(
            Config::from_file(&path),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn role_ordering() {
        assert!(Role::Admin > Role::Writer && Role::Writer > Role::Reader);
        assert_eq!(Role::parse("writer"), Some(Role::Writer));
        assert_eq!(Role::Admin.as_str(), "admin");
        assert_eq!(Role::parse("root"), None);
    }
}
