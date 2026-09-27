//! Tools for the chat database and the runner, shared by the `jobctl` CLI
//! and the `jobctl-mcp` server.
//!
//! * [`db`] resolves credentials and opens a connection.
//! * [`sql`] runs an arbitrary statement and renders the result as text.
//! * [`chats`] reads and writes `chats` / `chat_messages` the way the chat
//!   application does.
//! * [`api`] talks to a running daemon over its HTTP API.
//! * [`qbo`] reads a company's QuickBooks Online account for a job.
//!
//! Everything here returns data; printing is left to the binaries.

pub mod api;
pub mod chats;
pub mod db;
pub mod qbo;
pub mod sql;

/// Errors produced by this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The database driver reported an error.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    /// Credentials could not be resolved.
    #[error("{0}")]
    Config(String),
    /// A row referenced by the caller does not exist.
    #[error("{0}")]
    NotFound(String),
    /// Talking to the runner's HTTP API failed.
    #[error("{0}")]
    Http(String),
    /// A tool argument is missing or malformed.
    #[error("{0}")]
    InvalidArgument(String),
    /// The requester may not see what they asked for.
    #[error("{0}")]
    Denied(String),
    /// Local I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
