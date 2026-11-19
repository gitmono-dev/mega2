// Deterministic tests for release-gate.mjs (node:test, no network).
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { test } from "node:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import { createGitHubClient, evaluateReleaseGate, REQUIRED_ENV } from "./release-gate.mjs";

const SELF = "gitmono-dev/megaui";
const PARTNER = "gitmono-dev/mega2";
const BOTH = [SELF, PARTNER];
const INPUTS = {
  refName: "v1.2.3",
  selfRepository: SELF,
  selfCiWorkflow: "ci.yml",
  partnerRepository: PARTNER,
  partnerCiWorkflow: "repository-gates.yml",
};
const HEADS = { [SELF]: "a".repeat(40), [PARTNER]: "b".repeat(40) };

function fakeClient(overrides = {}) {
  const tags = { [SELF]: { object: { sha: "1".repeat(40) } }, [PARTNER]: { object: { sha: "2".repeat(40) } }, ...overrides.tags };
  const runs = {
    [SELF]: { head_sha: HEADS[SELF], status: "completed", conclusion: "success" },
    [PARTNER]: { head_sha: HEADS[PARTNER], status: "completed", conclusion: "success" },
    ...overrides.runs,
  };
  const heads = { ...HEADS, ...overrides.heads };
  return {
    async getTagRef(repository) { return tags[repository]; },
    async getMainHead(repository) { return heads[repository]; },
    async getLatestMainPushRun(repository) { return runs[repository]; },
  };
}

async function gate(overrides) {
  return evaluateReleaseGate({ client: fakeClient(overrides), inputs: INPUTS });
}

test("passes when both tags exist and both HEAD runs completed successfully", async () => {
  assert.equal((await gate()).ok, true);
});

for (const repository of BOTH) {
  test(`rejects when the tag is absent in ${repository}`, async () => {
    const result = await gate({ tags: { [repository]: "absent" } });
    assert.equal(result.ok, false);
    assert.ok(result.reason.includes(`does not exist in ${repository}`));
  });

  test(`rejects a malformed tag lookup result in ${repository}`, async () => {
    for (const bad of [null, undefined, "", 0, {}, [], { object: {} }, { object: { sha: "short" } }]) {
      assert.equal((await gate({ tags: { [repository]: bad } })).ok, false);
    }
  });

  test(`rejects when the run in ${repository} is not for main HEAD`, async () => {
    const result = await gate({ runs: { [repository]: { head_sha: "c".repeat(40), status: "completed", conclusion: "success" } } });
    assert.equal(result.ok, false);
    assert.match(result.reason, /not main HEAD/);
  });

  test(`rejects when the run in ${repository} is not completed`, async () => {
    const result = await gate({ runs: { [repository]: { head_sha: HEADS[repository], status: "in_progress", conclusion: null } } });
    assert.equal(result.ok, false);
    assert.match(result.reason, /in_progress, not completed/);
  });

  test(`rejects when the run in ${repository} did not succeed`, async () => {
    const result = await gate({ runs: { [repository]: { head_sha: HEADS[repository], status: "completed", conclusion: "failure" } } });
    assert.equal(result.ok, false);
    assert.match(result.reason, /concluded failure, not success/);
  });

  test(`rejects when ${repository} has no main push run`, async () => {
    const result = await gate({ runs: { [repository]: null } });
    assert.equal(result.ok, false);
    assert.match(result.reason, /has no main push run/);
  });

  test(`rejects when main HEAD and the run SHA are both missing in ${repository}`, async () => {
    const result = await gate({ heads: { [repository]: undefined }, runs: { [repository]: { status: "completed", conclusion: "success" } } });
    assert.equal(result.ok, false);
  });

  test(`rejects a run without a commit SHA in ${repository}`, async () => {
    const result = await gate({ runs: { [repository]: { head_sha: "", status: "completed", conclusion: "success" } } });
    assert.equal(result.ok, false);
  });
}

test("rejects when a lookup throws", async () => {
  const client = { ...fakeClient(), async getMainHead() { throw new Error("boom"); } };
  const result = await evaluateReleaseGate({ client, inputs: INPUTS });
  assert.equal(result.ok, false);
  assert.match(result.reason, /lookup failed: boom/);
});

// --- createGitHubClient (FSL-71) -------------------------------------------

const TOKEN = "ghs_secret_token_value_123";
const BASE = "https://api.example.test";

function jsonResponse(status, body) {
  return { status, text: async () => (typeof body === "string" ? body : JSON.stringify(body)) };
}

