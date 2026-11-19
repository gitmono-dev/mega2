// Release gate for paired megaui / mega2 releases (ER-08, ADR-FSL-04).
//
// This file is kept byte-identical in gitmono-dev/megaui and gitmono-dev/mega2
// (`.github/scripts/release-gate.mjs`). It has no third-party dependencies.

/**
 * Decides whether a release tag may be built.
 *
 * The injected client provides three read-only lookups:
 * - `getTagRef(repository, refName)` resolves to `"absent"` when the tag does
 *   not exist, otherwise to the tag reference `{ object: { sha } }` (the shape
 *   of GitHub's `git/ref` response);
 * - `getMainHead(repository)` resolves to the SHA of `main` HEAD;
 * - `getLatestMainPushRun(repository, workflow)` resolves to `null` when the
 *   workflow has no `main` push run, otherwise to
 *   `{ head_sha, status, conclusion }` of the latest one.
 *
 * Any thrown error is a rejection (fail-closed).
 *
 * @param {{ client: object, inputs: {
 *   refName: string,
 *   selfRepository: string, selfCiWorkflow: string,
 *   partnerRepository: string, partnerCiWorkflow: string,
 * } }} args
 * @returns {Promise<{ ok: boolean, reason: string }>}
 */
export async function evaluateReleaseGate({ client, inputs }) {
  const repositories = [
    { repository: inputs.selfRepository, workflow: inputs.selfCiWorkflow },
    { repository: inputs.partnerRepository, workflow: inputs.partnerCiWorkflow },
  ];
  try {
    for (const { repository } of repositories) {
      const tag = await client.getTagRef(repository, inputs.refName);
      if (tag === "absent") {
        return reject(`tag ${inputs.refName} does not exist in ${repository}`);
      }
      if (tag === null || typeof tag !== "object" || !isSha(tag.object?.sha)) {
        return reject(`tag lookup for ${inputs.refName} in ${repository} returned no tag reference`);
      }
    }
    for (const { repository, workflow } of repositories) {
      const head = await client.getMainHead(repository);
      if (!isSha(head)) {
        return reject(`main HEAD of ${repository} is not a commit SHA`);
      }
      const run = await client.getLatestMainPushRun(repository, workflow);
      if (run === null) {
        return reject(`${repository} has no main push run of ${workflow}`);
      }
      if (typeof run !== "object" || !isSha(run.head_sha)) {
        return reject(`latest ${workflow} main push run in ${repository} has no commit SHA`);
      }
      if (run.head_sha !== head) {
        return reject(`latest ${workflow} main push run in ${repository} is for ${run.head_sha}, not main HEAD ${head}`);
      }
      if (run.status !== "completed") {
        return reject(`latest ${workflow} main push run in ${repository} is ${run.status}, not completed`);
      }
      if (run.conclusion !== "success") {
        return reject(`latest ${workflow} main push run in ${repository} concluded ${run.conclusion}, not success`);
      }
    }
  } catch (error) {
    return reject(`lookup failed: ${error instanceof Error ? error.message : String(error)}`);
  }
  return {
    ok: true,
    reason: `tag ${inputs.refName} exists in both repositories and both main HEADs passed their quality gates`,
  };
}

function isSha(value) {
  return typeof value === "string" && /^[0-9a-f]{40}$/.test(value);
}

function reject(reason) {
  return { ok: false, reason };
}
