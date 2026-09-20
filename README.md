# claude-job-runner

A small daemon that turns the chat application's `chat_messages` table into
a queue of tasks for [Claude Code](https://code.claude.com). Every *agentic
user message* (`sender = 'user'`, `is_agentic = 1`) is run as a prompt in a
fresh, sandboxed, headless Claude Code session; the answer is written back
to the same chat as an `ai` message.

```
  ┌───────────────┐  user + agentic + status NULL  ┌────────┐  claude -p   ┌──────────────────┐
  │ chat_messages │ ─────────────────────────────▶ │ worker │ ───────────▶ │ workspaces/job-N │
  │   (MySQL)     │ ◀───────────────────────────── │        │ ◀─────────── │  (sandboxed cwd) │
  └───────────────┘  done + new 'ai' row / failed  └────────┘  JSON report └──────────────────┘
          ▲                                                                   deleted afterwards
          │ POST /jobs
   ┌──────┴─────┐
   │ Hyper API  │
   └────────────┘
```

Lifecycle of a user row: `status = NULL` → `pending` (claimed) → `done` or
`failed`. The claim is an atomic `UPDATE ... WHERE status IS NULL`, so a row
is never handed to two workers. Rows that are not agentic user messages
(`ai` replies, plain chat messages) are never touched and keep
`status = NULL` forever.

On `done`, the answer is inserted as a new row in the same chat with
`sender = 'ai'`, `is_agentic = 1` and `payload = {"in_reply_to": <user id>}`,
and the user row gets `payload.reply_id` pointing at it. On `failed`, the
user row gets `payload.error` (and `payload.result` if Claude produced a
partial answer) and no `ai` row is written. Existing keys in `payload` are
preserved.

Up to `MAX_CONCURRENT_JOBS` jobs run at once, each in its own `claude`
process. A row is claimed only when a slot is free for it, so `pending`
always means "running right now", never "queued inside the daemon".

## Safety model

Every job runs `claude --print` with the job's own directory as `cwd` and a
CLI-level `--settings` payload that:

* enables Claude Code's OS sandbox (Seatbelt on macOS, bubblewrap on Linux)
  with `allowUnsandboxedCommands: false` and `failIfUnavailable: true`, so
  shell commands and their children can **read the whole machine but only
  write inside the workspace**;
* uses the `acceptEdits` permission mode, so the `Edit`/`Write` tools are
  auto-approved under the workspace and denied everywhere else (print mode
  cannot prompt, so a prompt is a denial);
* adds a `Read(//**)` allow rule so the `Read` tool can open any file.

Verified on macOS: a job that reads `/Users/.../README.md`, writes
`notes.md` in its workspace, and tries to write `~/should_not_exist.txt`
reports the first two as successful and the third as blocked.

Things the sandbox does **not** do for you:

* Network access from shell commands is closed by default. Allow specific
  hosts with `CLAUDE_ALLOWED_DOMAINS` (for example `github.com`) if a job
  needs `git push` or `npm install`.
* `CLAUDE_PERMISSION_MODE=bypassPermissions` disables the permission system.
  The Bash sandbox still holds, but `Edit`/`Write` may then leave the
  workspace. Prefer the default.
* Reads of credentials such as `~/.ssh` are allowed by default, as in any
  Claude Code session. Add `denyRead` entries via `CLAUDE_CONFIG_DIR`
  settings if that matters to you.
* The job's environment is the runner's, minus `DATABASE_URL`: the writable
  login stays with the daemon, and jobs get `AGENT_DATABASE_URL` (a
  read-only login) as their `DATABASE_URL` instead. Since jobs run as the
  same OS user and can read files, keep the writable URL out of `.env` and
  pass it to the daemon through a root-only file (see *Deploying*).

Because the workspace is deleted after each job, a job that should change
one of your projects needs a way to get its work out. The intended pattern
is to tell it to `git clone`/`git worktree add` the project *into the
workspace*, do the work there, and push a branch (with the domain
allow-listed), or to return a diff as its answer.

## Setup

1. Make sure the database has the `chats`/`chat_messages` tables and the
   `status` column. Both come from the granite-webhooks migrations
   (`20260920153120_log-chats.sql` and
   `20260920170000_alter-chat-messages-add-status.sql`); run them with
   `sqlx migrate run` in that repository. `schema/mysql.sql` here is a
   reference copy (without the foreign keys to `users`/`company`) that the
   tests load into a throw-away database.

   The runner touches only `chat_messages`: it reads `id`, `chat_id`,
   `content`, `status`, `payload` of agentic user rows, updates `status` and
   `payload`, and inserts `ai` rows.

2. Copy `.env.example` to `.env` and set at least `DATABASE_URL`.

3. Make sure `claude` is installed and logged in for the user that will run
   the daemon (`claude --version`).

4. Build and run:

   ```sh
   cargo run --release
   ```

The daemon exits cleanly on Ctrl-C / SIGTERM. A job that is still running at
that moment is killed and recorded as `failed`.

## Deploying (Ubuntu + systemd)

The production layout this repository's `Makefile` assumes:

* a dedicated user `runner` with rustup and Claude Code installed, and the
  repository cloned to `/home/runner/claude-job-runner`;
* `.env` in that directory (owned by `runner`, mode `600`) with everything
  **except** the writable `DATABASE_URL`;
* `/etc/claude-job-runner/secrets.env` (root, mode `600`) with the values
  jobs must never see: the writable `DATABASE_URL` and
  `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`). The systemd unit
  loads it with `EnvironmentFile=` and runs the binary as `runner`.

