#!/usr/bin/env bash
# Test-only entry for the traces pg_trgm stack. It is not a mega2 user command.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CASE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --case)
      CASE="${2:-}"
      shift 2
      ;;
    *)
      echo "unknown argument" >&2
      exit 2
      ;;
  esac
done

if [[ -z "$CASE" ]]; then
  echo "missing --case" >&2
  exit 2
fi

run_psql() {
  local var_name="$1"
  local sql="$2"
  python3 - "$var_name" "$sql" <<'PY'
import os
import subprocess
import sys
from urllib.parse import unquote, urlparse

var_name = sys.argv[1]
sql = sys.argv[2]
raw = os.environ.get(var_name, "")
if not raw:
    sys.exit(f"{var_name} is unset")
url = urlparse(raw)
if url.scheme not in ("postgres", "postgresql"):
    sys.exit("database URL scheme is not postgres")
env = os.environ.copy()
env.update({
    "PGHOST": url.hostname or "",
    "PGPORT": str(url.port or 5432),
    "PGUSER": unquote(url.username or ""),
    "PGPASSWORD": unquote(url.password or ""),
    "PGDATABASE": url.path.lstrip("/"),
    "PGCONNECT_TIMEOUT": "30",
})
proc = subprocess.run(
    [
        "timeout", "30",
        "docker", "run", "--rm", "--network", "host",
        "-e", "PGHOST", "-e", "PGPORT", "-e", "PGUSER",
        "-e", "PGPASSWORD", "-e", "PGDATABASE", "-e", "PGCONNECT_TIMEOUT",
        "postgres:18.6-alpine3.24",
        "psql", "-v", "ON_ERROR_STOP=1", "-At", "-c", sql,
    ],
    env=env,
    text=True,
    capture_output=True,
)
sys.stdout.write(proc.stdout)
if proc.returncode != 0:
    sys.stderr.write("psql catalog query failed\n")
sys.exit(proc.returncode)
PY
}

ci_preflight() {
  if [[ -z "${MEGA_DATABASE__DB_URL:-}" || -z "${MEGA2_IT_DB_ADMIN_URL:-}" ]]; then
    echo "ci-preflight requires MEGA_DATABASE__DB_URL and MEGA2_IT_DB_ADMIN_URL" >&2
    exit 1
  fi
  echo "executed=1"
  local row createdb
  row="$(run_psql MEGA_DATABASE__DB_URL "SELECT e.extname, e.extversion, n.nspname, r.rolname, r.rolcanlogin::text FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace JOIN pg_catalog.pg_roles r ON r.oid = n.nspowner WHERE e.extname = 'pg_trgm'")"
  if [[ "$row" != "pg_trgm|1.6|mega2_extensions|mega2_traces_ext_owner|false" ]]; then
    echo "ci-preflight catalog mismatch" >&2
    echo "expected_extversion=1.6" >&2
    exit 1
  fi
  createdb="$(run_psql MEGA2_IT_DB_ADMIN_URL "SELECT rolcreatedb::text FROM pg_catalog.pg_roles WHERE rolname = current_user")"
  if [[ "$createdb" != "true" ]]; then
    echo "ci-preflight admin role lacks CREATEDB" >&2
    exit 1
  fi
  echo "extname=pg_trgm"
  echo "extversion=1.6"
  echo "expected_extversion=1.6"
  echo "namespace=mega2_extensions"
  echo "owner=mega2_traces_ext_owner"
}

fresh_test() {
  project="traces-pg-test-$$-1"
  net_name="traces-pg-test-net-$$-1"
  workdir="$(mktemp -d)"
  trap 'rm -rf "$workdir"' EXIT
  derived="$workdir/compose.json"
  port="$(python3 - <<'PY'
import socket
sock = socket.socket()
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
)"
  before="$(python3 - <<'PY'
import json, subprocess
proc = subprocess.run(
    ["docker", "network", "inspect", "mega2-test-network"],
    text=True, capture_output=True,
)
if proc.returncode != 0:
    print(json.dumps({"absent": True}, sort_keys=True))
else:
    item = json.loads(proc.stdout)[0]
    print(json.dumps({
        "absent": False,
        "name": item.get("Name"),
        "id": item.get("Id"),
        "labels": item.get("Labels") or {},
        "containers": sorted((item.get("Containers") or {})),
    }, sort_keys=True))
PY
)"
  docker compose -f "$ROOT/docker/docker-compose.test.yml" config --format json >"$workdir/base.json"
  python3 - "$workdir/base.json" "$derived" "$project" "$net_name" "$port" <<'PY'
import json
import sys

source, dest, project, net_name, port = sys.argv[1:]
with open(source, encoding="utf-8") as handle:
    data = json.load(handle)