function okRoutes(overrides = {}) {
  return {
    [`repos/${SELF}/git/ref/tags/v1.2.3`]: jsonResponse(200, { object: { sha: "1".repeat(40) } }),
    [`repos/${PARTNER}/git/ref/tags/v1.2.3`]: jsonResponse(200, { object: { sha: "2".repeat(40) } }),
    [`repos/${SELF}/git/ref/heads/main`]: jsonResponse(200, { object: { sha: HEADS[SELF] } }),
    [`repos/${PARTNER}/git/ref/heads/main`]: jsonResponse(200, { object: { sha: HEADS[PARTNER] } }),
    [`repos/${SELF}/actions/workflows/ci.yml/runs?branch=main&event=push&per_page=1`]:
      jsonResponse(200, { total_count: 1, workflow_runs: [{ head_sha: HEADS[SELF], status: "completed", conclusion: "success" }] }),
    [`repos/${PARTNER}/actions/workflows/repository-gates.yml/runs?branch=main&event=push&per_page=1`]:
      jsonResponse(200, { total_count: 1, workflow_runs: [{ head_sha: HEADS[PARTNER], status: "completed", conclusion: "success" }] }),
    ...overrides,
  };
}

function fakeFetch(routes, seen = []) {
  return async (url, init) => {
    seen.push({ url, init });
    const path = url.slice(BASE.length + 1);
    const route = routes[path];
    if (typeof route === "function") return route(init);
    if (!route) return jsonResponse(500, { message: "unexpected" });
    return route;
  };
}

async function gateWith(routes, extra = {}) {
  const seen = [];
  const client = createGitHubClient({ baseUrl: BASE, token: TOKEN, fetch: fakeFetch(routes, seen), timeoutMs: 1000, ...extra });
  const result = await evaluateReleaseGate({ client, inputs: INPUTS });
  return { result, seen };
}

test("client requests only the three endpoint kinds", async () => {
  const { result, seen } = await gateWith(okRoutes());
  assert.equal(result.ok, true);
  for (const { url, init } of seen) {
    const path = url.slice(BASE.length + 1);
    assert.match(path, /^repos\/[^/]+\/[^/]+\/(git\/ref\/tags\/[^/]+|git\/ref\/heads\/main|actions\/workflows\/[^/]+\/runs\?branch=main&event=push&per_page=1)$/);
    assert.equal(init.method, "GET");
    assert.equal(init.redirect, "manual");
  }
  assert.equal(seen.length, 6);
});

test("client maps a tag 404 to absent", async () => {
  const { result } = await gateWith(okRoutes({ [`repos/${PARTNER}/git/ref/tags/v1.2.3`]: jsonResponse(404, { message: "Not Found" }) }));
  assert.equal(result.ok, false);
  assert.match(result.reason, /does not exist in gitmono-dev\/mega2/);
});

test("client rejects every other non-2xx status", async () => {
  for (const path of Object.keys(okRoutes())) {
    for (const status of [301, 401, 403, 404, 500]) {
      if (status === 404 && path.includes("/git/ref/tags/")) continue;
      const { result } = await gateWith(okRoutes({ [path]: jsonResponse(status, { message: "x" }) }));
      assert.equal(result.ok, false, `${path} ${status}`);
      assert.match(result.reason, /lookup failed: .*HTTP/);
    }
  }
});

test("client aborts and rejects a request that exceeds timeoutMs", async () => {
  const timers = { setTimeout: (callback) => { queueMicrotask(callback); return 1; }, clearTimeout: () => {} };
  const hanging = (init) => new Promise((_, rejectRequest) => {
    init.signal.addEventListener("abort", () => rejectRequest(new Error("aborted")));
  });
  const { result } = await gateWith(okRoutes({ [`repos/${SELF}/git/ref/heads/main`]: hanging }), { timers });
  assert.equal(result.ok, false);
  assert.match(result.reason, /timed out/);
});

test("client rejects a body that is not JSON", async () => {
  for (const path of Object.keys(okRoutes())) {
    const { result } = await gateWith(okRoutes({ [path]: jsonResponse(200, "<html>oops</html>") }));
    assert.equal(result.ok, false, path);
    assert.match(result.reason, /did not answer JSON/);
  }
});

