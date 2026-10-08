#!/usr/bin/env bash
# Opt-in Libra black-box smoke for ready, unavailable, and inconsistent views.
set -uo pipefail

SCRIPT_DIR="${BASH_SOURCE[0]%/*}"
[ "$SCRIPT_DIR" = "${BASH_SOURCE[0]}" ] && SCRIPT_DIR=.
# shellcheck source=scripts/lib/smoke-runner.sh disable=SC1091
source "$SCRIPT_DIR/lib/smoke-runner.sh"

smoke_init "libra view"
require_tools libra git curl jq date sort ssh-keyscan ssh timeout
version=$(libra --version) || smoke_die 'cannot read libra version'
printf '%s\n' "$version"
[[ "$version" =~ ^libra[[:space:]]+([0-9]+\.[0-9]+\.[0-9]+)$ ]] \
    || smoke_die 'unrecognized libra version'
printf '0.30.8\n%s\n' "${BASH_REMATCH[1]}" | sort -V -C \
    || smoke_die 'libra 0.30.8 or newer is required'

RUN_ID="$(date -u +%Y%m%dt%H%M%S)$RANDOM"
WORK="/work/hp25-${RUN_ID}"
umask 077
mkdir -p "$WORK/.ssh" "$WORK/bin" || smoke_die 'cannot create isolated work directory'
trap 'rm -rf -- "$WORK"' EXIT
export HOME="$WORK" XDG_CONFIG_HOME="$WORK/config"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null

BASE_URL=http://127.0.0.1:9000
HTTP_VIEW="$BASE_URL/.view"
SSH_VIEW=ssh://git@127.0.0.1:2222/.view

show_error() {
    redact "${MEGA2_IT_SEED_TOKEN:-}" < "$1" >&2
}

prepare_ssh() {
    ssh-keyscan -T 5 -p 2222 127.0.0.1 > "$WORK/.ssh/known_hosts" 2>/dev/null
    [ -s "$WORK/.ssh/known_hosts" ] || { echo 'SSH host key scan failed' >&2; return 1; }
    cat > "$WORK/bin/hp25-ssh" <<EOF
#!/bin/sh
exec ssh -o UserKnownHostsFile=$WORK/.ssh/known_hosts -o StrictHostKeyChecking=yes "\$@"
EOF
    chmod 0755 "$WORK/bin/hp25-ssh"
    export LIBRA_SSH_COMMAND="$WORK/bin/hp25-ssh"
}

remote_head() {
    local ref
    ref=$(timeout 10 git ls-remote "$1" HEAD) || return 1
    [[ "$ref" == *$'\t'HEAD ]] || return 1
    printf '%s\n' "${ref%%$'\t'*}"
}

view_http_clone_fetch() {
    local url="$HTTP_VIEW/hp25-ok@1.git" before after got code deadline
    local token="${MEGA2_IT_SEED_TOKEN:-}"
    [ -n "$token" ] || { echo 'seed token is missing' >&2; return 1; }
    if ! timeout 60 libra clone "$url" "$WORK/ready" > "$WORK/ready-clone.out" 2>&1; then
        show_error "$WORK/ready-clone.out"
        return 1
    fi
    before=$(remote_head "$url") || { echo 'git observer cannot read ready view' >&2; return 1; }
    got=$(cd "$WORK/ready" && libra rev-parse HEAD) || return 1
    [ "$got" = "$before" ] || { echo 'Libra clone HEAD differs from git observer' >&2; return 1; }

    jq -n --arg name "hp25-${RUN_ID}.txt" --arg content "$RUN_ID" \
        '{is_directory:false,path:"/project/hp25-view-smoke/ok",name:$name,content:$content}' \
        > "$WORK/create.json"
    code=$(curl -sS --connect-timeout 5 --max-time 30 \
        -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
        --data @"$WORK/create.json" -o "$WORK/create.out" -w '%{http_code}' \
        "$BASE_URL/api/v1/create-entry")
    if [ "$code" != 200 ] || ! jq -e '.req_result == true' "$WORK/create.out" >/dev/null; then
        echo "create-entry failed (HTTP $code)" >&2
        return 1
    fi
    deadline=$((SECONDS + 30))
    after="$before"
    while [ "$SECONDS" -lt "$deadline" ]; do
        after=$(remote_head "$url" 2>/dev/null) || after="$before"
        [ "$after" != "$before" ] && break
        sleep 1
    done
    [ "$after" != "$before" ] || { echo 'ready view did not advance' >&2; return 1; }
    if ! (cd "$WORK/ready" && timeout 30 libra fetch origin) > "$WORK/ready-fetch.out" 2>&1; then
        show_error "$WORK/ready-fetch.out"
        return 1
    fi
    got=$(cd "$WORK/ready" && libra rev-parse origin/main) || return 1
    [ "$got" = "$after" ] || { echo 'Libra origin/main differs from git observer' >&2; return 1; }
    printf 'view HEAD: %s -> %s\n' "$before" "$after"
}

view_unready() {
    local url="$HTTP_VIEW/hp25-unready@1.git"
    if timeout 75 libra clone "$url" "$WORK/unready-http" > "$WORK/unready-http.out" 2>&1; then
        echo 'Libra HTTP clone unexpectedly succeeded' >&2
        return 1
    fi
    grep -qF 'HTTP 503' "$WORK/unready-http.out" \
        || { show_error "$WORK/unready-http.out"; return 1; }
    prepare_ssh
    if timeout 30 libra clone "$SSH_VIEW/hp25-unready@1.git" "$WORK/unready-ssh" \
        > "$WORK/unready-ssh.out" 2>&1; then
        echo 'Libra SSH clone unexpectedly succeeded' >&2
        return 1
    fi
    grep -qF 'status 75' "$WORK/unready-ssh.out" \
        || { show_error "$WORK/unready-ssh.out"; return 1; }
    if timeout 10 git ls-remote "$url" > "$WORK/unready-git.out" 2>&1; then
        echo 'unready view became readable' >&2
        return 1
    fi
    grep -qF 'returned error: 503' "$WORK/unready-git.out" \
        || { show_error "$WORK/unready-git.out"; return 1; }
}

view_l0_mismatch() {
    local blob tree output name url
    mkdir "$WORK/l0-recompute"
    git -C "$WORK/l0-recompute" init -q
    blob=$(printf 'hp25 fixed tree entry\n' | git -C "$WORK/l0-recompute" hash-object -w --stdin)
    tree=$(printf '100664 blob %s\tbad.txt\n' "$blob" | git -C "$WORK/l0-recompute" mktree)
    prepare_ssh
    for name in http ssh; do
        if [ "$name" = http ]; then
            url="$HTTP_VIEW/hp25-l0@1.git"
        else
            url="$SSH_VIEW/hp25-l0@1.git"
        fi
        output="$WORK/l0-$name.out"
        if timeout 30 libra clone "$url" "$WORK/l0-$name" > "$output" 2>&1; then
            echo "Libra $name clone unexpectedly succeeded" >&2
            return 1
        fi
        if ! awk -v tree="$tree" 'index($0, "remote reported an error:") && index($0, tree) { found = 1 } END { exit !found }' "$output"; then
            show_error "$output"
            return 1
        fi
    done
}

optin_case view_http_clone_fetch MEGA2_SMOKE_VIEWS view_http_clone_fetch
optin_case view_unready MEGA2_SMOKE_VIEWS view_unready
optin_case view_l0_mismatch MEGA2_SMOKE_VIEWS view_l0_mismatch
finish