Two Ubuntu 24.04 specifics:

* Claude Code's Linux sandbox needs `bubblewrap` and `socat`, and 24.04
  restricts unprivileged user namespaces, so `bwrap` fails with
  `RTM_NEWADDR: Operation not permitted` until it gets an AppArmor profile:

  ```
  # /etc/apparmor.d/bwrap
  abi <abi/4.0>,
  include <tunables/global>
  profile bwrap /usr/bin/bwrap flags=(unconfined) {
    userns,
  }
  ```

  then `apparmor_parser -r /etc/apparmor.d/bwrap`.
* `cargo build --release` needs more than 2 GB of RAM; on a 2 GB instance
  add a 2 GB swap file first.

`make help` lists the day-to-day targets (`make env`, `make secrets`,
`make deploy`, `make logs`, ...). Server targets run on the host, or forward
themselves over SSH when invoked from a laptop.

## Enqueuing work

Normally the chat application inserts the user message itself:

```sql
INSERT INTO chat_messages (chat_id, sender, content, is_agentic)
VALUES (12, 'user', 'Summarise /Users/me/projects/foo/README.md in three bullets.', 1);
```

and the runner picks it up on its next poll. For experiments there is a
helper script that posts a message through the HTTP API and prints the
answer when it arrives (it starts a runner if none is listening):

```sh
scripts/ask.sh --chat 12 "Summarise README.md in three bullets."
echo "List the three largest files under ~/projects/foo." | ASK_CHAT_ID=12 scripts/ask.sh
scripts/ask.sh --stop     # stop the runner the script started
```

