//! Running one job inside a headless, sandboxed Claude Code session.
//!
//! [`CliRunner`] shells out to the `claude` binary with `-p` (print mode)
//! and `--output-format json`, feeding the prompt over stdin. The process
//! runs with the job's workspace as `cwd` and a CLI-level `--settings`
//! payload that turns on Claude Code's OS sandbox, so shell commands can
//! read the whole machine but only write inside the workspace. The built-in
//! file tools are covered by permission rules: `acceptEdits` auto-approves
//! `Edit`/`Write` under `cwd` and (because print mode cannot prompt) rejects
//! everything else, while an explicit `Read(//**)` allow rule lets the
//! `Read` tool see the whole filesystem.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::job::JobOutcome;

/// MCP server that gives jobs SQL access to `agent_database_url`. Bash
/// cannot reach the database from inside the sandbox (its network only goes
/// out through an HTTP/SOCKS proxy), but MCP servers run outside it.
const DB_MCP_SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/db_mcp.py");

/// Appended to the system prompt when a database is available, so jobs know
/// to look there instead of assuming they only have the workspace.
const DB_SYSTEM_PROMPT: &str = "You have read-only access to the company's MySQL database \
(CRM data: deals, customers, sales reps, etc.) through the `mcp__db__query` tool. When a \
question is about business data, explore the schema (SHOW TABLES, DESCRIBE <table>) and \
answer from the database rather than asking the user where the data lives.";

/// Claude Code permission mode passed via `--permission-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionMode {
    /// Auto-approve edits inside the workspace; everything else that would
    /// prompt is denied. Recommended.
    #[default]
    AcceptEdits,
    /// Skip all permission checks. The Bash sandbox still applies, but the
    /// `Edit`/`Write` tools may then touch files outside the workspace.
    BypassPermissions,
}

/// Error returned for an unrecognised permission mode string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission mode {0:?}: expected acceptEdits or bypassPermissions")]
pub struct UnknownPermissionMode(pub String);

impl PermissionMode {
    /// The value Claude Code expects on the command line.
    #[must_use]
    pub const fn as_cli_value(self) -> &'static str {
        match self {
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

impl FromStr for PermissionMode {
    type Err = UnknownPermissionMode;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "acceptEdits" => Ok(Self::AcceptEdits),
            "bypassPermissions" => Ok(Self::BypassPermissions),
            other => Err(UnknownPermissionMode(other.to_owned())),
        }
    }
}

/// Claude Code effort level passed via `--effort`. Controls how much the
/// model thinks before acting; lower levels are cheaper and faster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Effort {
    /// Minimal thinking. The default for jobs: most of them are routine.
    #[default]
    Low,
    /// Moderate thinking.
    Medium,
    /// The CLI's own default.
    High,
    /// Deep thinking for hard coding and agentic work.
    XHigh,
    /// Maximum thinking; correctness over cost.
    Max,
}

/// Error returned for an unrecognised effort level string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown effort level {0:?}: expected low, medium, high, xhigh or max")]
pub struct UnknownEffort(pub String);

impl Effort {
    /// The value Claude Code expects on the command line.
    #[must_use]
    pub const fn as_cli_value(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl FromStr for Effort {
    type Err = UnknownEffort;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            other => Err(UnknownEffort(other.to_owned())),
        }
    }
}

/// Everything needed to launch the `claude` binary.
#[derive(Debug, Clone)]
pub struct ClaudeConfig {
    /// Path or name of the executable.
    pub binary: PathBuf,
    /// `--model`, if set.
    pub model: Option<String>,
    /// `--max-turns`: upper bound on agentic iterations.
    pub max_turns: u32,
    /// `--max-budget-usd`, if set.
    pub max_budget_usd: Option<f64>,
    /// Wall-clock limit; the process is killed when exceeded.
    pub timeout: Duration,
    /// `--permission-mode`.
    pub permission_mode: PermissionMode,
    /// `--effort`: how much the model thinks before acting.
    pub effort: Effort,
    /// Domains sandboxed shell commands may reach. Empty means none.
    pub allowed_domains: Vec<String>,
    /// Keep session transcripts under the Claude config dir after the job.
    pub persist_sessions: bool,
    /// `CLAUDE_CONFIG_DIR` for the child, if the runner should use its own
    /// settings instead of the invoking user's.
    pub config_dir: Option<PathBuf>,
    /// MCP configuration file for jobs. `None` means no MCP servers at all:
    /// every server is a separate Node process, so servers configured in the
    /// user's settings are never inherited.
    pub mcp_config: Option<PathBuf>,
    /// `DATABASE_URL` as seen by the job, normally a read-only login. The
    /// runner's own (writable) `DATABASE_URL` is never passed on; with
    /// `None` the job sees no `DATABASE_URL` at all.
    pub agent_database_url: Option<String>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("claude"),
            model: None,
            max_turns: 50,
            max_budget_usd: None,
            timeout: Duration::from_mins(30),
            permission_mode: PermissionMode::default(),
            effort: Effort::default(),
            allowed_domains: Vec::new(),
            persist_sessions: false,
            config_dir: None,
            mcp_config: None,
            agent_database_url: None,
        }
    }
}

