# Shared helpers for mega2 local IT scripts (sourced, not executed).
# Fact source: docs/development.md / docs/refactoring/test-infra.md

# shellcheck shell=bash

if [[ -n "${_MEGA2_IT_LIB_LOADED:-}" ]]; then
  return 0
fi
_MEGA2_IT_LIB_LOADED=1

_MEGA2_IT_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MEGA2_IT_ROOT="$(cd "${_MEGA2_IT_LIB_DIR}/../.." && pwd)"
MEGA2_IT_COMPOSE_FILE="${MEGA2_IT_ROOT}/docker-compose.test.yml"
MEGA2_IT_PROJECT="${MEGA2_IT_PROJECT:-mega2-it}"

mega2_it_die() {
  echo "error: $*" >&2
  exit 1
}

mega2_it_info() {
  echo "==> $*"
}

mega2_it_require_repo_root() {
  cd "${MEGA2_IT_ROOT}"
  [[ -f "${MEGA2_IT_COMPOSE_FILE}" ]] \
    || mega2_it_die "missing ${MEGA2_IT_COMPOSE_FILE}; run from mega2 checkout"
  [[ -f "${MEGA2_IT_ROOT}/Cargo.toml" ]] \
    || mega2_it_die "missing Cargo.toml at ${MEGA2_IT_ROOT}"
}

mega2_it_compose() {
  docker compose -p "${MEGA2_IT_PROJECT}" -f "${MEGA2_IT_COMPOSE_FILE}" "$@"
}

mega2_it_prepare_git_workdir() {
  local dir="${MEGA2_IT_GIT_WORKDIR:-/tmp/mega2-git}"
  mkdir -p "${dir}"
  chmod 1777 "${dir}"
  export MEGA2_IT_GIT_WORKDIR="${dir}"
  export MEGA2_IT_GIT_UID="${MEGA2_IT_GIT_UID:-$(id -u)}"
  export MEGA2_IT_GIT_GID="${MEGA2_IT_GIT_GID:-$(id -g)}"
  mega2_it_info "git workdir ${dir} (uid=${MEGA2_IT_GIT_UID} gid=${MEGA2_IT_GIT_GID})"
}

mega2_it_ensure_env_test() {
  local env_file="${MEGA2_IT_ROOT}/.env.test"
  local example="${MEGA2_IT_ROOT}/.env.test.example"
  if [[ ! -f "${env_file}" ]]; then
    [[ -f "${example}" ]] || mega2_it_die "missing ${example}"
    cp "${example}" "${env_file}"
    mega2_it_info "created ${env_file} from example"
  fi
  # shellcheck disable=SC1090
  set -a
  # shellcheck disable=SC1091
  source "${env_file}"
  set +a
  mega2_it_info "sourced ${env_file}"
}

mega2_it_up_data() {
  mega2_it_info "starting data plane (postgres/redis/rustfs/rustfs-init/mailpit)"
  mega2_it_compose up -d --wait
}

mega2_it_up_full() {
  mega2_it_prepare_git_workdir
  mega2_it_info "starting data plane + git-cli (--profile git)"
  mega2_it_compose --profile git up -d --wait
}

