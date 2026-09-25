//! Worker pipeline against a throw-away MySQL database and a fake `claude` binary.

mod common;

use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

use claude_job_runner::claude::{
    ClaudeConfig, ClaudeRunner, CliRunner, RunError, RunReport, RunRequest,
};
use claude_job_runner::job::{ChatId, JobOutcome, JobStatus, MessageId};
use claude_job_runner::store::JobStore;
use claude_job_runner::worker::Worker;
use claude_job_runner::workspace::WorkspaceRoot;
use sqlx::MySqlPool;
use tokio_util::sync::CancellationToken;

const ONE: NonZeroUsize = NonZeroUsize::MIN;

async fn setup(pool: MySqlPool, dir: &Path) -> (JobStore, ChatId, WorkspaceRoot, CliRunner) {
    let (store, chat) = common::store(pool).await;
    let root = WorkspaceRoot::prepare(dir.join("workspaces"))
        .await
        .unwrap();
    let runner = CliRunner::new(ClaudeConfig {
        binary: common::fake_claude(dir),
        timeout: Duration::from_secs(5),
        ..ClaudeConfig::default()
    });
    (store, chat, root, runner)
}

fn workspace_count(root: &WorkspaceRoot) -> usize {
    std::fs::read_dir(root.path()).unwrap().count()
}

#[sqlx::test(migrations = false)]
async fn job_runs_end_to_end_and_workspace_is_removed(pool: MySqlPool) {
    let tmp = tempfile::tempdir().unwrap();
    let (store, chat, root, runner) = setup(pool, tmp.path()).await;
    let worker = Worker::new(
        store.clone(),
        runner,
        root.clone(),
        Duration::from_millis(10),
        ONE,
    );
    let id = store.insert(chat, "ok:it worked").await.unwrap();

    let job = store.claim_next().await.unwrap().unwrap();
    let outcome = worker.process(job, &CancellationToken::new()).await;

    assert_eq!(
        outcome,
        JobOutcome::Success {
            result: "it worked".to_owned()
        }
    );
    let stored = store.get(id).await.unwrap().unwrap();
    assert_eq!(stored.status, JobStatus::Done);
    let reply = store.reply(stored.reply_id.unwrap()).await.unwrap();
    assert_eq!(reply.as_deref(), Some("it worked"));
    assert_eq!(workspace_count(&root), 0, "workspace should be deleted");
}

#[sqlx::test(migrations = false)]
async fn crashing_claude_marks_job_failed_and_still_cleans_up(pool: MySqlPool) {
    let tmp = tempfile::tempdir().unwrap();
    let (store, chat, root, runner) = setup(pool, tmp.path()).await;
    let worker = Worker::new(
        store.clone(),
        runner,
        root.clone(),
        Duration::from_millis(10),
        ONE,
    );
    let id = store.insert(chat, "crash").await.unwrap();

    let job = store.claim_next().await.unwrap().unwrap();
    worker.process(job, &CancellationToken::new()).await;

    let stored = store.get(id).await.unwrap().unwrap();
    assert_eq!(stored.status, JobStatus::Failed);
    assert!(
        stored.error.as_deref().unwrap().contains("boom"),
        "{:?}",
        stored.error
    );
    assert_eq!(workspace_count(&root), 0);
}

#[sqlx::test(migrations = false)]
async fn polling_loop_drains_queue_in_order_and_stops_on_shutdown(pool: MySqlPool) {
    let tmp = tempfile::tempdir().unwrap();
    let (store, chat, root, runner) = setup(pool, tmp.path()).await;
    let worker = Worker::new(
        store.clone(),
        runner,
        root.clone(),
        Duration::from_millis(10),
        ONE,
    );
    let ids = [
        store.insert(chat, "ok:one").await.unwrap(),
        store.insert(chat, "ok:two").await.unwrap(),
        store.insert(chat, "error").await.unwrap(),
    ];

    let shutdown = CancellationToken::new();
    let task = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { worker.run(shutdown).await }
    });

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let all_done = futures_done(&store, &ids).await;
            if all_done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("jobs finished in time");

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("worker stopped after cancel")
        .unwrap();

    let one = store.get(ids[0]).await.unwrap().unwrap();
    let two = store.get(ids[1]).await.unwrap().unwrap();
    let three = store.get(ids[2]).await.unwrap().unwrap();
    assert_eq!(one.status, JobStatus::Done);
    assert_eq!(reply_text(&store, &one).await.as_deref(), Some("one"));
    assert_eq!(two.status, JobStatus::Done);
    assert_eq!(reply_text(&store, &two).await.as_deref(), Some("two"));
    assert_eq!(three.status, JobStatus::Failed);
    assert_eq!(three.reply_id, None);
    assert!(three.error.as_deref().unwrap().contains("error_max_turns"));
    assert_eq!(workspace_count(&root), 0);
}

async fn reply_text(store: &JobStore, job: &claude_job_runner::job::Job) -> Option<String> {
    store.reply(job.reply_id?).await.unwrap()
}

async fn futures_done(store: &JobStore, ids: &[MessageId]) -> bool {
    for id in ids {
        let job = store.get(*id).await.unwrap().unwrap();
        if !job.status.is_terminal() {
            return false;
        }
    }
    true
}

