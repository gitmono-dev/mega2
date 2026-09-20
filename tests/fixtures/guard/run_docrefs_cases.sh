#!/usr/bin/env bash
# Isolated decision cases for scripts/guard/docrefs.sh (GS-19 VER-1).
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
DOCREFS=$REPO_ROOT/scripts/guard/docrefs.sh
FAIL=0

assert_exit() {
  local name=$1 got=$2 want=$3
  if [[ $got -ne $want ]]; then
    echo "FAIL $name: exit $got want $want" >&2
    FAIL=1
  fi
}

run_case() {
  local name=$1 want=$2
  shift 2
  local work rc
  work=$(mktemp -d)
  trap 'rm -rf "$work"' RETURN
  mkdir -p "$work/docs"
  printf '%s\n' 'see [ok](docs/ok.md)' >"$work/a.md"
  printf '%s\n' 'also [ok](docs/ok.md#anchor)' >"$work/b.md"
  printf '%s\n' 'no docs refs here' >"$work/empty.md"
  printf '%s\n' 'missing [gone](docs/gone.md)' >"$work/bad.md"
  printf '%s\n' 'prose `docs/*.md` and [`docs/ok.md`](docs/ok.md)、docs/ok.md' >"$work/prose.md"
  : >"$work/docs/ok.md"
  rc=0
  set +e
  (
    cd "$work"
    bash "$DOCREFS" "$@"
  )
  rc=$?
  set -e
  assert_exit "$name" "$rc" "$want"
}

run_case multi_ok 0 a.md b.md
run_case dangling 1 bad.md
run_case no_match 0 empty.md
run_case prefix_not_polluted 0 a.md b.md
run_case prose_glob_and_cjk 0 prose.md

if [[ $FAIL -ne 0 ]]; then
  echo "run_docrefs_cases: failures" >&2
  exit 1
fi
echo "run_docrefs_cases: ok"