impl ClaudeConfig {
    /// The `--settings` payload that confines the session to `workspace`.
    #[must_use]
    pub fn settings_json(&self) -> String {
        // The Read tool prompts for files outside cwd, and print mode turns
        // every prompt into a denial; this opens reads machine-wide
        // (user-level deny rules still win).
        let mut allow = vec!["Read(//**)"];
        if self.agent_database_url.is_some() {
            allow.push("mcp__db");
        }
        json!({
            "sandbox": {
                "enabled": true,
                "autoAllowBashIfSandboxed": true,
                "allowUnsandboxedCommands": false,
                "failIfUnavailable": true,
                "network": { "allowedDomains": self.allowed_domains },
            },
            "permissions": {
                "defaultMode": self.permission_mode.as_cli_value(),
                "allow": allow,
            },
        })
        .to_string()
    }

    /// Builds the command line for a run in `workspace` (prompt goes to stdin).
    #[must_use]
    pub fn command(&self, workspace: &Path) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.binary);
        cmd.current_dir(workspace)
            .arg("--print")
            .args(["--output-format", "json"])
            .args(["--permission-mode", self.permission_mode.as_cli_value()])
            .args(["--effort", self.effort.as_cli_value()])
            .args(["--max-turns", &self.max_turns.to_string()])
            .args(["--settings", &self.settings_json()]);
        if let Some(model) = &self.model {
            cmd.args(["--model", model]);
        }
        if let Some(budget) = self.max_budget_usd {
            cmd.args(["--max-budget-usd", &budget.to_string()]);
        }
        if !self.persist_sessions {
            cmd.arg("--no-session-persistence");
        }
        // Only the MCP servers we were told about, none by default.
        if let Some(path) = &self.mcp_config {
            cmd.arg("--mcp-config").arg(path);
        }
        if self.agent_database_url.is_some() {
            let db = json!({
                "mcpServers": { "db": { "command": "python3", "args": [DB_MCP_SCRIPT] } }
            });
            cmd.args(["--mcp-config", &db.to_string()]);
            cmd.args(["--append-system-prompt", DB_SYSTEM_PROMPT]);
        } else if self.mcp_config.is_none() {
            cmd.args(["--mcp-config", r#"{"mcpServers":{}}"#]);
        }
        cmd.arg("--strict-mcp-config");
        // A nested session refuses to start while this variable is set, so
        // strip it in case the runner itself was launched from Claude Code.
        cmd.env_remove("CLAUDECODE");
        // Headless jobs have no use for update checks, telemetry, or error
        // reporting; skipping them saves start-up time and a little memory.
        cmd.env("DISABLE_AUTOUPDATER", "1");
        cmd.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
        if let Some(dir) = &self.config_dir {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
        // The job must not inherit the runner's writable database login.
        cmd.env_remove("DATABASE_URL");
        if let Some(url) = &self.agent_database_url {
            cmd.env("DATABASE_URL", url);
        }
        cmd
    }
}

/// Inputs for a single run.
#[derive(Debug, Clone, Copy)]
pub struct RunRequest<'a> {
    /// The prompt, passed verbatim.
    pub prompt: &'a str,
    /// Directory to run in; the only writable location.
    pub workspace: &'a Path,
}

