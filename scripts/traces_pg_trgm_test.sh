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

shared_network_snapshot() {
  python3 - <<'PY'
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
}

rewrite_deploy_compose() {
  python3 - "$@" <<'PY'
import json
import sys

source, dest, project, net_name, port, strip = sys.argv[1:]
with open(source, encoding="utf-8") as handle:
    data = json.load(handle)
fixed = {
    "mega2-test-network",
    "docker_default",
    "mega2-trunk-pg",
    "mega2-trunk-redis",
    "mega2-trunk-data",
    "mega2-trunk-rustfs",
}

def rename(value):
    if value in fixed or "mega2-trunk-" in value:
        return f"{project}-{value}"
    return value

def rewrite(value):
    if isinstance(value, dict):
        renamed = {}
        for key, item in value.items():
            renamed[rename(key) if isinstance(key, str) else key] = rewrite(item)
        return renamed
    if isinstance(value, list):
        return [rewrite(item) for item in value]
    if isinstance(value, str):
        return rename(value)
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
    volumes = service.get("volumes") or []
    if name == "postgres" and strip == "1":
        kept = []
        for volume in volumes:
            target = volume.get("target", "") if isinstance(volume, dict) else ""
            if "docker-entrypoint-initdb.d" in str(target):
                continue
            kept.append(volume)
        service["volumes"] = kept
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
forbidden = (
    '"mega2-test-network"',
    '"mega2-trunk-pg"',
    '"mega2-trunk-redis"',
    '"mega2-trunk-data"',
    '"mega2-trunk-rustfs"',
    '"15432"',
    '"25432"',
)
if any(token in text for token in forbidden):
    sys.exit("derived compose still names a shared network, trunk volume, or fixed port")
with open(dest, "w", encoding="utf-8") as handle:
    handle.write(text)
PY
}

cleanup_owned() {
  local rc=$?
  if [[ "${cleaned:-0}" -eq 1 ]]; then
    exit "$rc"
  fi
  cleaned=1
  if [[ "${interrupted:-0}" -eq 1 ]]; then
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
  local after
  after="$(shared_network_snapshot)"
  if [[ -n "${before:-}" && "$before" != "$after" ]]; then
    echo "shared mega2-test-network snapshot changed" >&2
    rc=1
  fi
  rm -rf "${workdir:-}"
  exit "$rc"
}

begin_owned_stack() {
  local strip="$1"
  shift
  project="traces-pg-storage-$$-$(python3 -c 'import secrets; print(secrets.token_hex(3))')"
  net_name="${project}-net"
  workdir="$(mktemp -d)"
  derived="$workdir/compose.json"
  cleaned=0
  interrupted=0
  before="$(shared_network_snapshot)"
  trap 'rm -rf "$workdir"' EXIT
  port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
  local args=()
  local file
  for file in "$@"; do
    args+=(-f "$file")
  done
  docker compose "${args[@]}" config --format json >"$workdir/base.json"
  rewrite_deploy_compose "$workdir/base.json" "$derived" "$project" "$net_name" "$port" "$strip"
  trap cleanup_owned EXIT
  trap 'interrupted=1; exit 1' INT TERM
  echo "executed=1"
  if ! timeout 120 docker compose -p "$project" -f "$derived" up -d --wait --no-deps postgres; then
    echo "owned postgres did not become healthy within 120s" >&2
    exit 1
  fi
  local label
  label="$(docker network inspect "$net_name" --format '{{index .Labels "com.docker.compose.project"}}')"
  if [[ "$label" != "$project" ]]; then
    echo "owned network project label mismatch" >&2
    exit 1
  fi
}

stack_sql() {
  local database="$1"
  local sql="$2"
  docker compose -p "$project" -f "$derived" exec -T postgres \
    psql -U mega2 -d "$database" -v ON_ERROR_STOP=1 -At -c "$sql"
}

use_admin_url() {
  local database="$1"
  MEGA2_TRACES_PG_ADMIN_URL="postgres://mega2:mega2_storage_password@127.0.0.1:${port}/${database}"
  export MEGA2_TRACES_PG_ADMIN_URL
}

extension_row() {
  local database="$1"
  stack_sql "$database" "SELECT e.extname, e.extversion, n.nspname, r.rolname FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace JOIN pg_catalog.pg_roles r ON r.oid = n.nspowner WHERE e.extname = 'pg_trgm'"
}

