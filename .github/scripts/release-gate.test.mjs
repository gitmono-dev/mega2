// Deterministic tests for release-gate.mjs (node:test, no network).
import assert from "node:assert/strict";
import { test } from "node:test";

import { evaluateReleaseGate } from "./release-gate.mjs";

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
