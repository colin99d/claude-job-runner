//! `jobctl-mcp` - `jobctl` commands as MCP tools, over stdio.
//!
//! The runner starts one of these per job. MCP servers run outside Claude
//! Code's Bash sandbox, whose network only goes out through an HTTP/SOCKS
//! proxy, so this is how a job reaches the database. It connects with the
//! job's `DATABASE_URL`, which the runner sets to a read-only login; that
//! login, not this server, is what keeps jobs from writing.
//!
//! When the runner gives the job QuickBooks access (`QBO_ACCESS_TOKEN` and
//! friends, see [`jobctl::qbo`]), the `qbo_*` tools are offered as well.
//!
//! Adding a tool: describe it in [`tools`] and handle it in [`Server::call`],
//! reusing the `jobctl` library for the actual work.

use std::process::ExitCode;

use jobctl::{db, qbo, sql};
use serde_json::{Value, json};
use sqlx::mysql::MySqlConnection;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Answered when the client does not say which version it speaks.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut server = Server {
        qbo: qbo::Session::from_env().unwrap_or_else(|err| {
            eprintln!("jobctl-mcp: QuickBooks tools disabled: {err}");
            None
        }),
        ..Server::default()
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("jobctl-mcp: reading stdin: {err}");
                return ExitCode::FAILURE;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => server.handle(message).await,
            Err(err) => Some(error(&Value::Null, -32700, &format!("parse error: {err}"))),
        };
        if let Some(reply) = reply {
            let mut out = reply.to_string();
            out.push('\n');
            if stdout.write_all(out.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                return ExitCode::FAILURE;
            }
        }
    }
}

/// Every tool this server offers, as `tools/list` reports them.
fn tools(qbo: Option<&qbo::Session>) -> Value {
    let mut tools = json!([
        {
            "name": "sql",
            "description": "Run one SQL statement against the company's MySQL database \
                (read-only login) and return the result as tab-separated text with a header \
                row. Use it for SELECT, SHOW TABLES, DESCRIBE <table>, SHOW CREATE TABLE <table>, etc.",
            "inputSchema": {
                "type": "object",
                "properties": { "sql": { "type": "string", "description": "the SQL statement" } },
                "required": ["sql"],
            },
        },
    ]);
    if let (Some(session), Value::Array(list)) = (qbo, &mut tools) {
        list.extend(qbo::tools(session.scope().is_admin));
    }
    tools
}

/// Per-process state: the database connection, opened on first use, and the
/// job's QuickBooks session, if it has one.
#[derive(Default)]
struct Server {
    conn: Option<MySqlConnection>,
    qbo: Option<qbo::Session>,
}

impl Server {
    /// Answers one JSON-RPC message; notifications get no reply.
    async fn handle(&mut self, message: Value) -> Option<Value> {
        let id = message.get("id")?.clone();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match message["method"].as_str().unwrap_or_default() {
            "initialize" => json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or(DEFAULT_PROTOCOL_VERSION),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "jobctl", "version": env!("CARGO_PKG_VERSION") },
            }),
            "tools/list" => json!({ "tools": tools(self.qbo.as_ref()) }),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let Some((text, is_error)) = self.call(name, &params["arguments"]).await else {
                    return Some(error(&id, -32602, &format!("unknown tool {name:?}")));
                };
                json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
            }
            "ping" => json!({}),
            method => return Some(error(&id, -32601, &format!("method not found: {method}"))),
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    /// Runs a tool: `None` for an unknown name, else its text and error flag.
    async fn call(&mut self, name: &str, arguments: &Value) -> Option<(String, bool)> {
        let outcome = match name {
            "sql" => {
                self.sql(arguments["sql"].as_str().unwrap_or_default())
                    .await
            }
            _ => self.qbo(name.strip_prefix("qbo_")?, arguments).await?,
        };
        Some(match outcome {
            Ok(text) => (text, false),
            Err(err) => (err.to_string(), true),
        })
    }

    async fn sql(&mut self, statement: &str) -> jobctl::Result<String> {
        if statement.trim().is_empty() {
            return Err(jobctl::Error::Config(
                "the sql argument is empty".to_owned(),
            ));
        }
        let outcome = sql::query(self.connection().await?, statement).await;
        self.forget_broken_connection(&outcome);
        outcome
    }

    /// Runs QuickBooks tool `name`; `None` if this job has no such tool.
    async fn qbo(&mut self, name: &str, arguments: &Value) -> Option<jobctl::Result<String>> {
        let session = self.qbo.clone()?;
        if let Err(err) = session.authorize(name)? {
            return Some(Err(err));
        }
        let conn = match self.connection().await {
            Ok(conn) => conn,
            Err(err) => return Some(Err(err)),
        };
        let outcome = session.call(conn, name, arguments).await?;
        self.forget_broken_connection(&outcome);
        Some(outcome)
    }

    async fn connection(&mut self) -> jobctl::Result<&mut MySqlConnection> {
        Ok(match &mut self.conn {
            Some(conn) => conn,
            conn @ None => conn.insert(connect().await?),
        })
    }

    /// A dropped connection (idle timeout, server restart) is reopened on
    /// the next call instead of failing every call after it.
    fn forget_broken_connection<T>(&mut self, outcome: &jobctl::Result<T>) {
        if matches!(outcome, Err(jobctl::Error::Database(err)) if is_connection_error(err)) {
            self.conn = None;
        }
    }
}