#[sqlx::test(migrations = false)]
async fn shutdown_during_a_job_marks_it_failed(pool: MySqlPool) {
    let tmp = tempfile::tempdir().unwrap();
    let (store, chat, root, runner) = setup(pool, tmp.path()).await;
    let worker = Worker::new(
        store.clone(),
        runner,
        root.clone(),
        Duration::from_millis(10),
        ONE,
    );
    let id = store.insert(chat, "sleep:30").await.unwrap();

    let shutdown = CancellationToken::new();
    let task = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { worker.run(shutdown).await }
    });

    // Give the worker a moment to claim and start the job, then pull the plug.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        store.get(id).await.unwrap().unwrap().status,
        JobStatus::Pending
    );
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("worker stopped promptly despite a running job")
        .unwrap();

    let job = store.get(id).await.unwrap().unwrap();
    assert_eq!(job.status, JobStatus::Failed);
    assert!(
        job.error.as_deref().unwrap().contains("shut down"),
        "{:?}",
        job.error
    );
    assert_eq!(workspace_count(&root), 0);
}

/// Polls until the rows are in the `(new, pending, terminal)` state
/// `expected`, then checks that they stay there for a moment. The database
/// is remote, so each claim takes a few round-trips and fixed sleeps would
/// be flaky.
async fn expect_counts(store: &JobStore, ids: &[MessageId], expected: (usize, usize, usize)) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while status_counts(store, ids).await != expected {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("rows never reached {expected:?}"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(status_counts(store, ids).await, expected);
}

/// Counts rows in each state as `(new, pending, terminal)`.
async fn status_counts(store: &JobStore, ids: &[MessageId]) -> (usize, usize, usize) {
    let mut counts = (0, 0, 0);
    for id in ids {
        match store.get(*id).await.unwrap().unwrap().status {
            JobStatus::New => counts.0 += 1,
            JobStatus::Pending => counts.1 += 1,
            JobStatus::Done | JobStatus::Failed => counts.2 += 1,
        }
    }
    counts
}

#[sqlx::test(migrations = false)]
async fn runs_jobs_concurrently_up_to_the_limit(pool: MySqlPool) {
    let tmp = tempfile::tempdir().unwrap();
    let (store, chat, root, runner) = setup(pool, tmp.path()).await;
    let worker = Worker::new(
        store.clone(),
        runner,
        root.clone(),
        Duration::from_millis(10),
        NonZeroUsize::new(3).unwrap(),
    );
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(store.insert(chat, "sleep:30").await.unwrap());
    }

    let shutdown = CancellationToken::new();
    let task = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { worker.run(shutdown).await }
    });

    // Three slots, five jobs: exactly three are claimed, two stay untouched
    // until a slot frees up.
    expect_counts(&store, &ids, (2, 3, 0)).await;
    assert_eq!(workspace_count(&root), 3);

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("worker stopped promptly despite running jobs")
        .unwrap();

    // The three running jobs were failed by the shutdown; the two that never
    // got a slot are still unclaimed for the next start.
    assert_eq!(status_counts(&store, &ids).await, (2, 0, 3));
    assert_eq!(workspace_count(&root), 0);
}

#[sqlx::test(migrations = false)]
async fn a_freed_slot_claims_the_next_job(pool: MySqlPool) {
    let tmp = tempfile::tempdir().unwrap();
    let (store, chat, root, runner) = setup(pool, tmp.path()).await;
    let worker = Worker::new(
        store.clone(),
        runner,
        root.clone(),
        Duration::from_millis(10),
        NonZeroUsize::new(2).unwrap(),
    );
    let ids = [
        store.insert(chat, "sleep:1").await.unwrap(),
        store.insert(chat, "sleep:30").await.unwrap(),
        store.insert(chat, "ok:third").await.unwrap(),
    ];

    let shutdown = CancellationToken::new();
    let task = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { worker.run(shutdown).await }
    });

    expect_counts(&store, &ids, (1, 2, 0)).await;

    // Once the one-second job finishes, its slot goes to the third job.
    tokio::time::timeout(Duration::from_secs(5), async {
        while store.get(ids[2]).await.unwrap().unwrap().status != JobStatus::Done {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("third job ran after a slot freed up");
    assert_eq!(
        store.get(ids[0]).await.unwrap().unwrap().status,
        JobStatus::Done
    );
    assert_eq!(
        store.get(ids[1]).await.unwrap().unwrap().status,
        JobStatus::Pending
    );

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("worker stopped")
        .unwrap();
}

/// A runner that never touches a process, to show the worker is generic.
struct CannedRunner(Result<RunReport, ()>);

impl ClaudeRunner for CannedRunner {
    async fn run(&self, request: RunRequest<'_>) -> Result<RunReport, RunError> {
        assert!(request.workspace.is_dir(), "workspace exists while running");
        assert!(request.requester.is_some(), "the chat's owner is passed on");
        tokio::task::yield_now().await;
        self.0
            .clone()
            .map_err(|()| RunError::Timeout(Duration::from_secs(1)))
    }
}

#[sqlx::test(migrations = false)]
async fn worker_accepts_any_runner_implementation(pool: MySqlPool) {
    let tmp = tempfile::tempdir().unwrap();
    let (store, chat) = common::store(pool).await;
    let root = WorkspaceRoot::prepare(tmp.path().join("ws")).await.unwrap();
    let runner = CannedRunner(Ok(RunReport {
        result: Some("canned".to_owned()),
        is_error: false,
        subtype: "success".to_owned(),
        session_id: None,
        cost_usd: None,
        num_turns: None,
        permission_denials: 0,
    }));
    let worker = Worker::new(store.clone(), runner, root, Duration::from_millis(10), ONE);
    let id = store.insert(chat, "anything").await.unwrap();

    let job = store.claim_next().await.unwrap().unwrap();
    let outcome = worker.process(job, &CancellationToken::new()).await;

    assert_eq!(
        outcome,
        JobOutcome::Success {
            result: "canned".to_owned()
        }
    );
    assert_eq!(
        store.get(id).await.unwrap().unwrap().status,
        JobStatus::Done
    );
}
