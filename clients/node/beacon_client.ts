// Beacon's TypeScript client: no dependencies, Node 18+ (the global `fetch`) or a browser
// (spec/clients.md).
//
// `beacon gen --lang typescript` copies everything between the two `runtime` markers below into
// each generated file, so a service needs no package: the generated file is this client plus one
// typed function per declared kind. Used on its own, `send`, `entry` and `snapshot` at the end of
// this file take any kind by name, unchecked until Beacon answers.

// --- runtime start ---
export const DEFAULT_URL = "http://127.0.0.1:8210";
/** The spool's limits (spec/clients.md, "The spool"). */
export const SPOOL_LIMITS = { files: 1000, bytes: 8 * 1024 * 1024, ageMs: 24 * 60 * 60 * 1000, snapshotAgeMs: 5 * 60 * 1000 };
const SPOOL_LAST_SEQUENCE = 999_999;
const SPOOL_NAME_TRIES = 20;
/** `<ms>-<sequence>.json`, or the older `<ms>-<sequence>-<hex>.json`. */
const SPOOL_NAME = /^(\d{15})-(\d{6})(?:-[0-9a-f]+)?\.json$/;

/**
 * What Beacon answered. `ok` is true for a 2xx; otherwise `problem` says what went wrong.
 * `queued` is true when Beacon couldn't take it and it was written to the spool instead: `ok` is
 * true, `status` is the last answer (0 for none) and `problem` says why it waits.
 */
export interface Sent {
  ok: boolean;
  status: number;
  outcome: string | null;
  item: number | null;
  problem: string | null;
  queued: boolean;
}

/** What a `flush()` did: requests delivered, still waiting, and dropped (now and since start). */
export interface Flushed {
  sent: number;
  left: number;
  dropped: number;
  droppedTotal: number;
  problem: string | null;
}

/** The parts of `node:fs/promises` the spool uses, typed here so the file needs no `@types/node`. */
interface SpoolFs {
  mkdir(path: string, options: { recursive: true }): Promise<unknown>;
  readdir(path: string): Promise<string[]>;
  readFile(path: string, encoding: "utf8"): Promise<string>;
  writeFile(path: string, data: string, options: { flag: "wx" }): Promise<void>;
  link(from: string, to: string): Promise<void>;
  unlink(path: string): Promise<void>;
  stat(path: string): Promise<{ size: number }>;
}

interface Spooled {
  path: string;
  key: string;
  queued_at: number;
  body: Record<string, unknown>;
}

/** One firing entry of a snapshot. Make it with a kind's `…Entry` function. */
export interface Entry {
  readonly kind: string;
  readonly body: Readonly<Record<string, unknown>>;
}

/** A call that wasn't `ok`, thrown only in strict mode (`BEACON_STRICT=1` or `configure({ strict: true })`). */
export class BeaconError extends Error {}

interface Settings {
  url?: string;
  token?: string;
  strict?: boolean;
  disabled?: boolean;
  spoolDir?: string;
}

const settings: Settings = {};

/**
 * Overrides `BEACON_URL`, `BEACON_TOKEN`, `BEACON_STRICT`, `BEACON_DISABLED` and `BEACON_SPOOL_DIR`.
 * Left out keeps the current choice; `spoolDir: ""` turns the spool off.
 */
export function configure(options: {
  url?: string;
  token?: string;
  strict?: boolean;
  disabled?: boolean;
  spoolDir?: string;
}): void {
  if (options.url !== undefined) settings.url = options.url;
  if (options.token !== undefined) settings.token = options.token;
  if (options.strict !== undefined) settings.strict = options.strict;
  if (options.disabled !== undefined) settings.disabled = options.disabled;
  if (options.spoolDir !== undefined) settings.spoolDir = options.spoolDir;
}

interface EventOptions {
  key?: string | undefined;
  facts?: Record<string, unknown> | undefined;
  slots?: Record<string, string> | undefined;
  labels?: Record<string, string> | undefined;
  context?: string | undefined;
  url?: string | undefined;
  category?: string | undefined;
}

