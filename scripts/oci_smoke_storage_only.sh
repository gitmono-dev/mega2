#!/usr/bin/env bash
# Storage-only OCI Distribution (/v2) live smoke via host docker CLI.
# Independent of scripts/git_protocol_smoke_storage_only.sh (do not modify that).
#
# Prerequisites:
#   - Host docker CLI + daemon (DEP-DR-02)
#   - Reachable storage-only registry (default http://127.0.0.1:9000 with [oci] enabled)
#   - Push token: MEGA2_OCI_SMOKE_TOKEN, or secrets/mega2-push-token.local
#
# SKIP policy: docker missing/daemon down OR registry unreachable → all cases SKIP,
# explicit reason, exit 0 (never FAIL those preconditions).
#
# Note: VER may invoke with PATH=/nonexistent — early docker-missing SKIP uses only
# bash builtins; later steps use absolute paths for common tools when needed.
set -euo pipefail

# Absolute fallbacks for PATH=/nonexistent and minimal environments.
MKTEMP_BIN="${MKTEMP_BIN:-$(command -v mktemp 2>/dev/null || true)}"
[[ -n "${MKTEMP_BIN}" ]] || { [[ -x /usr/bin/mktemp ]] && MKTEMP_BIN=/usr/bin/mktemp; }
[[ -n "${MKTEMP_BIN}" ]] || { [[ -x /bin/mktemp ]] && MKTEMP_BIN=/bin/mktemp; }
TR_BIN="${TR_BIN:-$(command -v tr 2>/dev/null || true)}"
[[ -n "${TR_BIN}" ]] || { [[ -x /usr/bin/tr ]] && TR_BIN=/usr/bin/tr; }
RM_BIN="${RM_BIN:-$(command -v rm 2>/dev/null || true)}"
[[ -n "${RM_BIN}" ]] || { [[ -x /bin/rm ]] && RM_BIN=/bin/rm; }
MKDIR_BIN="${MKDIR_BIN:-$(command -v mkdir 2>/dev/null || true)}"
[[ -n "${MKDIR_BIN}" ]] || { [[ -x /bin/mkdir ]] && MKDIR_BIN=/bin/mkdir; }
TAR_BIN="${TAR_BIN:-$(command -v tar 2>/dev/null || true)}"
[[ -n "${TAR_BIN}" ]] || { [[ -x /usr/bin/tar ]] && TAR_BIN=/usr/bin/tar; }
[[ -n "${TAR_BIN}" ]] || { [[ -x /bin/tar ]] && TAR_BIN=/bin/tar; }

