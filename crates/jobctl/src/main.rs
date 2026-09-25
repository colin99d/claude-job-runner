//! `jobctl` - command-line tools for the chat database and the runner.

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use jobctl::api::{self, Api};
use jobctl::chats::{self, Message};
use jobctl::db::{self, DEFAULT_DATABASE};
use jobctl::sql;
use sqlx::mysql::MySqlConnection;

const USAGE: &str = "\
jobctl - tools for the chat database and the runner

usage:
  jobctl sql [DB] [SQL...]                 run SQL (from args or stdin), print TSV
  jobctl retrieve [DB] [--json] CHAT       print a whole chat, oldest message first
  jobctl enqueue [DB] [--chat N] [--user N] [--no-wait] [--poll-secs S] [PROMPT...]
                                           insert an agentic message and wait for the answer
  jobctl enqueue [DB] --show ID            watch an existing message instead
  jobctl ask [--chat N] [--no-start] [PROMPT...]
                                           post a job through the HTTP API and wait for it
  jobctl ask --stop                        stop a runner that `ask` started

DB options (sql, retrieve, enqueue):
  --env-file PATH   read DB_HOST/DB_USER/DB_PASSWORD[/DB_PORT] from PATH
  --database NAME   database for env-file credentials (default: main)
  Without --env-file, DATABASE_URL is used when set, else ~/general_datebase/.env.

A prompt or SQL left off the command line is read from stdin.
`ask` reads HTTP_ADDR (environment or .env, default 127.0.0.1:8080) and
ASK_CHAT_ID; unless --no-start is given it starts target/release/claude-job-runner
from the current directory when nothing answers, logging to runner.log.";

const DB_VALUES: &[&str] = &["--env-file", "--database"];
const PID_FILE: &str = ".runner.pid";
const LOG_FILE: &str = "runner.log";

type Failure = Box<dyn std::error::Error>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut argv = std::env::args().skip(1);
    let command = argv.next();
    let rest: Vec<String> = argv.collect();
    let outcome = match command.as_deref() {
        Some("sql") => cmd_sql(&rest).await,
        Some("retrieve") => cmd_retrieve(&rest).await,
        Some("enqueue") => cmd_enqueue(&rest).await,
        Some("ask") => cmd_ask(&rest).await,
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        Some(other) => Err(format!("unknown command {other:?}; see jobctl --help").into()),
        None => Err(USAGE.into()),
    };
    outcome.unwrap_or_else(|err| {
        eprintln!("jobctl: {err}");
        ExitCode::FAILURE
    })
}

// --- commands ------------------------------------------------------------------

async fn cmd_sql(args: &[String]) -> Result<ExitCode, Failure> {
    let args = Args::parse(args, DB_VALUES, &[])?;
    let statement = text_or_stdin(&args.positional, "SQL (end with Ctrl-D):")?;
    let mut conn = connect(&args).await?;
    println!("{}", sql::query(&mut conn, &statement).await?);
    Ok(ExitCode::SUCCESS)
}

async fn cmd_retrieve(args: &[String]) -> Result<ExitCode, Failure> {
    let args = Args::parse(args, DB_VALUES, &["--json"])?;
    let [chat_id] = args.positional.as_slice() else {
        return Err("usage: jobctl retrieve [--json] CHAT".into());
    };
    let chat_id: i64 = parse_num("CHAT", chat_id)?;
    let mut conn = connect(&args).await?;
    let chat = chats::chat(&mut conn, chat_id)
        .await?
        .ok_or_else(|| format!("chat {chat_id} does not exist"))?;
    let messages = chats::messages(&mut conn, chat_id).await?;

    if args.flag("--json") {
        for message in &messages {
            println!("{}", message.to_json());
        }
        return Ok(ExitCode::SUCCESS);
    }
    log(&format!(
        "chat {} (user {}, {}): {}",
        chat.id, chat.user_id, chat.created_at, chat.title
    ));
    for message in &messages {
        let mut header = format!("#{} {}  {}", message.id, message.sender, message.created_at);
        if message.sender == "user" && message.is_agentic {
            header.push_str("  [status ");
            header.push_str(message.status.as_deref().unwrap_or("NULL"));
            header.push(']');
            if let Some(error) = message.payload_str("error") {
                header.push_str("  error: ");
                header.push_str(error);
            }
        }
        println!();
        log(&header);
        println!("{}", message.content);
    }
    if messages.is_empty() {
        log("(no messages)");
    }
    Ok(ExitCode::SUCCESS)
}

