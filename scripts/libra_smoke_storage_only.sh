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
require_tools git mktemp mkdir rm date timeout

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
WORK="$(mktemp -d)" || smoke_die "cannot create a work directory"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/home" || smoke_die "cannot create an isolated home"
export HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/home/config"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export LIBRA_BIN MEGA2_BASE_URL RUN_ID WORK

timeout 5 git ls-remote "$MEGA2_BASE_URL/" > "$WORK/ls-remote" 2>/dev/null \
    || smoke_die "mega2 Git HTTP endpoint is not reachable with git ls-remote"

finish
