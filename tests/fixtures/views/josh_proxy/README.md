# Josh proxy clone expectations

Source: [`josh-project/josh` at `e6dfb95e`](https://github.com/josh-project/josh/tree/e6dfb95e/tests/proxy), MIT license. These fixtures transcribe only expected `git log --graph --pretty=%s` and tree output from `clone_subtree.t`, `clone_subsubtree.t`, and `clone_prefix.t`; no Josh implementation code is copied.

| Upstream proxy filter | Mega2 view filter | Fixture |
| --- | --- | --- |
| `:/sub1` | `:/project/hp22-subtree/sub1` | `subtree.*` |
| `:/sub1/subsub` | `:/project/hp22-subsubtree/sub1/subsub` | `subsubtree.*` |
| `:prefix=pre` | `:prefix=pre` composed with `:/project/hp22-prefix` | `prefix.*` |

The upstream `git-tree-pretty` output includes box drawing characters and blob contents. The `.tree` files instead record its path portion as `git ls-tree -r --name-only HEAD` emits it, preserving path names and ordering byte for byte. The `git log` lines are copied verbatim. Mega2 seeds each case under `/project/hp22-<case>` through trunk and projects it from the root, so commit IDs, remote URL, branch name (`main` instead of `master`), and tree drawing are intentionally excluded from the comparison. These three pinned `.t` files contain clone output but no later fetch segment. The `.fetch.*` files are a Mega2-only extension: one additional `add file3` commit in the selected path, followed by fetch and fast-forward. They are not claimed to be upstream expected output.
