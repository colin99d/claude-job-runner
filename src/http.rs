//! Minimal HTTP API on Hyper for enqueuing and inspecting jobs.
//!
//! Routes:
//!
//! | Method | Path         | Body                              | Response                 |
//! |--------|--------------|-----------------------------------|--------------------------|
//! | GET    | `/health`    |                                   | `{"status":"ok"}`        |
//! | POST   | `/jobs`      | `{"chat_id": 1, "content":"..."}` | `201` with the new job   |
//! | GET    | `/jobs/{id}` |                                   | the job, or `404`        |
//!
//! A job is `{"id","chat_id","content","status","reply_id","error","reply"}`,
//! where `reply` is the text of the `ai` message once the job is `done`.

use std::net::SocketAddr;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::job::{ChatId, Job, MessageId};
use crate::store::{JobStore, StoreError};
use crate::worker::chain;

/// Largest request body accepted, in bytes.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Errors from serving the API.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// Binding or accepting on the listener failed.
    #[error("http listener error on {addr}")]
    Listener {
        /// Address we tried to serve on.
        addr: SocketAddr,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
}

/// Request body for `POST /jobs`.
#[derive(Debug, Deserialize)]
pub struct CreateJob {
    /// Chat the message (and its answer) belong to.
    pub chat_id: ChatId,
    /// Prompt to hand to Claude Code.
    pub content: String,
}

/// Response body for a job: the row plus the answer text, if any.
#[derive(Debug, Serialize)]
pub struct JobView {
    /// The agentic user message.
    #[serde(flatten)]
    pub job: Job,
    /// Content of the `ai` message once the job is `done`.
    pub reply: Option<String>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

/// Failures a handler can turn into an HTTP status.
#[derive(Debug, thiserror::Error)]
enum ApiError {
    #[error("not found")]
    NotFound,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("internal error")]
    Store(#[from] StoreError),
}

impl ApiError {
    const fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

type ApiResponse = Response<Full<Bytes>>;

/// Routes one request. Pure with respect to the network, so it is unit-testable.
pub async fn handle(store: &JobStore, req: Request<Incoming>) -> ApiResponse {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let outcome = route(store, &method, &path, req).await;
    match outcome {
        Ok(response) => response,
        Err(err) => {
            if let ApiError::Store(source) = &err {
                error!(error = %chain(source), "store error while serving request");
            }
            let body = ErrorBody {
                error: &err.to_string(),
            };
            json_response(err.status(), &body)
        }
    }
}

async fn route(
    store: &JobStore,
    method: &Method,
    path: &str,
    req: Request<Incoming>,
) -> Result<ApiResponse, ApiError> {
    match (method, path) {
        (&Method::GET, "/health") => Ok(json_response(
            StatusCode::OK,
            &serde_json::json!({ "status": "ok" }),
        )),
        (&Method::POST, "/jobs") => {
            let create: CreateJob = read_json(req).await?;
            if create.content.trim().is_empty() {
                return Err(ApiError::BadRequest("content must not be empty".to_owned()));
            }
            let id = store.insert(create.chat_id, &create.content).await?;
            let view = load_view(store, id).await?;
            Ok(json_response(StatusCode::CREATED, &view))
        }
        (&Method::GET, _) if path.starts_with("/jobs/") => {
            let id = parse_job_id(path).ok_or(ApiError::NotFound)?;
            let view = load_view(store, id).await?;
            Ok(json_response(StatusCode::OK, &view))
        }
        _ => Err(ApiError::NotFound),
    }
}

async fn load_view(store: &JobStore, id: MessageId) -> Result<JobView, ApiError> {
    let job = store.get(id).await?.ok_or(ApiError::NotFound)?;
    let reply = match job.reply_id {
        Some(reply_id) => store.reply(reply_id).await?,
        None => None,
    };
    Ok(JobView { job, reply })
}

fn parse_job_id(path: &str) -> Option<MessageId> {
    path.strip_prefix("/jobs/")?
        .parse::<i64>()
        .ok()
        .filter(|raw| *raw > 0)
        .map(MessageId::new)
}

async fn read_json<T: for<'de> Deserialize<'de>>(req: Request<Incoming>) -> Result<T, ApiError> {
    let bytes = Limited::new(req.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
        .map_err(|err| ApiError::BadRequest(format!("could not read body: {err}")))?
        .to_bytes();
    serde_json::from_slice(&bytes)
        .map_err(|err| ApiError::BadRequest(format!("invalid JSON body: {err}")))
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> ApiResponse {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|err| {
        error!(error = %err, "could not serialise response");
        br#"{"error":"serialisation failure"}"#.to_vec()
    });
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(bytes)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

/// Binds a listener for [`serve`].
pub async fn bind(addr: SocketAddr) -> Result<TcpListener, ServeError> {
    TcpListener::bind(addr)
        .await
        .map_err(|source| ServeError::Listener { addr, source })
}

/// Serves the API on `listener` until `shutdown` is cancelled, then drains
/// in-flight connections.
pub async fn serve(
    store: JobStore,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> Result<(), ServeError> {
    if let Ok(addr) = listener.local_addr() {
        info!(%addr, "http api listening");
    }

    let graceful = GracefulShutdown::new();
    loop {
        let accepted = tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(err) => {
                warn!(error = %err, "accept failed");
                continue;
            }
        };
        let store = store.clone();
        let service = service_fn(move |req| {
            let store = store.clone();
            async move { Ok::<_, hyper::Error>(handle(&store, req).await) }
        });
        let conn = http1::Builder::new().serve_connection(TokioIo::new(stream), service);
        let conn = graceful.watch(conn);
        tokio::spawn(async move {
            if let Err(err) = conn.await {
                warn!(%peer, error = %err, "connection error");
            }
        });
    }

    info!("http api draining connections");
    graceful.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_id_parsing_is_strict() {
        assert_eq!(parse_job_id("/jobs/12"), Some(MessageId::new(12)));
        assert_eq!(parse_job_id("/jobs/0"), None);
        assert_eq!(parse_job_id("/jobs/-1"), None);
        assert_eq!(parse_job_id("/jobs/abc"), None);
        assert_eq!(parse_job_id("/jobs/"), None);
        assert_eq!(parse_job_id("/jobs/1/extra"), None);
    }
}
