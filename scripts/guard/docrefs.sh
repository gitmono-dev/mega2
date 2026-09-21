#!/usr/bin/env bash
# Check that docs/*.md references in markdown files exist on disk.
# Usage: docrefs.sh <markdown-file>...
# Exit: 0 pass (including no matches), 1 dangling ref, 2 command or input error.
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: docrefs.sh <markdown-file>..." >&2
  exit 2
fi

for f in "$@"; do
  if [[ ! -f $f ]]; then
    echo "docrefs.sh: missing input" >&2
    exit 2
  fi
done

extracted=$(mktemp) || exit 2
sorted=$(mktemp) || exit 2
trap 'rm -f -- "$extracted" "$sorted"' EXIT

set +e
rg -o --no-filename -- 'docs/[[:alnum:]_./-]+\.md' "$@" >"$extracted"
rg_rc=$?
set -e
if ((rg_rc > 1)); then
  echo "docrefs.sh: rg failed" >&2
  exit 2
fi
if ((rg_rc == 1)); then
  exit 0
fi

if ! LC_ALL=C sort -u "$extracted" >"$sorted"; then
  echo "docrefs.sh: sort failed" >&2
  exit 2
fi

verdict=0
while IFS= read -r ref || [[ -n $ref ]]; do
  [[ -z $ref ]] && continue
  if [[ ! -e $ref ]]; then
    echo "DANGLING $ref" >&2
    verdict=1
  fi
done <"$sorted"

exit "$verdict"
