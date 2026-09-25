#!/usr/bin/env python3
"""db_mcp.py - a minimal MCP server (stdio) that runs SQL against DATABASE_URL.

The runner starts it for every job that has AGENT_DATABASE_URL set. MCP
servers run outside Claude Code's Bash sandbox, whose network namespace only
lets traffic out through an HTTP/SOCKS proxy, so a plain `mysql` call from
Bash cannot reach the database. Safety comes from the login itself: jobs only
ever get a read-only one.

Standard library only; queries go through the `mysql` command-line client.
"""
import json
import os
import subprocess
import sys
from urllib.parse import parse_qs, unquote, urlparse

MAX_OUTPUT_CHARS = 100_000
QUERY_TIMEOUT_SECS = 120

TOOLS = [
    {
        "name": "query",
        "description": (
            "Run one SQL statement against the company's MySQL database (read-only login) "
            "and return the result as tab-separated text with a header row. Use it for "
            "SELECT, SHOW TABLES, DESCRIBE <table>, SHOW CREATE TABLE <table>, etc."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {"sql": {"type": "string", "description": "the SQL statement"}},
            "required": ["sql"],
        },
    }
]


def mysql_command(url: str) -> tuple[list[str], dict[str, str]]:
    parsed = urlparse(url)
    args = [
        "mysql",
        "--batch",
        "--connect-timeout=10",
        f"--host={parsed.hostname}",
        f"--port={parsed.port or 3306}",
        f"--user={unquote(parsed.username or '')}",
    ]
    ssl_mode = parse_qs(parsed.query).get("ssl-mode")
    if ssl_mode:
        args.append(f"--ssl-mode={ssl_mode[0]}")
    database = parsed.path.lstrip("/")
    if database:
        args.append(f"--database={database}")
    env = dict(os.environ, MYSQL_PWD=unquote(parsed.password or ""))
    return args, env


def run_query(sql: str) -> tuple[str, bool]:
    url = os.environ.get("DATABASE_URL")
    if not url:
        return "DATABASE_URL is not set; no database is configured for this job.", True
    args, env = mysql_command(url)
    try:
        proc = subprocess.run(
            args, input=sql, env=env, capture_output=True, text=True, timeout=QUERY_TIMEOUT_SECS
        )
    except subprocess.TimeoutExpired:
        return f"query timed out after {QUERY_TIMEOUT_SECS}s", True
    except FileNotFoundError:
        return "the mysql client is not installed on the runner host", True
    if proc.returncode != 0:
        return proc.stderr.strip() or f"mysql exited with {proc.returncode}", True
    out = proc.stdout or "(no rows)"
    if len(out) > MAX_OUTPUT_CHARS:
        out = out[:MAX_OUTPUT_CHARS] + "\n... (truncated; add LIMIT or aggregate)"
    return out, False


def handle(msg: dict) -> dict | None:
    method = msg.get("method")
    if "id" not in msg:
        return None  # notification
    if method == "initialize":
        result = {
            "protocolVersion": msg.get("params", {}).get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "db", "version": "1.0.0"},
        }
    elif method == "tools/list":
        result = {"tools": TOOLS}
    elif method == "tools/call":
        params = msg.get("params", {})
        if params.get("name") != "query":
            return error(msg["id"], -32602, f"unknown tool {params.get('name')!r}")
        text, is_error = run_query(params.get("arguments", {}).get("sql", ""))
        result = {"content": [{"type": "text", "text": text}], "isError": is_error}
    elif method == "ping":
        result = {}
    else:
        return error(msg["id"], -32601, f"method not found: {method}")
    return {"jsonrpc": "2.0", "id": msg["id"], "result": result}


def error(msg_id, code: int, message: str) -> dict:
    return {"jsonrpc": "2.0", "id": msg_id, "error": {"code": code, "message": message}}


def main() -> None:
    for line in sys.stdin:
        if not line.strip():
            continue
        try:
            reply = handle(json.loads(line))
        except Exception as e:  # keep serving; report instead of dying
            reply = error(None, -32603, str(e))
        if reply is not None:
            sys.stdout.write(json.dumps(reply) + "\n")
            sys.stdout.flush()


if __name__ == "__main__":
    main()
