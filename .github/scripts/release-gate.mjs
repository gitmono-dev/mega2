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

/**
 * Creates the read-only GitHub client used by `evaluateReleaseGate`.
 *
 * Only three endpoints are requested: `git/ref/tags/<ref>`, `git/ref/heads/main`
 * and `actions/workflows/<workflow>/runs?branch=main&event=push&per_page=1`.
 * Redirects are not followed. A tag lookup answered with HTTP 404 resolves to
 * `"absent"`; every other non-2xx status (including 3xx), a timeout while
 * waiting for the headers or the body, a non-JSON body or a missing field
 * throws. Error
 * messages never contain the token.
 *
 * @param {{ baseUrl: string, token: string, fetch?: typeof fetch, timeoutMs?: number,
 *   timers?: { setTimeout: typeof setTimeout, clearTimeout: typeof clearTimeout } }} options
 */
export function createGitHubClient({ baseUrl, token, fetch: fetchImpl = globalThis.fetch, timeoutMs = 10_000, timers = globalThis }) {
  const base = String(baseUrl).replace(/\/+$/, "");

  async function request(path, { allowNotFound = false } = {}) {
    const controller = new AbortController();
    const aborted = new Promise((_, rejectAbort) => {
      controller.signal.addEventListener("abort", () => rejectAbort(new Error("aborted")), { once: true });
    });
    aborted.catch(() => {});
    const timer = timers.setTimeout(() => controller.abort(), timeoutMs);
    const timedOut = () => new Error(`GET ${path} timed out after ${timeoutMs} ms`);
    try {
      let response;
      try {
        response = await Promise.race([
          fetchImpl(`${base}/${path}`, {
            method: "GET",
            redirect: "manual",
            headers: {
              accept: "application/vnd.github+json",
              authorization: `Bearer ${token}`,
              "x-github-api-version": "2022-11-28",
            },
            signal: controller.signal,
          }),
          aborted,
        ]);
      } catch {
        throw controller.signal.aborted ? timedOut() : new Error(`GET ${path} failed`);
      }
      if (allowNotFound && response.status === 404) return undefined;
      if (!(response.status >= 200 && response.status <= 299)) {
        throw new Error(`GET ${path} answered HTTP ${response.status}`);
      }
      let text;
      try {
        text = await Promise.race([response.text(), aborted]);
      } catch {
        throw controller.signal.aborted ? timedOut() : new Error(`GET ${path} body could not be read`);
      }
      try {
        return JSON.parse(text);
      } catch {
        throw new Error(`GET ${path} did not answer JSON`);
      }
    } finally {
      timers.clearTimeout(timer);
    }
  }

  function refSha(body, path) {
    const sha = body?.object?.sha;
    if (!isSha(sha)) throw new Error(`GET ${path} has no object.sha`);
    return sha;
  }

  return {
    async getTagRef(repository, refName) {
      const path = `repos/${repository}/git/ref/tags/${encodeURIComponent(refName)}`;
      const body = await request(path, { allowNotFound: true });
      if (body === undefined) return "absent";
      return { object: { sha: refSha(body, path) } };
    },
    async getMainHead(repository) {
      const path = `repos/${repository}/git/ref/heads/main`;
      return refSha(await request(path), path);
    },
    async getLatestMainPushRun(repository, workflow) {
      const path = `repos/${repository}/actions/workflows/${encodeURIComponent(workflow)}/runs?branch=main&event=push&per_page=1`;
      const body = await request(path);
      if (typeof body?.total_count !== "number" || !Array.isArray(body.workflow_runs)) {
        throw new Error(`GET ${path} has no total_count or workflow_runs`);
      }
      if (body.total_count === 0 || body.workflow_runs.length === 0) return null;
      const run = body.workflow_runs[0];
      if (!isSha(run?.head_sha) || typeof run.status !== "string" || !("conclusion" in run)) {
        throw new Error(`GET ${path} run has no head_sha, status or conclusion`);
      }
      return { head_sha: run.head_sha, status: run.status, conclusion: run.conclusion };
    },
  };
}

function isSha(value) {
  return typeof value === "string" && /^[0-9a-f]{40}$/.test(value);
}

function reject(reason) {
  return { ok: false, reason };
}
