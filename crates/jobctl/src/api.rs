//! A client for the daemon's HTTP API (`/health`, `/jobs`, `/jobs/{id}`).
//!
//! The API only ever listens on a local address, so this speaks plain
//! HTTP/1.1 over a `TcpStream` with `Connection: close` and reads the
//! response to EOF instead of pulling in an HTTP client.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::{Error, Result};

/// Address used when `HTTP_ADDR` is not set, same as the daemon's default.
pub const DEFAULT_ADDR: &str = "127.0.0.1:8080";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Handle to one daemon.
#[derive(Debug, Clone)]
pub struct Api {
    addr: String,
}

impl Api {
    /// Talks to the daemon listening on `addr` (`host:port`).
    #[must_use]
    pub fn new(addr: impl Into<String>) -> Self {
        Self { addr: addr.into() }
    }

    /// The `host:port` this handle talks to.
    #[must_use]
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Whether a daemon answers `/health` within two seconds.
    pub async fn healthy(&self) -> bool {
        matches!(
            timeout(Duration::from_secs(2), self.request("GET", "/health", None)).await,
            Ok(Ok((200, _)))
        )
    }

    /// `POST /jobs`: queues `content` in `chat_id` and returns the job.
    pub async fn create_job(&self, chat_id: i64, content: &str) -> Result<Value> {
        let body = json!({ "chat_id": chat_id, "content": content }).to_string();
        self.json("POST", "/jobs", Some(&body)).await
    }

    /// `GET /jobs/{id}`.
    pub async fn job(&self, id: i64) -> Result<Value> {
        self.json("GET", &format!("/jobs/{id}"), None).await
    }

    async fn json(&self, method: &str, path: &str, body: Option<&str>) -> Result<Value> {
        let (status, text) = timeout(REQUEST_TIMEOUT, self.request(method, path, body))
            .await
            .map_err(|_| Error::Http(format!("{method} {path}: timed out")))??;
        if !(200..300).contains(&status) {
            return Err(Error::Http(format!(
                "{method} {path}: HTTP {status}: {text}"
            )));
        }
        serde_json::from_str(&text)
            .map_err(|err| Error::Http(format!("{method} {path}: invalid JSON: {err}")))
    }

    async fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
        let mut stream = TcpStream::connect(&self.addr)
            .await
            .map_err(|err| Error::Http(format!("cannot reach {}: {err}", self.addr)))?;
        let body = body.unwrap_or("");
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            self.addr,
            body.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body.as_bytes()).await?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await?;
        parse_response(&String::from_utf8_lossy(&raw))
    }
}

/// Status code and body of a `Connection: close` response with a
/// `Content-Length` body (which is all the daemon ever sends).
fn parse_response(raw: &str) -> Result<(u16, String)> {
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| Error::Http("truncated HTTP response".to_owned()))?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| Error::Http(format!("bad status line: {head:.40}")))?;
    Ok((status, body.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_is_split_into_status_and_body() {
        let raw = "HTTP/1.1 201 Created\r\ncontent-length: 8\r\n\r\n{\"id\":1}";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 201);
        assert_eq!(body, r#"{"id":1}"#);
        assert!(parse_response("HTTP/1.1 200 OK\r\n").is_err());
    }
}
