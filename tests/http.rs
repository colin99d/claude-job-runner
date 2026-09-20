//! HTTP API served on a random local port.

mod common;

use std::net::SocketAddr;

use claude_job_runner::http;
use claude_job_runner::job::{JobOutcome, JobStatus};
use claude_job_runner::store::JobStore;
use serde_json::Value;
use sqlx::MySqlPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

struct Api {
    addr: SocketAddr,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), http::ServeError>>,
}

async fn start(store: JobStore) -> Api {
    let listener = http::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(http::serve(store, listener, shutdown.clone()));
    Api {
        addr,
        shutdown,
        task,
    }
}

/// Tiny HTTP/1.1 client: returns (status, body).
async fn call(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").expect("headers");
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code");
    (status, body.to_owned())
}

#[sqlx::test(migrations = false)]
async fn health_endpoint(pool: MySqlPool) {
    let (store, _) = common::store(pool).await;
    let api = start(store).await;

    let (status, body) = call(api.addr, "GET", "/health", None).await;
    assert_eq!(status, 200);
    assert_eq!(body, r#"{"status":"ok"}"#);

    api.shutdown.cancel();
    api.task.await.unwrap().unwrap();
}

#[sqlx::test(migrations = false)]
async fn create_then_fetch_job(pool: MySqlPool) {
    let (store, chat) = common::store(pool).await;
    let api = start(store.clone()).await;

    let body = format!(r#"{{"chat_id":{chat},"content":"say hi"}}"#);
    let (status, body) = call(api.addr, "POST", "/jobs", Some(&body)).await;
    assert_eq!(status, 201, "{body}");
    let created: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(created["chat_id"], chat.get());
    assert_eq!(created["content"], "say hi");
    assert_eq!(created["status"], "new");
    assert_eq!(created["reply"], Value::Null);

    let (status, body) = call(api.addr, "GET", &format!("/jobs/{}", created["id"]), None).await;
    assert_eq!(status, 200);
    let fetched: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(fetched, created);

    // The row is genuinely in the table the worker polls, and once it is
    // answered the API shows the reply text.
    let job = store.claim_next().await.unwrap().unwrap();
    assert_eq!(job.id.get(), created["id"].as_i64().unwrap());
    store
        .finish(
            job.id,
            &JobOutcome::Success {
                result: "hello".to_owned(),
            },
        )
        .await
        .unwrap();
    let (_, body) = call(api.addr, "GET", &format!("/jobs/{}", job.id), None).await;
    let fetched: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(fetched["status"], JobStatus::Done.to_string());
    assert_eq!(fetched["reply"], "hello");
    assert!(fetched["reply_id"].is_i64());

    api.shutdown.cancel();
    api.task.await.unwrap().unwrap();
}

#[sqlx::test(migrations = false)]
async fn validation_and_not_found(pool: MySqlPool) {
    let (store, chat) = common::store(pool).await;
    let api = start(store).await;

    let body = format!(r#"{{"chat_id":{chat},"content":"   "}}"#);
    let (status, body) = call(api.addr, "POST", "/jobs", Some(&body)).await;
    assert_eq!(status, 400);
    assert!(body.contains("must not be empty"), "{body}");

    let (status, body) = call(api.addr, "POST", "/jobs", Some(r#"{"content":"no chat"}"#)).await;
    assert_eq!(status, 400);
    assert!(body.contains("invalid JSON"), "{body}");

    let (status, body) = call(api.addr, "POST", "/jobs", Some("not json")).await;
    assert_eq!(status, 400);
    assert!(body.contains("invalid JSON"), "{body}");

    let (status, _) = call(api.addr, "GET", "/jobs/12345", None).await;
    assert_eq!(status, 404);

    let (status, _) = call(api.addr, "GET", "/jobs/abc", None).await;
    assert_eq!(status, 404);

    let (status, _) = call(api.addr, "DELETE", "/jobs/1", None).await;
    assert_eq!(status, 404);

    api.shutdown.cancel();
    api.task.await.unwrap().unwrap();
}
