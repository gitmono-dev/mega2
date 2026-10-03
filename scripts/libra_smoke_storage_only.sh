#!/usr/bin/env bash
# Libra client black-box smoke for the storage-only compose stack
# (docs/plan/plan-20261001.md, BB-50..BB-75). Runs inside interop-smoke;
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
if [ -z "$SMOKE_CASE_FILTER" ] || [ "$SMOKE_CASE_FILTER" = "LIBRA clone SSH" ] \
    || [ "$SMOKE_CASE_FILTER" = "LIBRA reject SSH push" ]; then
    require_tools ssh-keyscan ssh
fi

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

case_libra_multi_commit_push_http() {
    local WORK="$WORK/multi-push" url="$MEGA2_BASE_URL/project"
    local token="${MEGA2_IT_SEED_TOKEN:-}"
    local before_ref after_ref before_oid after_oid name expected_hash git_hash left i
    local push_rc=0 scan_rc=0
    local deadline=$((SECONDS + 55))
    [[ "$MEGA2_BASE_URL" =~ ^http://127\.0\.0\.1:[0-9]+$ ]] \
        || { echo "multi-commit push requires a loopback HTTP endpoint" >&2; return 1; }
    [ -n "$token" ] || { echo "MEGA2_IT_SEED_TOKEN is empty" >&2; return 1; }
    mkdir "$WORK" "$WORK/home" "$WORK/home/config"
    export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/config"
    export LIBRA_CONFIG_GLOBAL_DB="$WORK/home/config/config.db"
    before_ref=$(timeout 10 git ls-remote "$url" refs/heads/main) || return 1
    [[ "$before_ref" == *$'\t'refs/heads/main ]] \
        || { echo "git observer is missing refs/heads/main before multi-commit push" >&2; return 1; }
    before_oid=${before_ref%%$'\t'*}
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    timeout "$left" "$LIBRA_BIN" clone "$url" "$WORK/push-clone" > "$WORK/clone.out" 2>&1 \
        || { echo "libra HTTP clone for multi-commit push failed" >&2; return 1; }
    (
        run_remaining() {
            local remaining=$((deadline - SECONDS))
            [ "$remaining" -gt 0 ] || return 124
            timeout "$remaining" "$@"
        }
        cd "$WORK/push-clone" || exit 1
        run_remaining "$LIBRA_BIN" config set --local user.name 'Mega2 Smoke' || exit 1
        run_remaining "$LIBRA_BIN" config set --local user.email 'mega2-smoke@example.invalid' || exit 1
        prev_oid=$(run_remaining "$LIBRA_BIN" rev-parse HEAD) || exit 1
        for i in 1 2 3; do
            name="bb55-$RUN_ID-$i.txt"
            printf 'bb55 commit %s %s\n' "$RUN_ID" "$i" > "$name"
            run_remaining "$LIBRA_BIN" add "$name" || exit 1
            run_remaining "$LIBRA_BIN" commit -m "BB-55 multi-commit smoke $i" --no-gpg-sign || exit 1
            current_oid=$(run_remaining "$LIBRA_BIN" rev-parse HEAD) || exit 1
            [ "$current_oid" != "$prev_oid" ] || exit 1
            prev_oid=$current_oid
        done
    ) > "$WORK/commit.out" 2>&1 || { echo "libra multi-commit preparation failed" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    printf '%s\n' "$token" | timeout "$left" "$LIBRA_BIN" auth login \
        --host "${MEGA2_BASE_URL#http://}" --with-token \
        > "$WORK/auth.out" 2>&1 || { echo "libra loopback auth login failed" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    (cd "$WORK/push-clone" && timeout "$left" "$LIBRA_BIN" push origin main) \
        > "$WORK/push.out" 2>&1 || push_rc=$?
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    printf '%s\n' "$token" | timeout "$left" grep -R -F -q -f /dev/stdin -- "$WORK" || scan_rc=$?
    if [ "$scan_rc" -eq 0 ]; then
        echo "multi-commit push token was persisted in the case work directory" >&2
        return 1
    fi
    [ "$scan_rc" -eq 1 ] || { echo "cannot scan the multi-commit work directory" >&2; return 1; }
    [ "$push_rc" -eq 0 ] || { echo "libra multi-commit HTTP push failed" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    after_ref=$(timeout "$left" git ls-remote "$url" refs/heads/main) || return 1
    [[ "$after_ref" == *$'\t'refs/heads/main ]] \
        || { echo "git observer is missing refs/heads/main after multi-commit push" >&2; return 1; }
    after_oid=${after_ref%%$'\t'*}
    [ "$after_oid" != "$before_oid" ] \
        || { echo "git observer tip did not advance after multi-commit push" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    timeout "$left" git clone "$url" "$WORK/git-observer" > "$WORK/git-clone.out" 2>&1 \
        || { echo "git observer clone after multi-commit push failed" >&2; return 1; }
    for i in 1 2 3; do
        name="bb55-$RUN_ID-$i.txt"
        [ -f "$WORK/git-observer/$name" ] \
            || { echo "git observer clone is missing multi-commit file $i" >&2; return 1; }
        expected_hash=$(printf 'bb55 commit %s %s\n' "$RUN_ID" "$i" | sha256sum)
        expected_hash=${expected_hash%% *}
        git_hash=$(sha256sum "$WORK/git-observer/$name"); git_hash=${git_hash%% *}
        [ "$expected_hash" = "$git_hash" ] \
            || { echo "multi-commit file $i differs in git observer clone" >&2; return 1; }
    done
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    printf 'main: %s -> %s (3 local commits, one push)\n' "$before_oid" "$after_oid"
}

case_libra_reject_unauthenticated_push() {
    local WORK="$WORK/unauth-push" url="$MEGA2_BASE_URL/project" name="bb56-$RUN_ID.txt"
    local before_ref after_ref before_oid after_oid left push_rc=0
    local deadline=$((SECONDS + 55))
    [[ "$MEGA2_BASE_URL" =~ ^http://127\.0\.0\.1:[0-9]+$ ]] \
        || { echo "unauthenticated push requires a loopback HTTP endpoint" >&2; return 1; }
    unset MEGA2_IT_SEED_TOKEN
    mkdir "$WORK" "$WORK/home" "$WORK/home/config"
    export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/config"
    export LIBRA_CONFIG_GLOBAL_DB="$WORK/home/config/config.db"
    before_ref=$(timeout 10 git ls-remote "$url" refs/heads/main) || return 1
    [[ "$before_ref" == *$'\t'refs/heads/main ]] \
        || { echo "git observer is missing refs/heads/main before unauthenticated push" >&2; return 1; }
    before_oid=${before_ref%%$'\t'*}
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    timeout "$left" "$LIBRA_BIN" clone "$url" "$WORK/push-clone" > "$WORK/clone.out" 2>&1 \
        || { echo "libra HTTP clone for unauthenticated push failed" >&2; return 1; }
    printf 'bb56 unauthenticated push %s\n' "$RUN_ID" > "$WORK/push-clone/$name"
    (
        run_remaining() {
            local remaining=$((deadline - SECONDS))
            [ "$remaining" -gt 0 ] || return 124
            timeout "$remaining" "$@"
        }
        cd "$WORK/push-clone" || exit 1
        run_remaining "$LIBRA_BIN" config set --local user.name 'Mega2 Smoke' || exit 1
        run_remaining "$LIBRA_BIN" config set --local user.email 'mega2-smoke@example.invalid' || exit 1
        run_remaining "$LIBRA_BIN" add "$name" || exit 1
        run_remaining "$LIBRA_BIN" commit -m 'BB-56 unauthenticated push smoke' --no-gpg-sign || exit 1
    ) > "$WORK/commit.out" 2>&1 \
        || { echo "libra commit for unauthenticated push failed" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    (cd "$WORK/push-clone" && timeout "$left" "$LIBRA_BIN" push origin main < /dev/null) \
        > "$WORK/push.out" 2>&1 || push_rc=$?
    [ "$push_rc" -ne 0 ] && [ "$push_rc" -ne 124 ] \
        || { echo "unauthenticated push did not fail promptly" >&2; return 1; }
    grep -Fq 'fatal: authentication required' "$WORK/push.out" \
        || { echo "unauthenticated push failed without a 401 auth rejection" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    after_ref=$(timeout "$left" git ls-remote "$url" refs/heads/main) || return 1
    [[ "$after_ref" == *$'\t'refs/heads/main ]] \
        || { echo "git observer is missing refs/heads/main after unauthenticated push" >&2; return 1; }
    after_oid=${after_ref%%$'\t'*}
    [ "$after_oid" = "$before_oid" ] \
        || { echo "git observer tip advanced after unauthenticated push" >&2; return 1; }
    printf 'main unchanged: %s (push rejected with authentication required)\n' "$after_oid"
}

case_libra_reject_tag_push() {
    local WORK="$WORK/tag-push" url="$MEGA2_BASE_URL/project" tag="bb57-$RUN_ID"
    local token="${MEGA2_IT_SEED_TOKEN:-}"
    local before_refs after_refs left push_rc=0 scan_rc=0
    local deadline=$((SECONDS + 55))
    [[ "$MEGA2_BASE_URL" =~ ^http://127\.0\.0\.1:[0-9]+$ ]] \
        || { echo "tag push requires a loopback HTTP endpoint" >&2; return 1; }
    [ -n "$token" ] || { echo "MEGA2_IT_SEED_TOKEN is empty" >&2; return 1; }
    mkdir "$WORK" "$WORK/home" "$WORK/home/config"
    export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/config"
    export LIBRA_CONFIG_GLOBAL_DB="$WORK/home/config/config.db"
    before_refs=$(timeout 10 git ls-remote --tags "$url" "refs/tags/$tag") || return 1
    [ -z "$before_refs" ] \
        || { echo "git observer found the tag before push" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    timeout "$left" "$LIBRA_BIN" clone "$url" "$WORK/push-clone" > "$WORK/clone.out" 2>&1 \
        || { echo "libra HTTP clone for tag push failed" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    (cd "$WORK/push-clone" && timeout "$left" "$LIBRA_BIN" tag -m 'BB-57 tag push smoke' "$tag") \
        > "$WORK/tag.out" 2>&1 || { echo "libra local tag creation failed" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    printf '%s\n' "$token" | timeout "$left" "$LIBRA_BIN" auth login \
        --host "${MEGA2_BASE_URL#http://}" --with-token \
        > "$WORK/auth.out" 2>&1 || { echo "libra loopback auth login failed" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    (cd "$WORK/push-clone" && timeout "$left" "$LIBRA_BIN" push origin "$tag") \
        > "$WORK/push.out" 2>&1 || push_rc=$?
    [ "$push_rc" -ne 0 ] && [ "$push_rc" -ne 124 ] \
        || { echo "tag push did not fail promptly" >&2; return 1; }
    grep -Fq 'tag pushes are not supported' "$WORK/push.out" \
        || { echo "tag push failed without the server's tag rejection" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    printf '%s\n' "$token" | timeout "$left" grep -R -F -q -f /dev/stdin -- "$WORK" || scan_rc=$?
    if [ "$scan_rc" -eq 0 ]; then
        echo "tag push token was persisted in the case work directory" >&2
        return 1
    fi
    [ "$scan_rc" -eq 1 ] || { echo "cannot scan the tag-push work directory" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    after_refs=$(timeout "$left" git ls-remote --tags "$url" "refs/tags/$tag") || return 1
    [ -z "$after_refs" ] \
        || { echo "git observer found the rejected tag on the remote" >&2; return 1; }
    printf 'remote tag absent: refs/tags/%s\n' "$tag"
}

case_libra_reject_non_main_branch_push() {
    local WORK="$WORK/non-main-push" url="$MEGA2_BASE_URL/project"
    local branch="refs/heads/bb-$RUN_ID" name="bb58-$RUN_ID.txt"
    local token="${MEGA2_IT_SEED_TOKEN:-}"
    local before_ref after_ref left push_rc=0 scan_rc=0
    local deadline=$((SECONDS + 55))
    [[ "$MEGA2_BASE_URL" =~ ^http://127\.0\.0\.1:[0-9]+$ ]] \
        || { echo "non-main push requires a loopback HTTP endpoint" >&2; return 1; }
    [ -n "$token" ] || { echo "MEGA2_IT_SEED_TOKEN is empty" >&2; return 1; }
    mkdir "$WORK" "$WORK/home" "$WORK/home/config"
    export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/config"
    export LIBRA_CONFIG_GLOBAL_DB="$WORK/home/config/config.db"
    before_ref=$(timeout 10 git ls-remote "$url" "$branch") || return 1
    [ -z "$before_ref" ] \
        || { echo "git observer found the branch before push" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    timeout "$left" "$LIBRA_BIN" clone "$url" "$WORK/push-clone" > "$WORK/clone.out" 2>&1 \
        || { echo "libra HTTP clone for non-main push failed" >&2; return 1; }
    printf 'bb58 non-main push %s\n' "$RUN_ID" > "$WORK/push-clone/$name"
    (
        run_remaining() {
            local remaining=$((deadline - SECONDS))
            [ "$remaining" -gt 0 ] || return 124
            timeout "$remaining" "$@"
        }
        cd "$WORK/push-clone" || exit 1
        run_remaining "$LIBRA_BIN" config set --local user.name 'Mega2 Smoke' || exit 1
        run_remaining "$LIBRA_BIN" config set --local user.email 'mega2-smoke@example.invalid' || exit 1
        run_remaining "$LIBRA_BIN" add "$name" || exit 1
        run_remaining "$LIBRA_BIN" commit -m 'BB-58 non-main push smoke' --no-gpg-sign || exit 1
    ) > "$WORK/commit.out" 2>&1 \
        || { echo "libra commit for non-main push failed" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    printf '%s\n' "$token" | timeout "$left" "$LIBRA_BIN" auth login \
        --host "${MEGA2_BASE_URL#http://}" --with-token \
        > "$WORK/auth.out" 2>&1 || { echo "libra loopback auth login failed" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    (cd "$WORK/push-clone" && timeout "$left" "$LIBRA_BIN" push origin "main:$branch") \
        > "$WORK/push.out" 2>&1 || push_rc=$?
    [ "$push_rc" -ne 0 ] && [ "$push_rc" -ne 124 ] \
        || { echo "non-main push did not fail promptly" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    printf '%s\n' "$token" | timeout "$left" grep -R -F -q -f /dev/stdin -- "$WORK" || scan_rc=$?
    if [ "$scan_rc" -eq 0 ]; then
        echo "non-main push token was persisted in the case work directory" >&2
        return 1
    fi
    [ "$scan_rc" -eq 1 ] || { echo "cannot scan the non-main work directory" >&2; return 1; }
    grep -Fq "trunk push rejects ref '$branch'" "$WORK/push.out" \
        || { echo "non-main push failed without the server's trunk rejection" >&2; cat "$WORK/push.out" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    after_ref=$(timeout "$left" git ls-remote "$url" "$branch") || return 1
    [ -z "$after_ref" ] \
        || { echo "git observer found the rejected branch on the remote" >&2; return 1; }
    printf 'remote branch absent: %s\n' "$branch"
}

case_libra_clone_ssh() {
    local WORK="$WORK/ssh-clone" ssh_url="ssh://git@127.0.0.1:2222//project"
    local url="$MEGA2_BASE_URL/project" name="${1:-bb59}-$RUN_ID.txt"
    local token="${MEGA2_IT_SEED_TOKEN:-}"
    local kh code left seed_hash git_hash libra_hash git_oid ssh_ref ssh_oid clone_oid
    local deadline=$((SECONDS + 55))
    [ -n "$token" ] || { echo "MEGA2_IT_SEED_TOKEN is empty" >&2; return 1; }
    mkdir "$WORK" "$WORK/home" "$WORK/home/.ssh" "$WORK/home/config"
    export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/config"
    export LIBRA_CONFIG_GLOBAL_DB="$WORK/home/config/config.db"
    kh="$WORK/home/.ssh/known_hosts"
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    timeout "$left" ssh-keyscan -T 5 -t ed25519 -p 2222 127.0.0.1 \
        > "$kh" 2> "$WORK/keyscan.err" \
        || { echo "SSH host key scan failed" >&2; return 1; }
    grep -Fq '[127.0.0.1]:2222 ssh-ed25519 ' "$kh" \
        || { echo "SSH host key scan did not return an ed25519 key" >&2; return 1; }
    chmod 600 "$kh"
    cat > "$WORK/ssh-client" <<'EOF'
#!/usr/bin/env bash
exec ssh -F /dev/null -o UserKnownHostsFile="$MEGA2_SMOKE_KNOWN_HOSTS" \
    -o GlobalKnownHostsFile=/dev/null -o StrictHostKeyChecking=yes "$@"
EOF
    chmod 700 "$WORK/ssh-client"
    export MEGA2_SMOKE_KNOWN_HOSTS="$kh" LIBRA_SSH_COMMAND="$WORK/ssh-client"

    printf '%s SSH clone seed %s' "${1:-bb59}" "$RUN_ID" > "$WORK/seed.txt"
    jq -n --arg name "$name" --arg content "$(cat "$WORK/seed.txt")" \
        '{is_directory: false, name: $name, path: "/project", content: $content, skip_build: true}' \
        > "$WORK/create.json"
    printf 'Authorization: Bearer %s\n' "$token" > "$WORK/auth.header"
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    code=$(curl -sS --connect-timeout 5 --max-time "$left" -H @"$WORK/auth.header" \
        -H 'Content-Type: application/json' --data @"$WORK/create.json" \
        -o "$WORK/create.out" -w '%{http_code}' "$MEGA2_BASE_URL/api/v1/create-entry") \
        || return 1
    rm "$WORK/auth.header"
    if [ "$code" != 200 ] || ! jq -e '.req_result == true' "$WORK/create.out" > /dev/null; then
        echo "SSH clone seed create-entry failed (HTTP $code)" >&2
        return 1
    fi
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    timeout "$left" git clone "$url" "$WORK/git-observer" > "$WORK/git-clone.out" 2>&1 \
        || { echo "git observer clone failed" >&2; return 1; }
    while [ ! -f "$WORK/git-observer/$name" ]; do
        left=$((deadline - SECONDS)); [ "$left" -gt 2 ] || return 124
        sleep 1
        left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
        timeout "$left" git -C "$WORK/git-observer" pull --ff-only \
            > "$WORK/git-pull.out" 2>&1 \
            || { echo "git observer could not read the SSH clone seed" >&2; return 1; }
    done
    git_oid=$(git -C "$WORK/git-observer" rev-parse HEAD) || return 1
    while :; do
        left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
        ssh_ref=$(timeout "$left" "$LIBRA_BIN" ls-remote "$ssh_url" refs/heads/main) \
            || { echo "libra SSH ls-remote failed" >&2; return 1; }
        [[ "$ssh_ref" == *$'\t'refs/heads/main ]] \
            || { echo "libra SSH ls-remote is missing main" >&2; return 1; }
        ssh_oid=${ssh_ref%%$'\t'*}
        [ "$ssh_oid" = "$git_oid" ] && break
        left=$((deadline - SECONDS)); [ "$left" -gt 2 ] \
            || { echo "libra SSH tip did not catch up with git observer" >&2; return 124; }
        sleep 1
    done
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    timeout "$left" "$LIBRA_BIN" clone "$ssh_url" "$WORK/libra-clone" \
        > "$WORK/libra-clone.out" 2>&1 \
        || { echo "libra SSH clone failed" >&2; return 1; }
    clone_oid=$(cd "$WORK/libra-clone" && "$LIBRA_BIN" rev-parse HEAD) || return 1
    [ "$clone_oid" = "$ssh_oid" ] \
        || { echo "libra SSH clone tip differs from git observer" >&2; return 1; }
    [ -f "$WORK/libra-clone/$name" ] \
        || { echo "libra SSH clone is missing the seed file at tip $ssh_oid" >&2; return 1; }
    [ -f "$WORK/git-observer/$name" ] \
        || { echo "git observer clone is missing the seed file" >&2; return 1; }
    seed_hash=$(sha256sum "$WORK/seed.txt"); seed_hash=${seed_hash%% *}
    git_hash=$(sha256sum "$WORK/git-observer/$name"); git_hash=${git_hash%% *}
    libra_hash=$(sha256sum "$WORK/libra-clone/$name"); libra_hash=${libra_hash%% *}
    if [ "$seed_hash" != "$git_hash" ] || [ "$seed_hash" != "$libra_hash" ]; then
        echo "SSH clone file differs from seed or git observer" >&2
        return 1
    fi
    printf 'SSH clone seed sha256: %s\n' "$libra_hash"
}

case_libra_reject_ssh_push() {
    local WORK="$WORK/ssh-push" url="$MEGA2_BASE_URL/project" name="bb60-$RUN_ID.txt"
    local before_ref after_ref before_oid after_oid left push_rc=0
    local deadline=$((SECONDS + 55))
    mkdir "$WORK"
    case_libra_clone_ssh bb60-clone
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    before_ref=$(timeout "$left" git ls-remote "$url" refs/heads/main) || return 1
    [[ "$before_ref" == *$'\t'refs/heads/main ]] \
        || { echo "git observer is missing main before SSH push" >&2; return 1; }
    before_oid=${before_ref%%$'\t'*}
    printf 'bb60 SSH push %s\n' "$RUN_ID" > "$WORK/ssh-clone/libra-clone/$name"
    (
        run_remaining() {
            local remaining=$((deadline - SECONDS))
            [ "$remaining" -gt 0 ] || return 124
            timeout "$remaining" "$@"
        }
        cd "$WORK/ssh-clone/libra-clone" || exit 1
        run_remaining "$LIBRA_BIN" config set --local user.name 'Mega2 Smoke' || exit 1
        run_remaining "$LIBRA_BIN" config set --local user.email 'mega2-smoke@example.invalid' || exit 1
        run_remaining "$LIBRA_BIN" add "$name" || exit 1
        run_remaining "$LIBRA_BIN" commit -m 'BB-60 SSH push rejection smoke' --no-gpg-sign || exit 1
    ) > "$WORK/commit.out" 2>&1 \
        || { echo "libra commit for SSH push failed" >&2; return 1; }
    export MEGA2_SMOKE_SSH_BASE="$LIBRA_SSH_COMMAND"
    export MEGA2_SMOKE_SSH_ERROR_LOG="$WORK/ssh-push.err"
    cat > "$WORK/ssh-push-client" <<'EOF'
#!/usr/bin/env bash
exec "$MEGA2_SMOKE_SSH_BASE" "$@" 2> "$MEGA2_SMOKE_SSH_ERROR_LOG"
EOF
    chmod 700 "$WORK/ssh-push-client"
    export LIBRA_SSH_COMMAND="$WORK/ssh-push-client"
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    (cd "$WORK/ssh-clone/libra-clone" && timeout "$left" "$LIBRA_BIN" push origin main < /dev/null) \
        > "$WORK/push.out" 2>&1 || push_rc=$?
    if [ "$push_rc" -eq 0 ] || [ "$push_rc" -eq 124 ]; then
        echo "SSH push did not fail promptly" >&2
        return 1
    fi
    grep -Fq 'SSH receive-pack is disabled' "$WORK/ssh-push.err" \
        || { echo "SSH push failed without the server's receive-pack rejection" >&2; cat "$WORK/push.out" "$WORK/ssh-push.err" >&2; return 1; }
    left=$((deadline - SECONDS)); [ "$left" -gt 0 ] || return 124
    after_ref=$(timeout "$left" git ls-remote "$url" refs/heads/main) || return 1
    [[ "$after_ref" == *$'\t'refs/heads/main ]] \
        || { echo "git observer is missing main after SSH push" >&2; return 1; }
    after_oid=${after_ref%%$'\t'*}
    [ "$after_oid" = "$before_oid" ] \
        || { echo "git observer tip advanced after rejected SSH push" >&2; return 1; }
    printf 'main unchanged: %s (SSH receive-pack disabled)\n' "$after_oid"
}

run_case "LIBRA clone HTTP" case_libra_clone_http
run_case "LIBRA fetch HTTP" case_libra_fetch_http
run_case "LIBRA ls-remote HTTP" case_libra_ls_remote_http
run_case "LIBRA trunk push HTTP" case_libra_trunk_push_http
run_case "LIBRA multi-commit push HTTP" case_libra_multi_commit_push_http
run_case "LIBRA reject unauthenticated push" case_libra_reject_unauthenticated_push
run_case "LIBRA reject tag push" case_libra_reject_tag_push
run_case "LIBRA reject non-main branch push" case_libra_reject_non_main_branch_push
run_case "LIBRA clone SSH" case_libra_clone_ssh
run_case "LIBRA reject SSH push" case_libra_reject_ssh_push

finish
