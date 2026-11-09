#!/usr/bin/env bash
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXPECTED_LIBRA_VERSION="0.30.160"
EXPECTED_LIBRA_SOURCE_COMMIT="c5ee43e997670e87f89f13b373dd9d18fdeaeb8d"
CLI_TIMEOUT_SECONDS=90
CASE_EXECUTED=0
CASE_PASSED=0
CASE_FAILED=0
WORK_ROOT=""
TEMP_ROOT="${TMPDIR:-/tmp}"
ACTIVE_PID=""

umask 077

print_summary() {
    echo "traces interop http summary: executed=$CASE_EXECUTED passed=$CASE_PASSED failed=$CASE_FAILED skipped=0"
}

cleanup() {
    local exit_code=$?
    trap - EXIT HUP INT TERM
    if [ -n "$ACTIVE_PID" ]; then
        kill "$ACTIVE_PID" 2>/dev/null || true
        wait "$ACTIVE_PID" 2>/dev/null || true
    fi
    if [ -n "$WORK_ROOT" ]; then
        case "$WORK_ROOT" in
            "$TEMP_ROOT"/mega2-traces-interop.*)
                if [ -d "$WORK_ROOT" ]; then
                    rm -r "$WORK_ROOT" || exit_code=1
                fi
                ;;
            *)
                echo "FAIL: refusing to remove an unowned temporary path" >&2
                exit_code=1
                ;;
        esac
    fi
    exit "$exit_code"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if ! command -v bash >/dev/null 2>&1 \
    || ! command -v curl >/dev/null 2>&1 \
    || ! command -v git >/dev/null 2>&1 \
    || ! command -v jq >/dev/null 2>&1 \
    || ! command -v perl >/dev/null 2>&1 \
    || ! command -v shasum >/dev/null 2>&1 \
    || ! command -v sleep >/dev/null 2>&1 \
    || ! command -v awk >/dev/null 2>&1 \
    || ! command -v grep >/dev/null 2>&1 \
    || ! command -v tr >/dev/null 2>&1 \
    || ! command -v mktemp >/dev/null 2>&1 \
    || ! command -v chmod >/dev/null 2>&1 \
    || ! command -v rm >/dev/null 2>&1 \
    || ! command -v cat >/dev/null 2>&1 \
    || ! command -v date >/dev/null 2>&1; then
    echo "FAIL: a required local interop tool is unavailable" >&2
    print_summary
    exit 1
fi

TEMP_ROOT="${TEMP_ROOT%/}"
[ -n "$TEMP_ROOT" ] || TEMP_ROOT="/"
WORK_ROOT="$(mktemp -d "$TEMP_ROOT/mega2-traces-interop.XXXXXX")" || {
    echo "FAIL: could not create the guarded temporary workspace" >&2
    print_summary
    exit 1
}
chmod 700 "$WORK_ROOT"

require_fixture_value() {
    local name="$1"
    local value="${!name:-}"
    if [ -z "$value" ]; then
        echo "FAIL: live fixture is missing $name" >&2
        print_summary
        exit 1
    fi
}

for name in \
    LIBRA_TRACES_LIVE_ENABLED \
    LIBRA_TRACES_LIVE_HTTP_URL \
    LIBRA_TRACES_LIVE_TOKEN_FILE \
    LIBRA_TRACES_LIVE_TARGET \
    LIBRA_TRACES_LIVE_SERVER_ID \
    LIBRA_TRACES_LIVE_CLIENT_BINARY_SHA256 \
    LIBRA_TRACES_LIVE_CONTRACT_SHA256; do
    require_fixture_value "$name"
done

if [ "$LIBRA_TRACES_LIVE_ENABLED" != "1" ]; then
    echo "FAIL: live fixture is disabled" >&2
    print_summary
    exit 1
fi

if [ ! -f "$LIBRA_TRACES_LIVE_TOKEN_FILE" ] || [ ! -s "$LIBRA_TRACES_LIVE_TOKEN_FILE" ]; then
    echo "FAIL: primary HTTP token file is missing or empty" >&2
    print_summary
    exit 1
fi

