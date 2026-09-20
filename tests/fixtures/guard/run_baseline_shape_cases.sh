#!/usr/bin/env bash
# Isolated shape cases for scripts/guard/baseline.sh (GS-14 VER-1).
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
BASELINE=$REPO_ROOT/scripts/guard/baseline.sh
FAIL=0

assert_eq() {
  local name=$1 got=$2 want=$3
  if [[ $got != "$want" ]]; then
    echo "FAIL $name: got=$(printf %q "$got") want=$(printf %q "$want")" >&2
    FAIL=1
  fi
}

run_case() {
  local name=$1 status_body=$2 setup=$3 expected=$4
  local work bin out
  work=$(mktemp -d)
  trap 'rm -rf "$work"' RETURN
  bin=$work/bin
  mkdir -p "$bin"
  out=$work/baseline.txt
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
    printf '%s\n' "$status_body" >"$work/status.out"
    export GUARD_STATUS_FILE=$work/status.out
    PATH=$bin:$PATH bash "$BASELINE" "$out"
  )
  local got
  got=$(cat "$out")
  assert_eq "$name" "$got" "$expected"
}

plain=$'hello baseline\n'
hash=$(printf '%s' "$plain" | sha256sum)
hash=${hash%% *}

run_case file \
  ' M plain.txt' \
  "printf '%s' '$plain' >plain.txt && chmod 644 plain.txt" \
  "file|644|${hash}  plain.txt"

run_case symlink \
  '?? link' \
  'ln -s target-name link' \
  'symlink|-|target-name  link'

run_case absent \
  ' D gone.txt' \
  'true' \
  'absent|-|-  gone.txt'

run_case unsupported \
  '?? adir' \
  'mkdir adir' \
  'unsupported|-|-  adir'

run_case fifo \
  '?? pipe.fifo' \
  'mkfifo pipe.fifo' \
  'unsupported|-|-  pipe.fifo'

run_case arrow_literal \
  ' D removed -> name.txt' \
  'true' \
  'absent|-|-  removed -> name.txt'

run_case rename_both \
  'R  old.txt -> new.txt' \
  'true' \
  $'absent|-|-  new.txt\nabsent|-|-  old.txt'

run_case cquote_utf8 \
  '?? "tst-\344\270\255\346\226\207.md"' \
  "printf '%s' '$plain' >'tst-中文.md' && chmod 644 'tst-中文.md'" \
  "file|644|${hash}  tst-中文.md"

run_case cquote_quote \
  '?? "quote\"name.txt"' \
  "printf '%s' '$plain' >\$'quote\"name.txt' && chmod 644 \$'quote\"name.txt'" \
  "file|644|${hash}  quote\"name.txt"

if [[ $FAIL -ne 0 ]]; then
  echo "run_baseline_shape_cases: failures" >&2
  exit 1
fi
echo "run_baseline_shape_cases: ok"