usage() {
  # Avoid `cat` so --help still works when PATH is empty.
  printf '%s\n' \
    'Storage-only OCI /v2 docker CLI smoke (login → import → push → pull → logout).' \
    '' \
    'Optional environment:' \
    '  MEGA2_OCI_SMOKE_REGISTRY   Registry base URL (default http://127.0.0.1:9000)' \
    '  MEGA2_OCI_SMOKE_TOKEN      Push token password for docker login' \
    '                                 (default: secrets/mega2-push-token.local)' \
    '  MEGA2_OCI_SMOKE_USER       docker login username (default: oci-smoke; ignored by server)' \
    '  MEGA2_OCI_SMOKE_REPO       Multi-segment repo name (default: team/oci-smoke)' \
    '  MEGA2_OCI_SMOKE_TAG        Image tag (default: smoke)' \
    '  MEGA2_OCI_SMOKE_WORKDIR    Existing workdir for rootfs/tar (default: mktemp)' \
    '  MEGA2_OCI_SMOKE_KEEP_WORKDIR  Set to 1 to keep workdir' \
    '  MEGA2_SMOKE_CASE           Exact case name; run only that case (unmatched → exit 2)' \
    '' \
    'Examples:' \
    '  bash scripts/oci_smoke_storage_only.sh' \
    '  MEGA2_OCI_SMOKE_REGISTRY=http://127.0.0.1:9000 bash scripts/oci_smoke_storage_only.sh' \
    '  env PATH="/nonexistent" /bin/bash scripts/oci_smoke_storage_only.sh   # docker-missing SKIP' \
    '  MEGA2_OCI_SMOKE_REGISTRY=http://127.0.0.1:1 bash scripts/oci_smoke_storage_only.sh  # unreachable SKIP'
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
CASE_FILTER="${MEGA2_SMOKE_CASE:-}"
CASE_HIT=0

print_summary() {
  echo "oci smoke storage_only summary: ${PASS_COUNT} passed, ${FAIL_COUNT} failed (${SKIP_COUNT} skipped)"
}

skip_case() {
  local name="$1"
  local reason="${2:-skipped}"
  if [[ -n "${CASE_FILTER}" && "${name}" != "${CASE_FILTER}" ]]; then
    return 0
  fi
  CASE_HIT=1
  echo "SKIP: ${name} (${reason})"
  SKIP_COUNT=$((SKIP_COUNT + 1))
}

skip_all_cases() {
  local reason="$1"
  skip_case "docker login" "${reason}"
  skip_case "push/pull roundtrip" "${reason}"
  skip_case "login without token presents 401" "${reason}"
  skip_case "docker logout" "${reason}"
  if [[ -n "${CASE_FILTER}" && "${CASE_HIT}" -eq 0 ]]; then
    echo "FAIL: MEGA2_SMOKE_CASE='${CASE_FILTER}' matched no registered case" >&2
    print_summary
    exit 2
  fi
  print_summary
  exit 0
}

# VER PATH=/nonexistent: detect missing docker with builtins only, before dirname/mktemp.
if ! command -v docker >/dev/null 2>&1; then
  skip_all_cases "docker binary not found"
fi

# Resolve script/repo paths with builtins (no dirname).
_SRC="${BASH_SOURCE[0]}"
if [[ "${_SRC}" == /* ]]; then
  SCRIPT_DIR="${_SRC%/*}"
elif [[ "${_SRC}" == */* ]]; then
  SCRIPT_DIR="$(cd "${_SRC%/*}" && pwd)"
else
  SCRIPT_DIR="$(pwd)"
fi
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

REGISTRY_RAW="${MEGA2_OCI_SMOKE_REGISTRY:-http://127.0.0.1:9000}"
REGISTRY="${REGISTRY_RAW%/}"
case "${REGISTRY}" in
http://*) HOST="${REGISTRY#http://}" ;;
https://*) HOST="${REGISTRY#https://}" ;;
*) HOST="${REGISTRY}" ;;
esac

SMOKE_USER="${MEGA2_OCI_SMOKE_USER:-oci-smoke}"
SMOKE_REPO="${MEGA2_OCI_SMOKE_REPO:-team/oci-smoke}"
SMOKE_TAG="${MEGA2_OCI_SMOKE_TAG:-smoke}"
IMAGE="${HOST}/${SMOKE_REPO}:${SMOKE_TAG}"

TOKEN=""
load_token() {
  if [[ -n "${MEGA2_OCI_SMOKE_TOKEN:-}" ]]; then
    TOKEN="${MEGA2_OCI_SMOKE_TOKEN}"
    return 0
  fi
  local token_file="${REPO_ROOT}/secrets/mega2-push-token.local"
  if [[ -f "${token_file}" ]]; then
    if [[ -n "${TR_BIN}" ]]; then
      TOKEN="$("${TR_BIN}" -d '\r\n' <"${token_file}")"
    else
      TOKEN="$(<"${token_file}")"
      TOKEN="${TOKEN//$'\r'/}"
      TOKEN="${TOKEN//$'\n'/}"
    fi
    return 0
  fi
  echo "push token missing: set MEGA2_OCI_SMOKE_TOKEN or create secrets/mega2-push-token.local" >&2
  return 1
}

# Redact token from captured CLI output (ER-11). Literal replace only —
# bash `${text//${TOKEN}/…}` treats TOKEN as a glob pattern.
sanitize() {
  local text="$1"
  if [[ -z "${TOKEN}" ]]; then
    printf '%s' "${text}"
    return 0
  fi
  local py_bin=""
  py_bin="$(command -v python3 2>/dev/null || true)"
  [[ -n "${py_bin}" ]] || { [[ -x /usr/bin/python3 ]] && py_bin=/usr/bin/python3; }
  if [[ -n "${py_bin}" ]]; then
    text="$(
      TOKEN="${TOKEN}" TEXT="${text}" "${py_bin}" -c \
        'import os; print(os.environ["TEXT"].replace(os.environ["TOKEN"], "<redacted>"), end="")'
    )"
    printf '%s' "${text}"
    return 0
  fi
  # Fallback: escape glob metacharacters so // matches TOKEN literally.
  local escaped="" i c
  for ((i = 0; i < ${#TOKEN}; i++)); do
    c="${TOKEN:i:1}"
    # Escape bash glob metacharacters (one char each: \ * ? [ ]).
    case "${c}" in
      '\' | '*' | '?' | '[' | ']') escaped+="\\${c}" ;;
      *) escaped+="${c}" ;;
    esac
  done
  text="${text//${escaped}/<redacted>}"
  printf '%s' "${text}"
}

if [[ -n "${MEGA2_OCI_SMOKE_WORKDIR:-}" ]]; then
  ROOT_DIR="${MEGA2_OCI_SMOKE_WORKDIR}"
else
  if [[ -z "${MKTEMP_BIN}" ]]; then
    echo "mktemp not found" >&2
    exit 2
  fi
  ROOT_DIR="$("${MKTEMP_BIN}" -d)"
fi
if [[ ! -d "${ROOT_DIR}" ]]; then
  "${MKDIR_BIN:-mkdir}" -p "${ROOT_DIR}"
fi

cleanup() {
  if [[ -z "${MEGA2_OCI_SMOKE_WORKDIR:-}" && "${MEGA2_OCI_SMOKE_KEEP_WORKDIR:-}" != "1" ]]; then
    "${RM_BIN:-rm}" -rf "${ROOT_DIR}"
  else
    echo "smoke workdir kept at: ${ROOT_DIR}"
  fi
  docker rmi -f "${IMAGE}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

run_case() {
  local name="$1"
  shift
  if [[ -n "${CASE_FILTER}" && "${name}" != "${CASE_FILTER}" ]]; then
    return 0
  fi
  CASE_HIT=1
  echo "==> ${name}"
  if "$@"; then
    echo "PASS: ${name}"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    echo "FAIL: ${name}" >&2
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

curl_bin() {
  if command -v curl >/dev/null 2>&1; then
    command -v curl
  elif [[ -x /usr/bin/curl ]]; then
    echo /usr/bin/curl
  elif [[ -x /bin/curl ]]; then
    echo /bin/curl
  else
    return 1
  fi
}

probe_docker_daemon() {
  local info_err
  if ! info_err="$(docker info 2>&1 1>/dev/null)"; then
    local detail
    detail="$(sanitize "${info_err}")"
    detail="${detail//$'\n'/ }"
    while [[ "${detail}" == *"  "* ]]; do detail="${detail//  / }"; done
    detail="${detail:0:160}"
    if [[ -n "${detail}" ]]; then
      echo "docker daemon unavailable: ${detail}"
    else
      echo "docker daemon unavailable"
    fi
    return 1
  fi
  return 0
}

probe_registry() {
  local curl_path http_code curl_status
  if ! curl_path="$(curl_bin)"; then
    echo "curl not found (cannot probe registry)"
    return 1
  fi
  set +e
  http_code="$("${curl_path}" -sS -o /dev/null -w '%{http_code}' \
    --connect-timeout 2 --max-time 5 "${REGISTRY}/v2/" 2>/dev/null)"
  curl_status=$?
  set -e
  if [[ "${curl_status}" -ne 0 || -z "${http_code}" || "${http_code}" == "000" ]]; then
    echo "registry unreachable at ${REGISTRY}"
    return 1
  fi
  return 0
}

case_docker_login() {
  local out status
  set +e
  out="$(printf '%s\n' "${TOKEN}" | docker login -u "${SMOKE_USER}" --password-stdin "${HOST}" 2>&1)"
  status=$?
  set -e
  if [[ "${status}" -ne 0 ]]; then
    echo "docker login failed: $(sanitize "${out}")" >&2
    return 1
  fi
  return 0
}

case_push_pull_roundtrip() {
  local rootfs tar_path out status digest layers

  rootfs="${ROOT_DIR}/rootfs"
  tar_path="${ROOT_DIR}/rootfs.tar"
  "${RM_BIN:-rm}" -rf "${rootfs}"
  "${MKDIR_BIN:-mkdir}" -p "${rootfs}"
  printf 'mega2-oci-smoke %s\n' "$(date -u +%Y%m%dT%H%M%SZ)-$$" >"${rootfs}/hello.txt"

  if [[ -z "${TAR_BIN}" ]]; then
    echo "tar not found" >&2
    return 1
  fi
  if ! "${TAR_BIN}" -cf "${tar_path}" -C "${rootfs}" .; then
    echo "tar rootfs failed" >&2
    return 1
  fi

  set +e
  out="$(docker import "${tar_path}" "${IMAGE}" 2>&1)"
  status=$?
  set -e
  if [[ "${status}" -ne 0 ]]; then
    echo "docker import failed: $(sanitize "${out}")" >&2
    return 1
  fi

  set +e
  out="$(docker push "${IMAGE}" 2>&1)"
  status=$?
  set -e
  if [[ "${status}" -ne 0 ]]; then
    echo "docker push failed: $(sanitize "${out}")" >&2
    return 1
  fi

  docker rmi -f "${IMAGE}" >/dev/null 2>&1 || true

  set +e
  out="$(docker pull "${IMAGE}" 2>&1)"
  status=$?
  set -e
  if [[ "${status}" -ne 0 ]]; then
    echo "docker pull failed: $(sanitize "${out}")" >&2
    return 1
  fi

  digest="$(docker image inspect --format '{{index .RepoDigests 0}}' "${IMAGE}" 2>/dev/null || true)"
  if [[ -z "${digest}" || "${digest}" != *sha256:* ]]; then
    echo "expected RepoDigest with sha256 after pull, got: $(sanitize "${digest}")" >&2
    return 1
  fi

  layers="$(docker image inspect --format '{{len .RootFS.Layers}}' "${IMAGE}" 2>/dev/null || true)"
  if [[ -z "${layers}" || "${layers}" -lt 1 ]]; then
    echo "expected at least one RootFS layer after pull, got: ${layers}" >&2
    return 1
  fi

  echo "roundtrip digest=${digest} layers=${layers}"
  return 0
}

case_login_without_token_presents_401() {
  local out status rootfs tar_path probe_image

  # anonymous_access=true makes GET /v2/ return 200 without credentials, so
  # `docker login` with a bad password may still exit 0. Assert 401 on a write
  # path after clearing stored creds (matches AC: 无 token → docker 呈现 401).
  docker logout "${HOST}" >/dev/null 2>&1 || true

  probe_image="${HOST}/${SMOKE_REPO}:unauth-$$"
  rootfs="${ROOT_DIR}/unauth-rootfs"
  tar_path="${ROOT_DIR}/unauth-rootfs.tar"
  "${RM_BIN:-rm}" -rf "${rootfs}"
  "${MKDIR_BIN:-mkdir}" -p "${rootfs}"
  printf 'unauth-probe\n' >"${rootfs}/x.txt"
  if [[ -z "${TAR_BIN}" ]]; then
    echo "tar not found" >&2
    return 1
  fi
  "${TAR_BIN}" -cf "${tar_path}" -C "${rootfs}" . || return 1

  set +e
  out="$(docker import "${tar_path}" "${probe_image}" 2>&1)"
  status=$?
  set -e
  if [[ "${status}" -ne 0 ]]; then
    echo "docker import (unauth probe) failed: $(sanitize "${out}")" >&2
    return 1
  fi

  set +e
  out="$(docker push "${probe_image}" 2>&1)"
  status=$?
  set -e
  docker rmi -f "${probe_image}" >/dev/null 2>&1 || true

  if [[ "${status}" -eq 0 ]]; then
    echo "expected docker push without token to fail" >&2
    return 1
  fi

  # AC: docker must surface HTTP 401 (not merely generic denial wording).
  local grep_bin
  grep_bin="$(command -v grep 2>/dev/null || true)"
  [[ -n "${grep_bin}" ]] || { [[ -x /usr/bin/grep ]] && grep_bin=/usr/bin/grep; }
  [[ -n "${grep_bin}" ]] || { [[ -x /bin/grep ]] && grep_bin=/bin/grep; }
  if [[ -z "${grep_bin}" ]] || ! printf '%s\n' "${out}" | "${grep_bin}" -qE '401'; then
    echo "expected HTTP 401 in docker push output: $(sanitize "${out}")" >&2
    return 1
  fi
  return 0
}

case_docker_logout() {
  local out status
  set +e
  out="$(docker logout "${HOST}" 2>&1)"
  status=$?
  set -e
  if [[ "${status}" -ne 0 ]]; then
    echo "docker logout failed: $(sanitize "${out}")" >&2
    return 1
  fi
  return 0
}

# --- Preflight: daemon + registry (SKIP all, exit 0 on failure) ---

DOCKER_REASON=""
if ! DOCKER_REASON="$(probe_docker_daemon)"; then
  :
else
  DOCKER_REASON=""
fi

if [[ -n "${DOCKER_REASON}" ]]; then
  skip_all_cases "${DOCKER_REASON}"
fi

REG_REASON=""
if ! REG_REASON="$(probe_registry)"; then
  :
else
  REG_REASON=""
fi

if [[ -n "${REG_REASON}" ]]; then
  skip_all_cases "${REG_REASON}"
fi

if ! load_token; then
  echo "FAIL: cannot load push token" >&2
  FAIL_COUNT=$((FAIL_COUNT + 1))
  print_summary
  exit 1
fi

# --- Live cases ---

run_case "docker login" case_docker_login
run_case "push/pull roundtrip" case_push_pull_roundtrip
run_case "login without token presents 401" case_login_without_token_presents_401
run_case "docker logout" case_docker_logout

if [[ -n "${CASE_FILTER}" && "${CASE_HIT}" -eq 0 ]]; then
  echo "FAIL: MEGA2_SMOKE_CASE='${CASE_FILTER}' matched no registered case" >&2
  print_summary
  exit 2
fi

print_summary
if [[ "${FAIL_COUNT}" -gt 0 ]]; then
  exit 1
fi
exit 0
