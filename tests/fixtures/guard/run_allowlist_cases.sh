#!/usr/bin/env bash
# Isolated decision cases for scripts/guard/allowlist.sh (GS-22 VER-1).
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

run_case() {
  local name=$1 baseline_body=$2 status_body=$3 setup=$4 want=$5
  shift 5
  local work bin out rc
  work=$(mktemp -d)
  trap 'rm -rf "$work"' RETURN
  bin=$work/bin
  mkdir -p "$bin"
  cat >"$bin/libra" <<'EOF'
#!/usr/bin/env bash
if [[ ${1:-} == status && ${2:-} == --short ]]; then
  cat "${GUARD_STATUS_FILE:?}"
  exit 0
fi
echo "unexpected libra invocation: $*" >&2
exit 1
EOF
  chmod +x "$bin/libra"
  (
    cd "$work"
    eval "$setup"
    printf '%s\n' "$baseline_body" >"$work/baseline.txt"
    printf '%s\n' "$status_body" >"$work/status.out"
    export GUARD_STATUS_FILE=$work/status.out
    set +e
    PATH=$bin:$PATH bash "$ALLOWLIST" "$work/baseline.txt" "$@"
    rc=$?
    set -e
    printf '%s' "$rc" >"$work/rc"
  )
  rc=$(cat "$work/rc")
  assert_exit "$name" "$rc" "$want"
}

plain=$'hello baseline\n'
hash=$(printf '%s' "$plain" | sha256sum)
hash=${hash%% *}

run_case ok_allowed \
  "file|644|${hash}  allowed.txt" \
  ' M allowed.txt' \
  "printf '%s' 'changed' >allowed.txt && chmod 644 allowed.txt" \
  0 \
  allowed.txt

run_case dirty_modified \
  "file|644|${hash}  dirty.txt" \
  ' M dirty.txt' \
  "printf '%s' 'changed' >dirty.txt && chmod 644 dirty.txt" \
  1 \
  allowed.txt

run_case restored \
  "file|644|${hash}  dirty.txt" \
  ' M dirty.txt' \
  "printf '%s' '$plain' >dirty.txt && chmod 644 dirty.txt" \
  0 \
  allowed.txt

run_case new_undeclared \
  "file|644|${hash}  allowed.txt" \
  $' M allowed.txt\n?? extra.txt' \
  "printf '%s' '$plain' >allowed.txt && chmod 644 allowed.txt && printf '%s' '$plain' >extra.txt && chmod 644 extra.txt" \
  1 \
  allowed.txt

run_case deleted_from_status \
  "file|644|${hash}  gone.txt" \
  '' \
  'true' \
  1 \
  allowed.txt

run_case restored_to_head \
  "file|644|${hash}  dirty.txt" \
  '' \
  "printf '%s' 'head-bytes' >dirty.txt && chmod 644 dirty.txt" \
  1 \
  allowed.txt

run_case rename_src \
  "file|644|${hash}  old.txt" \
  'R  old.txt -> new.txt' \
  "printf '%s' '$plain' >new.txt && chmod 644 new.txt" \
  1 \
  new.txt

run_case rename_dst \
  "file|644|${hash}  old.txt" \
  'R  old.txt -> new.txt' \
  "printf '%s' '$plain' >new.txt && chmod 644 new.txt" \
  1 \
  old.txt

run_case mode_only \
  "file|644|${hash}  a.txt" \
  ' M a.txt' \
  "printf '%s' '$plain' >a.txt && chmod 755 a.txt" \
  1 \
  allowed.txt

run_case to_symlink \
  "file|644|${hash}  a.txt" \
  ' M a.txt' \
  "printf '%s' '$plain' >payload.txt && rm -f a.txt && ln -s payload.txt a.txt" \
  1 \
  allowed.txt

run_case unsupported_dir \
  '' \
  '?? adir' \
  'mkdir adir' \
  1 \
  adir

if [[ $FAIL -ne 0 ]]; then
  echo "run_allowlist_cases: failures" >&2
  exit 1
fi
echo "run_allowlist_cases: ok"
