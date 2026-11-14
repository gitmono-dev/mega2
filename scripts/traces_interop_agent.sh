#!/usr/bin/env bash
# Agent read and scope-isolation interop driver (MTR-60).
#
# Run through the dispatcher:
#   MEGA2_TRACES_INTEROP=1 scripts/traces_interop_smoke.sh --case agent
#
# Required fixture:
#   MEGA2_TRACES_INTEROP_HTTP_URL    mega2 origin, e.g. http://127.0.0.1:18000
#   MEGA2_TRACES_INTEROP_TARGET      canonical project path whose stream has
#                                    checkpoints with redacted transcript text
#   MEGA2_TRACES_INTEROP_TOKEN_FILE  HTTP token of the stream owner
#   MEGA2_TRACES_INTEROP_SEARCH_STREAM  stream UUID that contains the query text
#   MEGA2_TRACES_INTEROP_QUERY       literal text that must produce a hit
#   LIBRA_REPO                       the pinned Libra checkout (with .env.test
#                                    and .env.live-test ready)
#
# Optional: LIBRA_BIN, GIT_BIN, MEGA2_TRACES_INTEROP_WORKDIR,
# MEGA2_TRACES_INTEROP_KEEP_WORKDIR (same semantics as the HTTP driver).
#
# Cases: agent_search (AC-1), agent_transcript_budget (AC-2),
# agent_evidence_reread (AC-3), agent_code_clone_excludes_traces (AC-4),
# agent_peer_principal_write_denied (AC-5),
# agent_other_target_ref_isolation (AC-6), agent_other_source_ref_isolation
# (AC-7). Each LTR-21 selector is executed and counted independently; a
# selector that does not run is a FAIL, never a skip.
set -euo pipefail

LIBRA_BIN="${LIBRA_BIN:-libra}"
GIT_BIN="${GIT_BIN:-git}"
failures=0
executed=0
passed=0

fail_fixture() {
  echo "FAIL: fixture $1"
}

for var in MEGA2_TRACES_INTEROP_HTTP_URL \
           MEGA2_TRACES_INTEROP_TARGET \
           MEGA2_TRACES_INTEROP_TOKEN_FILE \
           MEGA2_TRACES_INTEROP_SEARCH_STREAM \
           MEGA2_TRACES_INTEROP_QUERY \
           LIBRA_REPO; do
  if [ -z "${!var:-}" ]; then
    fail_fixture "$var is required"
    exit 1
  fi
done
[ -r "$MEGA2_TRACES_INTEROP_TOKEN_FILE" ] || {
  fail_fixture "MEGA2_TRACES_INTEROP_TOKEN_FILE must be readable"
  exit 1
}
[ -d "$LIBRA_REPO" ] || {
  fail_fixture "LIBRA_REPO must name the pinned Libra checkout"
  exit 1
}
command -v "$LIBRA_BIN" >/dev/null 2>&1 || {
  fail_fixture "Libra binary '$LIBRA_BIN' is not runnable"
  exit 1
}
command -v "$GIT_BIN" >/dev/null 2>&1 || {
  fail_fixture "Git binary '$GIT_BIN' is not runnable"
  exit 1
}
command -v python3 >/dev/null 2>&1 || {
  fail_fixture "python3 is required to parse JSON responses"
  exit 1
}

BASE_URL="${MEGA2_TRACES_INTEROP_HTTP_URL%/}"
TOKEN="$(cat "$MEGA2_TRACES_INTEROP_TOKEN_FILE")"
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

run_case() {
  local name="$1"
  shift
  echo "==> $name"
  executed=$((executed + 1))
  if "$@"; then
    passed=$((passed + 1))
    echo "PASS: $name"
  else
    failures=$((failures + 1))
    echo "FAIL: $name"
  fi
}

api_get() {
  curl -sS --fail-with-body -H "Authorization: Bearer ${TOKEN}" "$1"
}

# AC-1: the machine search returns at least one hit with one representative
# piece of evidence. The pinned Libra CLI still calls the pre-MTR-20
# `/api/v1/traces/search` route and is registered as an H4 client gap; this
# case exercises the mounted server surface.
case_search() {
  local encoded
  encoded="$(python3 -c 'import sys,urllib.parse;print(urllib.parse.quote(sys.argv[1], safe=""))' "$MEGA2_TRACES_INTEROP_QUERY")"
  api_get "$BASE_URL/api/v1/traces/streams/$MEGA2_TRACES_INTEROP_SEARCH_STREAM/search?q=${encoded}" \
    >"$OWN_WORKDIR/search.json" || return 1
  python3 - "$OWN_WORKDIR/search.json" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1], encoding="utf-8"))
items = data.get("items") or []
assert items, "search returned no items"
hit = items[0]
assert hit.get("checkpoint_id"), "hit has no checkpoint_id"
assert int(hit.get("occurrence_count") or 0) >= 1, "hit has no occurrence count"
PY
}

checkpoint_id() {
  python3 - "$OWN_WORKDIR/search.json" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1], encoding="utf-8"))
