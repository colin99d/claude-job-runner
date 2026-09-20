#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["pymysql"]
# ///
"""enqueue.py - insert one agentic user message straight into `chat_messages`
and watch the runner answer it.

    scripts/enqueue.py "Summarise README.md in three bullets."   # new chat
    scripts/enqueue.py --chat 12 "Follow-up in an existing chat"
    scripts/enqueue.py --no-wait "Long task"                       # print ids and exit
    scripts/enqueue.py --show 345                                  # watch an existing row

Unlike ask.sh this does not go through the HTTP API: it does exactly what the
chat application does (INSERT with sender = 'user', is_agentic = 1, status
NULL), so the runner's poll -> claim -> answer path is what gets exercised.

Credentials come from the Granite Manager env file (DB_HOST, DB_USER,
DB_PASSWORD; default ~/general_datebase/.env), which is an admin login, so
the script can also create the `chats` row a message must belong to. The
database defaults to `main`, the one the deployed runner polls.
"""
import argparse
import json
import os
import re
import sys
import time
from datetime import datetime

import pymysql
import pymysql.cursors

DEFAULT_ENV_FILE = os.path.expanduser("~/general_datebase/.env")


def log(msg: str) -> None:
    print(f"\033[2m{msg}\033[0m", file=sys.stderr)


def die(msg: str) -> None:
    print(f"enqueue.py: {msg}", file=sys.stderr)
    sys.exit(1)


def read_env(path: str) -> dict[str, str]:
    try:
        text = open(path).read()
    except OSError as e:
        die(f"cannot read env file {path}: {e}")
    env = dict(re.findall(r"^(DB_\w+)=(.*)$", text, re.M))
    missing = [k for k in ("DB_HOST", "DB_USER", "DB_PASSWORD") if k not in env]
    if missing:
        die(f"{path} lacks {', '.join(missing)}")
    return env


def connect(env: dict[str, str], database: str) -> pymysql.Connection:
    return pymysql.connect(
        host=env["DB_HOST"],
        port=int(env.get("DB_PORT", 3306)),
        user=env["DB_USER"],
        password=env["DB_PASSWORD"],
        database=database,
        autocommit=True,
        cursorclass=pymysql.cursors.DictCursor,
    )


def new_chat(cur, user_id: int, title: str) -> int:
    cur.execute("SELECT company_id FROM users WHERE id = %s", (user_id,))
    row = cur.fetchone()
    if row is None:
        die(f"user {user_id} does not exist")
    cur.execute(
        "INSERT INTO chats (user_id, company_id, title) VALUES (%s, %s, %s)",
        (user_id, row["company_id"], title[:200]),
    )
    return cur.lastrowid


def insert_message(cur, chat_id: int, content: str) -> int:
    cur.execute(
        "INSERT INTO chat_messages (chat_id, sender, content, is_agentic) "
        "VALUES (%s, 'user', %s, 1)",
        (chat_id, content),
    )
    return cur.lastrowid


def show_rows(cur, ids: list[int]) -> None:
    marks = ", ".join(["%s"] * len(ids))
    cur.execute(
        "SELECT id, chat_id, sender, is_agentic, status, payload, created_at "
        f"FROM chat_messages WHERE id IN ({marks}) ORDER BY id",
        ids,
    )
    for row in cur.fetchall():
        log("  " + json.dumps(row, default=str))


def watch(cur, msg_id: int, poll_secs: float) -> int:
    last = object()
    while True:
        cur.execute(
            "SELECT status, payload FROM chat_messages "
            "WHERE id = %s AND sender = 'user' AND is_agentic = 1",
            (msg_id,),
        )
        row = cur.fetchone()
        if row is None:
            die(f"message #{msg_id} is not an agentic user message")
        status = row["status"]
        payload = json.loads(row["payload"]) if row["payload"] else {}
        if status != last:
            log(f"{datetime.now():%H:%M:%S}  status = {status or 'NULL'}")
            last = status
        if status == "done":
            reply_id = payload["reply_id"]
            log("rows as the chat application sees them:")
            show_rows(cur, [msg_id, reply_id])
            cur.execute("SELECT content FROM chat_messages WHERE id = %s", (reply_id,))
            log(f"ai message #{reply_id}:")
            print(cur.fetchone()["content"])
            return 0
        if status == "failed":
            show_rows(cur, [msg_id])
            log(f"job failed: {payload.get('error')}")
            if payload.get("result"):
                log("partial result:")
                print(payload["result"])
            return 1
        time.sleep(poll_secs)


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("prompt", nargs="*", help="the message; read from stdin if omitted")
    p.add_argument("--chat", type=int, help="existing chat id (default: create a new chat)")
    p.add_argument("--user", type=int, default=1, help="owner of a newly created chat (default: 1)")
    p.add_argument("--show", type=int, metavar="ID", help="watch an existing message instead of inserting")
    p.add_argument("--no-wait", action="store_true", help="insert and exit without waiting for the answer")
    p.add_argument("--env-file", default=DEFAULT_ENV_FILE, help=f"where DB_* come from (default: {DEFAULT_ENV_FILE})")
    p.add_argument("--database", default="main", help="database name (default: main)")
    p.add_argument("--poll-secs", type=float, default=2.0)
    args = p.parse_args()

    conn = connect(read_env(args.env_file), args.database)
    with conn.cursor() as cur:
        if args.show is not None:
            return watch(cur, args.show, args.poll_secs)

        prompt = " ".join(args.prompt) if args.prompt else sys.stdin.read()
        if not prompt.strip():
            die("prompt is empty")

        chat_id = args.chat
        if chat_id is None:
            chat_id = new_chat(cur, args.user, prompt.strip().splitlines()[0])
            log(f"created chat {chat_id} for user {args.user}")
        msg_id = insert_message(cur, chat_id, prompt)
        log(f"inserted user message #{msg_id} in chat {chat_id} (status NULL, waiting for the runner to claim it)")
        if args.no_wait:
            print(json.dumps({"chat_id": chat_id, "message_id": msg_id}))
            return 0
        return watch(cur, msg_id, args.poll_secs)


if __name__ == "__main__":
    sys.exit(main())