class BeaconRuntime {
  /** Waits before the second and third attempts. */
  retryDelays: number[] = [1000, 3000];
  /** Each attempt's limit. */
  timeout = 5000;
  /** Requests the spool has dropped since this process started. */
  droppedTotal = 0;
  /** The spool's clock, for tests. */
  now: () => number = () => Date.now();
  /** The flush running now: a second waits for it rather than sending the same files again. */
  flushing: Promise<Flushed> | undefined;

  env(name: string): string | undefined {
    const host = globalThis as { process?: { env?: Record<string, string | undefined> } };
    const value = host.process?.env?.[name];
    return value === "" ? undefined : value;
  }

  flag(name: string): boolean {
    return ["1", "true", "yes", "on"].includes((this.env(name) ?? "").trim().toLowerCase());
  }

  url(): string {
    return (settings.url ?? this.env("BEACON_URL") ?? DEFAULT_URL).replace(/\/+$/, "");
  }

  token(): string | undefined {
    return settings.token ?? this.env("BEACON_TOKEN");
  }

  strict(): boolean {
    return settings.strict ?? this.flag("BEACON_STRICT");
  }

  disabled(): boolean {
    return settings.disabled ?? this.flag("BEACON_DISABLED");
  }

  spoolDir(): string | undefined {
    const dir = settings.spoolDir ?? this.env("BEACON_SPOOL_DIR");
    return dir === "" ? undefined : dir;
  }

  timeMs(value: number | Date | undefined): number | undefined {
    return value instanceof Date ? value.getTime() : value;
  }

  /** The values that were given: an optional fact left out is not sent. */
  present(values: Record<string, unknown>): Record<string, unknown> {
    const out: Record<string, unknown> = {};
    for (const [name, value] of Object.entries(values)) {
      if (value !== undefined && value !== null) out[name] = value;
    }
    return out;
  }

  eventBody(kind: string, options: EventOptions): Record<string, unknown> {
    const body: Record<string, unknown> = { kind, key: options.key ?? "" };
    if (options.facts !== undefined) body["facts"] = options.facts;
    if (options.slots !== undefined) body["slots"] = options.slots;
    if (options.labels !== undefined && Object.keys(options.labels).length > 0) body["labels"] = options.labels;
    if (options.context !== undefined) body["context"] = options.context;
    if (options.url !== undefined) body["url"] = options.url;
    if (options.category !== undefined) body["category"] = options.category;
    return body;
  }

  event(kind: string, options: EventOptions & { state?: "resolved" | undefined }): Promise<Sent> {
    const body = this.eventBody(kind, options);
    if (options.state !== undefined) body["state"] = options.state;
    return this.post("/v1/events", body);
  }

  entry(kind: string, options: EventOptions): Entry {
    return { kind, body: this.eventBody(kind, options) };
  }

  snapshot(
    source: string,
    firing: readonly Entry[],
    options: { kinds?: readonly string[] | undefined; stale_after?: string | undefined; reason?: string | undefined },
  ): Promise<Sent> {
    const body: Record<string, unknown> = { source, observed: options.reason === undefined };
    if (options.reason !== undefined) body["reason"] = options.reason;
    else body["firing"] = firing.map((entry) => entry.body);
    if (options.kinds !== undefined) body["kinds"] = [...options.kinds];
    if (options.stale_after !== undefined) body["stale_after"] = options.stale_after;
    body["sent_at"] = Date.now();
    return this.post("/v1/snapshots", body);
  }

  async post(path: string, body: Record<string, unknown>): Promise<Sent> {
    if (this.disabled()) return { ok: true, status: 0, outcome: null, item: null, problem: null, queued: false };
    const key = this.idempotencyKey();
    const spool = this.spoolDir();
    if (spool !== undefined) {
      // What waits goes first; while anything still waits, this waits behind it, in order.
      const flushed = await this.flush();
      if (flushed.left > 0) {
        const waiting = { ok: false, status: 0, outcome: null, item: null, problem: flushed.problem, queued: false };
        return this.finish(await this.queue(spool, path, key, body, waiting));
      }
    }
    let sent = await this.attemptAll(path, JSON.stringify(body), key);
    if (spool !== undefined && this.retryable(sent.status)) sent = await this.queue(spool, path, key, body, sent);
    return this.finish(sent);
  }

