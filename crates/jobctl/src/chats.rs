//! Chats and their messages, read and written the way the chat application
//! does (a plain `INSERT` of an agentic user row, no HTTP).
//!
//! Integer and timestamp columns are cast to `SIGNED` / text in SQL so the
//! code does not depend on their exact MySQL types.

use serde_json::{Value, json};
use sqlx::mysql::{MySqlConnection, MySqlRow};
use sqlx::{AssertSqlSafe, Row};

use crate::{Error, Result};

/// A row of `chats`.
#[derive(Debug, Clone)]
pub struct Chat {
    /// Primary key.
    pub id: i64,
    /// Owner.
    pub user_id: i64,
    /// Title shown in the chat list.
    pub title: String,
    /// `YYYY-MM-DD HH:MM:SS`, server time.
    pub created_at: String,
}

/// A row of `chat_messages`.
#[derive(Debug, Clone)]
pub struct Message {
    /// Primary key.
    pub id: i64,
    /// Chat it belongs to.
    pub chat_id: i64,
    /// `user` or `ai`.
    pub sender: String,
    /// Whether the runner should answer it (for `user` rows).
    pub is_agentic: bool,
    /// `NULL` (not picked up), `pending`, `done` or `failed`.
    pub status: Option<String>,
    /// Bookkeeping: `reply_id`, `error`, `result`, `in_reply_to`.
    pub payload: Option<Value>,
    /// Message text.
    pub content: String,
    /// `YYYY-MM-DD HH:MM:SS`, server time.
    pub created_at: String,
}

impl Message {
    /// A string field of `payload`, if present.
    #[must_use]
    pub fn payload_str(&self, key: &str) -> Option<&str> {
        self.payload.as_ref()?.get(key)?.as_str()
    }

    /// An integer field of `payload`, if present.
    #[must_use]
    pub fn payload_i64(&self, key: &str) -> Option<i64> {
        self.payload.as_ref()?.get(key)?.as_i64()
    }

    /// Every column as a JSON object.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "chat_id": self.chat_id,
            "sender": self.sender,
            "is_agentic": self.is_agentic,
            "status": self.status,
            "payload": self.payload,
            "content": self.content,
            "created_at": self.created_at,
        })
    }
}

const MESSAGE_COLUMNS: &str = "CAST(id AS SIGNED) AS id, CAST(chat_id AS SIGNED) AS chat_id, \
     CAST(sender AS CHAR) AS sender, CAST(COALESCE(is_agentic, 0) AS SIGNED) AS is_agentic, \
     CAST(status AS CHAR) AS status, payload, content, \
     DATE_FORMAT(created_at, '%Y-%m-%d %H:%i:%s') AS created_at";

fn message_from_row(row: &MySqlRow) -> Result<Message> {
    Ok(Message {
        id: row.try_get("id")?,
        chat_id: row.try_get("chat_id")?,
        sender: row.try_get("sender")?,
        is_agentic: row.try_get::<i64, _>("is_agentic")? != 0,
        status: row.try_get("status")?,
        payload: row.try_get("payload")?,
        content: row.try_get("content")?,
        created_at: row.try_get("created_at")?,
    })
}

/// Fetches one chat.
pub async fn chat(conn: &mut MySqlConnection, id: i64) -> Result<Option<Chat>> {
    let row = sqlx::query(
        "SELECT CAST(id AS SIGNED) AS id, CAST(user_id AS SIGNED) AS user_id, title, \
         DATE_FORMAT(created_at, '%Y-%m-%d %H:%i:%s') AS created_at FROM chats WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;
    row.map(|row| {
        Ok(Chat {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            title: row.try_get("title")?,
            created_at: row.try_get("created_at")?,
        })
    })
    .transpose()
}

/// Every message of a chat, oldest first.
pub async fn messages(conn: &mut MySqlConnection, chat_id: i64) -> Result<Vec<Message>> {
    let sql = format!("SELECT {MESSAGE_COLUMNS} FROM chat_messages WHERE chat_id = ? ORDER BY id");
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(chat_id)
        .fetch_all(conn)
        .await?;
    rows.iter().map(message_from_row).collect()
}

/// One message by id.
pub async fn message(conn: &mut MySqlConnection, id: i64) -> Result<Option<Message>> {
    let sql = format!("SELECT {MESSAGE_COLUMNS} FROM chat_messages WHERE id = ?");
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.as_ref().map(message_from_row).transpose()
}

/// Creates a chat for `user_id` (in that user's company) and returns its id.
pub async fn create_chat(conn: &mut MySqlConnection, user_id: i64, title: &str) -> Result<i64> {
    let company: Option<i64> =
        sqlx::query_scalar("SELECT CAST(company_id AS SIGNED) FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *conn)
            .await?;
    let company =
        company.ok_or_else(|| Error::NotFound(format!("user {user_id} does not exist")))?;
    let title: String = title.chars().take(200).collect();
    let done = sqlx::query("INSERT INTO chats (user_id, company_id, title) VALUES (?, ?, ?)")
        .bind(user_id)
        .bind(company)
        .bind(title)
        .execute(conn)
        .await?;
    insert_id(done.last_insert_id())
}

/// Inserts an agentic user message (`status` NULL) and returns its id.
pub async fn insert_agentic(
    conn: &mut MySqlConnection,
    chat_id: i64,
    content: &str,
) -> Result<i64> {
    let done = sqlx::query(
        "INSERT INTO chat_messages (chat_id, sender, content, is_agentic) VALUES (?, 'user', ?, 1)",
    )
    .bind(chat_id)
    .bind(content)
    .execute(conn)
    .await?;
    insert_id(done.last_insert_id())
}

fn insert_id(raw: u64) -> Result<i64> {
    i64::try_from(raw).map_err(|_| Error::Config(format!("insert id {raw} is out of range")))
}
