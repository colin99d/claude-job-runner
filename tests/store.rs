//! Store behaviour against a real MySQL database (one per test).

mod common;

use std::collections::HashSet;

use claude_job_runner::job::{JobOutcome, JobStatus, MessageId, Requester};
use claude_job_runner::store::StoreError;
use sqlx::{MySqlPool, Row};

#[sqlx::test(migrations = false)]
async fn insert_then_get_returns_a_new_job(pool: MySqlPool) {
    let (store, chat) = common::store(pool).await;

    let id = store.insert(chat, "do the thing").await.unwrap();
    let job = store.get(id).await.unwrap().expect("job exists");

    assert_eq!(job.id, id);
    assert_eq!(job.chat_id, chat);
    assert_eq!(job.content, "do the thing");
    assert_eq!(job.status, JobStatus::New);
    assert_eq!(job.reply_id, None);
    assert_eq!(job.error, None);
    // `common::store` creates the chat for user 1 of company 1.
    assert_eq!(
        job.requester,
        Some(Requester {
            user_id: 1,
            company_id: 1
        })
    );
}

#[sqlx::test(migrations = false)]
async fn get_unknown_id_is_none(pool: MySqlPool) {
    let (store, _) = common::store(pool).await;
    assert_eq!(store.get(MessageId::new(999)).await.unwrap(), None);
}

#[sqlx::test(migrations = false)]
async fn only_agentic_user_messages_are_jobs(pool: MySqlPool) {
    let (store, chat) = common::store(pool.clone()).await;
    for (sender, agentic) in [("user", None), ("user", Some(0)), ("ai", Some(1))] {
        sqlx::query(
            "INSERT INTO chat_messages (chat_id, sender, content, is_agentic) VALUES (?, ?, ?, ?)",
        )
        .bind(chat.get())
        .bind(sender)
        .bind("not a job")
        .bind(agentic)
        .execute(&pool)
        .await
        .unwrap();
    }
    let job = store.insert(chat, "a job").await.unwrap();

    assert_eq!(store.claim_next().await.unwrap().unwrap().id, job);
    assert_eq!(store.claim_next().await.unwrap(), None);

    // Non-jobs are invisible through `get` as well.
    let ids: Vec<i64> = sqlx::query("SELECT id FROM chat_messages WHERE id <> ?")
        .bind(job.get())
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get("id"))
        .collect();
    assert_eq!(ids.len(), 3);
    for id in ids {
        assert_eq!(store.get(MessageId::new(id)).await.unwrap(), None);
    }
}

#[sqlx::test(migrations = false)]
async fn claim_marks_oldest_job_pending_and_hands_it_out_once(pool: MySqlPool) {
    let (store, chat) = common::store(pool).await;
    let first = store.insert(chat, "first").await.unwrap();
    let second = store.insert(chat, "second").await.unwrap();

    let claimed = store.claim_next().await.unwrap().expect("a job");
    assert_eq!(claimed.id, first);
    assert_eq!(claimed.status, JobStatus::Pending);
    assert_eq!(
        store.get(first).await.unwrap().unwrap().status,
        JobStatus::Pending
    );

    let claimed = store.claim_next().await.unwrap().expect("another job");
    assert_eq!(claimed.id, second);

    assert_eq!(store.claim_next().await.unwrap(), None);
}

#[sqlx::test(migrations = false)]
async fn concurrent_claimers_never_share_a_job(pool: MySqlPool) {
    const JOBS: usize = 20;
    let (store, chat) = common::store(pool).await;
    for i in 0..JOBS {
        store.insert(chat, &format!("job {i}")).await.unwrap();
    }

    let mut handles = Vec::new();
    for _ in 0..4 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            while let Some(job) = store.claim_next().await.unwrap() {
                mine.push(job.id);
            }
            mine
        }));
    }

    let mut all = Vec::new();
    for handle in handles {
        all.extend(handle.await.unwrap());
    }
    let unique: HashSet<_> = all.iter().copied().collect();
    assert_eq!(all.len(), JOBS, "every job claimed");
    assert_eq!(unique.len(), JOBS, "no job claimed twice");
}

#[sqlx::test(migrations = false)]
async fn finish_records_success_as_an_ai_message(pool: MySqlPool) {
    let (store, chat) = common::store(pool.clone()).await;
    let id = store.insert(chat, "x").await.unwrap();
    store.claim_next().await.unwrap();

    store
        .finish(
            id,
            &JobOutcome::Success {
                result: "all good".to_owned(),
            },
        )
        .await
        .unwrap();

    let job = store.get(id).await.unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Done);
    assert_eq!(job.error, None);
    let reply_id = job.reply_id.expect("reply id recorded");
    assert_eq!(
        store.reply(reply_id).await.unwrap().as_deref(),
        Some("all good")
    );

    // The reply is an agentic `ai` row in the same chat, linked back to the job.
    let row = sqlx::query(
        "SELECT chat_id, sender, is_agentic, payload->>'$.in_reply_to' AS in_reply_to \
         FROM chat_messages WHERE id = ?",
    )
    .bind(reply_id.get())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("chat_id"), chat.get());
    assert_eq!(row.get::<String, _>("sender"), "ai");
    assert_eq!(row.get::<Option<i8>, _>("is_agentic"), Some(1));
    assert_eq!(row.get::<String, _>("in_reply_to"), id.to_string());
    // A reply is never itself a job.
    assert_eq!(store.get(reply_id).await.unwrap(), None);
}

