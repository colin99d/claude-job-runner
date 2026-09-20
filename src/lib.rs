//! Polls the `chat_messages` table for agentic user messages and answers
//! each one from an isolated Claude Code session.
//!
//! The crate is organised as a small pipeline:
//!
//! * [`store::JobStore`] claims agentic user rows whose `status` is `NULL`,
//!   flips them to `pending`, and later inserts the answer as an `ai` row
//!   and marks the job `done` or `failed`.
//! * [`workspace::Workspace`] creates a throw-away directory for one job and
//!   removes it afterwards.
//! * [`claude::CliRunner`] spawns `claude -p` inside that directory with an
//!   OS-level sandbox so the session can read the whole machine but write
//!   only inside the workspace.
//! * [`worker::Worker`] wires the three together in a polling loop.
//! * [`http`] exposes a tiny Hyper API for enqueuing and inspecting jobs.

pub mod claude;
pub mod config;
pub mod http;
pub mod job;
pub mod store;
pub mod worker;
pub mod workspace;
