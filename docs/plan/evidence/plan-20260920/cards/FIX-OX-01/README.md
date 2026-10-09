# FIX-OX-01 evidence

State: A/B and ER-05 passed; this card is `in-progress / locally-accepted` after the local submission. Claude Code 2.1.288 returned literal `VERDICT: PASS` (exit 0; report SHA-256 `47e8fe7b8f32686ebce48098802b4d4c48d3818cb5f0df3aa04ac136e6dc4856`). Five non-blocking P3 notes are preserved in `review/claude-review.md`. This card does not publish a Docker image or satisfy the OX-284 D gate.

## Scope and source

The card owns the Docker manifest job and stable tag guard in `.github/workflows/docker.yml` (owner hunks 04 and 17). The current workflow candidate SHA-256 is `ecc44850a0ae3d0b8106739b6d7616fab35210d72fe5f954d15694dbca0a1dcb`. Its only post-checkpoint source correction replaces a bare digest regex condition with an explicit error and exit 1. Before the correction, Bash 3.2.57 on Darwin accepted a 64-character non-hex digest and passed it to `docker buildx imagetools create`; see `pre-fix-digest-validation-failure.json`. The corrected simulation rejects malformed, short, and uppercase values before invoking Docker.

The accepted predecessors are FIX-OX-08 (`346fe3c221b1f66636f8da6c151c0964b2a24800`) and FIX-OX-11 (`a5ba8febeaa90bcc97411d278f596753a05f1b8a`). Their checkpoint-to-current source/test/workflow delta is empty (`predecessor-source-diff.*`). The candidate owner coordinates and exact file hashes are in `owner-coordinate-map.json`.

## Acceptance evidence

- AC-1: the manifest job requires the whole `build` matrix and explicitly checks `needs.build.result == success` before checkout or publication.
- AC-2: publication requires exactly two digest files; each filename must be 64 lowercase hexadecimal characters.
- AC-3: metadata generates both `v<version>` and `<version>` immutable tags; mutable `major.minor` and `latest` are filtered from that create operation.
- AC-4: the created manifest is inspected and must contain exactly Linux `amd64` and `arm64` architectures.
- AC-5/AC-6: the actual channel script was simulated against older-patch, minor-latest, global-latest, and missing-current-tag sets. Only the newest tag in its minor updates the minor channel; only the globally newest tag updates `latest`; a missing current tag fails closed.
- AC-7: the stable version guard accepts canonical `v<major>.<minor>.<patch>` and rejects leading zeros, missing segments, prerelease, and unprefixed forms; it precedes checkout and Docker login.

`acceptance-simulation.json` records each input, output, exit code, Docker stub log, and the fact that no credential, network, or registry was used.

## A/B evidence

The four fresh Libra worktrees are recorded in `dry-run-results.json` and `apply-results.json`. A dry-run removes owner ordinals 17 then 04 with `patch -C -R -F0`; B is the empty reverse set. A apply removes those same owners and reaches expected workflow SHA-256 `c3b97f4709e1912b07b40f47a955e908908b217145e960e75232c1e1afab0eae`; only the workflow path changes. B apply preserves candidate SHA-256 `ecc44850a0ae3d0b8106739b6d7616fab35210d72fe5f954d15694dbca0a1dcb`. All recorded patch commands exit 0 without offset, fuzz, reject, or failed diagnostics. `.env.test` was copied into each temporary worktree with mode 0600 and is excluded from evidence and submission.

## Verification and Docker release boundary

`verification-results.json` indexes VER-1 through VER-3, their raw outputs, and the actionlint host workaround. The default actionlint v1.7.12 invocation on Darwin arm64 timed out; this is recorded as a non-pass. The temporary build from the merged upstream fix completed full actionlint and reported only the unsupported `concurrency.queue: max` field. The documented temporary-file fallback removes only that field and passed; it does not claim the original-file invocation passed. GitHub's concurrency documentation confirms `queue: max` is the requested field, and actionlint PR #651 documents the Darwin ShellCheck deadlock fixed by the temporary build.

Historical `v0.42.25` evidence is preserved separately: tag target `4b645af3035f8186f0ffdab3e39754667959afe0`, workflow run `37704534619`, Docker job failure due to the Docker Build Cloud prepaid-minute limit. This historical failure is not rewritten as success. No new tag, image, push, or release was created; actual Docker D remains with OX-284.