test("client rejects responses that lack required fields", async () => {
  const runs = `repos/${PARTNER}/actions/workflows/repository-gates.yml/runs?branch=main&event=push&per_page=1`;
  const cases = [
    { [`repos/${SELF}/git/ref/tags/v1.2.3`]: jsonResponse(200, {}) },
    { [`repos/${SELF}/git/ref/heads/main`]: jsonResponse(200, { object: {} }) },
    { [runs]: jsonResponse(200, { workflow_runs: [] }) },
    { [runs]: jsonResponse(200, { total_count: 1, workflow_runs: [{ status: "completed", conclusion: "success" }] }) },
    { [runs]: jsonResponse(200, { total_count: 1, workflow_runs: [{ head_sha: HEADS[PARTNER], conclusion: "success" }] }) },
    { [runs]: jsonResponse(200, { total_count: 1, workflow_runs: [{ head_sha: HEADS[PARTNER], status: "completed" }] }) },
  ];
  for (const overrides of cases) {
    const { result } = await gateWith(okRoutes(overrides));
    assert.equal(result.ok, false);
    assert.match(result.reason, /lookup failed/);
  }
});

test("client errors never contain the token", async () => {
  const failing = [
    okRoutes({ [`repos/${SELF}/git/ref/heads/main`]: jsonResponse(500, { message: TOKEN }) }),
    okRoutes({ [`repos/${SELF}/git/ref/heads/main`]: jsonResponse(200, `not json ${TOKEN}`) }),
    okRoutes({ [`repos/${SELF}/git/ref/heads/main`]: () => { throw new Error(`network ${TOKEN}`); } }),
  ];
  for (const routes of failing) {
    const { result, seen } = await gateWith(routes);
    assert.equal(result.ok, false);
    assert.ok(!result.reason.includes(TOKEN));
    assert.equal(seen[0].init.headers.authorization, `Bearer ${TOKEN}`);
  }
});

test("client rejects a redirect instead of following it", async () => {
  for (const status of [0, 301, 302, 307]) {
    const { result } = await gateWith(okRoutes({ [`repos/${PARTNER}/git/ref/heads/main`]: jsonResponse(status, "") }));
    assert.equal(result.ok, false);
    assert.match(result.reason, /HTTP/);
  }
});

test("client times out while the body is still pending", async () => {
  const timers = { setTimeout: (callback) => { setImmediate(callback); return 1; }, clearTimeout: () => {} };
  const stalledBody = { status: 200, text: () => new Promise(() => {}) };
  const { result } = await gateWith(okRoutes({ [`repos/${SELF}/git/ref/heads/main`]: stalledBody }), { timers });
  assert.equal(result.ok, false);
  assert.match(result.reason, /timed out/);
});

// --- command line entry (FSL-72) -------------------------------------------

const SCRIPT = fileURLToPath(new URL("./release-gate.mjs", import.meta.url));

function startStandIn(routes) {
  return new Promise((resolveServer) => {
    const server = createServer((request, response) => {
      const path = request.url.slice(1);
      const route = routes[path];
      if (!route) { response.writeHead(500); response.end("{}"); return; }
      response.writeHead(route.status, { "content-type": "application/json" });
      response.end(typeof route.body === "string" ? route.body : JSON.stringify(route.body));
    });
    server.listen(0, "127.0.0.1", () => resolveServer(server));
  });
}

function standInRoutes(overrides = {}) {
  return {
    [`repos/${SELF}/git/ref/tags/v1.2.3`]: { status: 200, body: { object: { sha: "1".repeat(40) } } },
    [`repos/${PARTNER}/git/ref/tags/v1.2.3`]: { status: 200, body: { object: { sha: "2".repeat(40) } } },
    [`repos/${SELF}/git/ref/heads/main`]: { status: 200, body: { object: { sha: HEADS[SELF] } } },
    [`repos/${PARTNER}/git/ref/heads/main`]: { status: 200, body: { object: { sha: HEADS[PARTNER] } } },
    [`repos/${SELF}/actions/workflows/ci.yml/runs?branch=main&event=push&per_page=1`]:
      { status: 200, body: { total_count: 1, workflow_runs: [{ head_sha: HEADS[SELF], status: "completed", conclusion: "success" }] } },
    [`repos/${PARTNER}/actions/workflows/repository-gates.yml/runs?branch=main&event=push&per_page=1`]:
      { status: 200, body: { total_count: 1, workflow_runs: [{ head_sha: HEADS[PARTNER], status: "completed", conclusion: "success" }] } },
    ...overrides,
  };
}

function baseEnv(apiUrl) {
  return {
    PATH: process.env.PATH,
    GH_TOKEN: TOKEN,
    REF_NAME: "v1.2.3",
    SELF_REPOSITORY: SELF,
    SELF_CI_WORKFLOW: "ci.yml",
    PARTNER_REPOSITORY: PARTNER,
    PARTNER_CI_WORKFLOW: "repository-gates.yml",
    ...(apiUrl ? { GITHUB_API_URL: apiUrl } : {}),
  };
}

