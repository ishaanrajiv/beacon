// Beacon's TypeScript client: no dependencies, Node 18+ (the global `fetch`) or a browser
// (spec/clients.md).
//
// `beacon gen --lang typescript` copies everything between the two `runtime` markers below into
// each generated file, so a service needs no package: the generated file is this client plus one
// typed function per declared kind. Used on its own, `send`, `entry` and `snapshot` at the end of
// this file take any kind by name, unchecked until Beacon answers.

// --- runtime start ---
export const DEFAULT_URL = "http://127.0.0.1:8210";

/** What Beacon answered. `ok` is true for a 2xx; otherwise `problem` says what went wrong. */
export interface Sent {
  ok: boolean;
  status: number;
  outcome: string | null;
  item: number | null;
  problem: string | null;
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
}

const settings: Settings = {};

/** Overrides `BEACON_URL`, `BEACON_TOKEN`, `BEACON_STRICT` and `BEACON_DISABLED`. Left out keeps the current choice. */
export function configure(options: { url?: string; token?: string; strict?: boolean; disabled?: boolean }): void {
  if (options.url !== undefined) settings.url = options.url;
  if (options.token !== undefined) settings.token = options.token;
  if (options.strict !== undefined) settings.strict = options.strict;
  if (options.disabled !== undefined) settings.disabled = options.disabled;
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
    if (this.disabled()) return { ok: true, status: 0, outcome: null, item: null, problem: null };
    const sent = await this.attemptAll(path, JSON.stringify(body));
    if (!sent.ok && this.strict()) throw new BeaconError(sent.problem ?? `Beacon answered ${sent.status}`);
    return sent;
  }

  async attemptAll(path: string, data: string): Promise<Sent> {
    const key = this.idempotencyKey();
    const delays = [...this.retryDelays];
    for (;;) {
      const sent = await this.attempt(path, data, key);
      const retry = sent.status === 0 || sent.status === 408 || sent.status === 429 || sent.status >= 500;
      const delay = delays.shift();
      if (!retry || delay === undefined) return sent;
      await new Promise((done) => setTimeout(done, delay));
    }
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
      return { ok: false, status: 0, outcome: null, item: null, problem };
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
      };
    }
    const error = this.object(answer["error"]);
    let problem = `Beacon answered ${status}: ${text.slice(0, 300)}`;
    if (Object.keys(error).length > 0) {
      const [at, message, fix] = [error["at"], error["message"], error["fix"]].map((v) => (typeof v === "string" ? v : ""));
      problem = at ? `${at}: ${message}` : `${message}`;
      if (fix) problem += `\n  fix: ${fix}`;
    }
    return { ok: false, status, outcome: null, item: null, problem };
  }
}

const beacon = new BeaconRuntime();
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
