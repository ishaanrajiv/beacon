// The client and the generated emitters against a fake Beacon: what is sent, retries, and results.
// Run from clients/node: `node --test tests/` (Node 22.18+ runs TypeScript as it is).

import assert from "node:assert/strict";
import { chmodSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { createServer, type IncomingHttpHeaders, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
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
  for (const name of ["BEACON_TOKEN", "BEACON_STRICT", "BEACON_DISABLED", "BEACON_SPOOL_DIR"]) delete process.env[name];
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
  assert.deepEqual(sent, { ok: true, status: 200, outcome: "opened", item: 7, problem: null, queued: false });
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

test("a category is sent only when given", async () => {
  const args = { thread: "42", tool: "shell", command: "rm -r old-logs", context: "assistant:thread:42" };
  await assistant.assistantPermission({ ...args, category: "PERMISSION_DESTRUCTIVE" });
  assert.equal(seen[0]?.body.category, "PERMISSION_DESTRUCTIVE");
  await assistant.assistantPermission(args);
  assert.equal("category" in (seen[1]?.body ?? {}), false);
  await assistant.snapshot([assistant.assistantPermissionEntry({ ...args, category: "PERMISSION" })]);
  assert.deepEqual(seen[2]?.body.firing, [
    {
      kind: "assistant.permission",
      key: "",
      facts: { thread: "42" },
      slots: { command: "rm -r old-logs", tool: "shell" },
      context: "assistant:thread:42",
      category: "PERMISSION",
    },
  ]);
  await client.send("assistant.permission", { category: "PERMISSION" });
  assert.equal(seen[3]?.body.category, "PERMISSION");
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
  assert.deepEqual(sent, { ok: true, status: 200, outcome: null, item: null, problem: null, queued: false });
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
    queued: false,
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
  assert.deepEqual(sent, { ok: true, status: 0, outcome: null, item: null, problem: null, queued: false });
  assert.equal(seen.length, 0);
});

test("the default address", () => {
  assert.equal(client.DEFAULT_URL, "http://127.0.0.1:8210");
  assert.equal(backup.DEFAULT_URL, client.DEFAULT_URL);
});

// The spool, against the fake Beacon refusing (503, or no answer), then accepting.

let spool = "";
let clock = 1794535200000;

function useSpool(): void {
  spool = join(mkdtempSync(join(tmpdir(), "beacon-spool-")), "spool");
  process.env.BEACON_SPOOL_DIR = spool;
  clock = Date.now();
  client.runtime.now = () => clock;
  client.runtime.droppedTotal = 0;
}

function spooled(): Record<string, unknown>[] {
  let names: string[];
  try {
    names = readdirSync(spool);
  } catch {
    return [];
  }
  return names
    .filter((n) => n.endsWith(".json") && !n.startsWith("."))
    .sort()
    .map((n) => JSON.parse(readFileSync(join(spool, n), "utf8")) as Record<string, unknown>);
}

const bodies = (): Record<string, unknown>[] => spooled().map((f) => f["body"] as Record<string, unknown>);
const refuse = (times = 3): void => {
  for (let i = 0; i < times; i++) answers.push([503, {}]);
};

afterEach(() => {
  if (spool !== "") rmSync(join(spool, ".."), { recursive: true, force: true });
  spool = "";
});

test("a send Beacon can't take is queued, then sent first with its key", async () => {
  useSpool();
  refuse();
  const sent = await client.send("backup.space", { facts: { used_percent: 96 } });
  assert.equal(sent.ok, true);
  assert.equal(sent.queued, true);
  assert.equal(sent.status, 503);
  const [file] = spooled();
  assert.ok(file);
  assert.equal(file["path"], "/v1/events");
  assert.equal(file["queued_at"], clock);
  const firstKey = seen[0]?.headers["idempotency-key"];
  assert.equal(file["key"], firstKey);

  seen.length = 0;
  const next = await client.send("backup.finished", { facts: { size: "2 GB", took: 1000 } });
  assert.deepEqual([next.ok, next.queued], [true, false]);
  assert.deepEqual(
    seen.map((s) => s.body["kind"]),
    ["backup.space", "backup.finished"],
  );
  assert.equal(seen[0]?.headers["idempotency-key"], firstKey);
  assert.deepEqual(spooled(), []);
});

test("no answer is queued too, and a generated file flushes what any client queued", async () => {
  useSpool();
  process.env.BEACON_URL = "http://127.0.0.1:9";
  const sent = await client.send("backup.space", { facts: { used_percent: 96 } });
  assert.equal(sent.queued, true);
  assert.match(sent.problem ?? "", /no answer from Beacon/);
  const waiting = await backup.flush();
  assert.deepEqual([waiting.sent, waiting.left], [0, 1]);
  process.env.BEACON_URL = url;
  const flushed = await backup.flush();
  assert.deepEqual([flushed.sent, flushed.left, flushed.dropped], [1, 0, 0]);
  assert.equal(seen[0]?.body["kind"], "backup.space");
});

test("a refusal is never queued", async () => {
  useSpool();
  answers.push([422, { error: { at: "event kind", message: "no", fix: "" } }]);
  const sent = await client.send("backup.faild");
  assert.deepEqual([sent.ok, sent.queued, sent.status], [false, false, 422]);
  assert.deepEqual(spooled(), []);
});

test("a flush keeps order and stops at the first that can't go", async () => {
  useSpool();
  for (const n of [0, 1, 2]) {
    refuse();
    await client.send("backup.space", { key: `k${n}`, facts: { used_percent: 90 + n } });
    clock += 1;
  }
  assert.deepEqual(
    bodies().map((b) => b["key"]),
    ["k0", "k1", "k2"],
  );
  seen.length = 0;
  answers.length = 0;
  answers.push([200, {}], [503, {}]);
  const first = await client.flush();
  assert.deepEqual([first.sent, first.left], [1, 2]);
  const second = await client.flush();
  assert.deepEqual([second.sent, second.left], [2, 0]);
  assert.deepEqual(
    seen.map((s) => s.body["key"]),
    ["k0", "k1", "k1", "k2"],
  );
});

test("a refusal during a flush is dropped and counted", async () => {
  useSpool();
  for (const n of [0, 1]) {
    refuse();
    await client.send("backup.space", { key: `k${n}` });
    clock += 1;
  }
  answers.length = 0;
  answers.push([422, {}], [200, {}]);
  const flushed = await client.flush();
  assert.deepEqual([flushed.sent, flushed.left, flushed.dropped], [1, 0, 1]);
});

test("after 23 hours a request is dropped and counted", async () => {
  useSpool();
  refuse();
  await client.send("backup.space");
  // An hour inside Beacon's 24-hour idempotency window, which may have begun before this was queued.
  clock += 23 * 60 * 60 * 1000 + 1;
  seen.length = 0;
  const flushed = await client.flush();
  assert.deepEqual([flushed.sent, flushed.left, flushed.dropped, flushed.droppedTotal], [0, 0, 1, 1]);
  assert.equal(seen.length, 0);
});

test("a newer snapshot replaces a queued one, and an old one is dropped", async () => {
  useSpool();
  const realNow = Date.now;
  Date.now = () => clock;
  try {
    refuse();
    await client.snapshot("backup", [client.entry("backup.space", { facts: { used_percent: 96 } })]);
    refuse(1);
    await client.snapshot("ship", []);
    refuse(1);
    await client.snapshot("backup", []);
  } finally {
    Date.now = realNow;
  }
  assert.deepEqual(
    bodies().map((b) => b["source"]),
    ["ship", "backup"],
  );
  assert.deepEqual(bodies()[1]?.["firing"], []);
  clock += 5 * 60 * 1000 + 1;
  seen.length = 0;
  const flushed = await client.flush();
  assert.deepEqual([flushed.sent, flushed.dropped], [0, 2]);
  assert.equal(seen.length, 0);
});

const pad = (ms: number, sequence: number): string => `${String(ms).padStart(15, "0")}-${String(sequence).padStart(6, "0")}`;
const names = (): string[] =>
  readdirSync(spool)
    .filter((n) => n.endsWith(".json") && !n.startsWith("."))
    .sort();
const requestText = (key: string): string =>
  JSON.stringify({ path: "/v1/events", key: "0".repeat(32), queued_at: clock, body: { kind: "backup.space", key } });
function anotherProcessQueued(name: string, key: string): void {
  mkdirSync(spool, { recursive: true });
  writeFileSync(join(spool, name), requestText(key));
}

test("a new file sorts after any already there, whatever wrote it", async () => {
  useSpool();
  // Another process's clock is ahead of this one, and it named its file the old way.
  anotherProcessQueued(`${pad(clock + 5000, 3)}-0a1b2c3d.json`, "ahead");
  refuse(1);
  await client.send("backup.space", { key: "mine" });
  assert.deepEqual(
    bodies().map((b) => b["key"]),
    ["ahead", "mine"],
  );
  assert.equal(names().at(-1), `${pad(clock + 5000, 4)}.json`);
});

test("within a millisecond the sequence goes on from the highest there", async () => {
  useSpool();
  anotherProcessQueued(`${pad(clock, 500)}.json`, "theirs");
  refuse(1);
  await client.send("backup.space", { key: "mine" });
  assert.deepEqual(names(), [`${pad(clock, 500)}.json`, `${pad(clock, 501)}.json`]);
  // Past the last sequence, the next millisecond.
  anotherProcessQueued(`${pad(clock, 999999)}.json`, "full");
  refuse(1);
  await client.send("backup.space", { key: "next" });
  assert.equal(names().at(-1), `${pad(clock + 1, 0)}.json`);
  assert.deepEqual(
    bodies().map((b) => b["key"]),
    ["theirs", "mine", "full", "next"],
  );
});

test("a name another process takes first is not overwritten", async () => {
  useSpool();
  const realFs = client.runtime.fs;
  let taken = false;
  const firstTaken =
    (real: (from: string, to: string) => Promise<void>) =>
    async (from: string, to: string): Promise<void> => {
      if (!taken && !basename(to).startsWith(".")) {
        taken = true;
        writeFileSync(to, requestText("theirs"));
      }
      await real(from, to);
    };
  client.runtime.fs = async () => {
    const fs = await realFs.call(client.runtime);
    return { ...fs, link: firstTaken(fs.link), rename: firstTaken(fs.rename) };
  };
  let sent: client.Sent | undefined;
  try {
    refuse();
    sent = await client.send("backup.space", { key: "mine" });
  } finally {
    client.runtime.fs = realFs;
  }
  assert.equal(sent.queued, true, sent.problem ?? "");
  assert.deepEqual(
    bodies().map((b) => b["key"]),
    ["theirs", "mine"],
  );
});

test("the folder and its files are private to this user", { skip: process.platform === "win32" }, async () => {
  useSpool();
  const realFs = client.runtime.fs;
  const temporary: number[] = [];
  client.runtime.fs = async () => {
    const fs = await realFs.call(client.runtime);
    const link = async (from: string, to: string): Promise<void> => {
      temporary.push(statSync(from).mode & 0o777);
      await fs.link(from, to);
    };
    return { ...fs, link };
  };
  const umask = process.umask(0o022);
  const shared = join(spool, "..", "shared");
  try {
    refuse();
    await client.send("backup.space", { key: "mine" });
    // A folder that is already there keeps its mode.
    mkdirSync(shared);
    chmodSync(shared, 0o750);
    process.env.BEACON_SPOOL_DIR = shared;
    refuse();
    await client.send("backup.space", { key: "shared" });
  } finally {
    process.umask(umask);
    client.runtime.fs = realFs;
  }
  assert.equal(statSync(spool).mode & 0o777, 0o700);
  assert.deepEqual(temporary, [0o600, 0o600]);
  assert.equal(statSync(join(spool, names()[0] ?? "")).mode & 0o777, 0o600);
  assert.equal(statSync(shared).mode & 0o777, 0o750);
});

test("a snapshot whose replacement can't be written is kept", async () => {
  useSpool();
  const realNow = Date.now;
  const realFs = client.runtime.fs;
  Date.now = () => clock;
  let sent: client.Sent | undefined;
  try {
    refuse();
    await client.snapshot("backup", [client.entry("backup.space", { facts: { used_percent: 96 } })]);
    client.runtime.fs = async () => {
      const fs = await realFs.call(client.runtime);
      return {
        ...fs,
        open: async (path: string, flags: "wx" | "r", mode?: number) => {
          if (flags === "wx") throw new Error("ENOSPC: no space left on device");
          return fs.open(path, flags, mode);
        },
      };
    };
    refuse(1);
    sent = await client.snapshot("backup", []);
  } finally {
    Date.now = realNow;
    client.runtime.fs = realFs;
  }
  assert.deepEqual([sent.ok, sent.queued], [false, false]);
  assert.match(sent.problem ?? "", /can't be written/);
  const [kept] = bodies();
  assert.equal(bodies().length, 1);
  assert.equal((kept?.["firing"] as unknown[]).length, 1);
});

test("a request is synced to disk before what it replaces is removed", async () => {
  useSpool();
  const realNow = Date.now;
  const realFs = client.runtime.fs;
  Date.now = () => clock;
  const steps: string[] = [];
  let sent: client.Sent | undefined;
  try {
    refuse();
    await client.snapshot("backup", [client.entry("backup.space", { facts: { used_percent: 96 } })]);
    client.runtime.fs = async () => {
      const fs = await realFs.call(client.runtime);
      return {
        ...fs,
        open: async (path: string, flags: "wx" | "r", mode?: number) => {
          const file = await fs.open(path, flags, mode);
          return {
            writeFile: (data: string) => file.writeFile(data),
            sync: async () => {
              steps.push(flags === "r" ? "sync folder" : "sync file");
              await file.sync();
            },
            close: () => file.close(),
          };
        },
        link: async (from: string, to: string) => {
          steps.push("publish");
          await fs.link(from, to);
        },
        unlink: async (path: string) => {
          steps.push(basename(path).startsWith(".") ? "remove temporary" : "remove replaced");
          await fs.unlink(path);
        },
      };
    };
    refuse(1);
    sent = await client.snapshot("backup", []);
  } finally {
    Date.now = realNow;
    client.runtime.fs = realFs;
  }
  assert.equal(sent.queued, true, sent.problem ?? "");
  assert.deepEqual(steps, ["sync file", "publish", "remove temporary", "sync folder", "remove replaced"]);
});

test("a write that fails part way leaves no temporary file", async () => {
  useSpool();
  const realFs = client.runtime.fs;
  client.runtime.fs = async () => {
    const fs = await realFs.call(client.runtime);
    return {
      ...fs,
      open: async (path: string, flags: "wx" | "r", mode?: number) => {
        const file = await fs.open(path, flags, mode);
        return {
          writeFile: async (data: string) => {
            await file.writeFile(data.slice(0, data.length / 2));
            throw new Error("ENOSPC: no space left on device");
          },
          sync: () => file.sync(),
          close: () => file.close(),
        };
      },
    };
  };
  let sent: client.Sent | undefined;
  try {
    refuse();
    sent = await client.send("backup.space", { key: "mine" });
  } finally {
    client.runtime.fs = realFs;
  }
  assert.deepEqual([sent.ok, sent.queued], [false, false]);
  assert.match(sent.problem ?? "", /can't be written/);
  assert.deepEqual(readdirSync(spool), []);
});

test("a temporary file a stopped process left is removed once stale", async () => {
  useSpool();
  mkdirSync(spool, { recursive: true });
  const left = (name: string, minutesAgo: number): string => {
    writeFileSync(join(spool, name), "half a requ");
    const at = (clock - minutesAgo * 60 * 1000) / 1000;
    utimesSync(join(spool, name), at, at);
    return name;
  };
  const stale = left(".0123456789abcdef.tmp", 11);
  const beingWritten = left(".fedcba9876543210.tmp", 1);
  const kinds = left(".kinds-backup.json", 600);
  await client.flush();
  assert.deepEqual(readdirSync(spool).sort(), [beingWritten, kinds].sort());
  // Queuing sweeps too: a service that only ever sends still clears them.
  left(stale, 11);
  refuse();
  await client.send("backup.space", { key: "mine" });
  assert.equal(readdirSync(spool).includes(stale), false);
  assert.equal(names().length, 1);
});

test("a flush leaves a new request that took the name of the one it sent", async () => {
  useSpool();
  refuse();
  await client.send("backup.space", { key: "first" });
  const [name] = names();
  const realAttempt = client.runtime.attempt;
  client.runtime.attempt = async function (this: typeof client.runtime, path: string, data: string, key: string) {
    const answer = await realAttempt.call(this, path, data, key);
    // Meanwhile another flush sent and removed the file, and a writer whose clock had stepped
    // back queued a new request under the same name.
    const request = { path: "/v1/events", key: "f".repeat(32), queued_at: clock, body: { kind: "backup.space", key: "second" } };
    writeFileSync(join(spool, name ?? ""), JSON.stringify(request));
    return answer;
  };
  let flushed: client.Flushed | undefined;
  try {
    flushed = await client.flush();
  } finally {
    client.runtime.attempt = realAttempt;
  }
  assert.deepEqual([flushed.sent, flushed.dropped], [1, 0]);
  assert.deepEqual(
    bodies().map((b) => b["key"]),
    ["second"],
  );
  flushed = await client.flush();
  assert.deepEqual([flushed.sent, flushed.left], [1, 0]);
  assert.deepEqual(
    seen.slice(-2).map((r) => (r.body as Record<string, unknown>)["key"]),
    ["first", "second"],
  );
});

test("a replaced snapshot whose name a new request took is not removed", async () => {
  useSpool();
  const realNow = Date.now;
  const realFs = client.runtime.fs;
  Date.now = () => clock;
  let sent: client.Sent | undefined;
  try {
    refuse();
    await client.snapshot("backup", [client.entry("backup.space", { facts: { used_percent: 96 } })]);
    const [name] = names();
    client.runtime.fs = async () => {
      const fs = await realFs.call(client.runtime);
      return {
        ...fs,
        link: async (from: string, to: string) => {
          await fs.link(from, to);
          writeFileSync(join(spool, name ?? ""), requestText("theirs"));
        },
      };
    };
    refuse(1);
    sent = await client.snapshot("backup", []);
  } finally {
    Date.now = realNow;
    client.runtime.fs = realFs;
  }
  assert.equal(sent.queued, true, sent.problem ?? "");
  assert.equal(names().length, 2);
  assert.equal(bodies()[0]?.["key"], "theirs");
});

test("the spool is bounded, oldest dropped first", async () => {
  useSpool();
  const limits = { ...client.SPOOL_LIMITS };
  client.SPOOL_LIMITS.files = 2;
  let sent: client.Sent | undefined;
  try {
    for (const n of [0, 1, 2]) {
      refuse(n === 0 ? 3 : 1);
      sent = await client.send("backup.space", { key: `k${n}` });
      clock += 1;
    }
    assert.match(sent?.problem ?? "", /dropped the 1 oldest/);
    assert.deepEqual(
      bodies().map((b) => b["key"]),
      ["k1", "k2"],
    );
  } finally {
    Object.assign(client.SPOOL_LIMITS, limits);
  }
});

test("a request larger than the whole spool is not queued and evicts nothing", async () => {
  useSpool();
  const limits = { ...client.SPOOL_LIMITS };
  let sent: client.Sent | undefined;
  try {
    for (const n of [0, 1]) {
      refuse(n === 0 ? 3 : 1);
      await client.send("backup.space", { key: `k${n}` });
    }
    client.SPOOL_LIMITS.bytes = 10;
    refuse(1);
    sent = await client.send("backup.space", { key: "big" });
  } finally {
    Object.assign(client.SPOOL_LIMITS, limits);
  }
  assert.deepEqual([sent.ok, sent.queued], [false, false]);
  assert.match(sent.problem ?? "", /larger than the spool's limit/);
  assert.deepEqual(
    bodies().map((b) => b["key"]),
    ["k0", "k1"],
  );
  assert.equal(client.runtime.droppedTotal, 1);
});

test("the spool's limit is in bytes, not UTF-16 units", async () => {
  useSpool();
  const limits = { ...client.SPOOL_LIMITS };
  const key = "\u00e9".repeat(1000);
  const request = JSON.stringify({ path: "/v1/events", key: "0".repeat(32), queued_at: clock, body: { kind: "backup.space", key } });
  let sent: client.Sent | undefined;
  try {
    // The request fits in UTF-16 units, not in bytes.
    client.SPOOL_LIMITS.bytes = request.length + 100;
    refuse();
    sent = await client.send("backup.space", { key });
  } finally {
    Object.assign(client.SPOOL_LIMITS, limits);
  }
  assert.deepEqual([sent.ok, sent.queued], [false, false]);
  assert.deepEqual(spooled(), []);
});

test("off without a folder", async () => {
  refuse();
  const sent = await client.send("backup.space");
  assert.deepEqual([sent.ok, sent.queued], [false, false]);
  assert.deepEqual(await client.flush(), { sent: 0, left: 0, dropped: 0, droppedTotal: client.runtime.droppedTotal, problem: null });
});