fresh_storage_base() {
  begin_owned_stack 0 "$ROOT/docker/docker-compose-storage-only.yml"
  local row
  row="$(extension_row mega2)"
  if [[ "$row" != "pg_trgm|1.6|mega2_extensions|mega2_traces_ext_owner" ]]; then
    echo "fresh storage catalog mismatch" >&2
    exit 1
  fi
  use_admin_url mega2
  MEGA2_TRACES_PG_APP_ROLE=mega2_traces_app
  export MEGA2_TRACES_PG_APP_ROLE
  "$ROOT/scripts/traces_pg_trgm_bootstrap.sh" --check
}

auth_none_inheritance() {
  local base_only="$ROOT/docker/docker-compose-storage-only.yml"
  local overlay="$ROOT/docker/docker-compose-storage-only.auth-none.yml"
  begin_owned_stack 0 "$base_only" "$overlay"
  python3 - "$derived" "$ROOT/docker/traces_pg_trgm_init.sql" <<'PY'
import json, sys
derived, init_sql = sys.argv[1:]
data = json.load(open(derived, encoding="utf-8"))
mounts = data["services"]["postgres"].get("volumes") or []
targets = [item.get("target") for item in mounts if isinstance(item, dict)]
sources = [item.get("source") for item in mounts if isinstance(item, dict)]
if "/docker-entrypoint-initdb.d/zz-traces-pg-trgm.sql" not in targets:
    sys.exit("auth-none compose dropped the init SQL mount")
if init_sql not in sources:
    sys.exit("auth-none compose does not mount the shared init SQL")
PY
  local row
  row="$(extension_row mega2)"
  if [[ "$row" != "pg_trgm|1.6|mega2_extensions|mega2_traces_ext_owner" ]]; then
    echo "auth-none catalog mismatch" >&2
    exit 1
  fi
  use_admin_url mega2
  MEGA2_TRACES_PG_APP_ROLE=mega2_traces_app
  export MEGA2_TRACES_PG_APP_ROLE
  "$ROOT/scripts/traces_pg_trgm_bootstrap.sh" --check
}

acl_snapshot() {
  stack_sql mega2 "SELECT n.nspacl::text FROM pg_catalog.pg_namespace n WHERE n.nspname = 'mega2_extensions' UNION ALL SELECT p.proname || '=' || COALESCE(p.proacl::text, '') FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = 'mega2_extensions' ORDER BY 1"
}

existing_volume() {
  begin_owned_stack 1 "$ROOT/docker/docker-compose-storage-only.yml"
  local count
  count="$(stack_sql mega2 "SELECT count(*) FROM pg_catalog.pg_extension WHERE extname = 'pg_trgm'")"
  if [[ "$count" != "0" ]]; then
    echo "existing volume already had pg_trgm" >&2
    exit 1
  fi
  stack_sql mega2 "CREATE ROLE mega2_traces_app NOLOGIN"
  use_admin_url mega2
  MEGA2_TRACES_PG_APP_ROLE=mega2_traces_app
  export MEGA2_TRACES_PG_APP_ROLE
  "$ROOT/scripts/traces_pg_trgm_bootstrap.sh" --apply >/dev/null
  local first
  first="$(acl_snapshot)"
  "$ROOT/scripts/traces_pg_trgm_bootstrap.sh" --apply >/dev/null
  local second
  second="$(acl_snapshot)"
  if [[ "$first" != "$second" ]]; then
    echo "second apply changed the catalog or ACL" >&2
    exit 1
  fi
  "$ROOT/scripts/traces_pg_trgm_bootstrap.sh" --check >/dev/null
}

views_bootstrap() {
  begin_owned_stack 0 "$ROOT/docker/docker-compose-storage-only.yml"
  stack_sql postgres "CREATE DATABASE mega2_hp25_views"
  stack_sql mega2_hp25_views "CREATE ROLE mega2_traces_app NOLOGIN"
  use_admin_url mega2_hp25_views
  MEGA2_TRACES_PG_APP_ROLE=mega2_traces_app
  export MEGA2_TRACES_PG_APP_ROLE
  "$ROOT/scripts/traces_pg_trgm_bootstrap.sh" --apply >/dev/null
  "$ROOT/scripts/traces_pg_trgm_bootstrap.sh" --check >/dev/null
  PROBE_URL="$MEGA2_TRACES_PG_ADMIN_URL"
  export PROBE_URL
  local db row
  db="$(run_psql PROBE_URL "SELECT current_database()")"
  row="$(run_psql PROBE_URL "SELECT e.extname, e.extversion, n.nspname, r.rolname FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace JOIN pg_catalog.pg_roles r ON r.oid = n.nspowner WHERE e.extname = 'pg_trgm'")"
  if [[ "$db" != "mega2_hp25_views" || "$row" != "pg_trgm|1.6|mega2_extensions|mega2_traces_ext_owner" ]]; then
    echo "views database catalog mismatch" >&2
    exit 1
  fi
}

