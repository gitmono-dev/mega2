#!/usr/bin/env bash
# Isolated failure cases for scripts/guard/docrefs.sh (GS-19 VER-2).
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

run_fail() {
  local name=$1 setup=$2
  local work bin rc
  work=$(mktemp -d)
  trap 'rm -rf "$work"' RETURN
  bin=$work/bin
  mkdir -p "$bin" "$work/docs"
  printf '%s\n' 'see [ok](docs/ok.md)' >"$work/a.md"
  : >"$work/docs/ok.md"
  (
    cd "$bin"
    eval "$setup"
  )
  rc=0
  set +e
  (
    cd "$work"
    PATH=$bin:$PATH bash "$DOCREFS" a.md
  )
  rc=$?
  set -e
  assert_exit "$name" "$rc" 2
}

run_fail rg_error \
  'printf "%s\n" "#!/usr/bin/env bash" "exit 2" >rg && chmod +x rg'

run_fail sort_error \
  'cat >sort <<EOF
#!/usr/bin/env bash
exit 1
EOF
chmod +x sort'

work=$(mktemp -d)
set +e
bash "$DOCREFS" "$work/missing.md"
rc=$?
set -e
assert_exit missing_input "$rc" 2

set +e
bash "$DOCREFS"
rc=$?
set -e
assert_exit no_args "$rc" 2
rm -rf "$work"

if [[ $FAIL -ne 0 ]]; then
  echo "run_docrefs_failure_cases: failures" >&2
  exit 1
fi
echo "run_docrefs_failure_cases: ok"
