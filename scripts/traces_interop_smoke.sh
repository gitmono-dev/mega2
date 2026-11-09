#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
    cat <<'EOF'
Usage: MEGA2_TRACES_INTEROP=1 scripts/traces_interop_smoke.sh --case http

Runs the real HTTP traces interop cases against the pinned live fixture.
Credentials are read from LIBRA_TRACES_LIVE_TOKEN_FILE; they are never placed
in a URL or printed to the report. Synthetic remote streams are retained by
the H1 fixture through LTR-08; the local temporary workspace is always removed.
EOF
}

if [ "${1:-}" = "--help" ] && [ "$#" -eq 1 ]; then
    usage
    exit 0
fi

if [ "$#" -ne 2 ] || [ "$1" != "--case" ]; then
    usage >&2
    exit 2
fi

case "$2" in
    http) ;;
    *)
        echo "FAIL: unsupported interop case '$2'" >&2
        exit 2
        ;;
esac

if [ "${MEGA2_TRACES_INTEROP:-}" != "1" ]; then
    echo "FAIL: MEGA2_TRACES_INTEROP=1 is required" >&2
    exit 1
fi

exec bash "$ROOT/scripts/traces_interop_http.sh"