if [[ ! "$LIBRA_TRACES_LIVE_HTTP_URL" =~ ^https://([A-Za-z0-9.-]+|\[[A-Fa-f0-9:]+\])(:[0-9]{1,5})?$ ]] \
    && [[ ! "$LIBRA_TRACES_LIVE_HTTP_URL" =~ ^http://(127\.0\.0\.1|localhost|\[::1\])(:[0-9]{1,5})?$ ]]; then
    echo "FAIL: HTTP fixture must use TLS or a loopback authority without credentials or path" >&2
    print_summary
    exit 1
fi

if [[ ! "$LIBRA_TRACES_LIVE_TARGET" =~ ^/[A-Za-z0-9._~-]+(/[A-Za-z0-9._~-]+)*$ ]] \
    || [[ "$LIBRA_TRACES_LIVE_TARGET" == *"/../"* ]] \
    || [[ "$LIBRA_TRACES_LIVE_TARGET" == *"/./"* ]] \
    || [[ "$LIBRA_TRACES_LIVE_TARGET" == */.. ]] \
    || [[ "$LIBRA_TRACES_LIVE_TARGET" == */. ]]; then
    echo "FAIL: fixture target must be a canonical project path" >&2
    print_summary
    exit 1
fi

if [[ ! "$LIBRA_TRACES_LIVE_SERVER_ID" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$ ]]; then
    echo "FAIL: fixture server id is not a UUID" >&2
    print_summary
    exit 1
fi

libra_bin="${MEGA2_TRACES_LIBRA_BIN:-${LIBRA_TRACES_LIVE_INSTALLED_BINARY:-}}"
if [ -z "$libra_bin" ]; then
    libra_bin="$(command -v libra 2>/dev/null || true)"
fi
if [ -z "$libra_bin" ] || [ ! -f "$libra_bin" ] || [ ! -x "$libra_bin" ]; then
    echo "FAIL: pinned Libra client binary is unavailable" >&2
    print_summary
    exit 1
fi

file_sha256() {
    shasum -a 256 "$1" | awk '{print $1}'
}

actual_client_sha="$(file_sha256 "$libra_bin")"
if [ "$actual_client_sha" != "$LIBRA_TRACES_LIVE_CLIENT_BINARY_SHA256" ]; then
    echo "FAIL: Libra client binary hash does not match the live fixture pin" >&2
    print_summary
    exit 1
fi

actual_contract_sha="$(file_sha256 "$ROOT/docs/refactoring/traces.md")"
if [ "$actual_contract_sha" != "$LIBRA_TRACES_LIVE_CONTRACT_SHA256" ]; then
    echo "FAIL: shared traces contract hash does not match the live fixture pin" >&2
    print_summary
    exit 1
fi

version_output="$WORK_ROOT/libra-version.out"
version_error="$WORK_ROOT/libra-version.err"
if ! (
    cd "$ROOT"
    env -u CODEX_HOME -u LIBRA_MEGA2_TOKEN \
        HOME="$WORK_ROOT/home" \
        XDG_CONFIG_HOME="$WORK_ROOT/home/config" \
        LIBRA_TEST_HOME="$WORK_ROOT/home" \
        LIBRA_CONFIG_GLOBAL_DB="$WORK_ROOT/home/config.db" \
        GIT_CONFIG_NOSYSTEM=1 \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_TERMINAL_PROMPT=0 \
        "$libra_bin" --version
) >"$version_output" 2>"$version_error"; then
    echo "FAIL: pinned Libra client could not report its version" >&2
    print_summary
    exit 1
fi

if [ "$(cat "$version_output")" != "libra $EXPECTED_LIBRA_VERSION" ]; then
    echo "FAIL: Libra client version does not match the H4 pin" >&2
    print_summary
    exit 1
fi

mkdir -p "$WORK_ROOT/home/config"
read -r primary_token < "$LIBRA_TRACES_LIVE_TOKEN_FILE"
if [ -z "$primary_token" ]; then
    echo "FAIL: primary HTTP token file is empty" >&2
    print_summary
    exit 1
fi
AUTH_HEADER="$WORK_ROOT/primary-auth.header"
printf 'Authorization: Bearer %s\n' "$primary_token" > "$AUTH_HEADER"
unset primary_token
chmod 600 "$AUTH_HEADER"

HTTP_URL="${LIBRA_TRACES_LIVE_HTTP_URL%/}"
printf 'pin: Libra %s source=%s binary_sha256=%s\n' \
    "$EXPECTED_LIBRA_VERSION" "$EXPECTED_LIBRA_SOURCE_COMMIT" "$actual_client_sha"
printf 'pin: Mega2 server_id=%s contract_sha256=%s target=%s\n' \
    "$LIBRA_TRACES_LIVE_SERVER_ID" "$actual_contract_sha" "$LIBRA_TRACES_LIVE_TARGET"

run_libra() {
    local repo="$1"
    local libra_home="$2"
    local stdout_path="$3"
    local stderr_path="$4"
    shift 4

    (
        cd "$repo" || exit 1
        env -u CODEX_HOME -u LIBRA_MEGA2_TOKEN \
            HOME="$libra_home" \
            XDG_CONFIG_HOME="$libra_home/config" \
            LIBRA_TEST_HOME="$libra_home" \
            LIBRA_CONFIG_GLOBAL_DB="$libra_home/config.db" \
            GIT_CONFIG_NOSYSTEM=1 \
            GIT_CONFIG_GLOBAL=/dev/null \
            GIT_TERMINAL_PROMPT=0 \
            "$libra_bin" "$@"
    ) >"$stdout_path" 2>"$stderr_path" &
    ACTIVE_PID=$!

    (
        sleep "$CLI_TIMEOUT_SECONDS"
        if kill -0 "$ACTIVE_PID" 2>/dev/null; then
            kill "$ACTIVE_PID" 2>/dev/null || true
        fi
    ) &
    local watchdog_pid=$!

    local command_status=0
    wait "$ACTIVE_PID" || command_status=$?
    ACTIVE_PID=""
    kill "$watchdog_pid" 2>/dev/null || true
    wait "$watchdog_pid" 2>/dev/null || true
    return "$command_status"
}

write_synthetic_transcript() {
    local repo="$1"
    local home="$2"
    local session_id="$3"
    local transcript="$home/.claude/projects/fixture/$session_id.jsonl"
    local now
    now="$(date -u +%Y-%m-%dT%H:%M:%SZ)" || return 1
    mkdir -p "$(dirname "$transcript")" || return 1
    jq -cn --arg session "$session_id" --arg cwd "$repo" --arg now "$now" \
        '{type:"user",uuid:"turn-1",sessionId:$session,cwd:$cwd,timestamp:$now,message:{role:"user",content:"synthetic traces HTTP interop"}}' \
        > "$transcript" || return 1
    jq -cn --arg session "$session_id" --arg cwd "$repo" --arg now "$now" \
        '{type:"assistant",uuid:"assistant-1",sessionId:$session,cwd:$cwd,timestamp:$now,message:{role:"assistant",content:[{type:"text",text:"redacted synthetic fixture payload"}]}}' \
        >> "$transcript" || return 1
    jq -cn --arg session "$session_id" --arg cwd "$repo" --arg now "$now" \
        '{type:"session_end",sessionId:$session,cwd:$cwd,timestamp:$now}' \
        >> "$transcript"
}

create_stream() {
    local name="$1"
    local token_file="$2"
    local target="$3"
    local repo="$WORK_ROOT/$name/repo"
    local home="$WORK_ROOT/$name/home"
    local session_stamp
    local session_id
    local transcript
    local output_path="$WORK_ROOT/$name.out"
    local error_path="$WORK_ROOT/$name.err"
    local push_url="$HTTP_URL$target.git"

    session_stamp="$(date -u +%Y%m%dT%H%M%S)" || return 1
    session_id="mtr58-${name}-${session_stamp}-$$-${RANDOM}"
    transcript="$home/.claude/projects/fixture/$session_id.jsonl"
    mkdir -p "$repo" "$home/config" || return 1
    repo="$(cd "$repo" && pwd -P)" || return 1
    home="$(cd "$home" && pwd -P)" || return 1

    if ! run_libra "$repo" "$home" "$output_path" "$error_path" init; then
        echo "synthetic Libra repository initialization failed" >&2
        return 1
    fi
    if ! write_synthetic_transcript "$repo" "$home" "$session_id"; then
        echo "synthetic checkpoint transcript creation failed" >&2
        return 1
    fi
    if ! run_libra "$repo" "$home" "$output_path" "$error_path" \
        agent import --path "$transcript" --agent claude-code --yes --json; then
        echo "synthetic checkpoint import failed" >&2
        return 1
    fi
    if ! jq -e '.ok == true and .data.results[0].status == "imported"' "$output_path" >/dev/null 2>&1; then
        echo "synthetic checkpoint import did not complete" >&2
        return 1
    fi
    if ! run_libra "$repo" "$home" "$output_path" "$error_path" \
        config set remote.origin.url "$push_url"; then
        echo "HTTP code remote setup failed" >&2
        return 1
    fi
    if ! run_libra "$repo" "$home" "$output_path" "$error_path" \
        --json agent push --mega2 --remote origin --target "$target" --token-file "$token_file"; then
        echo "real HTTP scoped push failed" >&2
        return 1
    fi
    if ! jq -e '.ok == true and .data.state == "confirmed"' "$output_path" >/dev/null 2>&1; then
        echo "HTTP scoped push was not confirmed" >&2
        return 1
    fi

    CREATE_SCOPE="$(jq -er '.data.scope | select(type == "string" and length > 0)' "$output_path")" || return 1
    CREATE_TIP="$(jq -er '.data.tip | select(type == "string" and length > 0)' "$output_path")" || return 1
    CREATE_LOCATOR="$(jq -er '.data.locator | select(type == "string" and length > 0)' "$output_path")" || return 1
    if [[ ! "$CREATE_SCOPE" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$ ]]; then
        echo "HTTP scoped push returned an invalid stream UUID" >&2
        return 1
    fi
    if [[ "$CREATE_TIP" =~ ^[0-9a-fA-F]{40}$ ]]; then
        CREATE_OBJECT_FORMAT=sha1
    elif [[ "$CREATE_TIP" =~ ^[0-9a-fA-F]{64}$ ]]; then
        CREATE_OBJECT_FORMAT=sha256
    else
        echo "HTTP scoped push returned an invalid tip object id" >&2
        return 1
    fi
    if [[ ! "$CREATE_LOCATOR" =~ ^/\.traces/(by-path|by-scope)/ ]]; then
        echo "HTTP scoped push returned an invalid traces locator" >&2
        return 1
    fi
}

run_case() {
    local case_name="$1"
    local function_name="$2"
    local rc=0
    CASE_EXECUTED=$((CASE_EXECUTED + 1))
    printf '==> %s\n' "$case_name"
    "$function_name" || rc=$?
    if [ "$rc" -eq 0 ]; then
        CASE_PASSED=$((CASE_PASSED + 1))
        printf 'PASS: %s\n' "$case_name"
    else
        CASE_FAILED=$((CASE_FAILED + 1))
        printf 'FAIL: %s\n' "$case_name" >&2
    fi
    printf 'case=%s executed=1 passed=%s failed=%s\n' \
        "$case_name" "$([ "$rc" -eq 0 ] && echo 1 || echo 0)" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
}

case_scoped_push() {
    create_stream primary "$LIBRA_TRACES_LIVE_TOKEN_FILE" "$LIBRA_TRACES_LIVE_TARGET" || return 1
    PRIMARY_SCOPE="$CREATE_SCOPE"
    PRIMARY_TIP="$CREATE_TIP"
    printf 'scope=%s tip=%s\n' "$PRIMARY_SCOPE" "$PRIMARY_TIP"
}

case_explicit_fetch() {
    [ -n "${PRIMARY_SCOPE:-}" ] || return 1
    local repo="$WORK_ROOT/fetch/repo"
    local home="$WORK_ROOT/fetch/home"
    local output_path="$WORK_ROOT/fetch.out"
    local error_path="$WORK_ROOT/fetch.err"
    local url="$HTTP_URL$LIBRA_TRACES_LIVE_TARGET.git"
    mkdir -p "$repo" "$home/config" || return 1
    repo="$(cd "$repo" && pwd -P)" || return 1
    home="$(cd "$home" && pwd -P)" || return 1
    run_libra "$repo" "$home" "$output_path" "$error_path" init || {
        echo "fresh Libra fetch repository initialization failed" >&2
        return 1
    }
    run_libra "$repo" "$home" "$output_path" "$error_path" config set remote.origin.url "$url" || {
        echo "fresh Libra fetch remote setup failed" >&2
        return 1
    }
    run_libra "$repo" "$home" "$output_path" "$error_path" \
        --json mega2 traces fetch --remote origin --stream "$PRIMARY_SCOPE" \
        --token-file "$LIBRA_TRACES_LIVE_TOKEN_FILE" || {
        echo "explicit authenticated HTTP traces fetch failed" >&2
        return 1
    }
    jq -e --arg server "$LIBRA_TRACES_LIVE_SERVER_ID" --arg tip "$PRIMARY_TIP" \
        '.ok == true and .data.body.server_id == $server and .data.body.tip == $tip' \
        "$output_path" >/dev/null 2>&1 || {
        echo "fetched stream identity or tip did not match the pinned fixture" >&2
        return 1
    }
}

case_unauthenticated_rejected() {
    [ -n "${PRIMARY_SCOPE:-}" ] || return 1
    local response="$WORK_ROOT/unauthenticated.body"
    local headers="$WORK_ROOT/unauthenticated.headers"
    local status
    status="$(curl -sS --connect-timeout 5 --max-time 30 \
        -D "$headers" -o "$response" -w '%{http_code}' \
        "$HTTP_URL/.traces/by-scope/$PRIMARY_SCOPE.git/info/refs?service=git-upload-pack" \
        2>/dev/null)" || {
        echo "unauthenticated HTTP discovery probe failed" >&2
        return 1
    }
    [ "$status" = "401" ] || {
        echo "unauthenticated HTTP discovery was not rejected with 401" >&2
        return 1
    }
    tr '[:upper:]' '[:lower:]' < "$headers" | grep -q '^www-authenticate:' || {
        echo "unauthenticated HTTP rejection omitted WWW-Authenticate" >&2
        return 1
    }
}

write_upload_request() {
    local request_file="$1"
    local want_oid="$2"
    local have_oid="${3:-}"
    : > "$request_file"
    printf '%04x%s' "$(( ${#want_oid} + 10 ))" "want $want_oid"$'\n' >> "$request_file"
    printf '0000' >> "$request_file"
    if [ -n "$have_oid" ]; then
        printf '%04x%s' "$(( ${#have_oid} + 10 ))" "have $have_oid"$'\n' >> "$request_file"
    fi
    printf '%04x%s' 9 "done"$'\n' >> "$request_file"
}

upload_pack_request() {
    local locator="$1"
    local request_file="$2"
    local response_file="$3"
    local token_header="$4"
    local status_file="$5"
    curl -sS --connect-timeout 5 --max-time 30 \
        -H @"$token_header" \
        -H 'Content-Type: application/x-git-upload-pack-request' \
        --data-binary @"$request_file" \
        -o "$response_file" -w '%{http_code}' \
        "$HTTP_URL$locator/git-upload-pack" 2>/dev/null > "$status_file"
}

case_cross_scope_want_rejected() {
    [ -n "${PRIMARY_SCOPE:-}" ] || return 1
    create_stream foreign "$LIBRA_TRACES_LIVE_TOKEN_FILE" "$LIBRA_TRACES_LIVE_TARGET" || return 1
    FOREIGN_SCOPE="$CREATE_SCOPE"
    FOREIGN_TIP="$CREATE_TIP"
    [ "$PRIMARY_SCOPE" != "$FOREIGN_SCOPE" ] && [ "$PRIMARY_TIP" != "$FOREIGN_TIP" ] || {
        echo "fixture did not create a distinct foreign stream" >&2
        return 1
    }
    [ "${#PRIMARY_TIP}" -eq "${#FOREIGN_TIP}" ] || {
        echo "fixture streams use different object formats" >&2
        return 1
    }

    local locator="/.traces/by-scope/$PRIMARY_SCOPE.git"
    local request_file="$WORK_ROOT/cross-scope-want.request"
    local response_file="$WORK_ROOT/cross-scope-want.response"
    local status_file="$WORK_ROOT/cross-scope-want.status"
    write_upload_request "$request_file" "$FOREIGN_TIP" || return 1
    upload_pack_request "$locator" "$request_file" "$response_file" "$AUTH_HEADER" "$status_file" || {
        echo "cross-scope want HTTP request failed" >&2
        return 1
    }
    [ "$(cat "$status_file")" = "404" ] || {
        echo "cross-scope want was not refused with HTTP 404" >&2
        return 1
    }
    grep -aFq 'unknown_scope' "$response_file" || {
        echo "cross-scope want refusal omitted the stable unknown_scope oracle" >&2
        return 1
    }
    if grep -aFq 'PACK' "$response_file"; then
        echo "cross-scope want refusal returned pack data" >&2
        return 1
    fi
}

case_cross_scope_delta_base_ignored() {
    [ -n "${PRIMARY_SCOPE:-}" ] && [ -n "${FOREIGN_TIP:-}" ] || return 1
    [ "${#PRIMARY_TIP}" -eq "${#FOREIGN_TIP}" ] || {
        echo "fixture streams use different object formats" >&2
        return 1
    }
    local locator="/.traces/by-scope/$PRIMARY_SCOPE.git"
    local request_file="$WORK_ROOT/cross-scope-delta.request"
    local response_file="$WORK_ROOT/cross-scope-delta.response"
    local status_file="$WORK_ROOT/cross-scope-delta.status"
    local pack_file="$WORK_ROOT/cross-scope-delta.pack"
    local odb="$WORK_ROOT/cross-scope-delta-odb"

    write_upload_request "$request_file" "$PRIMARY_TIP" "$FOREIGN_TIP" || return 1
    upload_pack_request "$locator" "$request_file" "$response_file" "$AUTH_HEADER" "$status_file" || {
        echo "cross-scope delta-base HTTP request failed" >&2
        return 1
    }
    [ "$(cat "$status_file")" = "200" ] || {
        echo "cross-scope delta-base probe did not complete with HTTP 200" >&2
        return 1
    }
    if grep -aFq "ACK $FOREIGN_TIP" "$response_file"; then
        echo "server acknowledged a have from another scope" >&2
        return 1
    fi
    perl -0777 -e '
        binmode STDIN;
        binmode STDOUT;
        local $/;
        my $body = <>;
        exit 1 unless defined($body) && substr($body, 0, 8) eq "0008NAK\n";
        print substr($body, 8);
    ' "$response_file" > "$pack_file" || {
        echo "cross-scope have response was not a complete v0 pack response" >&2
        return 1
    }
    grep -aFq 'PACK' "$pack_file" || {
        echo "cross-scope have response did not include the wanted stream pack" >&2
        return 1
    }
    git init --bare --quiet --object-format="$CREATE_OBJECT_FORMAT" "$odb" >/dev/null 2>&1 || return 1
    git --git-dir="$odb" index-pack --stdin < "$pack_file" >/dev/null 2>&1 || {
        echo "server returned a thin pack requiring an out-of-scope delta base" >&2
        return 1
    }
    if git --git-dir="$odb" cat-file -e "$FOREIGN_TIP^{commit}" >/dev/null 2>&1; then
        echo "fetch response included the foreign scope commit" >&2
        return 1
    fi
}

run_case "http.scoped_push" case_scoped_push
run_case "http.explicit_fetch" case_explicit_fetch
run_case "http.unauthenticated_rejected" case_unauthenticated_rejected
run_case "http.cross_scope_want_rejected" case_cross_scope_want_rejected
run_case "http.cross_scope_delta_base_ignored" case_cross_scope_delta_base_ignored

print_summary
if [ "$CASE_EXECUTED" -eq 0 ] || [ "$CASE_FAILED" -gt 0 ]; then
    echo "FAIL: http" >&2
    exit 1
fi
echo "PASS: http"