  finish(sent: Sent): Sent {
    if (!sent.ok && this.strict()) throw new BeaconError(sent.problem ?? `Beacon answered ${sent.status}`);
    return sent;
  }

  retryable(status: number): boolean {
    return status === 0 || status === 408 || status === 429 || status >= 500;
  }

  async attemptAll(path: string, data: string, key: string): Promise<Sent> {
    const delays = [...this.retryDelays];
    for (;;) {
      const sent = await this.attempt(path, data, key);
      const delay = delays.shift();
      if (!this.retryable(sent.status) || delay === undefined) return sent;
      await new Promise((done) => setTimeout(done, delay));
    }
  }

  async fs(): Promise<SpoolFs> {
    // A name the type checker doesn't follow, so neither it nor a browser bundle needs Node's types.
    const name = "node:fs/promises";
    return (await import(name)) as SpoolFs;
  }

  /** When a request file was queued, and its place among those queued in that millisecond. */
  position(name: string): [number, number] | undefined {
    const match = SPOOL_NAME.exec(name);
    return match ? [Number(match[1]), Number(match[2])] : undefined;
  }

  /** The spool's request files, oldest first: by the time and sequence their names start with, as numbers. */
  async spooled(fs: SpoolFs, spool: string): Promise<string[]> {
    let names: string[];
    try {
      names = (await fs.readdir(spool)).filter((n) => n.endsWith(".json") && !n.startsWith("."));
    } catch {
      return [];
    }
    return names.sort((x, y) => {
      const [a, b] = [this.position(x), this.position(y)];
      if (a === undefined || b === undefined) return a !== undefined ? -1 : b !== undefined ? 1 : x < y ? -1 : x > y ? 1 : 0;
      return a[0] - b[0] || a[1] - b[1] || (x < y ? -1 : x > y ? 1 : 0);
    });
  }

  /** After every file already there, whatever process wrote it, and never before the clock. */
  async nextName(fs: SpoolFs, spool: string, now: number): Promise<string> {
    let [ms, sequence] = [now, 0];
    for (const name of await this.spooled(fs, spool)) {
      const at = this.position(name);
      if (at !== undefined && (at[0] > ms || (at[0] === ms && at[1] >= sequence))) [ms, sequence] = [at[0], at[1] + 1];
    }
    if (sequence > SPOOL_LAST_SEQUENCE) [ms, sequence] = [ms + 1, 0];
    return `${String(ms).padStart(15, "0")}-${String(sequence).padStart(6, "0")}.json`;
  }

  /**
   * Writes `data` under a temporary name, then hard-links it to the next name, which fails if another
   * process took that name first; then it looks again. So no two processes take one name, and no reader
   * sees half a file.
   */
  async publish(fs: SpoolFs, spool: string, data: string, now: number): Promise<void> {
    const temporary = `.${this.idempotencyKey().slice(0, 16)}.tmp`;
    await fs.writeFile(`${spool}/${temporary}`, data, { flag: "wx" });
    try {
      for (let tries = 0; tries < SPOOL_NAME_TRIES; tries++) {
        try {
          await fs.link(`${spool}/${temporary}`, `${spool}/${await this.nextName(fs, spool, now)}`);
          return;
        } catch (error) {
          if ((error as { code?: unknown }).code !== "EEXIST") throw error;
        }
      }
      throw new Error(`another process took each of ${SPOOL_NAME_TRIES} names in a row`);
    } finally {
      await this.drop(fs, spool, temporary);
    }
  }

  async readSpooled(fs: SpoolFs, spool: string, name: string): Promise<Spooled | undefined> {
    let value: Record<string, unknown>;
    try {
      value = this.object(JSON.parse(await fs.readFile(`${spool}/${name}`, "utf8")));
    } catch {
      return undefined;
    }
    const { path, key, queued_at: queuedAt, body } = value;
    if (typeof path !== "string" || typeof key !== "string" || typeof queuedAt !== "number") return undefined;
    if (typeof body !== "object" || body === null || Array.isArray(body)) return undefined;
    return { path, key, queued_at: queuedAt, body: body as Record<string, unknown> };
  }

  async exists(fs: SpoolFs, file: string): Promise<boolean> {
    try {
      await fs.stat(file);
      return true;
    } catch {
      return false;
    }
  }