wrong_namespace() {
  begin_owned_stack 1 "$ROOT/docker/docker-compose-storage-only.yml"
  stack_sql postgres "CREATE DATABASE traces_wrong_ns"
  stack_sql traces_wrong_ns "CREATE EXTENSION pg_trgm"
  local before_row after_row
  before_row="$(extension_row traces_wrong_ns)"
  stack_sql traces_wrong_ns "CREATE ROLE mega2_traces_app NOLOGIN"
  use_admin_url traces_wrong_ns
  MEGA2_TRACES_PG_APP_ROLE=mega2_traces_app
  export MEGA2_TRACES_PG_APP_ROLE
  if "$ROOT/scripts/traces_pg_trgm_bootstrap.sh" --apply >"$workdir/apply.out" 2>"$workdir/apply.err"; then
    echo "wrong namespace apply was accepted" >&2
    exit 1
  fi
  if ! grep -q '^wrong_namespace$' "$workdir/apply.err"; then
    echo "wrong namespace apply did not report wrong_namespace" >&2
    exit 1
  fi
  after_row="$(extension_row traces_wrong_ns)"
  if [[ "$before_row" != "$after_row" ]]; then
    echo "wrong namespace apply changed the catalog" >&2
    exit 1
  fi
}

app_grants() {
  begin_owned_stack 0 "$ROOT/docker/docker-compose-storage-only.yml"
  local password
  password="$(python3 -c 'import secrets; print(secrets.token_hex(12))')"
  stack_sql mega2 "REVOKE USAGE ON SCHEMA mega2_extensions FROM PUBLIC"
  stack_sql mega2 "REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA mega2_extensions FROM PUBLIC"
  docker compose -p "$project" -f "$derived" exec -T postgres \
    psql -U mega2 -d mega2 -v ON_ERROR_STOP=1 >/dev/null <<SQL
CREATE ROLE mega2_traces_app LOGIN PASSWORD '${password}' NOSUPERUSER NOCREATEDB NOINHERIT;
SQL
  PROBE_URL="postgres://mega2_traces_app:${password}@127.0.0.1:${port}/mega2"
  export PROBE_URL
  if run_psql PROBE_URL "SELECT mega2_extensions.show_trgm('abc')" >/dev/null; then
    echo "app role could execute show_trgm before the grant" >&2
    exit 1
  fi
  use_admin_url mega2
  MEGA2_TRACES_PG_APP_ROLE=mega2_traces_app
  export MEGA2_TRACES_PG_APP_ROLE
  "$ROOT/scripts/traces_pg_trgm_bootstrap.sh" --apply >/dev/null
  local shown usage execute owner
  shown="$(run_psql PROBE_URL "SELECT mega2_extensions.show_trgm('abc')")"
  if [[ -z "$shown" ]]; then
    echo "app role could not execute show_trgm after the grant" >&2
    exit 1
  fi
  usage="$(stack_sql mega2 "SELECT has_schema_privilege('mega2_traces_app', 'mega2_extensions', 'USAGE')")"
  execute="$(stack_sql mega2 "SELECT has_function_privilege('mega2_traces_app', 'mega2_extensions.show_trgm(text)', 'EXECUTE')")"
  owner="$(stack_sql mega2 "SELECT r.rolname FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_roles r ON r.oid = n.nspowner WHERE n.nspname = 'mega2_extensions'")"
  if [[ "$usage" != "t" || "$execute" != "t" || "$owner" != "mega2_traces_ext_owner" ]]; then
    echo "app grant oracle mismatch" >&2
    exit 1
  fi
  "$ROOT/scripts/traces_pg_trgm_bootstrap.sh" --check
}

case "$CASE" in
  ci-preflight) ci_preflight ;;
  fresh-test) fresh_test ;;
  fresh-storage-base) fresh_storage_base ;;
  auth-none-inheritance) auth_none_inheritance ;;
  existing-volume) existing_volume ;;
  views-bootstrap) views_bootstrap ;;
  wrong-namespace) wrong_namespace ;;
  app-grants) app_grants ;;
  *)
    echo "unknown case" >&2
    exit 2
    ;;
esac
