#!/usr/bin/env bash
# Self-test for scripts/lib/smoke-runner.sh: exercises every exit-code path
# with stub cases and prints one `selftest ok:` line per assertion.
set -euo pipefail
LIB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/smoke-runner.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

stub() {
    cat > "$tmp/stub.sh" <<STUB
#!/usr/bin/env bash
source "$LIB"
smoke_init "selftest"
ok() { true; }
bad() { false; }
$1
finish
STUB
}

expect_rc() {
    local want="$1" label="$2" rc=0
    shift 2
    "$@" > "$tmp/out.txt" 2>&1 || rc=$?
    if [ "$rc" -ne "$want" ]; then
        echo "selftest FAIL: $label (exit $rc, want $want)" >&2
        cat "$tmp/out.txt" >&2
        exit 1
    fi
}

last_line_is() {
    [ "$(tail -n 1 "$tmp/out.txt")" = "$1" ] || {
        echo "selftest FAIL: last line is '$(tail -n 1 "$tmp/out.txt")', want '$1'" >&2
        exit 1
    }
}

stub 'run_case "a" ok; run_case "b" ok; skip_case "s" "not here"'
expect_rc 0 "all pass" env -u MEGA2_SMOKE_CASE bash "$tmp/stub.sh"
last_line_is 'selftest smoke storage_only summary: 2 passed, 0 failed (1 skipped)'
grep -A1 -x '==> s' "$tmp/out.txt" | grep -qx 'SKIP: s (not here)'
echo "selftest ok: all PASS exits 0"

stub 'failing_then_ok() { false; true; }; run_case "a" ok; run_case "b" failing_then_ok'
expect_rc 1 "one fail" env -u MEGA2_SMOKE_CASE bash "$tmp/stub.sh"
grep -qx 'FAIL: b' "$tmp/out.txt"
echo "selftest ok: any FAIL exits 1"

stub 'run_case "a" ok'
expect_rc 2 "unmatched" env MEGA2_SMOKE_CASE="nope" bash "$tmp/stub.sh"
last_line_is 'selftest smoke storage_only summary: 0 passed, 0 failed (0 skipped)'
echo "selftest ok: unmatched MEGA2_SMOKE_CASE exits 2"

stub 'optin_case "o" SELFTEST_SWITCH ok; skip_case "s" "not here"'
expect_rc 2 "opt-in named without switch" env -u SELFTEST_SWITCH MEGA2_SMOKE_CASE="o" bash "$tmp/stub.sh"
last_line_is 'selftest smoke storage_only summary: 0 passed, 0 failed (0 skipped)'
expect_rc 2 "skip_case named" env MEGA2_SMOKE_CASE="s" bash "$tmp/stub.sh"
last_line_is 'selftest smoke storage_only summary: 0 passed, 0 failed (0 skipped)'
echo "selftest ok: named opt-in without switch exits 2"

stub 'run_case "a" ok; run_case "b" bad; optin_case "o" SELFTEST_SWITCH ok; skip_case "s" "not here"'
expect_rc 0 "filtered summary" env -u SELFTEST_SWITCH MEGA2_SMOKE_CASE="a" bash "$tmp/stub.sh"
last_line_is 'selftest smoke storage_only summary: 1 passed, 0 failed (0 skipped)'
echo "selftest ok: summary counts only executed cases"

# shellcheck source=scripts/lib/smoke-runner.sh disable=SC1091
out=$(printf 'url http://x:%s@h/r?sig=abc\n' 'a+b.c' | (source "$LIB"; redact 'a+b.c'))
[ "$out" = 'url http://x:***@h/r?***' ]
echo "selftest ok: redact replaces the token and strips URL queries"
