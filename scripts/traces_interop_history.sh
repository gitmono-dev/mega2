#!/usr/bin/env bash
# Epoch history retention interop driver (MTR-68).
#
# Run through the dispatcher:
#   MEGA2_TRACES_INTEROP=1 scripts/traces_interop_smoke.sh --case history
#
# Required fixture:
#   LIBRA_REPO  the pinned Libra checkout with .env.test and .env.live-test
#
# The four LTR-22/23 selectors are executed one by one and counted
# independently; a selector that does not run is a FAIL, never a skip. The
# live fixture itself (URLs, token files, SSH keys, targets, server id and the
# pinned binary/contract hashes) is described by those env files.
#
# Output contract: ==> <case> / PASS: <case> / FAIL: <case> and a final
# `traces interop history summary: executed=<n> passed=<n> failed=<n>` line.
set -euo pipefail

failures=0
executed=0
passed=0

fail_fixture() {
  echo "FAIL: fixture $1"
}

if [ -z "${LIBRA_REPO:-}" ] || [ ! -d "${LIBRA_REPO}" ]; then
  fail_fixture "LIBRA_REPO must name the pinned Libra checkout"
  exit 1
fi
if [ ! -r "${LIBRA_REPO}/.env.test" ] || [ ! -r "${LIBRA_REPO}/.env.live-test" ]; then
  fail_fixture "LIBRA_REPO must carry .env.test and .env.live-test"
  exit 1
fi

OWN_WORKDIR="${MEGA2_TRACES_INTEROP_WORKDIR:-}"
if [ -z "$OWN_WORKDIR" ]; then
  OWN_WORKDIR="$(mktemp -d)"
  OWNED_WORKDIR=1
else
  mkdir -p "$OWN_WORKDIR"
  OWNED_WORKDIR=0
fi
echo "traces interop workdir: $OWN_WORKDIR"
cleanup() {
  if [ "$OWNED_WORKDIR" -eq 1 ] && [ "${MEGA2_TRACES_INTEROP_KEEP_WORKDIR:-}" != "1" ]; then
    rm -rf "$OWN_WORKDIR"
  else
    echo "traces interop workdir kept at: $OWN_WORKDIR"
  fi
}
trap cleanup EXIT

run_selector() {
  local name="$1"
  echo "==> $name"
  executed=$((executed + 1))
  if (
    cd "$LIBRA_REPO" &&
      set -a &&
      . ./.env.test &&
      . ./.env.live-test &&
      set +a &&
      RUST_MIN_STACK=33554432 cargo nextest run --run-ignored only \
        --test command_test --no-tests=fail \
        -E "test(=command::mega2_traces_live_test::${name})"
  ) >"$OWN_WORKDIR/${name}.log" 2>&1; then
    passed=$((passed + 1))
    echo "PASS: $name"
  else
    failures=$((failures + 1))
    echo "FAIL: $name"
  fi
}

run_selector live_epoch_history_retained
run_selector live_clean_rewrite_denied
run_selector live_new_stream_epoch_created
run_selector live_old_stream_bytes_readable

echo "traces interop history summary: executed=$executed passed=$passed failed=$failures"
[ "$failures" -eq 0 ] && [ "$executed" -gt 0 ]
