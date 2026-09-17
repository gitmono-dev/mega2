#!/usr/bin/env bash
# Local test runners for mega2 (see docs/development.md).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/lib/mega2-it.sh"

usage() {
  cat <<'USAGE'
mega2 local test helpers (docs/development.md)

Usage:
  scripts/dev-test.sh <command> [args...]

Stack commands:
  up-data              Start default data plane (postgres/redis/rustfs/rustfs-init/mailpit)
  up-full              Start data plane + git-cli (recommended for cargo test --all)
  up-scorpio [args]    Start data plane + mega2 + scorpiofs (--profile app --profile scorpio);
                       builds scorpiofs:local from ../scorpiofs on first run (pass --build to rebuild)
  down                 Tear down project (profiles git/app/web/smoke/scorpio, volumes)
  health               Postgres / Redis / Mailpit smoke checks

Test commands:
  unit [cargo-args...]          Unit tests only (no compose): cargo test -p mega2-core --lib
  basic [cargo-args...]         up-data + source .env.test + cargo test --all
  full [cargo-args...]          up-full + source .env.test + cargo test --all  (recommended IT)
  vault [cargo-args...]         Ensure data plane, then integration_vault
  git-cli [cargo-args...]       Ensure full stack, then integration_git_cli
  scorpio-smoke                 Stack-level ScorpioFS smoke against the linked mega2 (scripts/scorpiofs_smoke.sh)
  gates                         Submit gates: nightly fmt check, clippy -D warnings, full IT tests

Examples:
  ./scripts/dev-test.sh full
  ./scripts/dev-test.sh vault -- --nocapture --test-threads=1
  ./scripts/dev-test.sh unit config::
  ./scripts/dev-test.sh up-scorpio && ./scripts/dev-test.sh scorpio-smoke
  ./scripts/dev-test.sh down

Environment:
  MEGA2_IT_PROJECT       Compose project name (default: mega2-it)
  MEGA2_IT_GIT_WORKDIR   Shared git-cli host dir (default: /tmp/mega2-git)
  MEGA2_IT_GIT_UID/GID   Container user (default: current id -u/-g)
  MEGA2_IT_SCORPIO_URL   ScorpioFS HTTP API on the host (default: http://127.0.0.1:12725)
USAGE
}

cmd_unit() {
  mega2_it_require_repo_root
  mega2_it_info "cargo test -p mega2-core --lib $*"
  cargo test -p mega2-core --lib "$@"
}

cmd_basic() {
  mega2_it_require_repo_root
  mega2_it_up_data
  mega2_it_ensure_env_test
  mega2_it_info "cargo test --all $*"
  mega2_it_info "note: without git-cli, integration_git_cli will fail; use 'full' for complete IT"
  cargo test --all "$@"
}

cmd_full() {
  mega2_it_require_repo_root
  mega2_it_up_full
  mega2_it_ensure_env_test
  mega2_it_info "cargo test --all $*"
  mega2_it_info "tip: uncomment MEGA_OBJECT_STORAGE__* in .env.test for RustFS S3 smoke"
  cargo test --all "$@"
}

cmd_vault() {
  mega2_it_require_repo_root
  mega2_it_up_data
  mega2_it_ensure_env_test
  if [[ $# -eq 0 ]]; then
    set -- -- --nocapture --test-threads=1
  fi
  mega2_it_info "cargo test -p mega2 --test integration_vault $*"
  cargo test -p mega2 --test integration_vault "$@"
}

cmd_git_cli() {
  mega2_it_require_repo_root
  mega2_it_up_full
  mega2_it_ensure_env_test
  if [[ $# -eq 0 ]]; then
    set -- -- --nocapture --test-threads=1
  fi
  mega2_it_info "cargo test -p mega2 --test integration_git_cli $*"
  cargo test -p mega2 --test integration_git_cli "$@"
}

cmd_scorpio_smoke() {
  mega2_it_require_repo_root
  mega2_it_info "bash scripts/scorpiofs_smoke.sh"
  bash "${SCRIPT_DIR}/scorpiofs_smoke.sh"
}

cmd_gates() {
  mega2_it_require_repo_root
  mega2_it_up_full
  mega2_it_ensure_env_test
  mega2_it_info "cargo +nightly fmt --all --check"
  cargo +nightly fmt --all --check
  mega2_it_info "cargo clippy --all-targets --all-features -- -D warnings"
  cargo clippy --all-targets --all-features -- -D warnings
  mega2_it_info "cargo test --all"
  cargo test --all
  echo "OK: submit gates passed"
}

main() {
  local cmd="${1:-}"
  if [[ -z "${cmd}" || "${cmd}" == "-h" || "${cmd}" == "--help" ]]; then
    usage
    exit 0
  fi
  shift || true

  case "${cmd}" in
    up-data)
      mega2_it_require_repo_root
      mega2_it_up_data
      ;;
    up-full)
      mega2_it_require_repo_root
      mega2_it_up_full
      ;;
    up-scorpio)
      mega2_it_require_repo_root
      mega2_it_up_scorpio "$@"
      ;;
    down)
      mega2_it_require_repo_root
      mega2_it_down
      ;;
    health)
      mega2_it_require_repo_root
      mega2_it_health
      ;;
    unit)
      cmd_unit "$@"
      ;;
    basic)
      cmd_basic "$@"
      ;;
    full)
      cmd_full "$@"
      ;;
    vault)
      cmd_vault "$@"
      ;;
    git-cli)
      cmd_git_cli "$@"
      ;;
    scorpio-smoke)
      cmd_scorpio_smoke
      ;;
    gates)
      cmd_gates
      ;;
    *)
      usage >&2
      mega2_it_die "unknown command: ${cmd}"
      ;;
  esac
}

main "$@"
