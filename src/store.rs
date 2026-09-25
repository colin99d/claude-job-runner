//! Persistence layer over the `chat_messages` table (MySQL 8).
//!
//! A job is a `chat_messages` row with `sender = 'user'` and
//! `is_agentic = 1`. Claiming is an optimistic
//! `UPDATE ... WHERE status IS NULL` checked via `rows_affected`, so a row is
//! never handed to two workers. The answer is inserted as a new `sender = 'ai'`
//! row in the same chat, and bookkeeping lives in the JSON `payload` column:
//!
//! * user row, on success: `payload.reply_id` = id of the `ai` row;
//! * user row, on failure: `payload.error` (and `payload.result` when Claude
//!   produced a partial answer);
//! * ai row: `payload.in_reply_to` = id of the user row.
//!
//! The runner never writes `chats`; the chat application owns that table.
//! It only reads the chat's `user_id`/`company_id` to tell a job who asked.

use sqlx::mysql::MySqlPoolOptions;
use sqlx::{FromRow, MySqlPool, Row};

use crate::job::{ChatId, Job, JobOutcome, JobStatus, MessageId, Requester, UnknownStatus};

/// How many times [`JobStore::claim_next`] retries when another worker
/// wins the race for the same row.
const CLAIM_ATTEMPTS: usize = 5;

/// Errors produced by [`JobStore`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database driver reported an error.
    #[error("database error")]
    Database(#[from] sqlx::Error),
    /// A row holds a `status` value this crate does not understand.
    #[error("message {id} has an invalid status")]
    InvalidStatus {
        /// Which message.
        id: MessageId,
        /// What was wrong with it.
        #[source]
        source: UnknownStatus,
    },
    /// An update expected the job to be `pending` but it was not.
    #[error("message {0} is not pending")]
    NotPending(MessageId),
    /// The driver reported an inserted id that does not fit the id type.
    #[error("insert id {0} is out of range")]
    InsertIdOutOfRange(u64),
}

/// Converts MySQL's unsigned `LAST_INSERT_ID()` into a [`MessageId`].
fn insert_id(raw: u64) -> Result<MessageId, StoreError> {
    i64::try_from(raw)
        .map(MessageId::new)
        .map_err(|_| StoreError::InsertIdOutOfRange(raw))
}

/// Raw shape of a job row before status validation.
#[derive(Debug, FromRow)]
struct JobRow {
    id: i64,
    chat_id: i64,
    content: String,
    status: Option<String>,
    payload: Option<serde_json::Value>,
    /// From `chats`; `NULL` only if the chat row is missing.
    user_id: Option<i64>,
    company_id: Option<i64>,
}

impl TryFrom<JobRow> for Job {
    type Error = StoreError;

    fn try_from(row: JobRow) -> Result<Self, Self::Error> {
        let id = MessageId::new(row.id);
        let status = JobStatus::from_db(row.status.as_deref())
            .map_err(|source| StoreError::InvalidStatus { id, source })?;
        let payload = row.payload.as_ref();
        let reply_id = payload
            .and_then(|p| p.get("reply_id"))
            .and_then(serde_json::Value::as_i64)
            .map(MessageId::new);
        let error = payload
            .and_then(|p| p.get("error"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Ok(Self {
            id,
            chat_id: ChatId::new(row.chat_id),
            content: row.content,
            status,
            reply_id,
            error,
            requester: row
                .user_id
                .zip(row.company_id)
                .map(|(user_id, company_id)| Requester {
                    user_id,
                    company_id,
                }),
        })
    }
}

/// Handle to the `chat_messages` table.
#[derive(Debug, Clone)]
pub struct JobStore {
    pool: MySqlPool,
}

impl JobStore {
    /// Connects to `database_url` (a `mysql://` URL).
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = MySqlPoolOptions::new()
            .max_connections(max_connections)
            .connect(database_url)
            .await?;
        Ok(Self::new(pool))
    }

    /// Wraps an existing pool.
    #[must_use]
    pub fn new(pool: MySqlPool) -> Self {
        Self { pool }
    }

    /// The underlying pool, for callers that need raw access (tests, migrations).
    #[must_use]
    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }

    /// Inserts an agentic user message with `status = NULL` and returns its id.
    ///
    /// The chat application normally does this itself; the runner only
    /// needs it for the HTTP API and tests.
    pub async fn insert(&self, chat_id: ChatId, content: &str) -> Result<MessageId, StoreError> {
        let done = sqlx::query(
            "INSERT INTO chat_messages (chat_id, sender, content, is_agentic) \
             VALUES (?, 'user', ?, 1)",
        )
        .bind(chat_id.get())
        .bind(content)
        .execute(&self.pool)
        .await?;
        insert_id(done.last_insert_id())
    }

    /// Fetches a job by id. Rows that are not agentic user messages are
    /// reported as absent.
    pub async fn get(&self, id: MessageId) -> Result<Option<Job>, StoreError> {
        let row: Option<JobRow> = sqlx::query_as(
            "SELECT m.id, m.chat_id, m.content, m.status, m.payload, c.user_id, c.company_id \
             FROM chat_messages m LEFT JOIN chats c ON c.id = m.chat_id \
             WHERE m.id = ? AND m.sender = 'user' AND m.is_agentic = 1",
        )
        .bind(id.get())
        .fetch_optional(&self.pool)
        .await?;
        row.map(Job::try_from).transpose()
    }

