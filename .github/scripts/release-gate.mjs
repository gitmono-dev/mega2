// Release gate for paired megaui / mega2 releases (ER-08, ADR-FSL-04).
//
// This file is kept byte-identical in gitmono-dev/megaui and gitmono-dev/mega2
// (`.github/scripts/release-gate.mjs`). It has no third-party dependencies.
//
// Command line: `node .github/scripts/release-gate.mjs`, configured only by
// GH_TOKEN, REF_NAME, SELF_REPOSITORY, SELF_CI_WORKFLOW, PARTNER_REPOSITORY,
// PARTNER_CI_WORKFLOW and the optional GITHUB_API_URL. Exit code 0 lets the
// release proceed; 1 rejects it.

import { pathToFileURL } from "node:url";

export const PARTNER_TAG_POLL_INTERVAL_MS = 15_000;
export const PARTNER_TAG_DEADLINE_MS = 120_000;
// Timer jitter tolerance for every slot (see the protocol note below).
export const PARTNER_TAG_JITTER_MS = 1_000;

// Monotonic: wall-clock adjustments cannot move the polling deadline.
const realClock = {
  now: () => performance.now(),
  sleep: (ms) => new Promise((resolveSleep) => setTimeout(resolveSleep, ms)),
};

/**
 * Decides whether a release tag may be built.
 *
 * The injected client provides read-only lookups:
 * - `getTagRef(repository, refName)` resolves to `"absent"` when the tag does
 *   not exist (HTTP 404), otherwise to the tag reference
 *   `{ object: { type, sha } }` (GitHub's `git/ref` shape);
 * - `getAnnotatedTag(repository, sha)` resolves an annotated tag object to
 *   `{ object: { type, sha } }` (GitHub's `git/tags/<sha>` shape);
 * - `getMainHead(repository)` resolves to the SHA of `main` HEAD;
 * - `getLatestMainPushRun(repository, workflow)` resolves to `null` when the
 *   workflow has no `main` push run, otherwise to
 *   `{ head_sha, status, conclusion }` of the latest one.
 *
 * Both tags must dereference (lightweight `commit`, or annotated `tag` ->
 * `commit`) to the `main` HEAD read at the start; the partner tag is polled
 * while it answers 404 with the bounded slot protocol described below; both `main` HEADs are read again at
 * the end and must not have moved. Any thrown error is a rejection
 * (fail-closed).
 *
 * @param {{ client: object, inputs: {
 *   refName: string,
 *   selfRepository: string, selfCiWorkflow: string,
 *   partnerRepository: string, partnerCiWorkflow: string,
 * }, clock?: { now(): number, sleep(ms: number): Promise<void> },
 *   log?: (line: string) => void }} args
 * @returns {Promise<{ ok: boolean, reason: string }>}
 */
