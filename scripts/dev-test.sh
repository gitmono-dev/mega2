#!/usr/bin/env bash
# Local test runners for monoengine (see docs/development.md).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/lib/monoengine-it.sh"

usage() {
  cat <<'USAGE'
monoengine local test helpers (docs/development.md)

Usage:
  scripts/dev-test.sh <command> [args...]

Stack commands:
  up-data              Start default data plane (postgres/redis/rustfs/rustfs-init/mailpit)
  up-full              Start data plane + git-cli (Linux; recommended for cargo test --all)
  down                 Tear down project (profiles git/app/web, volumes)
  health               Postgres / Redis / Mailpit smoke checks

Test commands:
  unit [cargo-args...]          Unit tests only (no compose): cargo test -p monoengine-core --lib
  basic [cargo-args...]         up-data + source .env.test + cargo test --all
  full [cargo-args...]          up-full + source .env.test + cargo test --all  (recommended IT)
  vault [cargo-args...]         Ensure data plane, then integration_vault
  git-cli [cargo-args...]       Ensure full stack, then integration_git_cli
  gates                         Submit gates: nightly fmt check, clippy -D warnings, full IT tests

Examples:
  ./scripts/dev-test.sh full
  ./scripts/dev-test.sh vault -- --nocapture --test-threads=1
  ./scripts/dev-test.sh unit config::
  ./scripts/dev-test.sh down

Environment:
  MONOENGINE_IT_PROJECT       Compose project name (default: monoengine-it)
  MONOENGINE_IT_GIT_WORKDIR   Shared git-cli host dir (default: /tmp/monoengine-git)
  MONOENGINE_IT_GIT_UID/GID   Container user (default: current id -u/-g)
USAGE
}

cmd_unit() {
  monoengine_it_require_repo_root
  monoengine_it_info "cargo test -p monoengine-core --lib $*"
  cargo test -p monoengine-core --lib "$@"
}

cmd_basic() {
  monoengine_it_require_repo_root
  monoengine_it_up_data
  monoengine_it_ensure_env_test
  monoengine_it_info "cargo test --all $*"
  monoengine_it_info "note: without git-cli, integration_git_cli will fail; use 'full' for complete IT"
  cargo test --all "$@"
}

cmd_full() {
  monoengine_it_require_repo_root
  monoengine_it_up_full
  monoengine_it_ensure_env_test
  monoengine_it_info "cargo test --all $*"
  monoengine_it_info "tip: uncomment MEGA_OBJECT_STORAGE__* in .env.test for RustFS S3 smoke"
  cargo test --all "$@"
}

cmd_vault() {
  monoengine_it_require_repo_root
  monoengine_it_up_data
  monoengine_it_ensure_env_test
  if [[ $# -eq 0 ]]; then
    set -- -- --nocapture --test-threads=1
  fi
  monoengine_it_info "cargo test -p monoengine --test integration_vault $*"
  cargo test -p monoengine --test integration_vault "$@"
}

cmd_git_cli() {
  monoengine_it_require_repo_root
  monoengine_it_up_full
  monoengine_it_ensure_env_test
  if [[ $# -eq 0 ]]; then
    set -- -- --nocapture --test-threads=1
  fi
  monoengine_it_info "cargo test -p monoengine --test integration_git_cli $*"
  cargo test -p monoengine --test integration_git_cli "$@"
}

cmd_gates() {
  monoengine_it_require_repo_root
  monoengine_it_up_full
  monoengine_it_ensure_env_test
  monoengine_it_info "cargo +nightly fmt --all --check"
  cargo +nightly fmt --all --check
  monoengine_it_info "cargo clippy --all-targets --all-features -- -D warnings"
  cargo clippy --all-targets --all-features -- -D warnings
  monoengine_it_info "cargo test --all"
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
      monoengine_it_require_repo_root
      monoengine_it_up_data
      ;;
    up-full)
      monoengine_it_require_repo_root
      monoengine_it_up_full
      ;;
    down)
      monoengine_it_require_repo_root
      monoengine_it_down
      ;;
    health)
      monoengine_it_require_repo_root
      monoengine_it_health
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
    gates)
      cmd_gates
      ;;
    *)
      usage >&2
      monoengine_it_die "unknown command: ${cmd}"
      ;;
  esac
}

main "$@"
