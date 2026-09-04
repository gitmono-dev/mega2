#!/usr/bin/env bash
# authz_write_points_guard.sh — write-point allowlist guard (UN-16).
#
# The shared authorization snapshot is rebuilt only from main's
# `/.mega_cedar.json` via `notify_authz_changed` (src/contract/policy/notify.rs).
# Every code path that can write a branch ref / commit / tree on the main line
# must either be hooked (notify) or be a documented exception. This guard scans
# the tree for calls to the low-level write primitives and diffs the attributed
# `filename:function` set against a plan-fixed expectation list. Any unregistered
# hit (new write point) exits non-zero.
#
# Algorithm (plan-fixed, 2026-08-13; baseline verified on the real tree):
#   1. Scan: rg -n '<primitives>' src/ --glob '!**/tests.rs'
#   2. Filter:
#      a. drop src/jupiter/storage/ paths (bottom primitive/wrapper layer,
#         covered by caller entry points)
#      b. drop primitive `fn` definition lines
#      c. drop comment lines (line + block comments)
#      d. drop hits inside `#[cfg(test)]`-covered items / `mod tests` brace
#         bodies (block-range exclusion, NOT "everything after the first
#         marker" — code_edit/utils.rs has `#[cfg(test)]` before production fns)
#   3. Attribute each hit to the nearest preceding `fn <name>` line.
#   4. Dedup + sort, diff -u against the fixed expectation set.
#
# Modes:
#   (no args)            run the guard; exit 0 iff the normalized set matches
#   --selftest-add       inject a temporary unknown entry point; the guard must
#                        exit non-zero (gate 2)
#   --selftest-remove    drop one expectation line; the guard must exit non-zero
#                        (gate 3)
#   --ctor-guard         MonoApiService constructor zero-change guard: the five
#                        production struct literals must still carry exactly the
#                        UN-02 baseline field set (UN-16 adds no authz field)
#   -h | --help          usage

set -euo pipefail

# ---------------------------------------------------------------------------
# Fixed expectation set (11 entries). The diff comparison uses the plain
# `filename:function` lines; the trailing comments are the per-entry exception
# proof (plan-fixed).
# ---------------------------------------------------------------------------
EXPECTED=(
  "import_repo.rs:attach_to_monorepo_parent"        # hooked: UN-16 post-commit notify
  "mono_api_service.rs:apply_update_result"          # hooked: UN-16 post-commit notify
  "monorepo.rs:apply_cl_mega_ref_for_push_command"   # hooked: UN-16 main-delete rejection + notify
  "mono_api_service.rs:apply_update_result_cl_only"  # exception: CL-only funnel (no main ref write)
  "mono_service.rs:init_monorepo"                    # exception: first-create of the monorepo root
  "monorepo.rs:refs_with_head_hash"                  # exception: lazy-create of a missing branch ref
  "code_edit/utils.rs:create_repo_commit"            # exception: lazy-create of a missing branch ref
  "mono_api_service.rs:delete_tag"                   # exception: tag-only ref removal
  "import_api_service.rs:delete_tag"                 # exception: tag-only ref removal
  "import_repo.rs:update_refs"                       # exception: tag-only ref removal
  "buck_service.rs:complete_upload"                  # exception: CL-ref-only write
)

# ---------------------------------------------------------------------------
# MonoApiService constructor baseline (UN-02, card-fixed). ADR-UN-02 ⑥ forbids
# adding an authorization field to MonoApiService/Monorepo/ImportRepo, so every
# production `MonoApiService { .. }` literal must keep exactly this field set.
# The struct declaration is included so a field added there cannot slip through.
# ---------------------------------------------------------------------------
EXPECTED_CTORS=(
  "src/api/mod.rs:from -> git_object_cache,storage"                                  # From<&MonoApiServiceState>
  "src/ceres/api_service/mono_api_service.rs:from -> git_object_cache,storage"       # From<&Monorepo>
  "src/ceres/api_service/mono_api_service.rs:from -> git_object_cache,storage"       # From<&ImportRepo>
  "src/ceres/build_trigger/changes_calculator.rs:get_commit_blobs -> git_object_cache,storage"
  "src/ceres/build_trigger/changes_calculator.rs:cl_files_list -> git_object_cache,storage"
  "src/ceres/api_service/mono_api_service.rs:struct MonoApiService -> git_object_cache,storage"
)

