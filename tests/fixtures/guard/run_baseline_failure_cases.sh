#!/usr/bin/env bash
# Isolated failure cases for scripts/guard/baseline.sh (GS-14 VER-2).
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
BASELINE=$REPO_ROOT/scripts/guard/baseline.sh
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
  local name=$1 setup=$2 status_body=$3
  local work bin out rc
  work=$(mktemp -d)
  trap 'rm -rf "$work"' RETURN
  bin=$work/bin
  mkdir -p "$bin"
  out=$work/baseline.txt
  (
    cd "$bin"
    eval "$setup"
  )
  printf '%s\n' "$status_body" >"$work/status.out"
  local rc=0
  set +e
  (
    cd "$work"
    echo payload >plain.txt
    chmod 644 plain.txt
    ln -s target-name link
    export GUARD_STATUS_FILE=$work/status.out
    PATH=$bin:$PATH bash "$BASELINE" "$out"
  )
  rc=$?
  set -e
  assert_exit "$name" "$rc" 2
}

run_fail libra_status \
  'failing_tool libra' \
  ' M plain.txt'

run_fail sha256sum \
  'passthrough_libra; failing_tool sha256sum' \
  ' M plain.txt'

run_fail stat \
  'passthrough_libra; failing_tool stat' \
  ' M plain.txt'

run_fail readlink \
  'passthrough_libra; failing_tool readlink' \
  '?? link'

run_fail tab_path \
  'passthrough_libra' \
  $' D "a\\tb.txt"'

run_fail nl_path \
  'passthrough_libra' \
  $' D "a\\nb.txt"'

if [[ $FAIL -ne 0 ]]; then
  echo "run_baseline_failure_cases: failures" >&2
  exit 1
fi
echo "run_baseline_failure_cases: ok"
