//! Process configuration, read from environment variables (and `.env`).

use std::env::{self, VarError};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::claude::{ClaudeConfig, PermissionMode};

/// Errors from reading the environment.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A required variable is absent.
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    /// A variable is present but could not be parsed.
    #[error("invalid value for {name}: {reason}")]
    Invalid {
        /// Which variable.
        name: &'static str,
        /// Why it was rejected.
        reason: String,
    },
}

/// Fully resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// `DATABASE_URL`: `mysql://...`.
    pub database_url: String,
    /// `DB_MAX_CONNECTIONS` (default 5).
    pub db_max_connections: u32,
    /// `WORKSPACE_ROOT`: where per-job directories are created.
    pub workspace_root: PathBuf,
    /// `POLL_INTERVAL_SECS` (default 5).
    pub poll_interval: Duration,
    /// `MAX_CONCURRENT_JOBS` (default 1): how many jobs may run at once.
    pub max_concurrent_jobs: NonZeroUsize,
    /// `HTTP_ADDR` (default `127.0.0.1:8080`).
    pub http_addr: SocketAddr,
    /// `REQUEUE_PENDING_ON_START` (default `true`): reset orphaned `pending`
    /// rows at start-up. Disable when running several workers.
    pub requeue_pending_on_start: bool,
    /// Settings for the `claude` subprocess.
    pub claude: ClaudeConfig,
}

impl Config {
    /// Reads configuration from the process environment.
    ///
    /// Call [`dotenvy::dotenv`] first if `.env` support is wanted.
    pub fn from_env() -> Result<Self, ConfigError> {
        let claude = ClaudeConfig {
            binary: optional("CLAUDE_BIN")?.unwrap_or_else(|| PathBuf::from("claude")),
            model: optional("CLAUDE_MODEL")?,
            max_turns: optional("CLAUDE_MAX_TURNS")?.unwrap_or(50),
            max_budget_usd: optional("CLAUDE_MAX_BUDGET_USD")?,
            timeout: Duration::from_secs(optional("CLAUDE_TIMEOUT_SECS")?.unwrap_or(30 * 60)),
            permission_mode: optional::<PermissionMode>("CLAUDE_PERMISSION_MODE")?
                .unwrap_or_default(),
            allowed_domains: list("CLAUDE_ALLOWED_DOMAINS")?,
            persist_sessions: optional("CLAUDE_PERSIST_SESSIONS")?.unwrap_or(false),
            config_dir: optional("CLAUDE_CONFIG_DIR")?,
            mcp_config: optional("CLAUDE_MCP_CONFIG")?,
        };

        Ok(Self {
            database_url: required("DATABASE_URL")?,
            db_max_connections: optional("DB_MAX_CONNECTIONS")?.unwrap_or(5),
            workspace_root: optional("WORKSPACE_ROOT")?
                .unwrap_or_else(|| PathBuf::from("workspaces")),
            poll_interval: Duration::from_secs(optional("POLL_INTERVAL_SECS")?.unwrap_or(5)),
            max_concurrent_jobs: optional("MAX_CONCURRENT_JOBS")?.unwrap_or(NonZeroUsize::MIN),
            http_addr: optional("HTTP_ADDR")?
                .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 8080))),
            requeue_pending_on_start: optional("REQUEUE_PENDING_ON_START")?.unwrap_or(true),
            claude,
        })
    }
}

fn raw(name: &'static str) -> Result<Option<String>, ConfigError> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(ConfigError::Invalid {
            name,
            reason: "not valid unicode".to_owned(),
        }),
    }
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    raw(name)?.ok_or(ConfigError::Missing(name))
}

fn optional<T>(name: &'static str) -> Result<Option<T>, ConfigError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    raw(name)?
        .map(|value| {
            value
                .trim()
                .parse()
                .map_err(|err: T::Err| ConfigError::Invalid {
                    name,
                    reason: err.to_string(),
                })
        })
        .transpose()
}

fn list(name: &'static str) -> Result<Vec<String>, ConfigError> {
    Ok(raw(name)?
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}
