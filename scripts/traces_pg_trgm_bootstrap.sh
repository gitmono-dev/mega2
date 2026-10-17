#!/usr/bin/env bash
# Operator bootstrap for the reviewed pg_trgm schema. Not a mega2 user command.
# Reads MEGA2_TRACES_PG_ADMIN_URL and MEGA2_TRACES_PG_APP_ROLE from the environment.
set -euo pipefail

MODE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --check|--apply)
      if [[ -n "$MODE" ]]; then
        echo "conflicting mode" >&2
        exit 2
      fi
      MODE="${1#--}"
      shift
      ;;
    *)
      echo "unknown argument" >&2
      exit 2
      ;;
  esac
done

if [[ -z "$MODE" ]]; then
  echo "missing --check or --apply" >&2
  exit 2
fi
if [[ -z "${MEGA2_TRACES_PG_ADMIN_URL:-}" || -z "${MEGA2_TRACES_PG_APP_ROLE:-}" ]]; then
  echo "missing_admin_or_role" >&2
  exit 1
fi

python3 - "$MODE" <<'PY'
import os
import re
import subprocess
import sys
from urllib.parse import unquote, urlparse

mode = sys.argv[1]
role = os.environ.get("MEGA2_TRACES_PG_APP_ROLE", "")
if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,62}", role):
    sys.exit("app role identifier is not safe")
raw = os.environ.get("MEGA2_TRACES_PG_ADMIN_URL", "")
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
quoted_role = '"' + role.replace('"', '""') + '"'

def psql(sql: str) -> str:
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
    if proc.returncode != 0:
        sys.stderr.write("catalog query failed\n")
        sys.exit(proc.returncode)
    return proc.stdout.strip()

catalog_sql = (
    "SELECT e.extname, e.extversion, n.nspname, r.rolname "
    "FROM pg_catalog.pg_extension e "
    "JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace "
    "JOIN pg_catalog.pg_roles r ON r.oid = n.nspowner "
    "WHERE e.extname = 'pg_trgm'"
)
before = psql(catalog_sql)

def reject(reason: str) -> None:
    sys.stderr.write(reason + "\n")
    sys.exit(1)

if before:
    extname, extversion, namespace, owner = before.split("|")
    if namespace != "mega2_extensions" or owner != "mega2_traces_ext_owner":
        reject("wrong_namespace")
    if extversion != "1.6":
        reject("wrong_version")
elif mode == "check":
    reject("extension_missing")

if mode == "apply":
    if not before:
        psql(
            "DO $$ BEGIN "
            "IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'mega2_traces_ext_owner') THEN "
            "CREATE ROLE mega2_traces_ext_owner NOLOGIN; "
            "END IF; END $$"
        )
        psql("CREATE SCHEMA IF NOT EXISTS mega2_extensions AUTHORIZATION mega2_traces_ext_owner")
        psql("REVOKE CREATE ON SCHEMA mega2_extensions FROM PUBLIC")
        psql("CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA mega2_extensions VERSION '1.6'")
    psql(f"GRANT USAGE ON SCHEMA mega2_extensions TO {quoted_role}")
    psql(f"GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA mega2_extensions TO {quoted_role}")

after = psql(catalog_sql)
if after != "pg_trgm|1.6|mega2_extensions|mega2_traces_ext_owner":
    if before and after != before:
        reject("catalog_changed")
    reject("catalog_mismatch")
if mode == "apply":
    usage = psql(f"SELECT has_schema_privilege({role!r}, 'mega2_extensions', 'USAGE')")
    create = psql(f"SELECT has_schema_privilege({role!r}, 'mega2_extensions', 'CREATE')")
    execute = psql(
        "SELECT has_function_privilege("
        f"{role!r}, 'mega2_extensions.show_trgm(text)', 'EXECUTE')"
    )
    owner_is_app = psql(
        "SELECT r.rolname = "
        f"{role!r} FROM pg_catalog.pg_namespace n "
        "JOIN pg_catalog.pg_roles r ON r.oid = n.nspowner "
        "WHERE n.nspname = 'mega2_extensions'"
    )
    if usage != "t" or execute != "t" or create == "t" or owner_is_app == "t":
        reject("app_grant_mismatch")
    print(f"app_role={role}")
    print("usage=true")
    print("execute=true")
    print("app_is_owner=false")
print("extname=pg_trgm")
print("extversion=1.6")
print("expected_extversion=1.6")
print("namespace=mega2_extensions")
print("owner=mega2_traces_ext_owner")
PY
