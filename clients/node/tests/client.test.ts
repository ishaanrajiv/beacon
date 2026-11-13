// The client and the generated emitters against a fake Beacon: what is sent, retries, and results.
// Run from clients/node: `node --test tests/` (Node 22.18+ runs TypeScript as it is).

import assert from "node:assert/strict";
import { createServer, type IncomingHttpHeaders, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { after, afterEach, before, beforeEach, test } from "node:test";
import * as client from "../beacon_client.ts";
import * as assistant from "./gen/assistant_kinds.ts";
import * as backup from "./gen/backup_kinds.ts";
import * as ship from "./gen/ship_kinds.ts";

interface Seen {
  path: string;
  headers: IncomingHttpHeaders;
  body: Record<string, unknown>;
}

const seen: Seen[] = [];
const answers: [number, unknown][] = [];
let server: Server;
let url = "";

before(async () => {
  server = createServer((request, response) => {
    let text = "";
    request.on("data", (chunk: Buffer) => (text += chunk.toString()));
    request.on("end", () => {
      seen.push({ path: request.url ?? "", headers: request.headers, body: JSON.parse(text) });
      const [status, answer] = answers.shift() ?? [200, { outcome: "opened", item: { id: 7 } }];
      response.writeHead(status, { "Content-Type": "application/json" });
      response.end(JSON.stringify(answer));
    });
  });
  await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
  url = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
});

after(() => server.close());

const defaultDelays = [...client.runtime.retryDelays];

beforeEach(() => {
  seen.length = 0;
  answers.length = 0;
  process.env.BEACON_URL = url;
  for (const name of ["BEACON_TOKEN", "BEACON_STRICT", "BEACON_DISABLED"]) delete process.env[name];
  client.runtime.retryDelays = [0, 0];
});

afterEach(() => {
  delete process.env.BEACON_URL;
});

test("a generated function sends its kind, facts and slots", async () => {
  process.env.BEACON_TOKEN = "t0ken";
  const sent = await backup.backupFailed({
    last_ok: new Date(Date.UTC(2026, 10, 13, 2, 0)),
    reason: "The disk is full",
    key: "nightly",
    labels: { service: "backup" },
    url: "/system/backups",
  });
  assert.deepEqual(sent, { ok: true, status: 200, outcome: "opened", item: 7, problem: null });
  const [request] = seen;
  assert.ok(request);
  assert.equal(request.path, "/v1/events");
  assert.equal(request.headers["content-type"], "application/json");
  assert.equal(request.headers.authorization, "Bearer t0ken");
  assert.match(String(request.headers["idempotency-key"]), /^[0-9a-f]{32}$/);
  assert.deepEqual(request.body, {
    kind: "backup.failed",
    key: "nightly",
    facts: { last_ok: 1794535200000 },
    slots: { reason: "The disk is full" },
    labels: { service: "backup" },
    url: "/system/backups",
  });
});

test("durations are milliseconds and a time may be a number", async () => {
  await backup.backupFinished({ size: "2 GB", took: 240000 });
  await backup.backupStale({ since: 90000, last_ok: 1794535200000 });
  assert.deepEqual(seen[0]?.body.facts, { size: "2 GB", took: 240000 });
  assert.deepEqual(seen[1]?.body.facts, { since: 90000, last_ok: 1794535200000 });
  assert.equal(seen[0]?.headers.authorization, undefined);
});

test("resolving sends the state and no values", async () => {
  await backup.backupFailedResolved({ key: "nightly" });
  assert.deepEqual(seen[0]?.body, { kind: "backup.failed", key: "nightly", state: "resolved" });
  await backup.backupFailedResolved();
  assert.deepEqual(seen[1]?.body, { kind: "backup.failed", key: "", state: "resolved" });
  await assistant.assistantApprovalResolved({ context: "assistant:thread:42" });
  assert.equal(seen[2]?.body.context, "assistant:thread:42");
});

test("an alert with only a slot", async () => {
  await assistant.assistantUnavailable({ cause: "The tools are offline" });
  assert.deepEqual(seen[0]?.body.slots, { cause: "The tools are offline" });
});

test("a snapshot lists its entries", async () => {
  answers.push([200, { ok: true, opened: [4183], observed: true }]);
  const sent = await backup.snapshot([backup.backupSpaceEntry({ used_percent: 96, labels: { service: "backup" } })], {
    kinds: ["backup.space"],
    stale_after: "30m",
  });
  assert.deepEqual(sent, { ok: true, status: 200, outcome: null, item: null, problem: null });
  const body = { ...seen[0]?.body };
  assert.equal(seen[0]?.path, "/v1/snapshots");
  assert.equal(typeof body.sent_at, "number");
  delete body.sent_at;
  assert.deepEqual(body, {
    source: "backup",
    observed: true,
    firing: [{ kind: "backup.space", key: "", facts: { used_percent: 96 }, labels: { service: "backup" } }],
    kinds: ["backup.space"],
    stale_after: "30m",
  });
});

test("unobserved sends the reason and no entries", async () => {
  await backup.unobserved("The bucket's status page didn't load.");
  assert.equal(seen[0]?.body.observed, false);
  assert.equal(seen[0]?.body.reason, "The bucket's status page didn't load.");
  assert.equal(seen[0]?.body.firing, undefined);
});

test("5xx is retried with the same key, three attempts at most", async () => {
  answers.push([503, {}], [503, {}]);
  const sent = await client.send("backup.space", { facts: { used_percent: 96 } });
  assert.equal(sent.ok, true);
  assert.equal(seen.length, 3);
  assert.equal(new Set(seen.map((s) => s.headers["idempotency-key"])).size, 1);

  seen.length = 0;
  answers.push([429, {}], [408, {}], [500, {}], [200, {}]);
  const failed = await client.send("backup.space", { facts: { used_percent: 96 } });
  assert.equal(failed.status, 500);
  assert.equal(seen.length, 3);
});

test("retries wait 1 s, then 3 s", () => {
  assert.deepEqual(defaultDelays, [1000, 3000]);
});

test("a refusal is final and carries the problem", async () => {
  const problem = { at: "event kind", message: "`backup.faild` is not a declared kind", fix: "did you mean `backup.failed`?" };
  answers.push([422, { error: problem, problems: [problem] }]);
  const sent = await client.send("backup.faild");
  assert.equal(seen.length, 1);
  assert.deepEqual(sent, {
    ok: false,
    status: 422,
    outcome: null,
    item: null,
    problem: "event kind: `backup.faild` is not a declared kind\n  fix: did you mean `backup.failed`?",
  });
});

test("no answer is status 0", async () => {
  process.env.BEACON_URL = "http://127.0.0.1:9";
  const sent = await client.send("backup.space", { facts: { used_percent: 96 } });
  assert.equal(sent.ok, false);
  assert.equal(sent.status, 0);
  assert.match(sent.problem ?? "", /no answer from Beacon/);
});

test("strict mode rejects", async () => {
  process.env.BEACON_STRICT = "1";
  answers.push([422, { error: { at: "event", message: "no", fix: "" } }]);
  await assert.rejects(client.send("backup.space"), (error: unknown) => {
    assert.ok(error instanceof client.BeaconError);
    assert.equal(error.message, "event: no");
    return true;
  });
});

test("configure overrides the environment", async () => {
  // Each generated file has its own settings; only this test configures ship.
  ship.configure({ url, strict: true });
  process.env.BEACON_URL = "http://127.0.0.1:9";
  answers.push([400, {}]);
  await assert.rejects(ship.shipDeployed({ app: "web", summary: "Two fixes" }), ship.BeaconError);
  ship.configure({ disabled: true });
  assert.equal((await ship.shipDeployed({ app: "web", summary: "Two fixes" })).status, 0);
  assert.equal(seen.length, 1);
});

test("disabled sends nothing", async () => {
  process.env.BEACON_DISABLED = "1";
  const sent = await backup.backupSpace({ used_percent: 96 });
  assert.deepEqual(sent, { ok: true, status: 0, outcome: null, item: null, problem: null });
  assert.equal(seen.length, 0);
});

test("the default address", () => {
  assert.equal(client.DEFAULT_URL, "http://127.0.0.1:8300");
  assert.equal(backup.DEFAULT_URL, client.DEFAULT_URL);
});
