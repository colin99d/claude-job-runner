//! Where credentials come from and how a connection is opened.
//!
//! Two sources, in this order:
//!
//! 1. an env file with `DB_HOST`, `DB_USER`, `DB_PASSWORD` (and optionally
//!    `DB_PORT`), when one is passed explicitly;
//! 2. `DATABASE_URL`, when set (this is what jobs get: a read-only login);
//! 3. otherwise the Granite Manager env file, [`DEFAULT_ENV_FILE`], which
//!    holds an admin login.
//!
//! Env-file credentials connect to `database` (default [`DEFAULT_DATABASE`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sqlx::mysql::{MySqlConnectOptions, MySqlConnection};
use sqlx::{ConnectOptions, Connection};

use crate::{Error, Result};

/// Env file used when neither `--env-file` nor `DATABASE_URL` is given,
/// relative to the home directory.
pub const DEFAULT_ENV_FILE: &str = "general_datebase/.env";

/// Database the deployed runner polls.
pub const DEFAULT_DATABASE: &str = "main";

/// Resolves connection options; see the module docs for the order.
pub fn resolve(env_file: Option<&Path>, database: &str) -> Result<MySqlConnectOptions> {
    if let Some(path) = env_file {
        return from_env_file(path, database);
    }
    if let Some(url) = std::env::var("DATABASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    {
        return from_url(&url);
    }
    from_env_file(&default_env_file()?, database)
}

/// Options from a `mysql://` URL.
pub fn from_url(url: &str) -> Result<MySqlConnectOptions> {
    url.parse()
        .map_err(|err| Error::Config(format!("invalid DATABASE_URL: {err}")))
}

/// Options from an env file holding `DB_*` variables.
pub fn from_env_file(path: &Path, database: &str) -> Result<MySqlConnectOptions> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| Error::Config(format!("cannot read env file {}: {err}", path.display())))?;
    let env = parse_db_vars(&text);
    let missing: Vec<_> = ["DB_HOST", "DB_USER", "DB_PASSWORD"]
        .into_iter()
        .filter(|key| !env.contains_key(*key))
        .collect();
    if !missing.is_empty() {
        return Err(Error::Config(format!(
            "{} lacks {}",
            path.display(),
            missing.join(", ")
        )));
    }
    let port = match env.get("DB_PORT") {
        Some(raw) => raw
            .parse()
            .map_err(|_| Error::Config(format!("invalid DB_PORT {raw:?}")))?,
        None => 3306,
    };
    Ok(MySqlConnectOptions::new()
        .host(&env["DB_HOST"])
        .port(port)
        .username(&env["DB_USER"])
        .password(&env["DB_PASSWORD"])
        .database(database))
}

/// Opens one connection (no pool: every tool here runs a handful of queries).
pub async fn connect(options: &MySqlConnectOptions) -> Result<MySqlConnection> {
    Ok(MySqlConnection::connect_with(&options.clone().disable_statement_logging()).await?)
}

fn default_env_file() -> Result<PathBuf> {
    std::env::home_dir()
        .map(|home| home.join(DEFAULT_ENV_FILE))
        .ok_or_else(|| Error::Config("cannot locate the home directory".to_owned()))
}

/// `DB_*=value` lines, last one wins.
fn parse_db_vars(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| {
            key.starts_with("DB_") && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_vars_ignore_everything_but_db_lines() {
        let vars = parse_db_vars("# c\nDB_HOST=h\nOTHER=x\n DB_USER=indented\nDB_PASSWORD=a=b\n");
        assert_eq!(vars.get("DB_HOST").map(String::as_str), Some("h"));
        assert_eq!(vars.get("DB_PASSWORD").map(String::as_str), Some("a=b"));
        assert!(!vars.contains_key("OTHER"));
        assert!(!vars.contains_key(" DB_USER"));
    }

    #[test]
    fn env_file_must_have_host_user_and_password() {
        let dir = std::env::temp_dir().join(format!("jobctl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "DB_HOST=h\n").unwrap();
        let err = from_env_file(&path, "main").unwrap_err().to_string();
        assert!(err.ends_with("lacks DB_USER, DB_PASSWORD"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