async fn cmd_enqueue(args: &[String]) -> Result<ExitCode, Failure> {
    let values = [DB_VALUES, &["--chat", "--user", "--show", "--poll-secs"]].concat();
    let args = Args::parse(args, &values, &["--no-wait"])?;
    let poll = Duration::from_secs_f64(args.num("--poll-secs")?.unwrap_or(2.0));
    let mut conn = connect(&args).await?;

    if let Some(id) = args.num("--show")? {
        return watch_row(&mut conn, id, poll).await;
    }

    let prompt = text_or_stdin(&args.positional, "Prompt (end with Ctrl-D):")?;
    let chat_id = if let Some(chat_id) = args.num("--chat")? {
        chat_id
    } else {
        let user = args.num("--user")?.unwrap_or(1);
        let title = prompt.trim().lines().next().unwrap_or_default();
        let chat_id = chats::create_chat(&mut conn, user, title).await?;
        log(&format!("created chat {chat_id} for user {user}"));
        chat_id
    };
    let id = chats::insert_agentic(&mut conn, chat_id, &prompt).await?;
    log(&format!(
        "inserted user message #{id} in chat {chat_id} (status NULL, waiting for the runner to claim it)"
    ));
    if args.flag("--no-wait") {
        println!(
            "{}",
            serde_json::json!({ "chat_id": chat_id, "message_id": id })
        );
        return Ok(ExitCode::SUCCESS);
    }
    watch_row(&mut conn, id, poll).await
}

/// Polls an agentic user row until the runner has answered or failed it.
async fn watch_row(
    conn: &mut MySqlConnection,
    id: i64,
    poll: Duration,
) -> Result<ExitCode, Failure> {
    let started = Instant::now();
    let mut last: Option<Option<String>> = None;
    loop {
        let row = chats::message(conn, id)
            .await?
            .filter(|m| m.sender == "user" && m.is_agentic)
            .ok_or_else(|| format!("message #{id} is not an agentic user message"))?;
        if last.as_ref() != Some(&row.status) {
            log(&format!(
                "+{:>4}s  status = {}",
                started.elapsed().as_secs(),
                row.status.as_deref().unwrap_or("NULL")
            ));
            last = Some(row.status.clone());
        }
        match row.status.as_deref() {
            Some("done") => {
                let reply_id = row
                    .payload_i64("reply_id")
                    .ok_or("done row has no payload.reply_id")?;
                let reply = chats::message(conn, reply_id)
                    .await?
                    .ok_or_else(|| format!("reply #{reply_id} is missing"))?;
                log("rows as the chat application sees them:");
                show_row(&row);
                show_row(&reply);
                log(&format!("ai message #{reply_id}:"));
                println!("{}", reply.content);
                return Ok(ExitCode::SUCCESS);
            }
            Some("failed") => {
                show_row(&row);
                log(&format!(
                    "job failed: {}",
                    row.payload_str("error").unwrap_or("?")
                ));
                if let Some(partial) = row.payload_str("result") {
                    log("partial result:");
                    println!("{partial}");
                }
                return Ok(ExitCode::FAILURE);
            }
            _ => tokio::time::sleep(poll).await,
        }
    }
}

fn show_row(message: &Message) {
    let mut row = message.to_json();
    if let Some(object) = row.as_object_mut() {
        object.remove("content");
    }
    log(&format!("  {row}"));
}

async fn cmd_ask(args: &[String]) -> Result<ExitCode, Failure> {
    let args = Args::parse(args, &["--chat"], &["--stop", "--no-start"])?;
    if args.flag("--stop") {
        return stop_runner();
    }
    // Only for HTTP_ADDR / ASK_CHAT_ID; a missing .env is fine.
    let _ = dotenvy::dotenv();
    let chat_id: i64 = match args.value("--chat") {
        Some(raw) => parse_num("--chat", raw)?,
        None => match std::env::var("ASK_CHAT_ID") {
            Ok(raw) => parse_num("ASK_CHAT_ID", &raw)?,
            Err(_) => return Err("a chat id is required (--chat N or ASK_CHAT_ID)".into()),
        },
    };
    let prompt = text_or_stdin(&args.positional, "Prompt (end with Ctrl-D):")?;
    let addr = std::env::var("HTTP_ADDR").unwrap_or_else(|_| api::DEFAULT_ADDR.to_owned());
    let poll = Duration::from_secs_f64(match std::env::var("ASK_POLL_SECS") {
        Ok(raw) => parse_num("ASK_POLL_SECS", &raw)?,
        Err(_) => 2.0,
    });
    let api = Api::new(addr);

    if !api.healthy().await {
        if args.flag("--no-start") {
            return Err(format!("no runner answers on {}", api.addr()).into());
        }
        start_runner(&api).await?;
    }

    let job = api.create_job(chat_id, &prompt).await?;
    let id = job["id"]
        .as_i64()
        .ok_or("the API returned a job without an id")?;
    log(&format!("job #{id} queued; waiting ..."));
    let started = Instant::now();
    loop {
        let job = api.job(id).await?;
        match job["status"].as_str() {
            Some("done") => {
                eprint!("\r\x1b[K");
                println!("{}", job["reply"].as_str().unwrap_or_default());
                return Ok(ExitCode::SUCCESS);
            }
            Some("failed") => {
                eprint!("\r\x1b[K");
                eprintln!(
                    "job #{id} failed:\n{}",
                    job["error"].as_str().unwrap_or("?")
                );
                return Ok(ExitCode::FAILURE);
            }
            status => {
                eprint!(
                    "\r\x1b[2m[{}] {:>4}s\x1b[0m",
                    status.unwrap_or("?"),
                    started.elapsed().as_secs()
                );
                let _ = std::io::stderr().flush();
            }
        }
        tokio::time::sleep(poll).await;
    }
}