    /// Fetches the text of an `ai` message, typically a job's `reply_id`.
    pub async fn reply(&self, id: MessageId) -> Result<Option<String>, StoreError> {
        let row = sqlx::query("SELECT content FROM chat_messages WHERE id = ? AND sender = 'ai'")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| r.try_get("content"))
            .transpose()
            .map_err(StoreError::from)
    }

    /// Atomically claims the oldest unclaimed job, marking it `pending`.
    ///
    /// Returns `None` when there is nothing to do. Safe to call from
    /// several workers at once: each row is handed out exactly once.
    pub async fn claim_next(&self) -> Result<Option<Job>, StoreError> {
        for _ in 0..CLAIM_ATTEMPTS {
            let Some(id) = self.oldest_unclaimed_id().await? else {
                return Ok(None);
            };
            if self.try_claim(id).await? {
                let job = self.get(id).await?;
                return Ok(job);
            }
            // Another worker got there first; look for the next row.
        }
        Ok(None)
    }

    async fn oldest_unclaimed_id(&self) -> Result<Option<MessageId>, StoreError> {
        let row = sqlx::query(
            "SELECT id FROM chat_messages \
             WHERE sender = 'user' AND is_agentic = 1 AND status IS NULL \
             ORDER BY id LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| r.try_get::<i64, _>("id").map(MessageId::new))
            .transpose()
            .map_err(StoreError::from)
    }

    async fn try_claim(&self, id: MessageId) -> Result<bool, StoreError> {
        let done = sqlx::query(
            "UPDATE chat_messages SET status = ? \
             WHERE id = ? AND sender = 'user' AND is_agentic = 1 AND status IS NULL",
        )
        .bind(JobStatus::Pending.to_db())
        .bind(id.get())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Records the outcome of a `pending` job.
    ///
    /// On success the answer is inserted as an `ai` message in the same chat
    /// and the job becomes `done`; on failure the job becomes `failed` with
    /// the error in its `payload`. Fails with [`StoreError::NotPending`] if
    /// the row is not `pending`, which guards against two workers finishing
    /// the same job.
    pub async fn finish(&self, id: MessageId, outcome: &JobOutcome) -> Result<(), StoreError> {
        match outcome {
            JobOutcome::Success { result } => self.finish_done(id, result).await,
            JobOutcome::Failed { error, result } => {
                self.finish_failed(id, error, result.as_deref()).await
            }
        }
    }

    async fn finish_done(&self, id: MessageId, result: &str) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        // Copying `chat_id` from the pending row itself makes the insert a
        // no-op when the job is not pending any more.
        let inserted = sqlx::query(
            "INSERT INTO chat_messages (chat_id, sender, content, is_agentic, payload) \
             SELECT chat_id, 'ai', ?, 1, JSON_OBJECT('in_reply_to', id) \
             FROM chat_messages WHERE id = ? AND status = ?",
        )
        .bind(result)
        .bind(id.get())
        .bind(JobStatus::Pending.to_db())
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(StoreError::NotPending(id));
        }
        let reply_id = insert_id(inserted.last_insert_id())?;
        sqlx::query(
            "UPDATE chat_messages \
             SET status = ?, payload = JSON_SET(COALESCE(payload, JSON_OBJECT()), '$.reply_id', ?) \
             WHERE id = ?",
        )
        .bind(JobStatus::Done.to_db())
        .bind(reply_id.get())
        .bind(id.get())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn finish_failed(
        &self,
        id: MessageId,
        error: &str,
        result: Option<&str>,
    ) -> Result<(), StoreError> {
        let sql = if result.is_some() {
            "UPDATE chat_messages \
             SET status = ?, payload = JSON_SET(COALESCE(payload, JSON_OBJECT()), '$.error', ?, '$.result', ?) \
             WHERE id = ? AND status = ?"
        } else {
            "UPDATE chat_messages \
             SET status = ?, payload = JSON_SET(COALESCE(payload, JSON_OBJECT()), '$.error', ?) \
             WHERE id = ? AND status = ?"
        };
        let mut query = sqlx::query(sql).bind(JobStatus::Failed.to_db()).bind(error);
        if let Some(result) = result {
            query = query.bind(result);
        }
        let done = query
            .bind(id.get())
            .bind(JobStatus::Pending.to_db())
            .execute(&self.pool)
            .await?;
        if done.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StoreError::NotPending(id))
        }
    }

    /// Puts every `pending` job back to `NULL`.
    ///
    /// Intended for start-up of a single-worker deployment: anything still
    /// `pending` was orphaned by a crash and should be retried.
    pub async fn requeue_pending(&self) -> Result<u64, StoreError> {
        let done = sqlx::query("UPDATE chat_messages SET status = NULL WHERE status = ?")
            .bind(JobStatus::Pending.to_db())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}
