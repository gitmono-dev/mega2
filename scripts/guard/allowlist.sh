#!/usr/bin/env bash
# Compare baseline ∪ current dirty fingerprints against an allowlist.
# Usage: allowlist.sh <baseline> <allowed-path>...
# Exit: 0 pass, 1 out of allowlist, 2 command or input error.
set -euo pipefail

if [[ $# -lt 2 || -z $1 ]]; then
  echo "usage: allowlist.sh <baseline> <allowed-path>..." >&2
  exit 2
fi

baseline=$1
shift

if [[ ! -f $baseline ]]; then
  echo "allowlist.sh: missing baseline" >&2
  exit 2
fi

if [[ $# -lt 1 ]]; then
  echo "usage: allowlist.sh <baseline> <allowed-path>..." >&2
  exit 2
fi

printf 'ok\n' | grep -F ok >/dev/null || exit 2

here=$(cd "$(dirname "$0")" && pwd)

declare -A allowed=()
for path in "$@"; do
  allowed[$path]=1
done

fingerprint_line() {
  local path=$1
  if [[ $path == *$'\n'* || $path == *$'\t'* ]]; then
    echo "allowlist.sh: path contains tab or newline" >&2
    exit 2
  fi
  if [[ -L $path ]]; then
    local target
    target=$(readlink -- "$path") || exit 2
    if [[ $target == *"  "* ]]; then
      echo "allowlist.sh: symlink target contains double space" >&2
      exit 2
    fi
    printf '%s' "symlink|-|${target}  ${path}"
    return 0
  fi
  if [[ ! -e $path ]]; then
    printf '%s' "absent|-|-  ${path}"
    return 0
  fi
  if [[ -f $path ]]; then
    local mode hash
    mode=$(stat -c '%a' -- "$path") || exit 2
    hash=$(sha256sum -- "$path") || exit 2
    hash=${hash%% *}
    hash=${hash#\\}
    printf '%s' "file|${mode}|${hash}  ${path}"
    return 0
  fi
  printf '%s' "unsupported|-|-  ${path}"
}

count_double_space() {
  local s=$1 n=0
  while [[ $s == *"  "* ]]; do
    s=${s#*  }
    n=$((n + 1))
  done
  printf '%s' "$n"
}

parse_fp_line() {
  local line=$1
  local kind rest fp path n
  kind=${line%%|*}
  rest=${line#*|}
  rest=${rest#*|}
  case $kind in
    file | absent | unsupported)
      fp=${rest%%  *}
      path=${rest#*  }
      if [[ $rest != *"  "* || $fp == *" "* || -z $path ]]; then
        echo "allowlist.sh: unreadable fingerprint line" >&2
        exit 2
      fi
      ;;
    symlink)
      n=$(count_double_space "$rest")
      if [[ $n -ne 1 ]]; then
        echo "allowlist.sh: ambiguous symlink record" >&2
        exit 2
      fi
      fp=${rest%%  *}
      path=${rest#*  }
      if [[ -z $path ]]; then
        echo "allowlist.sh: unreadable fingerprint line" >&2
        exit 2
      fi
      ;;
    *)
      echo "allowlist.sh: unknown kind" >&2
      exit 2
      ;;
  esac
  if [[ $path == *$'\n'* || $path == *$'\t'* ]]; then
    echo "allowlist.sh: path contains tab or newline" >&2
    exit 2
  fi
  parsed_path=$path
}

parse_fp_file() {
  local file=$1
  local -n dest=$2
  local line
  while IFS= read -r line || [[ -n $line ]]; do
    [[ -z $line ]] && continue
    parsed_path=
    parse_fp_line "$line"
    dest[$parsed_path]=$line
  done <"$file"
}

declare -A base_full=()
declare -A cur_full=()
parse_fp_file "$baseline" base_full

cur=$(mktemp)
union=$(mktemp)
trap 'rm -f -- "$cur" "$union"' EXIT
bash "$here/baseline.sh" "$cur" || exit 2
parse_fp_file "$cur" cur_full

{
  for path in "${!base_full[@]}"; do
    printf '%s\n' "$path"
  done
  for path in "${!cur_full[@]}"; do
    printf '%s\n' "$path"
  done
} | LC_ALL=C sort -u >"$union" || exit 2

verdict=0
while IFS= read -r path || [[ -n $path ]]; do
  [[ -z $path ]] && continue
  now=
  if [[ -n ${cur_full[$path]+x} ]]; then
    now=${cur_full[$path]}
  else
    now=$(fingerprint_line "$path")
  fi
  kind=${now%%|*}
  if [[ $kind == unsupported ]]; then
    echo "OUT $path" >&2
    verdict=1
    continue
  fi
  if [[ -n ${allowed[$path]+x} ]]; then
    continue
  fi
  was=${base_full[$path]:-}
  if [[ $now != "$was" ]]; then
    echo "OUT $path" >&2
    verdict=1
  fi
done <"$union"

exit "$verdict"