function runCli(env, nodeArgs = []) {
  return new Promise((resolveRun) => {
    const child = spawn(process.execPath, [...nodeArgs, SCRIPT], { env, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("close", (code) => resolveRun({ code, stdout, stderr }));
  });
}

async function withStandIn(routes, body) {
  const server = await startStandIn(routes);
  try {
    return await body(`http://127.0.0.1:${server.address().port}`);
  } finally {
    await new Promise((done) => server.close(done));
  }
}

test("cli reads the API address from GITHUB_API_URL and passes", async () => {
  await withStandIn(standInRoutes(), async (url) => {
    const run = await runCli(baseEnv(url));
    assert.equal(run.code, 0, run.stderr);
    assert.match(run.stdout, /release-gate: pass:/);
  });
});

test("cli defaults the API address to https://api.github.com", async () => {
  const dir = await mkdtemp(join(tmpdir(), "release-gate-test-"));
  try {
    const preload = join(dir, "record-fetch.mjs");
    await writeFile(preload, 'globalThis.fetch = async (url) => { process.stderr.write(`FETCH ${url}\\n`); return { status: 404, text: async () => "{}" }; };\n');
    const env = baseEnv(undefined);
    const run = await runCli(env, ["--import", pathToFileURL(preload).href]);
    const urls = run.stderr.split("\n").filter((line) => line.startsWith("FETCH ")).map((line) => line.slice(6));
    assert.ok(urls.length > 0, run.stderr);
    assert.ok(urls.every((url) => url.startsWith("https://api.github.com/")), urls.join(","));
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("cli reads inputs only from the six documented variables", async () => {
  assert.deepEqual(REQUIRED_ENV, ["GH_TOKEN", "REF_NAME", "SELF_REPOSITORY", "SELF_CI_WORKFLOW", "PARTNER_REPOSITORY", "PARTNER_CI_WORKFLOW"]);
  await withStandIn(standInRoutes(), async (url) => {
    const decoys = { GITHUB_REPOSITORY: "evil/repo", GITHUB_REF_NAME: "v9.9.9", GITHUB_TOKEN: "decoy", CI_WORKFLOW: "docker.yml" };
    const run = await runCli({ ...baseEnv(url), ...decoys });
    assert.equal(run.code, 0, run.stderr);
  });
});

test("cli exits 1 and names only the missing variables", async () => {
  for (const name of REQUIRED_ENV) {
    const env = baseEnv("http://127.0.0.1:9");
    delete env[name];
    const run = await runCli(env);
    assert.equal(run.code, 1);
    assert.equal(run.stderr.trim(), `release-gate: missing environment variables: ${name}`);
    assert.ok(!run.stderr.includes(TOKEN));
  }
});

test("cli exits 1 with a reason when the gate rejects or the client fails", async () => {
  const cases = [
    standInRoutes({ [`repos/${PARTNER}/git/ref/tags/v1.2.3`]: { status: 404, body: { message: "Not Found" } } }),
    standInRoutes({ [`repos/${SELF}/git/ref/heads/main`]: { status: 500, body: { message: TOKEN } } }),
  ];
  for (const routes of cases) {
    await withStandIn(routes, async (url) => {
      const run = await runCli(baseEnv(url));
      assert.equal(run.code, 1);
      assert.match(run.stderr, /release-gate: reject: /);
    });
  }
});

test("cli output never contains the token", async () => {
  // The token is chosen so that it occurs in every message the gate can print.
  const tokenInRef = "v1.2.3";
  const refRoutes = standInRoutes();
  await withStandIn(refRoutes, async (url) => {
    const pass = await runCli({ ...baseEnv(url), GH_TOKEN: tokenInRef });
    assert.equal(pass.code, 0, pass.stderr);
    assert.ok(!pass.stdout.includes(tokenInRef) && !pass.stderr.includes(tokenInRef), pass.stdout);
  });
  await withStandIn(standInRoutes({ [`repos/${PARTNER}/git/ref/tags/v1.2.3`]: { status: 404, body: {} } }), async (url) => {
    const reject = await runCli({ ...baseEnv(url), GH_TOKEN: tokenInRef });
    assert.equal(reject.code, 1);
    assert.ok(!reject.stdout.includes(tokenInRef) && !reject.stderr.includes(tokenInRef), reject.stderr);
  });
  const env = { ...baseEnv("http://127.0.0.1:9"), GH_TOKEN: "REF_NAME" };
  delete env.REF_NAME;
  const missing = await runCli(env);
  assert.equal(missing.code, 1);
  assert.ok(!missing.stderr.includes("REF_NAME") && !missing.stdout.includes("REF_NAME"), missing.stderr);
});
