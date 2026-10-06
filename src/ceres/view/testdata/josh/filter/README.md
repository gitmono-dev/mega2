# Josh filter fixtures

Source: `josh-project/josh` at revision `e6dfb95e`, fetched during HP-06.

These are linear P0 ports, not copies of Josh command scripts. The checked-in
`*.expected` files are static, generated from the in-memory root histories in
`josh_filter_linear_ports`; every file contains the same `git log --graph`
shaped list and the tree reached at every root-chain sequence. If the source
revision cannot be fetched, DEP-HP-03 permits a hand-written equivalent fixture
and requires its difference to be recorded here.

- `prefix.t`: ports the one-commit `:prefix=subtree` forward case.
- `subtree_prefix.t`: Josh's full case requires `:rev` and
  `history="keep-trivial-merges"`, outside P0. Its port is the P0 linear
  `:/subtree` extraction fragment.
- `deleted_dir.t`: its three upstream commits end with removal of `sub1`, which
  writes the final EMPTY_TREE commit.
- `moved_dir.t`: moving `sub1` to `sub1_new` ends in an EMPTY_TREE commit; the
  final unrelated root update is elided by J4.
- `empty_head.t`: starts with NULL, elides the `sub1/file2` update before
  `sub2` appears, writes the first view commit when it appears, then elides
  Josh's final `sub1/file5` update.
- `exclude_compose.t`: ports its first two sections exactly as
  `:exclude[::sub2/]` and `:exclude[::sub1/,::sub2/]`. The third
  `:exclude[sub1=:/sub3]` section is excluded because P0 Exclude accepts only
  `::` selectors (design §2.1).
- `gpgsig.t`: ports only signature removal. `gpgsig-sha256.expected` is the
  mega2 extension for the second fixed signature header.

`filter_id.t` groups exercised by `josh_filter_id_canonical_rules` are:

- `:/a:/b` = `:/a/b` (design §2.3 rule 2, adjacent Subdir).
- `:prefix=a/b:prefix=c` = `:prefix=c/a/b` (rule 2, adjacent Prefix).
- `:prefix=x/y:/x` = `:prefix=y` (rule 3, Prefix then Subdir).
- `:[:empty,:/a]` = `:/a` (rule 4, remove empty Compose member).
- mega2's rule-6 adaptation checks selector sorting with
  `:exclude[::b/,::a/]` = `:exclude[::a/,::b/]`; Josh prints only the latter.
- mega2's §2.1 golden-vector adaptation checks Compose sorting with
  `:[:/b:prefix=y,:/a:prefix=x]` = `:[:/a:prefix=x,:/b:prefix=y]`; it is not
  a `filter_id.t` pair.

Every remaining `filter_id.t` entry is deliberately registered below.

- `:[:/a,:/b]`: valid P0 syntax, but §2.2 rejects registration because both
  members have output path `/`; its printed tree representation is therefore
  not a registered P0 filter fixture.
- `:/"a"` → `:/a` and `:/"a%\\"$"`: PATH parsing and printing are §2.1
  vectors, not §2.3 rewrites; the former is covered by the HP-02 PATH golden
  vector and the latter has no alternate §2.3-equivalent spelling.
- `:/a~`: rejected by §2.1 because `~` is not a bare PATH character.
- `:[:/a:/b,:/a/b]`: Compose members overlap, so §2.2 rejects registration.
- `:[:empty,:/a]` reverse output, every `--reverse` invocation, and all named
  mappings (`x=:/…`, `a/b = :/…`): reverse/mapping syntax is outside §2.1.
- `:exclude[:/a:/b]` and `:exclude[:/a,:/b]`: Exclude arguments must be `::`
  selectors, so they are outside §2.1. They are not §2.3 incompleteness cases.
- `:[::a,::b]:/c`, `:[::a,::b]::c`, and
  `:[:/a:prefix=a,:/b:prefix=b]:exclude[::a/a,::b/b]`: selector-as-filter,
  selector suffix, and named Exclude arguments are outside §2.1.
- `:[:/a,:/b]:[:empty,:/]`: `:/` has an empty PATH and is rejected by §2.1.
- Every `:subtract[...]` form, including the repeated-name and `:empty`
  examples: Subtract is outside the P0 grammar (§2.1).
- Every `--file f` mapping fixture: file-specified and named-mapping syntax is
  outside §2.1.
- `::file.txt`, `::dest.txt=src.txt`, `::*.txt`, `::dir/`, `::a/b/c/`, plus
  `::*.txt=src.txt`, `::dest.txt=*.txt`, and `::*.txt=*.txt`: standalone file,
  mapping, directory, and glob filters are outside P0; `::` exists only as an
  Exclude selector (§2.1).
- `:FOLD`, `:PATHS`, `:INDEX`, `:INVERT`, `:linear`, `:linear[::x/]`,
  `:prune=trivial-merge`, `:SQUASH`, and `:replace(...)`: non-P0 operators
  under §1.2 / §2.1; `:INDEX` also requires Josh experimental opt-in.
- `:$.={#}`, `:~(history="embed")[…]`,
  `:~(key1="value1",key2="value2",a="b")[…]`, `:rev(_:/a)`, and their
  experimental-option diagnostics: meta or revision operators are outside
  §1.2 / §2.1.
- `:unsign`: P0 removes the two fixed signature headers during commit rewrite;
  it is not a user filter (§1.2 and HP-05).
- `:workspace=…`, `:+…`, and `:hook=…`: workspace, stored, and hook features
  are outside §1.2.
- `:author=…`, `:committer=…`, `:"commit message"`, and
  `:"commit message";".*"`: metadata rewrite filters are outside §1.2.
- `:pin[:/a]`: Pin is outside the P0 grammar (§2.1).

Copyright (c) 2022-2026 Josh Project
Copyright (c) 2016-2021 ESRLabs AG

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
