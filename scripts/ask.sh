#!/usr/bin/env bash
# ask.sh — post one agentic message to a chat and print Claude's answer.
#
#   scripts/ask.sh --chat 12 "Summarise README.md in three bullets."
#   echo "List the largest files under ~/projects" | ASK_CHAT_ID=12 scripts/ask.sh
#   scripts/ask.sh --chat 12      # prompts interactively
#   scripts/ask.sh --stop         # stop a runner this script started
#
# The chat (a row of `chats`) must already exist; pass its id with --chat or
# ASK_CHAT_ID. If no runner answers on HTTP_ADDR, one is started in the
# background using the DATABASE_URL from .env. The runner keeps running
# between calls; stop it with --stop.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Only the setting the script itself needs; the runner loads .env on its own.
env_value() {
    [ -f .env ] && grep -E "^$1=" .env | tail -n1 | cut -d= -f2- || true
}
HTTP_ADDR="${HTTP_ADDR:-$(env_value HTTP_ADDR)}"
HTTP_ADDR="${HTTP_ADDR:-127.0.0.1:8080}"
CHAT_ID="${ASK_CHAT_ID:-}"

API="http://$HTTP_ADDR"
BIN="target/release/claude-job-runner"
PID_FILE=".runner.pid"
LOG_FILE="runner.log"
POLL_SECS="${ASK_POLL_SECS:-2}"

log() { printf '\033[2m%s\033[0m\n' "$*" >&2; }
die() { printf 'ask.sh: %s\n' "$*" >&2; exit 1; }

for tool in curl jq; do
    command -v "$tool" >/dev/null || die "$tool is required"
done

healthy() { curl -fsS --max-time 2 "$API/health" >/dev/null 2>&1; }

stop_runner() {
    [ -f "$PID_FILE" ] || die "no $PID_FILE; the runner was not started by this script"
    local pid
    pid="$(cat "$PID_FILE")"
    if kill "$pid" 2>/dev/null; then
        log "sent SIGTERM to runner (pid $pid)"
    else
        log "runner (pid $pid) was not running"
    fi
    rm -f "$PID_FILE"
}

start_runner() {
    if [ ! -x "$BIN" ]; then
        log "building $BIN ..."
        cargo build --release --quiet
    fi
    log "starting runner on $HTTP_ADDR (log: $LOG_FILE)"
    HTTP_ADDR="$HTTP_ADDR" nohup "$BIN" >>"$LOG_FILE" 2>&1 &
    echo $! > "$PID_FILE"
    local i
    for i in $(seq 1 30); do
        healthy && return
        kill -0 "$(cat "$PID_FILE")" 2>/dev/null || die "runner exited; see $LOG_FILE"
        sleep 0.5
    done
    die "runner did not become healthy; see $LOG_FILE"
}

read_prompt() {
    if [ $# -gt 0 ]; then
        printf '%s' "$*"
    elif [ ! -t 0 ]; then
        cat
    else
        printf 'Prompt (end with Ctrl-D):\n' >&2
        cat
    fi
}

case "${1:-}" in
    -h|--help) sed -n '2,12p' "$0" | cut -c3-; exit 0 ;;
    --stop) stop_runner; exit 0 ;;
    --chat) [ $# -ge 2 ] || die "--chat needs an id"; CHAT_ID="$2"; shift 2 ;;
esac
[[ "$CHAT_ID" =~ ^[0-9]+$ ]] || die "a numeric chat id is required (--chat N or ASK_CHAT_ID)"

prompt="$(read_prompt "$@")"
[ -n "$(printf '%s' "$prompt" | tr -d '[:space:]')" ] || die "prompt is empty"

healthy || start_runner

job="$(jq -n --argjson chat_id "$CHAT_ID" --arg content "$prompt" '{chat_id: $chat_id, content: $content}' \
    | curl -fsS -X POST "$API/jobs" -H 'content-type: application/json' --data-binary @-)"
id="$(jq -r .id <<<"$job")"
log "job #$id queued; waiting ..."

started=$(date +%s)
while :; do
    job="$(curl -fsS "$API/jobs/$id")"
    status="$(jq -r .status <<<"$job")"
    case "$status" in
        done)
            printf '\r\033[K' >&2
            jq -r .reply <<<"$job"
            exit 0
            ;;
        failed)
            printf '\r\033[K' >&2
            printf 'job #%s failed:\n' "$id" >&2
            jq -r .error <<<"$job" >&2
            exit 1
            ;;
    esac
    printf '\r\033[2m[%s] %4ds\033[0m' "$status" "$(( $(date +%s) - started ))" >&2
    sleep "$POLL_SECS"
done