print(data["items"][0]["checkpoint_id"])
PY
}

transcript_url() {
  printf '%s/api/v1/traces/streams/%s/sessions/%s/checkpoints/%s/transcript' \
    "$BASE_URL" "$MEGA2_TRACES_INTEROP_SEARCH_STREAM" "$1" "$2"
}

checkpoint_session() {
  python3 - "$OWN_WORKDIR/search.json" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1], encoding="utf-8"))
print(data["items"][0]["session_id"])
PY
}

# AC-2: the bounded read continues from its cursor and both slices together
# reproduce the unbounded text.
case_transcript_budget() {
  local session checkpoint url first_cursor full joined
  session="$(checkpoint_session)"
  checkpoint="$(checkpoint_id)"
  url="$(transcript_url "$session" "$checkpoint")"
  api_get "${url}?max_bytes=16" >"$OWN_WORKDIR/first.json" || return 1
  first_cursor="$(python3 - "$OWN_WORKDIR/first.json" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1], encoding="utf-8"))
print(data.get("next_cursor") or "")
PY
)"
  api_get "${url}?max_bytes=131072" >"$OWN_WORKDIR/full.json" || return 1
  if [ -z "$first_cursor" ]; then
    # A shorter transcript needs no continuation; the full read must match.
    python3 - "$OWN_WORKDIR/first.json" "$OWN_WORKDIR/full.json" <<'PY'
import json
import sys

first = json.load(open(sys.argv[1], encoding="utf-8"))
full = json.load(open(sys.argv[2], encoding="utf-8"))
assert first["text"] == full["text"], "bounded read differs from the full read"
PY
    return
  fi
  api_get "${url}?max_bytes=131072&cursor=${first_cursor}" >"$OWN_WORKDIR/rest.json" || return 1
  python3 - "$OWN_WORKDIR/first.json" "$OWN_WORKDIR/rest.json" "$OWN_WORKDIR/full.json" <<'PY'
import json
import sys

first = json.load(open(sys.argv[1], encoding="utf-8"))
rest = json.load(open(sys.argv[2], encoding="utf-8"))
full = json.load(open(sys.argv[3], encoding="utf-8"))
joined = first["text"] + rest["text"]
assert joined == full["text"], "continuation does not reproduce the full transcript"
assert full["truncated"] is False, "the full read is still truncated"
PY
}

# AC-3: the representative evidence from the search hit is re-readable.
case_evidence_reread() {
  local session checkpoint url
  session="$(checkpoint_session)"
  checkpoint="$(checkpoint_id)"
  url="$(transcript_url "$session" "$checkpoint")"
  api_get "${url}?max_bytes=131072" >"$OWN_WORKDIR/evidence.json" || return 1
  python3 - "$OWN_WORKDIR/evidence.json" "$MEGA2_TRACES_INTEROP_QUERY" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1], encoding="utf-8"))
query = sys.argv[2].lower()
text = data["text"].lower()
assert query in text, f"evidence text does not contain {sys.argv[2]!r}"
assert data["content_digest"], "evidence has no content digest"
PY
}

# AC-4: an ordinary code clone never carries the traces-only transcript text.
case_code_clone() {
  "$GIT_BIN" clone --quiet "$BASE_URL$MEGA2_TRACES_INTEROP_TARGET.git" \
    "$OWN_WORKDIR/code-clone" >/dev/null 2>&1 || return 1
  if grep -rIl --exclude-dir=.git -- "$MEGA2_TRACES_INTEROP_QUERY" "$OWN_WORKDIR/code-clone" >/dev/null 2>&1; then
    echo "ordinary clone contains the traces-only sentinel" >&2
    return 1
  fi
  return 0
}

# AC-5..AC-7: the three LTR-21 selectors, each executed and counted on its own.
run_live_selector() {
  local name="$1"
  (
    cd "$LIBRA_REPO" &&
      set -a &&
      . ./.env.test &&
      . ./.env.live-test &&
      set +a &&
      RUST_MIN_STACK=33554432 cargo nextest run --run-ignored only \
        --test command_test --no-tests=fail \
        -E "test(=command::mega2_traces_live_test::${name})"
  ) >"$OWN_WORKDIR/${name}.log" 2>&1
}

case_peer_denied() {
  run_live_selector live_peer_principal_write_denied
}

case_other_target() {
  run_live_selector live_other_target_ref_isolation
}

case_other_source() {
  run_live_selector live_other_source_ref_isolation
}

run_case agent_search case_search
run_case agent_transcript_budget case_transcript_budget
run_case agent_evidence_reread case_evidence_reread
run_case agent_code_clone_excludes_traces case_code_clone
run_case agent_peer_principal_write_denied case_peer_denied
run_case agent_other_target_ref_isolation case_other_target
run_case agent_other_source_ref_isolation case_other_source

echo "traces interop agent summary: executed=$executed passed=$passed failed=$failures"
[ "$failures" -eq 0 ] && [ "$executed" -gt 0 ]
