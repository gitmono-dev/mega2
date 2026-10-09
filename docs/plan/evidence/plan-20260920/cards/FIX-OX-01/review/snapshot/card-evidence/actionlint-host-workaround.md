# FIX-OX-01 VER-1 host workaround

The prescribed local checker is actionlint 1.7.12 on Darwin arm64 with ShellCheck 0.11.0. The bounded default run of `actionlint .github/workflows/docker.yml` ended with timeout exit 124 and empty stdout/stderr (`actionlint-v1712-default.*`). A timeout is not a lint pass.

The schema-only invocation (`actionlint -shellcheck= .github/workflows/docker.yml`) exited 1 and emitted one diagnostic: actionlint 1.7.12 does not recognize `concurrency.queue: max`. This result does not run ShellCheck and is not treated as full workflow lint.

For a full integration check without changing repository files, actionlint was built temporarily from merged upstream commit `011a6d15e749bb3f2d771eed9c7aa0e7e3e10ee7` (PR [#651](https://github.com/rhysd/actionlint/pull/651), merged 2026-04-19). Its default actionlint plus ShellCheck run completed with exit 1 and only the same unknown `queue` key diagnostic (`actionlint-fixed-full.*`). The upstream PR describes the Darwin deadlock in ShellCheck stdin handling and the merged fix.

The plan's exact fallback then made a temporary workflow copy, asserted that the exact `queue: max` line occurred once, removed only that line, printed the one-line diff, and ran actionlint on the copy. It exited 0 (`actionlint-queue-only-fallback.*`). The repository workflow was not modified for this fallback. This validates the remainder of the workflow syntax and shell integration while preserving the distinction that the original workflow's full actionlint run did not pass under the pinned local release. The eventual tag build and Docker publication remain subject to OX-284's actual D gate.

GitHub documents the `queue` concurrency setting at [Control the concurrency of workflows and jobs](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency). The queue field remains in the repository source.
