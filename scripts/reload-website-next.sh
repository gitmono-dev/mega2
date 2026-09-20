#!/usr/bin/env bash
# Debounced rebuild and restart of website-next in the mega2-it compose stack.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/docker/docker-compose.test.yml"
PROJECT="${MEGA2_IT_PROJECT:-mega2-it}"
DEBOUNCE_SEC="${WEBSITE_NEXT_RELOAD_DEBOUNCE_SEC:-3}"
STATE_DIR="${TMPDIR:-/tmp}/mega2-reload-website-next"
REQUEST_FILE="$STATE_DIR/request.ts"
WORKER_PID_FILE="$STATE_DIR/worker.pid"
LOG_FILE="$STATE_DIR/build.log"

mkdir -p "$STATE_DIR"
date +%s >"$REQUEST_FILE"

start_worker() {
  if [[ -f "$WORKER_PID_FILE" ]]; then
    local worker_pid
    worker_pid="$(cat "$WORKER_PID_FILE")"
    if kill -0 "$worker_pid" 2>/dev/null; then
      return 0
    fi
  fi

  (
    # Singleton worker lock. flock is Linux-only; fall back to an atomic
    # mkdir lock on macOS (and other flock-less hosts).
    if command -v flock >/dev/null 2>&1; then
      flock -n 9 || exit 0
    elif ! mkdir "$STATE_DIR/build.lock" 2>/dev/null; then
      exit 0
    fi
    echo $$ >"$WORKER_PID_FILE"
    trap 'rm -f "$WORKER_PID_FILE"; rmdir "$STATE_DIR/build.lock" 2>/dev/null || true' EXIT

    while true; do
      local last_request current_request
      last_request="$(cat "$REQUEST_FILE")"
      sleep "$DEBOUNCE_SEC"
      current_request="$(cat "$REQUEST_FILE")"
      [[ "$current_request" == "$last_request" ]] && break
    done

    {
      echo "[$(date -Iseconds)] building website-next (project=$PROJECT)..."
      docker compose -p "$PROJECT" -f "$COMPOSE_FILE" --profile web build website-next
      docker compose -p "$PROJECT" -f "$COMPOSE_FILE" --profile web up -d --no-build --force-recreate --wait website-next
      echo "[$(date -Iseconds)] website-next is healthy"
    } >>"$LOG_FILE" 2>&1
  ) 9>"$STATE_DIR/build.flock" &
}

start_worker
