#!/usr/bin/env bash
# Libra client black-box smoke for the storage-only compose stack
# (docs/plan/plan-20261001.md, BB-50..BB-64). Runs inside interop-smoke;
# git is only a read-only observer (ADR-BB-03).
#
#   docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml --profile interop \
#     exec -T [-e MEGA2_SMOKE_CASE='<case>'] interop-smoke bash /repo/scripts/libra_smoke_storage_only.sh
#
# Output and exit codes: scripts/lib/smoke-runner.sh.
set -uo pipefail

SCRIPT_DIR="${BASH_SOURCE[0]%/*}"
[ "$SCRIPT_DIR" = "${BASH_SOURCE[0]}" ] && SCRIPT_DIR=.
# shellcheck source=scripts/lib/smoke-runner.sh disable=SC1091
source "$SCRIPT_DIR/lib/smoke-runner.sh"

smoke_init "libra"
require_tools git mkdir rm date timeout curl jq sha256sum cat grep

LIBRA_BIN="${MEGA2_LIBRA_BIN:-libra}"
command -v "$LIBRA_BIN" >/dev/null 2>&1 || smoke_die "libra binary not found: $LIBRA_BIN"
version=$("$LIBRA_BIN" --version 2>/dev/null) || smoke_die "cannot read libra version"
version="${version%%$'\n'*}"
printf 'libra version: %s\n' "$version"
if [[ "$version" =~ ^libra[[:space:]]+([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
    major=${BASH_REMATCH[1]} minor=${BASH_REMATCH[2]} patch=${BASH_REMATCH[3]}
else
    smoke_die "unrecognized libra version"
fi
if (( major == 0 && (minor < 30 || (minor == 30 && patch < 8)) )); then
    smoke_die "libra 0.30.8 or newer is required"
fi

MEGA2_BASE_URL="${MEGA2_BASE_URL:-http://127.0.0.1:9000}"
MEGA2_BASE_URL="${MEGA2_BASE_URL%/}"
RUN_ID="$(date -u +%Y%m%dt%H%M%S)" || smoke_die "cannot build RUN_ID"
RUN_ID="$RUN_ID$RANDOM"
WORK="/work/bb-$RUN_ID"
umask 077
mkdir "$WORK" || smoke_die "cannot create a work directory"
trap 'rm -rf "$WORK"' EXIT
mkdir "$WORK/home" || smoke_die "cannot create an isolated home"
export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/config"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export LIBRA_BIN MEGA2_BASE_URL RUN_ID WORK

timeout 5 git ls-remote "$MEGA2_BASE_URL/" > "$WORK/ls-remote" 2>/dev/null \
    || smoke_die "mega2 Git HTTP endpoint is not reachable with git ls-remote"

case_libra_clone_http() {
    local repo_path="/project" url="$MEGA2_BASE_URL/project" name="${1:-bb51}-$RUN_ID.txt"
    local auth="$WORK/auth.header" code seed_hash git_hash libra_hash left
    local deadline=$((SECONDS + 55))
    local token="${MEGA2_IT_SEED_TOKEN:-}"
    [ -n "$token" ] || { echo "MEGA2_IT_SEED_TOKEN is empty" >&2; return 1; }
    printf 'Authorization: Bearer %s\n' "$token" > "$auth"
    printf 'bb51 clone seed %s' "$RUN_ID" > "$WORK/seed.txt"
    jq -n --arg path "$repo_path" --arg name "$name" --arg content "$(cat "$WORK/seed.txt")" \
        '{is_directory: false, name: $name, path: $path, content: $content, skip_build: true}' \
        > "$WORK/create.json"
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    code=$(curl -sS --connect-timeout 5 --max-time "$left" -H @"$auth" \
        -H 'Content-Type: application/json' --data @"$WORK/create.json" \
        -o "$WORK/create.out" -w '%{http_code}' "$MEGA2_BASE_URL/api/v1/create-entry")
    if [ "$code" != 200 ] || ! jq -e '.req_result == true' "$WORK/create.out" > /dev/null; then
        echo "seed create-entry failed (HTTP $code)" >&2
        return 1
    fi

    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    timeout "$left" git clone "$url" "$WORK/git-observer" > "$WORK/git-clone.out" 2>&1 \
        || { echo "git observer clone failed" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    timeout "$left" "$LIBRA_BIN" clone "$url" "$WORK/libra-clone" > "$WORK/libra-clone.out" 2>&1 \
        || { echo "libra HTTP clone failed" >&2; return 1; }
    [ "$HOME" = "$WORK/home" ] && [[ "$HOME" == /work/bb-"$RUN_ID"/* ]] \
        || { echo "libra HOME was not isolated under the run directory" >&2; return 1; }
    printf 'libra home: %s\n' "$HOME"
    [ -f "$WORK/git-observer/$name" ] \
        || { echo "git observer clone is missing the seed file" >&2; return 1; }
    [ -f "$WORK/libra-clone/$name" ] \
        || { echo "libra clone is missing the seed file" >&2; return 1; }
    seed_hash=$(sha256sum "$WORK/seed.txt"); seed_hash=${seed_hash%% *}
    git_hash=$(sha256sum "$WORK/git-observer/$name"); git_hash=${git_hash%% *}
    libra_hash=$(sha256sum "$WORK/libra-clone/$name"); libra_hash=${libra_hash%% *}
    [ "$seed_hash" = "$git_hash" ] && [ "$seed_hash" = "$libra_hash" ] \
        || { echo "seed.txt sha256 differs between the seed and clones" >&2; return 1; }
}

case_libra_fetch_http() {
    local WORK="$WORK/fetch"
    local url="$MEGA2_BASE_URL/project" name="bb52-fetch-$RUN_ID.txt"
    local code before_ref before_oid clone_oid git_ref git_oid libra_oid left
    local deadline=$((SECONDS + 55))
    mkdir "$WORK" "$WORK/home"
    export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/config"
    case_libra_clone_http bb52-clone
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    before_ref=$(timeout "$left" git ls-remote "$url" refs/heads/main) || return 1
    [[ "$before_ref" == *$'\t'refs/heads/main ]] \
        || { echo "git observer is missing refs/heads/main before create-entry" >&2; return 1; }
    before_oid=${before_ref%%$'\t'*}
    clone_oid=$(cd "$WORK/libra-clone" && "$LIBRA_BIN" rev-parse refs/remotes/origin/main) \
        || return 1
    [ "$clone_oid" = "$before_oid" ] \
        || { echo "libra clone tip differs from git observer before create-entry" >&2; return 1; }
    jq -n --arg name "$name" --arg content "bb52 fetch seed $RUN_ID" \
        '{is_directory: false, name: $name, path: "/project", content: $content, skip_build: true}' \
        > "$WORK/fetch-create.json"
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    code=$(curl -sS --connect-timeout 5 --max-time "$left" -H @"$WORK/auth.header" \
        -H 'Content-Type: application/json' --data @"$WORK/fetch-create.json" \
        -o "$WORK/fetch-create.out" -w '%{http_code}' "$MEGA2_BASE_URL/api/v1/create-entry")
    if [ "$code" != 200 ] || ! jq -e '.req_result == true' "$WORK/fetch-create.out" > /dev/null; then
        echo "fetch seed create-entry failed (HTTP $code)" >&2
        return 1
    fi

    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    git_ref=$(timeout "$left" git ls-remote "$url" refs/heads/main) || return 1
    [[ "$git_ref" == *$'\t'refs/heads/main ]] \
        || { echo "git observer is missing refs/heads/main" >&2; return 1; }
    git_oid=${git_ref%%$'\t'*}
    [ "$git_oid" != "$before_oid" ] \
        || { echo "git observer tip did not advance after create-entry" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    (cd "$WORK/libra-clone" && timeout "$left" "$LIBRA_BIN" fetch origin) \
        > "$WORK/fetch.out" 2>&1 || { echo "libra HTTP fetch failed" >&2; return 1; }
    libra_oid=$(cd "$WORK/libra-clone" && "$LIBRA_BIN" rev-parse refs/remotes/origin/main) \
        || return 1
    [ "$git_oid" = "$libra_oid" ] \
        || { echo "libra origin/main differs from git observer tip" >&2; return 1; }
    printf 'origin/main: %s -> %s\n' "$before_oid" "$libra_oid"
}

case_libra_ls_remote_http() {
    local url="$MEGA2_BASE_URL/project" git_ref libra_ref git_oid libra_oid
    git_ref=$(timeout 10 git ls-remote "$url" refs/heads/main) || return 1
    libra_ref=$(timeout 10 "$LIBRA_BIN" ls-remote "$url" refs/heads/main) || return 1
    [[ "$git_ref" == *$'\t'refs/heads/main ]] \
        || { echo "git observer is missing refs/heads/main" >&2; return 1; }
    [[ "$libra_ref" == *$'\t'refs/heads/main ]] \
        || { echo "libra ls-remote is missing refs/heads/main" >&2; return 1; }
    git_oid=${git_ref%%$'\t'*}
    libra_oid=${libra_ref%%$'\t'*}
    [ "$git_oid" = "$libra_oid" ] \
        || { echo "libra ls-remote main differs from git observer" >&2; return 1; }
    printf '%s\n' "$libra_ref"
}

case_libra_trunk_push_http() {
    local WORK="$WORK/push" url="$MEGA2_BASE_URL/project" name="bb54-$RUN_ID.txt"
    local token="${MEGA2_IT_SEED_TOKEN:-}"
    local before_ref after_ref before_oid after_oid seed_hash git_hash left
    local push_rc=0 scan_rc=0
    local deadline=$((SECONDS + 55))
    [[ "$MEGA2_BASE_URL" =~ ^http://127\.0\.0\.1:[0-9]+$ ]] \
        || { echo "trunk push requires a loopback HTTP endpoint" >&2; return 1; }
    [ -n "$token" ] || { echo "MEGA2_IT_SEED_TOKEN is empty" >&2; return 1; }
    mkdir "$WORK" "$WORK/home" "$WORK/home/config"
    export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/config"
    export LIBRA_CONFIG_GLOBAL_DB="$WORK/home/config/config.db"
    before_ref=$(timeout 10 git ls-remote "$url" refs/heads/main) || return 1
    [[ "$before_ref" == *$'\t'refs/heads/main ]] \
        || { echo "git observer is missing refs/heads/main before push" >&2; return 1; }
    before_oid=${before_ref%%$'\t'*}
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    timeout "$left" "$LIBRA_BIN" clone "$url" "$WORK/push-clone" > "$WORK/clone.out" 2>&1 \
        || { echo "libra HTTP clone for push failed" >&2; return 1; }
    printf 'bb54 trunk push %s\n' "$RUN_ID" > "$WORK/push-clone/$name"
    (
        cd "$WORK/push-clone" || exit 1
        "$LIBRA_BIN" config set --local user.name 'Mega2 Smoke'
        "$LIBRA_BIN" config set --local user.email 'mega2-smoke@example.invalid'
        "$LIBRA_BIN" add "$name"
        "$LIBRA_BIN" commit -m 'BB-54 trunk push smoke' --no-gpg-sign
    ) > "$WORK/commit.out" 2>&1 || { echo "libra commit for push failed" >&2; return 1; }
    printf '%s\n' "$token" | "$LIBRA_BIN" auth login \
        --host "${MEGA2_BASE_URL#http://}" --with-token \
        > "$WORK/auth.out" 2>&1 || { echo "libra loopback auth login failed" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    (cd "$WORK/push-clone" && timeout "$left" "$LIBRA_BIN" push origin main) \
        > "$WORK/push.out" 2>&1 || push_rc=$?
    printf '%s\n' "$token" | grep -R -F -q -f /dev/stdin -- "$WORK" || scan_rc=$?
    if [ "$scan_rc" -eq 0 ]; then
        echo "push token was persisted in the case work directory" >&2
        return 1
    fi
    [ "$scan_rc" -eq 1 ] || { echo "cannot scan the case work directory" >&2; return 1; }
    [ "$push_rc" -eq 0 ] || { echo "libra trunk HTTP push failed" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    after_ref=$(timeout "$left" git ls-remote "$url" refs/heads/main) || return 1
    [[ "$after_ref" == *$'\t'refs/heads/main ]] \
        || { echo "git observer is missing refs/heads/main after push" >&2; return 1; }
    after_oid=${after_ref%%$'\t'*}
    [ "$after_oid" != "$before_oid" ] \
        || { echo "git observer tip did not advance after libra push" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    timeout "$left" git clone "$url" "$WORK/git-observer" > "$WORK/git-clone.out" 2>&1 \
        || { echo "git observer clone after libra push failed" >&2; return 1; }
    [ -f "$WORK/git-observer/$name" ] \
        || { echo "git observer clone is missing the pushed file" >&2; return 1; }
    seed_hash=$(sha256sum "$WORK/push-clone/$name"); seed_hash=${seed_hash%% *}
    git_hash=$(sha256sum "$WORK/git-observer/$name"); git_hash=${git_hash%% *}
    [ "$seed_hash" = "$git_hash" ] \
        || { echo "pushed file differs in git observer clone" >&2; return 1; }
    printf 'main: %s -> %s\n' "$before_oid" "$after_oid"
}

run_case "LIBRA clone HTTP" case_libra_clone_http
run_case "LIBRA fetch HTTP" case_libra_fetch_http
run_case "LIBRA ls-remote HTTP" case_libra_ls_remote_http
run_case "LIBRA trunk push HTTP" case_libra_trunk_push_http

finish
