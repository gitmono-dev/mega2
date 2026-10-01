#!/usr/bin/env bash
# One-off cleanup of the per-test PostgreSQL schemas (`mega2_test_<pid>_<n>`)
# that test runs left in the IT database before src/jupiter/tests.rs dropped
# them itself (docs/development.md, "测试 schema 堆积").
#
# DESTRUCTIVE: a dropped schema is gone with every table in it. Only schemas
# matching the test pattern are considered; `public` is never touched. It is a
# dry run unless --apply is given.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/lib/mega2-it.sh"

usage() {
  cat <<'USAGE'
Drop leftover per-test schemas (mega2_test_<pid>_<n>) from the IT Postgres.

Usage:
  scripts/drop_test_schemas.sh [--apply] [--all] [--limit N] [--vacuum] [--list]

Dry run by default: reports what would be dropped and exits.

Options:
  --apply      Drop the selected schemas. Each one is its own
               `DROP SCHEMA ... CASCADE` statement in its own transaction, so a
               failure (e.g. a lock timeout) skips that schema and continues.
  --all        Include schemas whose <pid> is a live process on this host.
               Those are skipped by default: they may belong to a test run that
               is still going, possibly in another session.
  --limit N    Select at most N schemas, oldest first (by OID).
  --vacuum     After dropping, VACUUM (ANALYZE) the catalogs the drops bloat
               (pg_class, pg_attribute, pg_depend, pg_type, pg_index,
               pg_constraint, pg_statistic).
  --list       Print every candidate with its verdict and, when the schema
               carries one, the name of the test that created it.
  -h, --help   This text.

Environment:
  MEGA2_IT_PROJECT     compose project (default: mega2-it)
  MEGA2_IT_PGUSER      psql user inside the postgres service (default: mega2)
  MEGA2_IT_PGDATABASE  database holding the test schemas (default: mega2)

Example (a backlog of ~27k schemas takes on the order of an hour):
  ./scripts/drop_test_schemas.sh                 # report only
  ./scripts/drop_test_schemas.sh --apply --vacuum
USAGE
}

APPLY=0
ALL=0
LIMIT=0
VACUUM=0
LIST=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --apply) APPLY=1 ;;
    --all) ALL=1 ;;
    --limit)
      [[ $# -ge 2 && "$2" =~ ^[0-9]+$ ]] || mega2_it_die "--limit needs a number"
      LIMIT="$2"
      shift
      ;;
    --vacuum) VACUUM=1 ;;
    --list) LIST=1 ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      mega2_it_die "unknown argument: $1"
      ;;
  esac
  shift
done

mega2_it_require_repo_root
PG_USER="${MEGA2_IT_PGUSER:-mega2}"
PG_DB="${MEGA2_IT_PGDATABASE:-mega2}"

psql_it() {
  mega2_it_compose exec -T postgres psql -U "${PG_USER}" -d "${PG_DB}" "$@"
}

count_schemas() {
  psql_it -qAt -c "SELECT count(*) FROM pg_namespace WHERE nspname ~ '^mega2_test_[0-9]+_[0-9]+\$'"
}

db_size() {
  psql_it -qAt -c "SELECT pg_size_pretty(pg_database_size(current_database()))"
}

# Live pids are read once; a per-schema `ps` would take minutes on a backlog.
LIVE_PIDS=" $(ps -axo pid= | tr -s ' \n' '  ') "
pid_is_alive() {
  [[ "${LIVE_PIDS}" == *" $1 "* ]]
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/mega2-drop-test-schemas.XXXXXX")"
trap 'rm -rf "${WORK}"' EXIT
CANDIDATES="${WORK}/candidates"
SELECTED="${WORK}/selected"
: > "${SELECTED}"

mega2_it_info "postgres service of project ${MEGA2_IT_PROJECT}, database ${PG_DB}"
psql_it -qAt -F $'\t' -c "SELECT nspname, coalesce(obj_description(oid, 'pg_namespace'), '') FROM pg_namespace WHERE nspname ~ '^mega2_test_[0-9]+_[0-9]+\$' ORDER BY oid" > "${CANDIDATES}"

total=0
skipped_live=0
selected=0
while IFS=$'\t' read -r name owner; do
  [[ -n "${name}" ]] || continue
  total=$((total + 1))
  owner="${owner:+  (${owner})}"
  pid="${name#mega2_test_}"
  pid="${pid%%_*}"
  if [[ "${ALL}" -eq 0 ]] && pid_is_alive "${pid}"; then
    skipped_live=$((skipped_live + 1))
    [[ "${LIST}" -eq 1 ]] && echo "skip  ${name}  (pid ${pid} is alive)${owner}"
    continue
  fi
  if [[ "${LIMIT}" -gt 0 && "${selected}" -ge "${LIMIT}" ]]; then
    [[ "${LIST}" -eq 1 ]] && echo "keep  ${name}  (--limit ${LIMIT} reached)${owner}"
    continue
  fi
  selected=$((selected + 1))
  printf '%s\n' "${name}" >> "${SELECTED}"
  [[ "${LIST}" -eq 1 ]] && echo "drop  ${name}${owner}"
done < "${CANDIDATES}"

echo "test schemas: ${total} total, ${skipped_live} skipped (live pid), ${selected} selected; database size $(db_size)"

if [[ "${selected}" -eq 0 ]]; then
  echo "nothing to drop"
  exit 0
fi
if [[ "${APPLY}" -eq 0 ]]; then
  echo "dry run: re-run with --apply to drop the ${selected} selected schema(s) (use --list to see them)"
  exit 0
fi

# One statement per schema, autocommitted, so a failure (reported by psql and
# ignored) costs that schema only. Identifiers are quoted; the pattern above
# guarantees they need no escaping.
SQL="${WORK}/drop.sql"
{
  echo "SET lock_timeout = '60s';"
  n=0
  while IFS= read -r name; do
    n=$((n + 1))
    echo "DROP SCHEMA IF EXISTS \"${name}\" CASCADE;"
    if (( n % 100 == 0 )); then
      printf '\\echo dropped %s/%s\n' "${n}" "${selected}"
    fi
  done < "${SELECTED}"
  printf '\\echo dropped %s/%s\n' "${n}" "${selected}"
} > "${SQL}"

mega2_it_info "dropping ${selected} schema(s)"
psql_it -q -v ON_ERROR_STOP=0 -f - < "${SQL}"

if [[ "${VACUUM}" -eq 1 ]]; then
  mega2_it_info "vacuuming system catalogs"
  psql_it -q -c "VACUUM (ANALYZE) pg_catalog.pg_class, pg_catalog.pg_attribute, pg_catalog.pg_depend, pg_catalog.pg_type, pg_catalog.pg_index, pg_catalog.pg_constraint, pg_catalog.pg_statistic"
fi

echo "test schemas remaining: $(count_schemas); database size $(db_size)"