export async function evaluateReleaseGate({ client, inputs, clock = realClock, log = (line) => process.stdout.write(line) }) {
  const self = { repository: inputs.selfRepository, workflow: inputs.selfCiWorkflow };
  const partner = { repository: inputs.partnerRepository, workflow: inputs.partnerCiWorkflow };
  try {
    for (const target of [self, partner]) {
      target.head = await client.getMainHead(target.repository);
      if (!isSha(target.head)) {
        return reject(`main HEAD of ${target.repository} is not a commit SHA`);
      }
    }

    const selfTag = await client.getTagRef(self.repository, inputs.refName);
    if (selfTag === "absent") {
      return reject(`tag ${inputs.refName} does not exist in ${self.repository}`);
    }
    const selfCommit = await tagCommit(client, self.repository, inputs.refName, selfTag);
    if (!selfCommit.ok) return selfCommit;
    if (selfCommit.sha !== self.head) {
      return reject(`tag ${inputs.refName} in ${self.repository} points at ${selfCommit.sha}, not main HEAD ${self.head}`);
    }

    const partnerTag = await pollPartnerTag(client, partner.repository, inputs.refName, clock, log);
    if (partnerTag === "absent") {
      return reject(`tag ${inputs.refName} did not appear in ${partner.repository} within ${PARTNER_TAG_DEADLINE_MS / 1000} s`);
    }
    const partnerCommit = await tagCommit(client, partner.repository, inputs.refName, partnerTag);
    if (!partnerCommit.ok) return partnerCommit;
    if (partnerCommit.sha !== partner.head) {
      return reject(`tag ${inputs.refName} in ${partner.repository} points at ${partnerCommit.sha}, not main HEAD ${partner.head}`);
    }

    for (const { repository, workflow, head } of [self, partner]) {
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

    for (const { repository, head } of [self, partner]) {
      const again = await client.getMainHead(repository);
      if (again !== head) {
        return reject(`main HEAD of ${repository} moved from ${head} to ${again} during the check`);
      }
    }
  } catch (error) {
    return reject(`lookup failed: ${error instanceof Error ? error.message : String(error)}`);
  }
  return {
    ok: true,
    reason: `tag ${inputs.refName} points at the verified main HEAD in both repositories and both HEADs passed their quality gates`,
  };
}

// Partner tag polling protocol (plan-20261118 FSL-39): a bounded poll. Queries
// are planned at slots start + k * 15 s for k = 0..8 (at most nine queries) on
// a monotonic clock and run strictly one after another. A slot's query is sent
// only if it starts at most 1 s (PARTNER_TAG_JITTER_MS) after the slot, which
// absorbs timer jitter; a later wake, or a slot crossed by a slow previous
// query, is skipped, and the next query goes to the first slot after the
// current time. No query is therefore started later than 121 s after the first
// one. A sent query is handled by its result whenever it returns: a tag
// continues the check, a 404 moves on to the next slot, and any other error or
// timeout rejects. When no slot is left the gate rejects (fail-closed); the
// publisher re-runs the job once the partner tag exists (ER-08). The poll does
// not promise to observe every tag that appears before the last slot.
async function pollPartnerTag(client, repository, refName, clock, log) {
  const maxQueries = PARTNER_TAG_DEADLINE_MS / PARTNER_TAG_POLL_INTERVAL_MS + 1;
  const start = clock.now();
  let plannedTime = start;
  let queries = 0;
  for (;;) {
    const late = clock.now() - plannedTime;
    if (late >= 0 && late <= PARTNER_TAG_JITTER_MS && queries < maxQueries) {
      queries += 1;
      const tag = await client.getTagRef(repository, refName);
      if (tag !== "absent") return tag;
    }
    const nextSlot = Math.max(
      (plannedTime - start) / PARTNER_TAG_POLL_INTERVAL_MS + 1,
      Math.floor((clock.now() - start) / PARTNER_TAG_POLL_INTERVAL_MS) + 1,
    );
    plannedTime = start + nextSlot * PARTNER_TAG_POLL_INTERVAL_MS;
    if (plannedTime - start > PARTNER_TAG_DEADLINE_MS) return "absent";
    const wait = plannedTime - clock.now();
    log(`release-gate: tag ${refName} not yet in ${repository}; retrying in ${Math.max(0, Math.round(wait / 1000))} s\n`);
    if (wait > 0) await clock.sleep(wait);
  }
}

async function tagCommit(client, repository, refName, ref) {
  if (ref === null || typeof ref !== "object" || !isSha(ref.object?.sha)) {
    return reject(`tag lookup for ${refName} in ${repository} returned no tag reference`);
  }
  if (ref.object.type === "commit") return { ok: true, sha: ref.object.sha };
  if (ref.object.type !== "tag") {
    return reject(`tag ${refName} in ${repository} is a ${ref.object.type}, not a commit or annotated tag`);
  }
  const annotated = await client.getAnnotatedTag(repository, ref.object.sha);
  if (annotated === null || typeof annotated !== "object" || !isSha(annotated.object?.sha) || annotated.object.type !== "commit") {
    return reject(`annotated tag ${refName} in ${repository} does not point at a commit`);
  }
  return { ok: true, sha: annotated.object.sha };
}

/**
 * Creates the read-only GitHub client used by `evaluateReleaseGate`.
 *
 * Only four endpoint kinds are requested: `git/ref/tags/<ref>`,
 * `git/tags/<sha>` (annotated tag objects), `git/ref/heads/main` and
 * `actions/workflows/<workflow>/runs?branch=main&event=push&per_page=1`.
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

  function refObject(body, path) {
    const type = body?.object?.type;
    if (typeof type !== "string") throw new Error(`GET ${path} has no object.type`);
    return { object: { type, sha: refSha(body, path) } };
  }

  return {
    async getTagRef(repository, refName) {
      const path = `repos/${repository}/git/ref/tags/${encodeURIComponent(refName)}`;
      const body = await request(path, { allowNotFound: true });
      if (body === undefined) return "absent";
      return refObject(body, path);
    },
    async getAnnotatedTag(repository, sha) {
      if (!isSha(sha)) throw new Error("annotated tag SHA is malformed");
      const path = `repos/${repository}/git/tags/${sha}`;
      return refObject(await request(path), path);
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

export const REQUIRED_ENV = [
  "GH_TOKEN",
  "REF_NAME",
  "SELF_REPOSITORY",
  "SELF_CI_WORKFLOW",
  "PARTNER_REPOSITORY",
  "PARTNER_CI_WORKFLOW",
];

/**
 * Runs the gate from environment variables and returns the exit code.
 *
 * @param {{ env: Record<string, string | undefined>, stdout: { write(s: string): unknown },
 *   stderr: { write(s: string): unknown }, fetch?: typeof fetch,
 *   clock?: { now(): number, sleep(ms: number): Promise<void> } }} io
 */
export async function main({ env, stdout, stderr, fetch: fetchImpl = globalThis.fetch, clock = realClock }) {
  const token = env.GH_TOKEN || "";
  const mask = (text) => (token ? text.split(token).join("***") : text);
  const missing = REQUIRED_ENV.filter((name) => !env[name]);
  if (missing.length > 0) {
    stderr.write(mask(`release-gate: missing environment variables: ${missing.join(", ")}\n`));
    return 1;
  }
  const client = createGitHubClient({
    baseUrl: env.GITHUB_API_URL || "https://api.github.com",
    token,
    fetch: fetchImpl,
  });
  const result = await evaluateReleaseGate({
    client,
    clock,
    log: (line) => stdout.write(mask(line)),
    inputs: {
      refName: env.REF_NAME,
      selfRepository: env.SELF_REPOSITORY,
      selfCiWorkflow: env.SELF_CI_WORKFLOW,
      partnerRepository: env.PARTNER_REPOSITORY,
      partnerCiWorkflow: env.PARTNER_CI_WORKFLOW,
    },
  });
  if (result.ok) {
    stdout.write(mask(`release-gate: pass: ${result.reason}\n`));
    return 0;
  }
  stderr.write(mask(`release-gate: reject: ${result.reason}\n`));
  return 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main({ env: process.env, stdout: process.stdout, stderr: process.stderr }).then(
    (code) => { process.exitCode = code; },
    () => { process.stderr.write("release-gate: reject: unexpected failure\n"); process.exitCode = 1; },
  );
}