async fn connect() -> jobctl::Result<MySqlConnection> {
    let url = std::env::var("DATABASE_URL").map_err(|_| {
        jobctl::Error::Config(
            "DATABASE_URL is not set; no database is configured for this job".to_owned(),
        )
    })?;
    db::connect(&db::from_url(&url)?).await
}

fn is_connection_error(err: &sqlx::Error) -> bool {
    matches!(
        err,
        sqlx::Error::Io(_) | sqlx::Error::Protocol(_) | sqlx::Error::Tls(_)
    )
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ask(server: &mut Server, message: Value) -> Option<Value> {
        server.handle(message).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initialize_echoes_the_protocol_version() {
        let reply = ask(
            &mut Server::default(),
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}),
        )
        .await
        .unwrap();
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["protocolVersion"], "2099-01-01");
        assert_eq!(reply["result"]["serverInfo"]["name"], "jobctl");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn notifications_get_no_reply() {
        let reply = ask(
            &mut Server::default(),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
        assert!(reply.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tools_are_listed_and_unknown_ones_rejected() {
        let mut server = Server::default();
        let list = ask(
            &mut server,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await
        .unwrap();
        assert_eq!(list["result"]["tools"][0]["name"], "sql");
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 1);

        let unknown = ask(
            &mut server,
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"nope","arguments":{}}}),
        )
        .await
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32602);

        let missing = ask(&mut server, json!({"jsonrpc":"2.0","id":4,"method":"nope"}))
            .await
            .unwrap();
        assert_eq!(missing["error"]["code"], -32601);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quickbooks_tools_follow_the_session() {
        let scope = |is_admin| qbo::Scope {
            company_id: 1,
            user_id: 2,
            is_admin,
        };
        let mut server = Server {
            qbo: Some(qbo::Session::new("https://x", "123", "t", scope(false)).unwrap()),
            ..Server::default()
        };
        let list = ask(
            &mut server,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await
        .unwrap();
        let names: Vec<_> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        assert!(names.contains(&"qbo_invoice".to_owned()));
        assert!(!names.contains(&"qbo_query".to_owned()));

        // Unknown and admin-only tools are refused before the database or
        // QuickBooks is contacted (this test has neither).
        let reply = ask(
            &mut server,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"qbo_query","arguments":{"query":"select * from Invoice"}}}),
        )
        .await
        .unwrap();
        assert_eq!(reply["result"]["isError"], true);
        assert!(
            reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("only admins")
        );

        let reply = ask(
            &mut server,
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"qbo_nope","arguments":{}}}),
        )
        .await
        .unwrap();
        assert_eq!(reply["error"]["code"], -32602);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_sql_is_a_tool_error() {
        let reply = ask(
            &mut Server::default(),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"sql","arguments":{"sql":" "}}}),
        )
        .await
        .unwrap();
        assert_eq!(reply["result"]["isError"], true);
    }
}
