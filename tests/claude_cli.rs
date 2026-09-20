//! `CliRunner` against a scripted stand-in for the `claude` binary.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use claude_job_runner::claude::{ClaudeConfig, ClaudeRunner, CliRunner, RunError, RunRequest};
use claude_job_runner::job::JobOutcome;

fn runner(binary: PathBuf) -> CliRunner {
    CliRunner::new(ClaudeConfig {
        binary,
        model: Some("opus".to_owned()),
        max_turns: 3,
        timeout: Duration::from_secs(5),
        ..ClaudeConfig::default()
    })
}

#[tokio::test]
async fn successful_run_is_parsed_and_invoked_correctly() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = runner(common::fake_claude(tmp.path()));
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();

    let report = runner
        .run(RunRequest {
            prompt: "ok:hello \"world\"",
            workspace: &ws,
        })
        .await
        .unwrap();

    assert_eq!(report.result.as_deref(), Some("hello \"world\""));
    assert!(!report.is_error);
    assert_eq!(report.subtype, "success");
    assert_eq!(report.session_id.as_deref(), Some("fake-session"));
    assert_eq!(report.cost_usd, Some(0.01));
    assert_eq!(report.num_turns, Some(2));
    assert_eq!(report.permission_denials, 1);

    // The fake binary saw the right cwd, the prompt on stdin, and our flags.
    let cwd = std::fs::read_to_string(ws.join("cwd.txt")).unwrap();
    assert_eq!(
        PathBuf::from(cwd.trim()).canonicalize().unwrap(),
        ws.canonicalize().unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("prompt.txt")).unwrap(),
        "ok:hello \"world\""
    );
    let args = std::fs::read_to_string(ws.join("args.txt")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert!(args.contains(&"--print"));
    assert!(args.windows(2).any(|w| w == ["--output-format", "json"]));
    assert!(args.windows(2).any(|w| w == ["--model", "opus"]));
    assert!(args.windows(2).any(|w| w == ["--max-turns", "3"]));
    assert!(
        args.windows(2)
            .any(|w| w == ["--permission-mode", "acceptEdits"])
    );
    let settings_index = args.iter().position(|a| *a == "--settings").unwrap();
    let settings: serde_json::Value = serde_json::from_str(args[settings_index + 1]).unwrap();
    assert_eq!(settings["sandbox"]["enabled"], true);
}

#[tokio::test]
async fn error_report_becomes_a_failed_outcome_with_partial_result() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = runner(common::fake_claude(tmp.path()));

    let report = runner
        .run(RunRequest {
            prompt: "error",
            workspace: tmp.path(),
        })
        .await
        .unwrap();

    assert!(report.is_error);
    assert_eq!(
        report.into_outcome(),
        JobOutcome::Failed {
            error: "claude reported an error: error_max_turns".to_owned(),
            result: Some("partial answer".to_owned()),
        }
    );
}

#[tokio::test]
async fn non_zero_exit_surfaces_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = runner(common::fake_claude(tmp.path()));

    let err = runner
        .run(RunRequest {
            prompt: "crash",
            workspace: tmp.path(),
        })
        .await
        .unwrap_err();

    match err {
        RunError::Exited { status, stderr } => {
            assert_eq!(status.code(), Some(2));
            assert_eq!(stderr, "boom");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn unparseable_output_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = runner(common::fake_claude(tmp.path()));

    let err = runner
        .run(RunRequest {
            prompt: "garbage",
            workspace: tmp.path(),
        })
        .await
        .unwrap_err();

    match err {
        RunError::InvalidOutput { stdout, .. } => assert_eq!(stdout.trim(), "this is not json"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn timeout_kills_the_process() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = CliRunner::new(ClaudeConfig {
        binary: common::fake_claude(tmp.path()),
        timeout: Duration::from_millis(300),
        ..ClaudeConfig::default()
    });

    let started = Instant::now();
    let err = runner
        .run(RunRequest {
            prompt: "sleep:30",
            workspace: tmp.path(),
        })
        .await
        .unwrap_err();

    assert!(matches!(err, RunError::Timeout(t) if t == Duration::from_millis(300)));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "process was not killed promptly"
    );
}

#[tokio::test]
async fn missing_binary_is_a_spawn_error() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = runner(tmp.path().join("does-not-exist"));

    let err = runner
        .run(RunRequest {
            prompt: "ok:x",
            workspace: tmp.path(),
        })
        .await
        .unwrap_err();

    assert!(matches!(err, RunError::Spawn { .. }));
}
