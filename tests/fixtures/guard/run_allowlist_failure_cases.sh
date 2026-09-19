#!/usr/bin/env bash
# Isolated failure cases for scripts/guard/allowlist.sh (GS-22 VER-2).
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
ALLOWLIST=$REPO_ROOT/scripts/guard/allowlist.sh
FAIL=0

assert_exit() {
  local name=$1 got=$2 want=$3
  if [[ $got -ne $want ]]; then
    echo "FAIL $name: exit $got want $want" >&2
    FAIL=1
  fi
}

failing_tool() {
  local name=$1
  cat >"$name" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
  chmod +x "$name"
}

passthrough_libra() {
  cat >libra <<'EOF'
#!/usr/bin/env bash
if [[ ${1:-} == status && ${2:-} == --short ]]; then
  cat "${GUARD_STATUS_FILE:?}"
  exit 0
fi
echo "unexpected libra invocation: $*" >&2
exit 1
EOF
  chmod +x libra
}

run_fail() {
  local name=$1 setup=$2 status_body=$3 baseline_body=$4
  local work bin rc
  work=$(mktemp -d)
  trap 'rm -rf "$work"' RETURN
  bin=$work/bin
  mkdir -p "$bin"
  (
    cd "$bin"
    eval "$setup"
  )
  printf '%s\n' "$status_body" >"$work/status.out"
  printf '%s\n' "$baseline_body" >"$work/baseline.txt"
  rc=0
  set +e
  (
    cd "$work"
    echo payload >plain.txt
    chmod 644 plain.txt
    ln -s target-name link
    mkdir adir
    export GUARD_STATUS_FILE=$work/status.out
    PATH=$bin:$PATH bash "$ALLOWLIST" "$work/baseline.txt" allowed.txt
  )
  rc=$?
  set -e
  assert_exit "$name" "$rc" 2
}

plain=$'payload\n'
hash=$(printf '%s' "$plain" | sha256sum)
hash=${hash%% *}
base_line="file|644|${hash}  plain.txt"

run_fail libra_status \
  'failing_tool libra' \
  ' M plain.txt' \
  "$base_line"

run_fail sha256sum \
  'passthrough_libra; failing_tool sha256sum' \
  ' M plain.txt' \
  "$base_line"

run_fail stat \
  'passthrough_libra; failing_tool stat' \
  ' M plain.txt' \
  "$base_line"

run_fail readlink \
  'passthrough_libra; failing_tool readlink' \
  '?? link' \
  'symlink|-|target-name  link'

run_fail grep \
  'passthrough_libra; failing_tool grep' \
  ' M plain.txt' \
  "$base_line"

run_fail sort \
  'passthrough_libra; failing_tool sort' \
  ' M plain.txt' \
  "$base_line"

run_fail ambiguous_symlink \
  'passthrough_libra' \
  '' \
  'symlink|-|a  b  c'

work=$(mktemp -d)
set +e
bash "$ALLOWLIST" "$work/missing.baseline" allowed.txt
rc=$?
set -e
assert_exit missing_baseline "$rc" 2

set +e
bash "$ALLOWLIST" "$work/missing.baseline"
rc=$?
set -e
assert_exit empty_allowlist_usage "$rc" 2

: >"$work/empty.baseline"
set +e
bash "$ALLOWLIST" "$work/empty.baseline"
rc=$?
set -e
assert_exit empty_allowlist "$rc" 2
rm -rf "$work"

if [[ $FAIL -ne 0 ]]; then
  echo "run_allowlist_failure_cases: failures" >&2
  exit 1
fi
echo "run_allowlist_failure_cases: ok"