/// Starts the daemon built next to this binary, in the background.
async fn start_runner(api: &Api) -> Result<(), Failure> {
    let bin = std::env::current_exe()?.with_file_name("claude-job-runner");
    if !bin.exists() {
        return Err(format!(
            "{} not found; run `cargo build --release --workspace`",
            bin.display()
        )
        .into());
    }
    log(&format!(
        "starting runner on {} (log: {LOG_FILE})",
        api.addr()
    ));
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_FILE)?;
    let mut child = Command::new(&bin)
        .env("HTTP_ADDR", api.addr())
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        // Its own process group, so Ctrl-C here does not stop it.
        .process_group(0)
        .spawn()?;
    std::fs::write(PID_FILE, child.id().to_string())?;
    for _ in 0..30 {
        if api.healthy().await {
            return Ok(());
        }
        if child.try_wait()?.is_some() {
            return Err(format!("runner exited; see {LOG_FILE}").into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!("runner did not become healthy; see {LOG_FILE}").into())
}

fn stop_runner() -> Result<ExitCode, Failure> {
    let pid = std::fs::read_to_string(PID_FILE)
        .map_err(|_| format!("no {PID_FILE}; the runner was not started by `jobctl ask`"))?;
    let pid = pid.trim();
    if Command::new("kill").arg(pid).status()?.success() {
        log(&format!("sent SIGTERM to runner (pid {pid})"));
    } else {
        log(&format!("runner (pid {pid}) was not running"));
    }
    std::fs::remove_file(PID_FILE)?;
    Ok(ExitCode::SUCCESS)
}

// --- helpers -------------------------------------------------------------------

async fn connect(args: &Args) -> Result<MySqlConnection, Failure> {
    let env_file = args.value("--env-file").map(expand_home);
    let database = args.value("--database").unwrap_or(DEFAULT_DATABASE);
    let options = db::resolve(env_file.as_deref(), database)?;
    Ok(db::connect(&options).await?)
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// Positional words joined by spaces, or stdin when there are none.
fn text_or_stdin(words: &[String], hint: &str) -> Result<String, Failure> {
    let text = if words.is_empty() {
        if std::io::stdin().is_terminal() {
            eprintln!("{hint}");
        }
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    } else {
        words.join(" ")
    };
    if text.trim().is_empty() {
        return Err("input is empty".into());
    }
    Ok(text)
}

fn parse_num<T: std::str::FromStr>(name: &str, raw: &str) -> Result<T, Failure> {
    raw.trim()
        .parse()
        .map_err(|_| format!("{name} must be a number, got {raw:?}").into())
}

fn log(msg: &str) {
    eprintln!("\x1b[2m{msg}\x1b[0m");
}

/// Flags and positional arguments of one subcommand. Options that take a
/// value accept both `--name value` and `--name=value`; `--` ends options.
#[derive(Debug, Default)]
struct Args {
    values: HashMap<String, String>,
    flags: HashSet<String>,
    positional: Vec<String>,
}

impl Args {
    fn parse(raw: &[String], value_opts: &[&str], flag_opts: &[&str]) -> Result<Self, Failure> {
        let mut args = Self::default();
        let mut iter = raw.iter();
        while let Some(arg) = iter.next() {
            if arg == "--" {
                args.positional.extend(iter.by_ref().cloned());
                break;
            }
            if !arg.starts_with("--") {
                args.positional.push(arg.clone());
                continue;
            }
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (arg.as_str(), None),
            };
            if value_opts.contains(&name) {
                let value = match inline {
                    Some(value) => value,
                    None => iter
                        .next()
                        .ok_or_else(|| format!("{name} needs a value"))?
                        .clone(),
                };
                args.values.insert(name.to_owned(), value);
            } else if flag_opts.contains(&name) && inline.is_none() {
                args.flags.insert(name.to_owned());
            } else {
                return Err(format!("unknown option {arg}; see jobctl --help").into());
            }
        }
        Ok(args)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    fn num<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, Failure> {
        self.value(name).map(|raw| parse_num(name, raw)).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn args_take_values_flags_and_positionals() {
        let args = Args::parse(
            &strings(&[
                "--chat",
                "12",
                "--no-wait",
                "hello",
                "--user=3",
                "--",
                "--not-a-flag",
            ]),
            &["--chat", "--user"],
            &["--no-wait"],
        )
        .unwrap();
        assert_eq!(args.value("--chat"), Some("12"));
        assert_eq!(args.num::<i64>("--user").unwrap(), Some(3));
        assert!(args.flag("--no-wait"));
        assert_eq!(args.positional, ["hello", "--not-a-flag"]);
    }

    #[test]
    fn args_reject_unknown_options_and_missing_values() {
        assert!(Args::parse(&strings(&["--nope"]), &[], &[]).is_err());
        assert!(Args::parse(&strings(&["--chat"]), &["--chat"], &[]).is_err());
        assert!(Args::parse(&strings(&["--json=1"]), &[], &["--json"]).is_err());
    }
}