/// What Claude Code reported for a finished run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    /// Final assistant message, when there was one.
    pub result: Option<String>,
    /// Claude's own error flag.
    pub is_error: bool,
    /// `success`, `error_max_turns`, `error_during_execution`, ...
    pub subtype: String,
    /// Session id, useful for `claude --resume` when transcripts are kept.
    pub session_id: Option<String>,
    /// Total API spend for the run.
    pub cost_usd: Option<f64>,
    /// Number of agentic turns used.
    pub num_turns: Option<u32>,
    /// How many tool calls were refused by the permission system.
    pub permission_denials: usize,
}

impl RunReport {
    /// Maps the report onto the job's final state.
    #[must_use]
    pub fn into_outcome(self) -> JobOutcome {
        match (self.is_error, self.result) {
            (false, Some(result)) => JobOutcome::Success { result },
            (false, None) => JobOutcome::failed("claude finished without a result"),
            (true, result) => JobOutcome::Failed {
                error: format!("claude reported an error: {}", self.subtype),
                result,
            },
        }
    }
}

/// Shape of the `--output-format json` document. Unknown fields are ignored
/// and missing ones default so minor CLI changes do not break parsing.
#[derive(Debug, Deserialize)]
struct RawReport {
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    subtype: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    num_turns: Option<u32>,
    #[serde(default)]
    permission_denials: Vec<serde_json::Value>,
}

impl From<RawReport> for RunReport {
    fn from(raw: RawReport) -> Self {
        Self {
            result: raw.result,
            is_error: raw.is_error,
            subtype: raw.subtype,
            session_id: raw.session_id,
            cost_usd: raw.total_cost_usd,
            num_turns: raw.num_turns,
            permission_denials: raw.permission_denials.len(),
        }
    }
}