# Host directories the `scorpiofs` service bind-mounts with rshared
# propagation: <workdir>/mount (workspace) and <workdir>/antares (Antares mount
# root). ScorpioFS refuses a non-empty mountpoint, and Docker would auto-create
# a missing source root-owned, so create them as the test user before `up`.
mega2_it_prepare_scorpio_workdir() {
  local dir="${MEGA2_IT_SCORPIO_WORKDIR:-/tmp/mega2-scorpiofs}"
  export MEGA2_IT_SCORPIO_WORKDIR="${dir}"
  # A running daemon legitimately has <workdir>/mount mounted and non-empty.
  if [[ -n "$(mega2_it_compose --profile app --profile scorpio ps -q --status running scorpiofs 2>/dev/null)" ]]; then
    mega2_it_info "scorpiofs already running; keeping workdir ${dir}"
    return 0
  fi
  local stale
  stale="$(findmnt -rn -o TARGET 2>/dev/null | grep -E "^${dir}(/|$)" || true)"
  if [[ -n "${stale}" ]]; then
    mega2_it_die "stale FUSE mounts under ${dir} (SIGKILLed scorpiofs?); unmount with: sudo umount -l <path>"$'\n'"${stale}"
  fi
  mkdir -p "${dir}/mount" "${dir}/antares"
  # The workspace mountpoint itself must be empty (ScorpioFS refuses otherwise).
  if [[ -n "$(ls -A "${dir}/mount" 2>/dev/null)" ]]; then
    mega2_it_die "${dir}/mount must be empty (ScorpioFS refuses a non-empty mountpoint)"
  fi
  # Antares mounts at <root>/<mount_id>; a previous run may leave empty
  # per-mount directories behind (mode 0777, removable by the parent's owner).
  find "${dir}/antares" -mindepth 1 -maxdepth 1 -type d -empty -exec rmdir {} + 2>/dev/null || true
  mega2_it_info "scorpiofs workdir ${dir} (host-visible FUSE mounts: ${dir}/mount, ${dir}/antares/<mount_id>)"
}

# True when <path> is itself a mountpoint on the host (findmnt exact match).
mega2_it_scorpio_is_mounted() {
  command -v findmnt >/dev/null 2>&1 || return 1
  findmnt -n -o TARGET "$1" >/dev/null 2>&1
}

# After `down`: report FUSE mounts that survived (SIGKILLed daemon), otherwise
# remove the now-empty mountpoint directories.
mega2_it_cleanup_scorpio_workdir() {
  local dir="${MEGA2_IT_SCORPIO_WORKDIR:-/tmp/mega2-scorpiofs}"
  [[ -d "${dir}" ]] || return 0
  local stale=""
  local target
  while IFS= read -r target; do
    [[ -n "${target}" ]] && stale+="${target}"$'\n'
  done < <(findmnt -rn -o TARGET 2>/dev/null | grep -E "^${dir}(/|$)" || true)
  if [[ -n "${stale}" ]]; then
    echo "warning: stale FUSE mounts under ${dir} (unmount with: sudo umount -l <path>):" >&2
    printf '%s' "${stale}" >&2
    return 0
  fi
  # Only ever remove EMPTY directories (leftover Antares mountpoints, then the
  # two roots); anything else under the workdir is left for the user.
  find "${dir}/antares" -mindepth 1 -maxdepth 1 -type d -empty -exec rmdir {} + 2>/dev/null || true
  rmdir "${dir}/mount" "${dir}/antares" "${dir}" 2>/dev/null || true
}

# ScorpioFS linked to the compose-hosted mega2 (profiles app + scorpio).
# `scorpiofs:local` is built from the sibling checkout `../scorpiofs` on the
# first run (or with `--build`); see docs/refactoring/test-infra.md.
mega2_it_up_scorpio() {
  local scorpiofs_dir="${MEGA2_IT_ROOT}/../scorpiofs"
  [[ -f "${scorpiofs_dir}/Dockerfile" ]] \
    || mega2_it_die "missing sibling checkout ${scorpiofs_dir} (needed to build scorpiofs:local)"
  mega2_it_prepare_scorpio_workdir
  mega2_it_info "starting data plane + mega2 + scorpiofs (--profile app --profile scorpio)"
  mega2_it_compose --profile app --profile scorpio up -d --wait "$@"
}

mega2_it_down() {
  mega2_it_info "tearing down ${MEGA2_IT_PROJECT} (profiles git+app+web+smoke+scorpio, -v)"
  mega2_it_compose --profile git --profile app --profile web --profile smoke --profile scorpio down -v
  mega2_it_cleanup_scorpio_workdir
}

mega2_it_health() {
  mega2_it_info "postgres"
  mega2_it_compose exec -T postgres pg_isready -U mega2 -d mega2
  mega2_it_info "redis"
  mega2_it_compose exec -T redis redis-cli ping
  mega2_it_info "mailpit"
  curl -fsS "http://127.0.0.1:18025/api/v1/messages" >/dev/null
  echo "OK: data-plane health checks passed"
}