  async drop(fs: SpoolFs, spool: string, name: string): Promise<void> {
    try {
      await fs.unlink(`${spool}/${name}`);
    } catch {
      // Already gone.
    }
  }

  /** Past 24 hours (the server's idempotency window), or a snapshot past what `sent_at` allows. */
  expired(request: Spooled, now: number): boolean {
    if (now - request.queued_at > SPOOL_LIMITS.ageMs) return true;
    const sentAt = request.body["sent_at"];
    return request.path === "/v1/snapshots" && typeof sentAt === "number" && now - sentAt > SPOOL_LIMITS.snapshotAgeMs;
  }

  /** Two snapshots of one source covering the same kinds: the newer says all the older did. */
  sameSnapshot(a: Record<string, unknown>, b: Record<string, unknown>): boolean {
    const kinds = (v: unknown) => (Array.isArray(v) ? JSON.stringify([...(v as string[])].sort()) : v === undefined ? "" : "?");
    return a["source"] === b["source"] && kinds(a["kinds"]) === kinds(b["kinds"]);
  }

  /** Writes the request to the spool and answers `queued`; a spool that can't be written leaves `sent` as it was. */
  async queue(spool: string, path: string, key: string, body: Record<string, unknown>, sent: Sent): Promise<Sent> {
    const now = this.now();
    const data = JSON.stringify({ path, key, queued_at: now, body });
    const why = sent.problem ?? `Beacon answered ${sent.status}`;
    let dropped = 0;
    try {
      const fs = await this.fs();
      await fs.mkdir(spool, { recursive: true });
      let names = await this.spooled(fs, spool);
      // Removed only once the new file is written, so a failed write loses nothing.
      const replaced: string[] = [];
      if (path === "/v1/snapshots") {
        for (const name of names) {
          const older = await this.readSpooled(fs, spool, name);
          if (older?.path === path && this.sameSnapshot(older.body, body)) replaced.push(name);
        }
        names = names.filter((name) => !replaced.includes(name));
      }
      const sizes: number[] = [];
      for (const name of names) sizes.push(await fs.stat(`${spool}/${name}`).then((s) => s.size, () => 0));
      let total = sizes.reduce((a, b) => a + b, 0);
      const evicted: string[] = [];
      while (names.length > 0 && (names.length >= SPOOL_LIMITS.files || total + data.length > SPOOL_LIMITS.bytes)) {
        evicted.push(names.shift() ?? "");
        total -= sizes.shift() ?? 0;
      }
      await this.publish(fs, spool, data, now);
      for (const old of [...replaced, ...evicted]) await this.drop(fs, spool, old);
      dropped = evicted.length;
      this.droppedTotal += dropped;
    } catch (error) {
      const reason = error instanceof Error ? error.message : String(error);
      return { ...sent, ok: false, queued: false, problem: `${why}\n  and the spool ${spool} can't be written: ${reason}` };
    }
    const full = dropped > 0 ? `\n  the spool was full: dropped the ${dropped} oldest` : "";
    return { ok: true, status: sent.status, outcome: null, item: null, problem: why + full, queued: true };
  }

  /** Sends what the spool holds, oldest first, once each; stops at the first that still can't go. */
  flush(): Promise<Flushed> {
    if (this.flushing === undefined) {
      this.flushing = this.flushOnce().finally(() => {
        this.flushing = undefined;
      });
    }
    return this.flushing;
  }

  async flushOnce(): Promise<Flushed> {
    const spool = this.spoolDir();
    let sent = 0;
    let dropped = 0;
    const done = (left: number, problem: string | null): Flushed => {
      this.droppedTotal += dropped;
      return { sent, left, dropped, droppedTotal: this.droppedTotal, problem };
    };
    if (spool === undefined || this.disabled()) return done(0, null);
    const fs = await this.fs();
    // Two snapshots of one source and kinds can both be here for a moment (a newer one is written
    // before the older one it replaces is removed): sending the older, then the newer, is right.
    const names = await this.spooled(fs, spool);
    for (const [index, name] of names.entries()) {
      const request = await this.readSpooled(fs, spool, name);
      if (request === undefined || this.expired(request, this.now())) {
        if (request !== undefined || (await this.exists(fs, `${spool}/${name}`))) dropped += 1;
        await this.drop(fs, spool, name);
        continue;
      }
      const answer = await this.attempt(request.path, JSON.stringify(request.body), request.key);
      if (this.retryable(answer.status)) return done(names.length - index, answer.problem);
      await this.drop(fs, spool, name);
      // A refusal (4xx) will never be taken: it doesn't hold up what is behind it.
      if (answer.ok) sent += 1;
      else dropped += 1;
    }
    return done(0, null);
  }

