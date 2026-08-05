# Shared helpers for monoengine local IT scripts (sourced, not executed).
# Fact source: docs/development.md / docs/refactoring/test-infra.md

# shellcheck shell=bash

if [[ -n "${_MONOENGINE_IT_LIB_LOADED:-}" ]]; then
  return 0
fi
_MONOENGINE_IT_LIB_LOADED=1

_MONOENGINE_IT_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MONOENGINE_IT_ROOT="$(cd "${_MONOENGINE_IT_LIB_DIR}/../.." && pwd)"
MONOENGINE_IT_COMPOSE_FILE="${MONOENGINE_IT_ROOT}/docker-compose.test.yml"
MONOENGINE_IT_PROJECT="${MONOENGINE_IT_PROJECT:-monoengine-it}"

monoengine_it_die() {
  echo "error: $*" >&2
  exit 1
}

monoengine_it_info() {
  echo "==> $*"
}

monoengine_it_require_repo_root() {
  cd "${MONOENGINE_IT_ROOT}"
  [[ -f "${MONOENGINE_IT_COMPOSE_FILE}" ]] \
    || monoengine_it_die "missing ${MONOENGINE_IT_COMPOSE_FILE}; run from monoengine checkout"
  [[ -f "${MONOENGINE_IT_ROOT}/Cargo.toml" ]] \
    || monoengine_it_die "missing Cargo.toml at ${MONOENGINE_IT_ROOT}"
}

monoengine_it_compose() {
  docker compose -p "${MONOENGINE_IT_PROJECT}" -f "${MONOENGINE_IT_COMPOSE_FILE}" "$@"
}

monoengine_it_prepare_git_workdir() {
  local dir="${MONOENGINE_IT_GIT_WORKDIR:-/tmp/monoengine-git}"
  mkdir -p "${dir}"
  chmod 1777 "${dir}"
  export MONOENGINE_IT_GIT_WORKDIR="${dir}"
  export MONOENGINE_IT_GIT_UID="${MONOENGINE_IT_GIT_UID:-$(id -u)}"
  export MONOENGINE_IT_GIT_GID="${MONOENGINE_IT_GIT_GID:-$(id -g)}"
  monoengine_it_info "git workdir ${dir} (uid=${MONOENGINE_IT_GIT_UID} gid=${MONOENGINE_IT_GIT_GID})"
}

monoengine_it_ensure_env_test() {
  local env_file="${MONOENGINE_IT_ROOT}/.env.test"
  local example="${MONOENGINE_IT_ROOT}/.env.test.example"
  if [[ ! -f "${env_file}" ]]; then
    [[ -f "${example}" ]] || monoengine_it_die "missing ${example}"
    cp "${example}" "${env_file}"
    monoengine_it_info "created ${env_file} from example"
  fi
  # shellcheck disable=SC1090
  set -a
  # shellcheck disable=SC1091
  source "${env_file}"
  set +a
  monoengine_it_info "sourced ${env_file}"
}

monoengine_it_require_linux_for_git() {
  if [[ "$(uname -s)" != "Linux" ]]; then
    monoengine_it_die "git-cli profile requires Linux (host networking); uname=$(uname -s)"
  fi
}

monoengine_it_up_data() {
  monoengine_it_info "starting data plane (postgres/redis/rustfs/rustfs-init/mailpit)"
  monoengine_it_compose up -d --wait
}

monoengine_it_up_full() {
  monoengine_it_require_linux_for_git
  monoengine_it_prepare_git_workdir
  monoengine_it_info "starting data plane + git-cli (--profile git)"
  monoengine_it_compose --profile git up -d --wait
}

monoengine_it_down() {
  monoengine_it_info "tearing down ${MONOENGINE_IT_PROJECT} (profiles git+app+web, -v)"
  monoengine_it_compose --profile git --profile app --profile web down -v
}

monoengine_it_health() {
  monoengine_it_info "postgres"
  monoengine_it_compose exec -T postgres pg_isready -U monoengine -d monoengine
  monoengine_it_info "redis"
  monoengine_it_compose exec -T redis redis-cli ping
  monoengine_it_info "mailpit"
  curl -fsS "http://127.0.0.1:18025/api/v1/messages" >/dev/null
  echo "OK: data-plane health checks passed"
}