#[sqlx::test(migrations = false)]
async fn finish_records_failure_with_partial_result(pool: MySqlPool) {
    let (store, chat) = common::store(pool.clone()).await;
    let id = store.insert(chat, "x").await.unwrap();
    store.claim_next().await.unwrap();

    store
        .finish(
            id,
            &JobOutcome::Failed {
                error: "timed out".to_owned(),
                result: Some("half".to_owned()),
            },
        )
        .await
        .unwrap();

    let job = store.get(id).await.unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.error.as_deref(), Some("timed out"));
    assert_eq!(job.reply_id, None);
    let partial: String =
        sqlx::query("SELECT payload->>'$.result' AS r FROM chat_messages WHERE id = ?")
            .bind(id.get())
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("r");
    assert_eq!(partial, "half");
    // No `ai` row was written for a failure.
    let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM chat_messages WHERE sender = 'ai'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("n");
    assert_eq!(count, 0);
}

#[sqlx::test(migrations = false)]
async fn finish_keeps_existing_payload_keys(pool: MySqlPool) {
    let (store, chat) = common::store(pool.clone()).await;
    let id = store.insert(chat, "x").await.unwrap();
    sqlx::query(
        "UPDATE chat_messages SET payload = JSON_OBJECT('attachment', 'a.png') WHERE id = ?",
    )
    .bind(id.get())
    .execute(&pool)
    .await
    .unwrap();
    store.claim_next().await.unwrap();

    store.finish(id, &JobOutcome::failed("nope")).await.unwrap();

    let payload: serde_json::Value = sqlx::query("SELECT payload FROM chat_messages WHERE id = ?")
        .bind(id.get())
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("payload");
    assert_eq!(payload["attachment"], "a.png");
    assert_eq!(payload["error"], "nope");
}

#[sqlx::test(migrations = false)]
async fn finish_refuses_jobs_that_are_not_pending(pool: MySqlPool) {
    let (store, chat) = common::store(pool.clone()).await;
    let id = store.insert(chat, "x").await.unwrap();
    let failure = JobOutcome::failed("nope");
    let success = JobOutcome::Success {
        result: "late".to_owned(),
    };

    // Never claimed.
    assert!(matches!(
        store.finish(id, &failure).await,
        Err(StoreError::NotPending(got)) if got == id
    ));
    assert!(matches!(
        store.finish(id, &success).await,
        Err(StoreError::NotPending(got)) if got == id
    ));

    // Already finished.
    store.claim_next().await.unwrap();
    store.finish(id, &failure).await.unwrap();
    assert!(matches!(
        store.finish(id, &failure).await,
        Err(StoreError::NotPending(_))
    ));
    assert!(matches!(
        store.finish(id, &success).await,
        Err(StoreError::NotPending(_))
    ));
    assert_eq!(
        store.get(id).await.unwrap().unwrap().status,
        JobStatus::Failed
    );
    // The rejected success did not leave a stray reply behind.
    let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM chat_messages WHERE sender = 'ai'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("n");
    assert_eq!(count, 0);
}

#[sqlx::test(migrations = false)]
async fn requeue_pending_resets_only_pending_rows(pool: MySqlPool) {
    let (store, chat) = common::store(pool).await;
    let orphaned = store.insert(chat, "orphaned").await.unwrap();
    let finished = store.insert(chat, "finished").await.unwrap();
    let untouched = store.insert(chat, "untouched").await.unwrap();
    store.claim_next().await.unwrap();
    store.claim_next().await.unwrap();
    store
        .finish(
            finished,
            &JobOutcome::Success {
                result: "ok".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(store.requeue_pending().await.unwrap(), 1);

    assert_eq!(
        store.get(orphaned).await.unwrap().unwrap().status,
        JobStatus::New
    );
    assert_eq!(
        store.get(finished).await.unwrap().unwrap().status,
        JobStatus::Done
    );
    assert_eq!(
        store.get(untouched).await.unwrap().unwrap().status,
        JobStatus::New
    );
    // The orphan is claimable again, and comes first because it is oldest.
    assert_eq!(store.claim_next().await.unwrap().unwrap().id, orphaned);
}
