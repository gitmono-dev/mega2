#!/usr/bin/env bash
# Traces client ↔ mega2 interop smoke dispatcher (MTR-58..69).
#
# Usage:
#   MEGA2_TRACES_INTEROP=1 scripts/traces_interop_smoke.sh --case <name>
#   scripts/traces_interop_smoke.sh --list-cases
#
# Cases (each one is a separate, audited driver):
#   http        HTTP scoped push, explicit fetch, unauthenticated and
#               cross-scope want/delta rejection (scripts/traces_interop_http.sh)
#   ssh         SSH scoped push/fetch and none-auth/cross-scope rejection
#               (scripts/traces_interop_ssh.sh)
#   agent       Agent read and scope isolation (scripts/traces_interop_agent.sh)
#   history     Epoch history retention (scripts/traces_interop_history.sh)
#   ref-reject  ref delete and non-fast-forward rejection
#               (scripts/traces_interop_ref_reject.sh)
#
# The real cases only run against an operator-provided fixture: every driver
# refuses to run (exit 1, `FAIL: fixture …`) when a required environment value
# is missing, and `MEGA2_TRACES_INTEROP=1` is mandatory so an accidental run
# in a normal test job cannot reach a live service. The offline selector
# `cargo test --test integration_traces interop_http_` exercises only the
# argument parsing, the missing-fixture failure and the cleanup guard.
#
# Exit codes: 0 = every case in the selected driver passed; 1 = at least one
# case failed (including a missing fixture); 2 = usage or precondition error
# (unknown case, interop switch off, driver not present), nothing was run.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CASES=(http ssh agent history ref-reject)

usage() {
  cat <<'USAGE'
Traces interop smoke dispatcher.

Usage:
  MEGA2_TRACES_INTEROP=1 scripts/traces_interop_smoke.sh --case <name>
  scripts/traces_interop_smoke.sh --list-cases

Cases: http, ssh, agent, history, ref-reject
Exit codes: 0 all cases passed; 1 at least one case failed; 2 usage/precondition.
USAGE
}

case="${MEGA2_TRACES_INTEROP_CASE:-}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --list-cases)
      printf '%s\n' "${CASES[@]}"
      exit 0
      ;;
    --case)
      [ "$#" -ge 2 ] || { echo "traces interop: --case requires a name" >&2; exit 2; }
      case="$2"
      shift 2
      ;;
    --case=*)
      case="${1#--case=}"
      shift
      ;;
    *)
      echo "traces interop: unknown argument '$1'" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ -z "$case" ]; then
  echo "traces interop: --case <name> is required" >&2
  usage >&2
  exit 2
fi

known=0
for candidate in "${CASES[@]}"; do
  [ "$candidate" = "$case" ] && known=1
done
if [ "$known" -ne 1 ]; then
  echo "traces interop: unknown case '$case'" >&2
  exit 2
fi

if [ "${MEGA2_TRACES_INTEROP:-}" != "1" ]; then
  echo "traces interop: set MEGA2_TRACES_INTEROP=1 to run against a live fixture" >&2
  exit 2
fi

driver="$ROOT/scripts/traces_interop_${case//-/_}.sh"
if [ ! -f "$driver" ]; then
  echo "traces interop: driver for case '$case' is not available: $driver" >&2
  exit 2
fi

exec bash "$driver"
