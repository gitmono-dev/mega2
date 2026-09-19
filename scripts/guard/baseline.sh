#!/usr/bin/env bash
# Content + metadata baseline for dirty paths from `libra status --short`.
# Usage: baseline.sh <out>
# Line format: <kind>|<mode>|<fingerprint>  <path>
# Exit: 0 success, 2 command or usage error.
set -euo pipefail

if [[ $# -ne 1 || -z $1 ]]; then
  echo "usage: baseline.sh <out>" >&2
  exit 2
fi

out=$1

status=$(libra status --short) || exit 2

declare -A seen=()
records=()

c_unquote() {
  local s=$1
  if [[ ${#s} -lt 2 || ${s:0:1} != '"' || ${s: -1} != '"' ]]; then
    printf '%s' "$s"
    return 0
  fi
  s=${s:1:${#s}-2}
  local out='' i=0 n=${#s} c oct d take
  while ((i < n)); do
    c=${s:i:1}
    if [[ $c != \\ ]]; then
      out+=$c
      i=$((i + 1))
      continue
    fi
    i=$((i + 1))
    if ((i >= n)); then
      out+='\'
      break
    fi
    c=${s:i:1}
    case $c in
      n) out+=$'\n'; i=$((i + 1)) ;;
      t) out+=$'\t'; i=$((i + 1)) ;;
      r) out+=$'\r'; i=$((i + 1)) ;;
      a) out+=$'\a'; i=$((i + 1)) ;;
      b) out+=$'\b'; i=$((i + 1)) ;;
      f) out+=$'\f'; i=$((i + 1)) ;;
      v) out+=$'\v'; i=$((i + 1)) ;;
      \\) out+='\'; i=$((i + 1)) ;;
      \") out+='"'; i=$((i + 1)) ;;
      \') out+="'"; i=$((i + 1)) ;;
      [0-7])
        oct=$c
        i=$((i + 1))
        take=1
        while ((take < 3 && i < n)); do
          d=${s:i:1}
          [[ $d == [0-7] ]] || break
          oct+=$d
          i=$((i + 1))
          take=$((take + 1))
        done
        printf -v c '%b' "\\${oct}"
        out+=$c
        ;;
      *)
        out+=$c
        i=$((i + 1))
        ;;
    esac
  done
  printf '%s' "$out"
}

record() {
  local path=$1
  if [[ $path == *$'\n'* || $path == *$'\t'* ]]; then
    echo "baseline.sh: path contains tab or newline" >&2
    exit 2
  fi
  if [[ -n ${seen[$path]+x} ]]; then
    return 0
  fi
  seen[$path]=1

  local line
  if [[ -L $path ]]; then
    local target
    target=$(readlink -- "$path") || exit 2
    line="symlink|-|${target}  ${path}"
  elif [[ ! -e $path ]]; then
    line="absent|-|-  ${path}"
  elif [[ -f $path ]]; then
    local mode hash
    mode=$(stat -c '%a' -- "$path") || exit 2
    hash=$(sha256sum -- "$path") || exit 2
    hash=${hash%% *}
    hash=${hash#\\}
    line="file|${mode}|${hash}  ${path}"
  else
    line="unsupported|-|-  ${path}"
  fi
  records+=("${path}"$'\t'"${line}")
}

while IFS= read -r line || [[ -n $line ]]; do
  [[ -z $line ]] && continue
  [[ $line == \#\#* ]] && continue
  [[ ${#line} -lt 4 ]] && continue

  xy=${line:0:2}
  entry=${line:3}
  if [[ $xy == *R* && $entry == *" -> "* ]]; then
    record "$(c_unquote "${entry%% -> *}")"
    record "$(c_unquote "${entry##* -> }")"
  else
    record "$(c_unquote "$entry")"
  fi
done <<<"$status"

{
  if ((${#records[@]} > 0)); then
    printf '%s\n' "${records[@]}" | LC_ALL=C sort -t $'\t' -k1,1 | cut -f2-
  fi
} >"$out" || exit 2
