//! Shared fixtures: a store on a throw-away MySQL database and a fake
//! `claude` binary.
//!
//! Tests are `#[sqlx::test(migrations = false)]`: sqlx creates one database
//! per test on the server named by `DATABASE_URL` (read from `.env`) and
//! drops it afterwards. [`store`] loads `schema/mysql.sql` into it.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use claude_job_runner::job::ChatId;
use claude_job_runner::store::JobStore;
use sqlx::MySqlPool;

/// Tables the runner needs, without the foreign keys to the rest of the app.
const SCHEMA: &str = include_str!("../../schema/mysql.sql");

/// Creates the tables in `pool` plus one chat to hang messages on.
pub async fn store(pool: MySqlPool) -> (JobStore, ChatId) {
    sqlx::raw_sql(SCHEMA)
        .execute(&pool)
        .await
        .expect("create tables");
    let done = sqlx::query("INSERT INTO chats (user_id, company_id) VALUES (1, 1)")
        .execute(&pool)
        .await
        .expect("create chat");
    let chat = ChatId::new(i64::try_from(done.last_insert_id()).unwrap());
    (JobStore::new(pool), chat)
}

/// A stand-in for the real CLI, driven entirely by the prompt on stdin:
///
/// * `ok:<text>`     – success report whose `result` is `<text>`
/// * `error`         – `is_error: true` report with a partial result
/// * `sleep:<secs>`  – sleeps, then succeeds
/// * `crash`         – writes to stderr and exits 2
/// * `garbage`       – exits 0 with non-JSON output
///
/// It also records its arguments and cwd in `args.txt` / `cwd.txt` inside
/// the working directory so tests can inspect how it was invoked.
pub fn fake_claude(dir: &Path) -> PathBuf {
    let script = dir.join("fake-claude.sh");
    std::fs::write(
        &script,
        r#"#!/usr/bin/env bash
set -u
prompt="$(cat)"
printf '%s\n' "$@" > args.txt
pwd > cwd.txt
printf '%s' "$prompt" > prompt.txt
escape() { local s="$1"; s="${s//\\/\\\\}"; s="${s//\"/\\\"}"; printf '%s' "$s"; }
success() {
  printf '{"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"%s","session_id":"fake-session","total_cost_usd":0.01,"permission_denials":[{"tool_name":"Bash"}]}' "$(escape "$1")"
}
case "$prompt" in
  ok:*)    success "${prompt#ok:}" ;;
  error)   printf '{"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":9,"result":"partial answer"}' ;;
  sleep:*) sleep "${prompt#sleep:}"; success "woke up" ;;
  crash)   echo "boom" >&2; exit 2 ;;
  garbage) echo "this is not json" ;;
  *)       echo "unknown prompt" >&2; exit 1 ;;
esac
"#,
    )
    .expect("write fake claude");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake claude");
    }
    script
}