/// Ways a run can fail before producing a report.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The binary could not be started.
    #[error("could not start {}", binary.display())]
    Spawn {
        /// What we tried to execute.
        binary: PathBuf,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// Waiting for the process failed.
    #[error("i/o error while running claude")]
    Io(#[from] std::io::Error),
    /// The wall-clock limit was hit; the process was killed.
    #[error("claude did not finish within {0:?}")]
    Timeout(Duration),
    /// Non-zero exit without a parseable report.
    #[error("claude exited with {status}: {stderr}")]
    Exited {
        /// Exit status of the process.
        status: ExitStatus,
        /// Captured standard error, trimmed.
        stderr: String,
    },
    /// Exit was clean but stdout was not the expected JSON.
    #[error("could not parse claude output")]
    InvalidOutput {
        /// The parse error.
        #[source]
        source: serde_json::Error,
        /// What we got instead.
        stdout: String,
    },
}

/// Something that can execute a prompt and report back.
///
/// Abstracted so the worker can be tested without a real `claude` binary.
pub trait ClaudeRunner: Send + Sync {
    /// Runs the prompt to completion.
    fn run(
        &self,
        request: RunRequest<'_>,
    ) -> impl Future<Output = Result<RunReport, RunError>> + Send;
}

/// The production runner: spawns the real CLI.
#[derive(Debug, Clone)]
pub struct CliRunner {
    config: ClaudeConfig,
}

impl CliRunner {
    /// Creates a runner from its configuration.
    #[must_use]
    pub const fn new(config: ClaudeConfig) -> Self {
        Self { config }
    }

    /// The configuration in use.
    #[must_use]
    pub const fn config(&self) -> &ClaudeConfig {
        &self.config
    }

    fn parse_report(stdout: &[u8]) -> Result<RunReport, serde_json::Error> {
        serde_json::from_slice::<RawReport>(stdout).map(RunReport::from)
    }
}

impl ClaudeRunner for CliRunner {
    async fn run(&self, request: RunRequest<'_>) -> Result<RunReport, RunError> {
        let mut command = Command::from(self.config.command(request.workspace));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        debug!(binary = %self.config.binary.display(), workspace = %request.workspace.display(), "spawning claude");

        let mut child = command.spawn().map_err(|source| RunError::Spawn {
            binary: self.config.binary.clone(),
            source,
        })?;

        // Feed the prompt concurrently with draining stdout so a large
        // prompt can never deadlock against a chatty child.
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| RunError::Io(std::io::Error::other("child stdin was not captured")))?;
        let prompt = request.prompt.to_owned();
        let feeder = tokio::spawn(async move {
            if let Err(err) = stdin.write_all(prompt.as_bytes()).await {
                warn!(error = %err, "failed to write prompt to claude stdin");
            }
            drop(stdin);
        });

        let waited = timeout(self.config.timeout, child.wait_with_output()).await;
        feeder.abort();
        let output = match waited {
            Ok(output) => output?,
            // Dropping the timed-out future drops the child, which kills it.
            Err(_elapsed) => return Err(RunError::Timeout(self.config.timeout)),
        };

        match Self::parse_report(&output.stdout) {
            Ok(report) => Ok(report),
            Err(source) if output.status.success() => Err(RunError::InvalidOutput {
                source,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            }),
            Err(_) => Err(RunError::Exited {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(cmd: &std::process::Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn command_line_carries_every_setting() {
        let config = ClaudeConfig {
            binary: PathBuf::from("/opt/bin/claude"),
            model: Some("opus".to_owned()),
            max_turns: 7,
            max_budget_usd: Some(1.5),
            permission_mode: PermissionMode::BypassPermissions,
            effort: Effort::XHigh,
            persist_sessions: false,
            config_dir: Some(PathBuf::from("/cfg")),
            mcp_config: Some(PathBuf::from("/cfg/mcp.json")),
            agent_database_url: Some("mysql://ro:pw@db/main".to_owned()),
            ..ClaudeConfig::default()
        };
        let ws = Path::new("/tmp/ws");
        let cmd = config.command(ws);
        let args = args_of(&cmd);

        assert_eq!(cmd.get_program(), "/opt/bin/claude");
        assert_eq!(cmd.get_current_dir(), Some(ws));
        assert!(args.contains(&"--print".to_owned()));
        assert!(args.windows(2).any(|w| w == ["--output-format", "json"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--permission-mode", "bypassPermissions"])
        );
        assert!(args.windows(2).any(|w| w == ["--max-turns", "7"]));
        assert!(args.windows(2).any(|w| w == ["--effort", "xhigh"]));
        assert!(args.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(args.windows(2).any(|w| w == ["--max-budget-usd", "1.5"]));
        assert!(args.contains(&"--no-session-persistence".to_owned()));
        assert!(
            args.windows(2)
                .any(|w| w == ["--mcp-config", "/cfg/mcp.json"])
        );
        assert!(args.contains(&"--strict-mcp-config".to_owned()));
        assert!(args.iter().any(|a| a.contains("db_mcp.py")));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--append-system-prompt" && w[1].contains("mcp__db__query"))
        );

        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.iter().any(|(k, v)| *k == "CLAUDECODE" && v.is_none()));
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "DISABLE_AUTOUPDATER" && v.is_some_and(|v| v == "1"))
        );
        assert!(envs.iter().any(|(k, v)| {
            *k == "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC" && v.is_some_and(|v| v == "1")
        }));
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "CLAUDE_CONFIG_DIR" && v.is_some())
        );
        assert!(envs.iter().any(|(k, v)| {
            *k == "DATABASE_URL" && v.is_some_and(|v| v == "mysql://ro:pw@db/main")
        }));
    }

    #[test]
    fn optional_flags_are_omitted_by_default() {
        let args = args_of(&ClaudeConfig::default().command(Path::new("/tmp")));
        assert!(!args.contains(&"--model".to_owned()));
        assert!(!args.contains(&"--max-budget-usd".to_owned()));
        assert!(
            args.windows(2)
                .any(|w| w == ["--permission-mode", "acceptEdits"])
        );
        assert!(args.windows(2).any(|w| w == ["--effort", "low"]));
        // No MCP servers unless a file is given, and never inherited ones.
        assert!(
            args.windows(2)
                .any(|w| w == ["--mcp-config", r#"{"mcpServers":{}}"#])
        );
        assert!(args.contains(&"--strict-mcp-config".to_owned()));
        assert!(!args.contains(&"--append-system-prompt".to_owned()));
    }

    #[test]
    fn database_access_is_allowed_only_with_an_agent_url() {
        let config = ClaudeConfig {
            agent_database_url: Some("mysql://ro:pw@db/main".to_owned()),
            ..ClaudeConfig::default()
        };
        let settings: serde_json::Value = serde_json::from_str(&config.settings_json()).unwrap();
        assert_eq!(
            settings["permissions"]["allow"],
            json!(["Read(//**)", "mcp__db"])
        );
        let args = args_of(&config.command(Path::new("/tmp")));
        assert!(!args.contains(&r#"{"mcpServers":{}}"#.to_owned()));
    }

    #[test]
    fn runner_database_url_is_never_inherited() {
        let cmd = ClaudeConfig::default().command(Path::new("/tmp"));
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.iter().any(|(k, v)| *k == "DATABASE_URL" && v.is_none()));
    }

    #[test]
    fn persisted_sessions_drop_the_no_persistence_flag() {
        let config = ClaudeConfig {
            persist_sessions: true,
            ..ClaudeConfig::default()
        };
        let args = args_of(&config.command(Path::new("/tmp")));
        assert!(!args.contains(&"--no-session-persistence".to_owned()));
    }

    #[test]
    fn settings_enable_a_strict_sandbox() {
        let config = ClaudeConfig {
            allowed_domains: vec!["github.com".to_owned()],
            ..ClaudeConfig::default()
        };
        let settings: serde_json::Value = serde_json::from_str(&config.settings_json()).unwrap();
        assert_eq!(settings["sandbox"]["enabled"], true);
        assert_eq!(settings["sandbox"]["autoAllowBashIfSandboxed"], true);
        assert_eq!(settings["sandbox"]["allowUnsandboxedCommands"], false);
        assert_eq!(settings["sandbox"]["failIfUnavailable"], true);
        assert_eq!(
            settings["sandbox"]["network"]["allowedDomains"],
            json!(["github.com"])
        );
        assert_eq!(settings["permissions"]["defaultMode"], "acceptEdits");
        assert_eq!(settings["permissions"]["allow"], json!(["Read(//**)"]));
    }

    #[test]
    fn permission_mode_parses_cli_values() {
        assert_eq!("acceptEdits".parse(), Ok(PermissionMode::AcceptEdits));
        assert_eq!(
            "bypassPermissions".parse(),
            Ok(PermissionMode::BypassPermissions)
        );
        assert_eq!(
            "yolo".parse::<PermissionMode>(),
            Err(UnknownPermissionMode("yolo".to_owned()))
        );
    }

    #[test]
    fn report_parses_real_cli_output_and_ignores_extras() {
        let stdout = br#"{"type":"result","subtype":"success","is_error":false,"duration_ms":4946,
            "num_turns":3,"result":"done","session_id":"abc","total_cost_usd":0.027,
            "permission_denials":[{"tool_name":"Bash"}],"usage":{"input_tokens":18}}"#;
        let report = CliRunner::parse_report(stdout).unwrap();
        assert_eq!(
            report,
            RunReport {
                result: Some("done".to_owned()),
                is_error: false,
                subtype: "success".to_owned(),
                session_id: Some("abc".to_owned()),
                cost_usd: Some(0.027),
                num_turns: Some(3),
                permission_denials: 1,
            }
        );
    }

    #[test]
    fn report_tolerates_missing_fields() {
        let report = CliRunner::parse_report(br#"{"type":"result"}"#).unwrap();
        assert_eq!(report.result, None);
        assert!(!report.is_error);
        assert_eq!(report.permission_denials, 0);
    }

    #[test]
    fn outcome_mapping_covers_every_case() {
        let ok = RunReport {
            result: Some("answer".to_owned()),
            is_error: false,
            subtype: "success".to_owned(),
            session_id: None,
            cost_usd: None,
            num_turns: None,
            permission_denials: 0,
        };
        assert_eq!(
            ok.clone().into_outcome(),
            JobOutcome::Success {
                result: "answer".to_owned()
            }
        );

        let empty = RunReport {
            result: None,
            ..ok.clone()
        };
        assert!(matches!(
            empty.into_outcome(),
            JobOutcome::Failed { result: None, .. }
        ));

        let errored = RunReport {
            is_error: true,
            subtype: "error_max_turns".to_owned(),
            ..ok
        };
        assert_eq!(
            errored.into_outcome(),
            JobOutcome::Failed {
                error: "claude reported an error: error_max_turns".to_owned(),
                result: Some("answer".to_owned()),
            }
        );
    }
}