def rewrite(value):
    if isinstance(value, dict):
        return {key: rewrite(item) for key, item in value.items()}
    if isinstance(value, list):
        return [rewrite(item) for item in value]
    if value == "mega2-test-network":
        return net_name
    return value

data = rewrite(data)
data["name"] = project
networks = data.setdefault("networks", {})
default = networks.get("default")
if not isinstance(default, dict):
    default = {}
networks["default"] = default
default["name"] = net_name
default.pop("external", None)
for name, service in (data.get("services") or {}).items():
    if name == "postgres":
        service["ports"] = [{
            "mode": "ingress",
            "host_ip": "127.0.0.1",
            "target": 5432,
            "published": str(port),
            "protocol": "tcp",
        }]
    else:
        service["ports"] = []
text = json.dumps(data)
if "mega2-test-network" in text or "15432" in text or "25432" in text:
    sys.exit("derived compose still names the shared network or a fixed port")
with open(dest, "w", encoding="utf-8") as handle:
    handle.write(text)
PY
  local digest
  digest="$(sha256sum "$derived" | awk '{print $1}')"
  cleaned=0
  interrupted=0
  cleanup() {
    local rc=$?
    if [[ "$cleaned" -eq 1 ]]; then
      exit "$rc"
    fi
    cleaned=1
    if [[ "$interrupted" -eq 1 ]]; then
      rc=1
    fi
    set +e
    if [[ -n "${project:-}" && -f "${derived:-}" ]]; then
      timeout 60 docker compose -p "$project" -f "$derived" down -v --remove-orphans
      if [[ $? -ne 0 ]]; then
        rc=1
      fi
      if [[ -n "$(docker ps -aq --filter "label=com.docker.compose.project=${project}")" ]]; then
        echo "owned containers remained" >&2
        rc=1
      fi
      if [[ -n "$(docker volume ls -q --filter "label=com.docker.compose.project=${project}")" ]]; then
        echo "owned volumes remained" >&2
        rc=1
      fi
      if docker network inspect "$net_name" >/dev/null 2>&1; then
        echo "owned network remained" >&2
        rc=1
      fi
    fi
    after="$(python3 - <<'PY'
import json, subprocess
proc = subprocess.run(
    ["docker", "network", "inspect", "mega2-test-network"],
    text=True, capture_output=True,
)
if proc.returncode != 0:
    print(json.dumps({"absent": True}, sort_keys=True))
else:
    item = json.loads(proc.stdout)[0]
    print(json.dumps({
        "absent": False,
        "name": item.get("Name"),
        "id": item.get("Id"),
        "labels": item.get("Labels") or {},
        "containers": sorted((item.get("Containers") or {})),
    }, sort_keys=True))
PY
)"
    if [[ "$before" != "$after" ]]; then
      echo "shared mega2-test-network snapshot changed" >&2
      rc=1
    fi
    rm -rf "$workdir"
    exit "$rc"
  }
  on_signal() {
    interrupted=1
    exit 1
  }
  trap cleanup EXIT
  trap on_signal INT TERM
  echo "executed=1"
  if ! timeout 120 docker compose -p "$project" -f "$derived" up -d --wait --no-deps postgres; then
    echo "fresh postgres did not become healthy within 120s" >&2
    exit 1
  fi
  if [[ "$(sha256sum "$derived" | awk '{print $1}')" != "$digest" ]]; then
    echo "derived compose file changed during startup" >&2
    exit 1
  fi
  local label
  label="$(docker network inspect "$net_name" --format '{{index .Labels "com.docker.compose.project"}}')"
  if [[ "$label" != "$project" ]]; then
    echo "owned network project label mismatch" >&2
    exit 1
  fi
  local row
  row="$(timeout 30 docker compose -p "$project" -f "$derived" exec -T postgres \
    psql -U mega2 -d mega2 -v ON_ERROR_STOP=1 -At -F '|' -c \
    "SELECT e.extname, e.extversion, n.nspname, r.rolname, r.rolcanlogin::text FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace JOIN pg_catalog.pg_roles r ON r.oid = n.nspowner WHERE e.extname = 'pg_trgm'")"
  if [[ "$row" != "pg_trgm|1.6|mega2_extensions|mega2_traces_ext_owner|false" ]]; then
    echo "fresh catalog mismatch" >&2
    echo "expected_extversion=1.6" >&2
    exit 1
  fi
  echo "extname=pg_trgm"
  echo "extversion=1.6"
  echo "expected_extversion=1.6"
  echo "namespace=mega2_extensions"
  echo "owner=mega2_traces_ext_owner"
  echo "derived=$derived"
  echo "project=$project"
}

case "$CASE" in
  ci-preflight) ci_preflight ;;
  fresh-test) fresh_test ;;
  *)
    echo "unknown case" >&2
    exit 2
    ;;
esac