  idempotencyKey(): string {
    const bytes = new Uint8Array(16);
    const random = (globalThis as { crypto?: { getRandomValues?: (array: Uint8Array) => Uint8Array } }).crypto;
    if (random?.getRandomValues !== undefined) random.getRandomValues(bytes);
    else for (let i = 0; i < bytes.length; i++) bytes[i] = Math.floor(Math.random() * 256);
    return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  }

  async attempt(path: string, data: string, key: string): Promise<Sent> {
    const url = this.url();
    const headers: Record<string, string> = { "Content-Type": "application/json", "Idempotency-Key": key };
    const token = this.token();
    if (token !== undefined) headers["Authorization"] = `Bearer ${token}`;
    let status: number;
    let text: string;
    try {
      const response = await fetch(url + path, {
        method: "POST",
        headers,
        body: data,
        signal: AbortSignal.timeout(this.timeout),
      });
      status = response.status;
      text = await response.text();
    } catch (error) {
      const why = error instanceof Error ? error.message : String(error);
      const problem = `${url}: no answer from Beacon: ${why}\n  fix: check Beacon is running and BEACON_URL points at it`;
      return { ok: false, status: 0, outcome: null, item: null, problem, queued: false };
    }
    return this.answer(status, text);
  }

  object(value: unknown): Record<string, unknown> {
    return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
  }

  answer(status: number, text: string): Sent {
    let parsed: unknown = null;
    try {
      parsed = JSON.parse(text);
    } catch {
      parsed = null;
    }
    const answer = this.object(parsed);
    if (status >= 200 && status < 300) {
      const outcome = answer["outcome"];
      const item = this.object(answer["item"])["id"];
      return {
        ok: true,
        status,
        outcome: typeof outcome === "string" ? outcome : null,
        item: typeof item === "number" ? item : null,
        problem: null,
        queued: false,
      };
    }
    const error = this.object(answer["error"]);
    let problem = `Beacon answered ${status}: ${text.slice(0, 300)}`;
    if (Object.keys(error).length > 0) {
      const [at, message, fix] = [error["at"], error["message"], error["fix"]].map((v) => (typeof v === "string" ? v : ""));
      problem = at ? `${at}: ${message}` : `${message}`;
      if (fix) problem += `\n  fix: ${fix}`;
    }
    return { ok: false, status, outcome: null, item: null, problem, queued: false };
  }
}

const beacon = new BeaconRuntime();

/** Sends what the spool holds (`spoolDir` or `BEACON_SPOOL_DIR`), oldest first. Call it on a timer. */
export function flush(): Promise<Flushed> {
  return beacon.flush();
}
// --- runtime end ---

/** The runtime, for tests that shorten the retry delays. */
export const runtime = beacon;

/** One `POST /v1/events` for any kind. Prefer the typed functions `beacon gen` writes. */
export function send(kind: string, options: EventOptions & { resolve?: boolean } = {}): Promise<Sent> {
  return beacon.event(kind, { ...options, state: options.resolve === true ? "resolved" : undefined });
}

/** One firing entry for `snapshot`. */
export function entry(kind: string, options: EventOptions = {}): Entry {
  return beacon.entry(kind, options);
}

/** One `POST /v1/snapshots`: everything of `source` that is open now. */
export function snapshot(
  source: string,
  firing: readonly Entry[],
  options: { kinds?: readonly string[]; stale_after?: string } = {},
): Promise<Sent> {
  return beacon.snapshot(source, firing, options);
}

/** Says `source` couldn't see what it watches; its alerts are muted until its next snapshot. */
export function unobserved(source: string, reason: string, options: { stale_after?: string } = {}): Promise<Sent> {
  return beacon.snapshot(source, [], { ...options, reason });
}