To exercise the same path the chat application uses (a plain `INSERT`, no
HTTP), `scripts/enqueue.py` writes the row directly and polls the table until
the runner has answered, then prints both rows and the `ai` reply. It uses the
admin login from the Granite Manager env file (`DB_HOST`/`DB_USER`/
`DB_PASSWORD` in `~/general_datebase/.env`, override with `--env-file`) against
the `main` database, so it can also create the `chats` row a message needs.
It runs with [uv](https://docs.astral.sh/uv/), which fetches `pymysql` itself.

```sh
scripts/enqueue.py "Summarise README.md in three bullets."   # new chat for user 1
scripts/enqueue.py --chat 12 "Follow-up in an existing chat"
scripts/enqueue.py --no-wait "Long task"                       # print ids and exit
scripts/enqueue.py --show 345                                  # watch an existing row
```

The HTTP API itself:

| Method | Path         | Body                                 | Response                         |
|--------|--------------|--------------------------------------|----------------------------------|
| GET    | `/health`    |                                      | `{"status":"ok"}`                |
| POST   | `/jobs`      | `{"chat_id": 12, "content": "..."}`  | `201` with the created job       |
| GET    | `/jobs/{id}` |                                      | the job or `404`                 |

A job is `{"id","chat_id","content","status","reply_id","error","reply"}`;
`reply` is the text of the `ai` message once `status` is `done`. Ids of
rows that are not agentic user messages answer `404`.

```sh
curl -X POST localhost:8080/jobs -H 'content-type: application/json' \
     -d '{"chat_id":12,"content":"List the three largest files under /Users/me/projects/foo."}'
curl localhost:8080/jobs/1
```

## Configuration

All settings come from the environment (a `.env` file is loaded if present).

| Variable                   | Default            | Meaning                                              |
|----------------------------|--------------------|------------------------------------------------------|
| `DATABASE_URL`             | required           | `mysql://…` (the database holding `chat_messages`)   |
| `DB_MAX_CONNECTIONS`       | `5`                | Pool size                                            |
| `AGENT_DATABASE_URL`       | (none)             | `DATABASE_URL` exported to jobs (read-only login); the runner's own is never passed on |
| `WORKSPACE_ROOT`           | `./workspaces`     | Parent of per-job directories; `job-*` dirs are swept on start |
| `POLL_INTERVAL_SECS`       | `5`                | Sleep between polls when the queue is empty          |
| `MAX_CONCURRENT_JOBS`      | `1`                | Jobs (and `claude` processes) running at the same time |
| `HTTP_ADDR`                | `127.0.0.1:8080`   | Bind address of the API                              |
| `REQUEUE_PENDING_ON_START` | `true`             | Reset orphaned `pending` rows; set `false` with several workers |
| `CLAUDE_BIN`               | `claude`           | Executable to run                                    |
| `CLAUDE_MODEL`             | (CLI default)      | `--model`                                            |
| `CLAUDE_MAX_TURNS`         | `50`               | `--max-turns`                                        |
| `CLAUDE_MAX_BUDGET_USD`    | (none)             | `--max-budget-usd`                                   |
| `CLAUDE_TIMEOUT_SECS`      | `1800`             | Wall-clock limit per job; the process is killed      |
| `CLAUDE_PERMISSION_MODE`   | `acceptEdits`      | or `bypassPermissions` (see above)                   |
| `CLAUDE_EFFORT`            | `low`              | `--effort`: `low`, `medium`, `high`, `xhigh` or `max` |
| `CLAUDE_ALLOWED_DOMAINS`   | (none)             | Comma-separated hosts sandboxed shell commands may reach |
| `CLAUDE_PERSIST_SESSIONS`  | `false`            | Keep transcripts for `claude --resume <session_id>`  |
| `CLAUDE_CONFIG_DIR`        | (inherit)          | Separate Claude config dir for jobs                  |
| `CLAUDE_MCP_CONFIG`        | (none)             | MCP servers JSON file for jobs; unset = no MCP servers |
| `RUST_LOG`                 | `info`             | Log filter                                           |

`CLAUDE_CONFIG_DIR` is worth setting if your personal `~/.claude` carries
a `CLAUDE.md` or hooks you do not want every job to inherit. MCP servers are
never inherited: jobs run with `--strict-mcp-config` and only the servers in
`CLAUDE_MCP_CONFIG`, if any.

## Resource usage

The daemon itself is a few megabytes. Each running job is a `claude`
process, which is a bundled JavaScript runtime: roughly 150–250 MB resident
for a short job, more on long ones with big transcripts, plus whatever the
job's shell commands spawn (`npm install`, test suites, compilers). Size
memory as `MAX_CONCURRENT_JOBS × ~0.3 GB` plus head-room for those
commands. CPU is mostly idle: a session spends nearly all of its time
waiting on the API.

Jobs are started with `DISABLE_AUTOUPDATER=1` and
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, and with no MCP servers unless
`CLAUDE_MCP_CONFIG` says otherwise, since every MCP server is another Node
process.

## Development

```sh
cargo test            # unit + integration tests (MySQL, fake claude binary)
cargo clippy --all-targets
```

The integration tests never call the real CLI: `tests/common/mod.rs`
provides a scripted `fake-claude.sh`. Database tests are
`#[sqlx::test]`: for each test sqlx creates a fresh database on the server
named by `DATABASE_URL` (from `.env`), the test loads `schema/mysql.sql`
into it, and the database is dropped afterwards. Point `DATABASE_URL` at
the test server, never at production.

## Layout

```
src/
  config.rs     environment → Config
  job.rs        MessageId, ChatId, JobStatus, Job, JobOutcome
  store.rs      JobStore: insert / get / reply / claim_next / finish / requeue_pending
  workspace.rs  WorkspaceRoot (sweep) and Workspace (create / remove)
  claude.rs     ClaudeConfig, CliRunner (spawn + parse), ClaudeRunner trait
  worker.rs     polling loop with a semaphore bounding concurrent jobs
  http.rs       Hyper API
  main.rs       wiring and graceful shutdown
schema/         reference copy of the chat tables (source of truth: granite-webhooks)
tests/          integration tests
```