usage() {
  cat <<'USAGE'
authz_write_points_guard.sh — write-point allowlist guard (UN-16).

Usage:
  bash scripts/authz_write_points_guard.sh             run the guard
  bash scripts/authz_write_points_guard.sh --selftest-add
  bash scripts/authz_write_points_guard.sh --selftest-remove
  bash scripts/authz_write_points_guard.sh --ctor-guard
  bash scripts/authz_write_points_guard.sh -h | --help
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

# ---------------------------------------------------------------------------
# Normalizer (embedded Python): scan + filter + attribute + dedup/sort.
# Emits one `filename:function` line per registered write point.
# ---------------------------------------------------------------------------
normalize() {
  python3 - <<'PY'
import re, subprocess, sys
from collections import defaultdict

PRIMITIVES = (
    r'batch_update_by_path_concurrent|attach_to_monorepo_parent_in_txn|'
    r'remove_ref|mega_head_hash_with_txn|batch_save_model_with_txn|'
    r'save_or_update_cl_ref'
)
FN_DEF = re.compile(r'\bfn\s+(?:' + PRIMITIVES + r')\b')
FN_ANY = re.compile(r'\bfn\s+([A-Za-z_][A-Za-z0-9_]*)')
CFG_TEST = re.compile(r'^\s*#\[cfg\(test\)\]')
MOD_TESTS = re.compile(r'^\s*mod\s+tests\b')

def excluded_ranges(lines):
    """1-based line numbers inside #[cfg(test)] items / `mod tests` bodies."""
    excluded = set()
    n = len(lines)
    i = 0
    while i < n:
        line = lines[i]
        if CFG_TEST.match(line):
            j = i + 1
            while j < n and (not lines[j].strip() or lines[j].strip().startswith('#')):
                j += 1
            if j >= n:
                break
            if re.match(r'^\s*mod\s+\w+\s*;', lines[j]):
                i = j + 1
                continue
            k = j
            while k < n and '{' not in lines[k]:
                k += 1
            if k >= n:
                i = j + 1
                continue
            depth = 0
            m = k
            while m < n:
                depth += lines[m].count('{') - lines[m].count('}')
                if depth <= 0:
                    break
                m += 1
            for x in range(k, m + 1):
                excluded.add(x + 1)
            i = m + 1
        elif MOD_TESTS.match(line):
            if re.search(r';\s*(//.*)?$', line):
                i += 1
                continue
            k = i
            while k < n and '{' not in lines[k]:
                k += 1
            if k >= n:
                i += 1
                continue
            depth = 0
            m = k
            while m < n:
                depth += lines[m].count('{') - lines[m].count('}')
                if depth <= 0:
                    break
                m += 1
            for x in range(k, m + 1):
                excluded.add(x + 1)
            i = m + 1
        else:
            i += 1
    return excluded

def block_comment_lines(lines):
    """1-based line numbers inside /* ... */ block comments."""
    in_block = False
    out = set()
    for idx, line in enumerate(lines, start=1):
        t = line.strip()
        if in_block:
            out.add(idx)
            if '*/' in line:
                in_block = False
        else:
            if t.startswith('/*'):
                out.add(idx)
                if '*/' not in line:
                    in_block = True
            elif t.startswith('//') or t.startswith('*'):
                out.add(idx)
    return out

proc = subprocess.run(
    ['rg', '-n', PRIMITIVES, 'src/', '--glob', '!**/tests.rs'],
    capture_output=True, text=True)
if proc.returncode not in (0, 1):
    sys.stderr.write(proc.stderr)
    sys.exit(2)

by_file = defaultdict(list)
for line in proc.stdout.splitlines():
    path, lineno, text = line.split(':', 2)
    by_file[path].append((int(lineno), text))

# Disambiguate basenames that appear under more than one path (e.g. utils.rs
# exists in src/common/ and src/ceres/code_edit/): prefix with the immediate
# parent directory so the expectation set stays unambiguous. The ambiguity is
# computed over every .rs file under src/ (not just files with hits), so a
# basename shared with a hit-free file still gets the parent prefix.
from collections import Counter
all_files = subprocess.run(
    ['rg', '--files', 'src/', '-g', '*.rs', '-g', '!**/tests.rs'],
    capture_output=True, text=True).stdout.splitlines()
basename_counts = Counter(p.split('/')[-1] for p in all_files)

def display_name(path):
    base = path.split('/')[-1]
    if basename_counts[base] > 1:
        parent = path.split('/')[-2]
        return f"{parent}/{base}"
    return base

results = set()
for path in sorted(by_file):
    if path.startswith('src/jupiter/storage/'):
        continue
    with open(path) as f:
        lines = f.read().splitlines()
    excluded = excluded_ranges(lines) | block_comment_lines(lines)
    for lineno, text in by_file[path]:
        if lineno in excluded:
            continue
        if FN_DEF.search(text):
            continue
        fn_name = None
        for j in range(lineno - 2, -1, -1):
            m = FN_ANY.search(lines[j])
            if m:
                fn_name = m.group(1)
                break
        if fn_name is None:
            continue
        results.add(f"{display_name(path)}:{fn_name}")

for r in sorted(results):
    print(r)
PY
}

# ---------------------------------------------------------------------------
# Core guard: normalize + diff against EXPECTED. Exit 0 iff identical.
# ---------------------------------------------------------------------------
run_guard() {
  local expected_file actual_file rc
  expected_file="$(mktemp)"
  actual_file="$(mktemp)"
  trap 'rm -f "$expected_file" "$actual_file"' RETURN
  printf '%s\n' "${EXPECTED[@]}" | sort >"$expected_file"
  normalize | sort >"$actual_file"
  if diff -u "$expected_file" "$actual_file"; then
    echo "authz write-point guard: PASS (${#EXPECTED[@]} registered write points)"
    rc=0
  else
    echo "authz write-point guard: FAIL — unregistered write point(s) detected" >&2
    rc=1
  fi
  return "$rc"
}

# ---------------------------------------------------------------------------
# Constructor normalizer: emit `path:owner -> <sorted field names>` for the
# MonoApiService struct declaration and for every production `MonoApiService {`
# struct literal (`#[cfg(test)]` / `mod tests` bodies excluded, so the unit-test
# helper does not enter the set).
# ---------------------------------------------------------------------------
normalize_ctors() {
  python3 - <<'PY'
import re, subprocess, sys

FN_ANY = re.compile(r'\bfn\s+([A-Za-z_][A-Za-z0-9_]*)')
FIELD = re.compile(r'^\s*(?:pub\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*:')
# Top-level `#[cfg(test)] mod ... {` (column 0). The test module is the file's
# last item by convention, so everything from that line on is test code and
# never a production ctor. A `#[cfg(test)] fn ...` is NOT a boundary —
# `changes_calculator.rs` has one before its production construction points.
TOP_CFG_TEST = re.compile(r'^#\[cfg\(test\)\]')
TOP_MOD = re.compile(r'^mod\s+\w+\s*\{')

def first_test_line(lines):
    for idx, line in enumerate(lines, start=1):
        if TOP_CFG_TEST.match(line) and idx < len(lines) and TOP_MOD.match(lines[idx]):
            return idx
    return None

def fields_from(lines, start):
    """Field names of the brace body opened on line index `start`."""
    out, depth, i = [], 0, start
    while i < len(lines):
        depth += lines[i].count('{') - lines[i].count('}')
        if i > start:
            m = FIELD.match(lines[i])
            if m and depth >= 1:
                out.append(m.group(1))
            if depth <= 0:
                break
        i += 1
    return sorted(set(out))

proc = subprocess.run(
    ['rg', '-n', r'MonoApiService \{', 'src/', '--glob', '!**/tests.rs'],
    capture_output=True, text=True)
if proc.returncode not in (0, 1):
    sys.stderr.write(proc.stderr)
    sys.exit(2)

results = []
cache = {}
for raw in proc.stdout.splitlines():
    path, lineno, text = raw.split(':', 2)
    lineno = int(lineno)
    if path not in cache:
        with open(path) as f:
            src = f.read().splitlines()
        cache[path] = (src, first_test_line(src))
    lines, test_from = cache[path]
    if test_from is not None and lineno >= test_from:
        continue
    stripped = text.strip()
    idx = lineno - 1
    if re.match(r'^pub\s+struct\s+MonoApiService\s*\{$', stripped):
        owner = 'struct MonoApiService'
    elif (stripped.endswith('MonoApiService {')
          and '->' not in stripped
          and not re.search(r'\b(impl|fn|struct|for|where|dyn)\b', stripped)):
        # A struct literal (bare, or the tail of `let x = MonoApiService {`).
        # Anything else carrying the token is an `impl` header, a return type,
        # or a trait bound — not a construction point.
        owner = None
        for j in range(idx - 1, -1, -1):
            m = FN_ANY.search(lines[j])
            if m:
                owner = m.group(1)
                break
        if owner is None:
            continue
    else:
        continue
    results.append(f"{path}:{owner} -> {','.join(fields_from(lines, idx))}")

for r in sorted(results):
    print(r)
PY
}

run_ctor_guard() {
  local expected_file actual_file rc
  expected_file="$(mktemp)"
  actual_file="$(mktemp)"
  trap 'rm -f "$expected_file" "$actual_file"' RETURN
  printf '%s\n' "${EXPECTED_CTORS[@]}" | sort >"$expected_file"
  normalize_ctors | sort >"$actual_file"
  if diff -u "$expected_file" "$actual_file"; then
    echo "MonoApiService ctor guard: PASS (5 construction points + the struct declaration unchanged)"
    rc=0
  else
    echo "MonoApiService ctor guard: FAIL — constructor point changed vs the UN-02 baseline" >&2
    rc=1
  fi
  return "$rc"
}

mode="${1:-}"
case "$mode" in
  --ctor-guard)
    run_ctor_guard
    ;;
  --selftest-add)
    # Gate 2: inject a temporary unknown entry point; the guard must fail.
    tmp="src/ceres/pack/guard_selftest_tmp.rs"
    trap 'rm -f "$tmp"' EXIT
    cat >"$tmp" <<'RS'
// Temporary selftest injection for scripts/authz_write_points_guard.sh
// --selftest-add. Created and removed by the guard's self-test; never
// compiled (no `mod` declaration reaches this file).
pub async fn guard_selftest_injected_entry(storage: &Storage) {
    storage.remove_ref(Default::default()).await.unwrap();
}
RS
    if run_guard >/dev/null 2>&1; then
      echo "selftest-add: FAIL — guard did not reject the injected entry point" >&2
      exit 1
    fi
    echo "selftest-add: PASS — guard rejected the injected entry point"
    ;;
  --selftest-remove)
    # Gate 3: drop one expectation line; the guard must fail.
    if [[ ${#EXPECTED[@]} -lt 2 ]]; then
      echo "selftest-remove: FAIL — expectation set too small" >&2
      exit 1
    fi
    EXPECTED=("${EXPECTED[@]:1}")
    if run_guard >/dev/null 2>&1; then
      echo "selftest-remove: FAIL — guard did not reject the removed expectation" >&2
      exit 1
    fi
    echo "selftest-remove: PASS — guard rejected the removed expectation"
    ;;
  "")
    run_guard
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
