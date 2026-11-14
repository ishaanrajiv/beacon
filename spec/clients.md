# Clients: generated emitters and the `beacon` command

A producer sends through one of two things, both built from its `kinds.toml`:

1. **A generated file** (`beacon gen`): one typed function per declared kind, in Python or
   TypeScript. Application code calls only these, so an undeclared kind, a missing fact or slot, or
   a fact of the wrong type fails the service's own type check (pyright, tsc) before it runs.
2. **The `beacon` command**, for shell scripts: it reads the kinds from the server (or a local
   folder) and refuses an undeclared kind or a missing value with the fix in its message.

The generated files are self-contained: standard library only (Python 3.10+; TypeScript for
Node 18+ with the global `fetch`, or a browser), with no client package to install. Each one is a
client followed by the kinds' functions: `beacon gen` copies everything between the
`--- runtime start ---` and `--- runtime end ---` markers of `clients/python/beacon_client.py` or
`clients/node/beacon_client.ts` into the file. Those two files are the clients: tested on their
own against a fake Beacon, and usable alone (`send(kind, …)`, `entry`, `snapshot`, `unobserved`
take any kind by name, and `send` and `entry` any `category`, unchecked until Beacon answers). The generator is `crates/codegen`;
`examples/homelab-ish/` holds the declarations its tests generate from, and
`clients/*/tests/gen/` the files generated from them.

## What every emitter guarantees

| Behaviour | Rule |
|---|---|
| Typed | One function per kind. Facts and slots are required keyword arguments (`?` facts optional), typed as below. A kind that isn't declared has no function. |
| One request | Each call is one `POST /v1/events` (or `/v1/snapshots`), sent before the call returns (Python) or resolves (TypeScript). Nothing is queued in memory; on disk only with a spool. A caller that must not wait runs the call off its main path. |
| Spool | Off unless given a folder (`spool_dir` / `spoolDir` in `configure`, or `BEACON_SPOOL_DIR`). Then a call Beacon couldn't take (no answer, `408`, `429`, `5xx`, after the retries) is kept on disk and sent later, before the next call and by `flush()`; see "The spool". |
| Idempotent | Each call makes a random Idempotency-Key (32 hex characters) and sends it with every retry of that call, so a retry after a lost answer is recorded once. |
| Retries | No answer, `408`, `429` or `5xx`: tried again after 1 s, then 3 s; three attempts at most. Each attempt times out after 5 s. Any other answer is final. |
| Never throws | A failure returns a result with `ok` false and a `problem` string; it never raises into application code, unless strict mode is on. |
| Strict mode | For tests: `BEACON_STRICT=1`, or `strict` in `configure`. A result that isn't `ok` raises `BeaconError` (Python) or rejects (TypeScript). |
| Off switch | `BEACON_DISABLED=1`: every call returns `ok` at once and sends nothing (and spools nothing). |
| Where | `BEACON_URL` (default `http://127.0.0.1:8210`); `BEACON_TOKEN`, when set, is sent as `Authorization: Bearer` (the server's `auth.producer_token`). `configure()` overrides each. |
| Up to date | The file's first three lines name the source and the generator's version, say not to edit it, and give a hash of the declarations' JSON form (`declarations: fnv1a64 <16 hex digits>`; FNV-1a over the compact JSON `beacon publish` sends, so comments and layout don't change it). `beacon gen --check` fails when the file differs from what the folder would generate now, and says whether the declarations changed. |

The result (`Sent`):

| Field | |
|---|---|
| `ok` | true for a `2xx` |
| `status` | the HTTP status, or 0 when Beacon never answered (or when disabled) |
| `outcome` | the event's `outcome` (`opened`, `updated`, `resolved`, `recorded`, `ignored`…), or null |
| `item` | the item's id, or null |
| `problem` | the first problem's text (`at: message\n  fix: …`) when not `ok`; why it waits when `queued`; else null |
| `queued` | true when the call was kept in the spool to send later; `ok` is then true and `status` is Beacon's last answer, 0 for none |

Fact types in the generated signatures:

| Fact type | Python | TypeScript | Sent as |
|---|---|---|---|
| `string` | `str` | `string` | as is |
| `int` | `int` | `number` (an integer) | as is |
| `float` | `float` | `number` | as is |
| `duration` | `int \| timedelta` | `number` (milliseconds) | whole milliseconds |
| `time` | `int \| datetime` (an aware datetime) | `number \| Date` | milliseconds since 1970 UTC |
| slot | `str` | `string` | as is (Beacon cuts it to fit) |

Names: a kind's function is its name with `.` and `-` turned into `_` in Python
(`backup_failed`), and camelCase in TypeScript (`backupFailed`, `assistantTaskResult`). Fact and
slot argument names are exactly the declared names, in both (declarations can't use `key`,
`labels`, `url`, `context` or `category`, the emitters' own arguments). `beacon gen`, `check` and `publish`
refuse a folder where two kinds would get the same function name, counting the `_resolved` and
`_entry` companions (`ship.down_resolved` beside an alert `ship.down`), and a fact or slot named
after a Python keyword (`from`, `class`), which can't be an argument.

### Python

```python
# Generated by `beacon gen` 0.1.0 from the declarations of `backup`. Do not edit.
# Change kinds.toml and run `beacon gen` again; `beacon gen --check` fails while this file is stale.
# declarations: fnv1a64 6867d21600e772ba
from __future__ import annotations

# (the runtime of clients/python/beacon_client.py: imports, then)
DEFAULT_URL = "http://127.0.0.1:8210"

class BeaconError(Exception): ...

@dataclass(frozen=True)
class Sent:
    ok: bool
    status: int
    outcome: str | None
    item: int | None
    problem: str | None
    queued: bool = False

@dataclass(frozen=True)
class Entry:
    """One firing entry of a snapshot. Make it with a kind's `_entry` function."""
    kind: str
    body: Mapping[str, object]

def configure(*, url: str | None = None, token: str | None = None, strict: bool | None = None, disabled: bool | None = None, spool_dir: str | None = None) -> None: ...

@dataclass(frozen=True)
class Flushed:
    sent: int           # delivered now
    left: int           # still waiting
    dropped: int        # dropped by this flush
    dropped_total: int  # dropped by this process since it started
    problem: str | None # why the first still waiting couldn't go

def flush() -> Flushed: ...

# (end of the runtime)
SOURCE = "backup"

def backup_failed(
    *,
    last_ok: int | datetime,
    reason: str,
    key: str = "",
    labels: Mapping[str, str] | None = None,
    url: str | None = None,
) -> Sent:
    """The last backup run failed."""

def backup_failed_resolved(*, key: str = "") -> Sent: ...
def backup_failed_entry(*, last_ok: int | datetime, reason: str, key: str = "", labels: Mapping[str, str] | None = None, url: str | None = None) -> Entry: ...

def backup_finished(*, size: str, took: int | timedelta, key: str = "", labels: Mapping[str, str] | None = None) -> Sent: ...

SnapshotKind = Literal["backup.failed", "backup.space", "backup.stale"]

def snapshot(
    firing: Sequence[Entry],
    *,
    kinds: Sequence[SnapshotKind] | None = None,
    stale_after: str | None = None,
) -> Sent: ...

def unobserved(reason: str, *, stale_after: str | None = None) -> Sent: ...
```

- Every kind gets its function. Alert and action kinds also get `<name>_resolved` (state
  `resolved`, by key, with no facts or slots) and `<name>_entry` (a snapshot entry, taking the
  same arguments as the kind's function).
- A kind with `context = true` takes a required `context: str`, in its function, its `_entry` and
  its `_resolved` (`POST /v1/events` requires it whatever the state).
- `url` overrides the kind's link; it is left out of a kind's signature when the kind has no `open`.
- A kind that declares `categories` (`spec/declarations.md`) takes an optional `category`, typed as
  the declared names, in its function and its `_entry` (not its `_resolved`). It is sent as the
  event's or entry's top-level `category` only when given; left out, Beacon uses the first
  declared. A kind without `categories` has no `category` argument, so naming one is a type error:

  ```python
  # categories = ["PERMISSION", "PERMISSION_DESTRUCTIVE"]
  def assistant_permission(
      *,
      thread: str,
      command: str,
      tool: str,
      context: str,
      key: str = "",
      labels: Mapping[str, str] | None = None,
      url: str | None = None,
      category: Literal["PERMISSION", "PERMISSION_DESTRUCTIVE"] | None = None,
  ) -> Sent: ...
  ```
- An optional (`?`) fact is an argument defaulting to `None`, and isn't sent when left out.
- `snapshot` posts `POST /v1/snapshots` for `SOURCE` with `observed: true` and `sent_at` (the
  moment of the call, the same on every retry); `unobserved` posts one with `observed: false` and
  the reason. `kinds` only takes the source's alert and action kinds (`SnapshotKind`; plain `str`
  for a source with none).
- A naive `datetime` is refused (`TypeError`): times must say their zone.
- `configure` and the settings belong to the file: two generated files are configured apart.

### TypeScript

```ts
// Generated by `beacon gen` 0.1.0 from the declarations of `backup`. Do not edit.
// Change kinds.toml and run `beacon gen` again; `beacon gen --check` fails while this file is stale.
// declarations: fnv1a64 6867d21600e772ba

// (the runtime of clients/node/beacon_client.ts:)
export const DEFAULT_URL = "http://127.0.0.1:8210";
export interface Sent { ok: boolean; status: number; outcome: string | null; item: number | null; problem: string | null; queued: boolean }
export interface Flushed { sent: number; left: number; dropped: number; droppedTotal: number; problem: string | null }
export interface Entry { readonly kind: string; readonly body: Readonly<Record<string, unknown>> }
export class BeaconError extends Error {}
export function configure(options: { url?: string; token?: string; strict?: boolean; disabled?: boolean; spoolDir?: string }): void;
export function flush(): Promise<Flushed>;
// (end of the runtime)

export const SOURCE = "backup";
export interface Common { key?: string; labels?: Record<string, string> }

/** The last backup run failed. */
export function backupFailed(args: { last_ok: number | Date; reason: string; url?: string } & Common): Promise<Sent>;
export function backupFailedResolved(args: { key?: string } = {}): Promise<Sent>;
export function backupFailedEntry(args: { last_ok: number | Date; reason: string; url?: string } & Common): Entry;
export function backupFinished(args: { size: string; took: number } & Common): Promise<Sent>;

// A kind with categories = ["PERMISSION", "PERMISSION_DESTRUCTIVE"] (from `assistant`):
export function assistantPermission(args: { thread: string; command: string; tool: string; context: string; url?: string; category?: "PERMISSION" | "PERMISSION_DESTRUCTIVE" } & Common): Promise<Sent>;

export type SnapshotKind = "backup.failed" | "backup.space" | "backup.stale";
export function snapshot(firing: readonly Entry[], options: { kinds?: readonly SnapshotKind[]; stale_after?: string } = {}): Promise<Sent>;
export function unobserved(reason: string, options: { stale_after?: string } = {}): Promise<Sent>;
```

It type-checks under `tsc --strict --noEmit` with `noUnusedLocals`, `noUnusedParameters`,
`noUncheckedIndexedAccess`, `exactOptionalPropertyTypes` and `noPropertyAccessFromIndexSignature`
on (it needs `lib: ["dom"]` or `@types/node` for `fetch`), reads `process.env` when there is one,
and uses `AbortSignal.timeout` for the 5 s limit. The spool needs Node: it loads `node:fs/promises`
only when a spool folder is set, so the file still type-checks and runs in a browser without one. An optional fact is an optional property; a kind
whose function would take only `Common` can be called with no argument. The argument object is an
object literal at the call, so an undeclared fact or slot is an excess-property error, and so is a
`category` on a kind that declares no categories.

## The spool

A notification sent while Beacon restarts (every deploy of it) is otherwise lost. With a spool
folder, the emitters and the `beacon` command keep it instead:

- **What is kept.** A call that gets no answer (it can't connect, or times out), or `408`, `429` or
  `5xx`, after its retries, is written to the folder and answered `queued` (`ok` true), not failed.
  Any other answer is final: a `4xx` is never kept, since sending it again would be refused again.
- **When it is sent.** Before every call, the folder is flushed first; and by `flush()` (or `beacon
  flush`), which a service can call on a timer. A flush sends the files oldest first, one attempt
  each, and stops at the first that still can't go, so order is kept. While anything still waits,
  a new call isn't tried: it is queued behind, in order. A file answered `2xx` is removed; a file
  answered `4xx` is removed and counted as dropped, and the flush goes on.
- **Recorded once.** Each request carries the Idempotency-Key made when it was first sent, and
  keeps it in its file, so a request that did arrive but whose answer was lost is recorded once
  (`spec/delivery.md`, "Idempotency").
- **Snapshots.** A snapshot keeps its `sent_at`. A new snapshot replaces any waiting one of the
  same source covering the same kinds (the same `kinds`, or both none): it says all the older one
  did. The new one is written first and the older removed after, so a write that fails loses
  neither; for that moment both wait, and a flush sends the older, then the newer, which is right.
  One of other kinds stays, in order. A waiting snapshot is dropped, not sent, once its `sent_at`
  is more than 5 minutes old (Beacon would refuse it, `spec/api.md`).
- **Bounded.** A file waits at most 24 hours (Beacon's idempotency window), then is dropped. The
  folder holds at most 1,000 files and 8 MB: queuing one more drops the oldest first, once the new
  one is written. Every drop is counted: `Flushed.dropped` (this flush) and `dropped_total` /
  `droppedTotal` (this process), the `queued` answer's `problem` when it had to drop the oldest,
  and `beacon flush`'s `dropped`.

The files are the same for every client, so the Python and TypeScript emitters and the command can
share one folder, and any of them flushes the others'. One file per request, named
`<ms since 1970, 15 digits>-<sequence, 6 digits>.json`. Files are listed, and sent, in the order of
those two numbers, compared as numbers. No lock file is needed to keep that order across processes:

- **Naming.** A writer first lists the folder and finds the greatest `(ms, sequence)` there. The new
  name's `ms` is the later of its clock and that one's; its sequence is one more than that one's
  when the `ms` is the same, else 0; past 999999 it is the next `ms` with sequence 0. So a new file
  never sorts before one already there, whatever process wrote it, and a clock that steps back
  doesn't reorder. `queued_at` in the file is the writer's clock, which ages it; the name only
  orders it.
- **Publishing.** The request is written to a temporary name starting with `.`, then hard-linked
  to its name, which fails if that name exists, and the temporary name removed. So two processes
  can't take one name, and a reader never sees half a file. A writer whose name was taken first
  lists again and tries the next, up to 20 times, then answers that the spool can't be written.
  The folder must be on a file system with hard links.
- **Older names.** Files named `<ms>-<sequence>-<hex>.json` by earlier versions are still read,
  ordered by their `(ms, sequence)` among the rest, and sent.

A file holds:

```json
{"path": "/v1/events", "key": "<32 hex>", "queued_at": 1794571200000, "body": {"kind": "backup.failed", "key": "nightly", "facts": {}}}
```

Names that don't end `.json`, or start with `.`, are not requests: the command keeps the
declarations it last read from Beacon there (`.kinds-<source>.json`), so `send` and `snapshot` can
still check their values and queue while Beacon is away. A file that doesn't read as a request is
dropped and counted. Two processes flushing one folder at once may both send a file; its
Idempotency-Key (an event) or `sent_at` (a snapshot) keeps that harmless. Use one folder per Beacon.

## The `beacon` command

```sh
beacon serve [--config beacon.toml]
beacon check <folder>...
beacon publish <folder>... [--keep] [--server URL]
beacon gen <folder> --lang python|typescript [--out FILE] [--check]
beacon send <kind> [name=value]... [--key K] [--resolve] [--label name=value]... [--context C] [--open LINK] [--decls FOLDER] [--server URL] [--json]
beacon resolve <kind> [--key K] [--context C] [--decls FOLDER] [--server URL] [--json]
beacon snapshot <source> [--fire '<kind> [key=K] [context=C] [url=LINK] name=value...']... [--kinds K,K] [--stale-after D] [--unobserved REASON] [--label name=value]... [--decls FOLDER] [--server URL] [--json]
beacon flush [--spool DIR] [--server URL] [--json]
beacon silence add [--source S] [--kind K [--key K]] [--label name=value]... (--for D | --until TIME) --reason R [--server URL]
beacon silence extend <id> (--for D | --until TIME) [--server URL]
beacon silence end <id> [--server URL]
beacon silence list [--all] [--server URL] [--json]
```

`--server` defaults to `BEACON_URL`, else `http://127.0.0.1:8210`; `BEACON_TOKEN` is sent as for
the emitters.

| Command | Does |
|---|---|
| `serve` | Runs the server (`spec/config.md`). |
| `check` | Reads and checks each folder's `kinds.toml` (a folder or the file itself), prints `ok  backup: 4 kinds` or every problem. Several folders are checked together: a source may appear once. |
| `publish` | Checks, then `PUT /v1/registry/{source}` for each (`spec/declarations.md`, "Reaching the server"). `--keep` sends `?keep=true`: kinds already published that a folder lacks stay. Prints what it kept, and how many open items of removed kinds a plain publish closed. |
| `gen` | Writes the emitter file to `--out`, or prints it without one. `--check` (needs `--out`) writes nothing and fails if the file differs. |
| `send` | One `POST /v1/events`. |
| `resolve` | `send --resolve`: an event with state `resolved` and no values. |
| `flush` | Sends what the spool (`--spool`, else `BEACON_SPOOL_DIR`) holds, as "The spool" says, and prints `sent N, waiting N, dropped N` (`--json`: `{"sent", "left", "dropped", "problem"}`). Exit `3` while any still wait. |
| `snapshot` | One `POST /v1/snapshots`, with `sent_at` now. Each `--fire` is one entry, written like a `send` in one quoted word (`'…'` or `"…"` inside keep spaces; `key=`, `context=` and `url=` set the entry's own fields); none means nothing is firing. `--label` applies to every entry. `--unobserved REASON` posts `observed: false` and needs no declarations. |
| `silence add` | `POST /v1/silences`; prints the silence's id. |
| `silence extend` | `PATCH /v1/silences/{id}`. |
| `silence end` | `DELETE /v1/silences/{id}`. |
| `silence list` | Active silences, one per line: id, until, match, reason. |

`send` and `snapshot` know a kind's facts and slots from `GET /v1/registry/{source}`, or from a
local folder with `--decls`. Before sending they refuse, with the fix in the message:

- an undeclared kind (naming the closest one, and saying to declare it and run `beacon publish`);
- a name that isn't a fact or slot of the kind (naming the closest one);
- a missing required fact or any missing slot (`reason=""` sends an empty slot);
- a value that doesn't read as its type;
- a missing `--context` on a kind with `context = true`, or one given for a kind without;
- `--resolve` (or `resolve`) on an update or record, and a `--fire` or `--kinds` naming one.

`resolve` sends no facts or slots; `send --resolve` with values checks them all.

Values are written `name=value`. A string fact or slot takes the rest as text. `int` and `float`
take a number. `duration` takes `90s`, `5m`, `2h`, `7d`, or whole milliseconds. `time` takes
whole milliseconds since 1970, `now`, a time ago or ahead (`-15m`, `+2h`), or an RFC 3339 time with
its offset (`2026-11-13T02:00:00+00:00`).

```sh
beacon send backup.failed reason="The disk is full" last_ok=-1d --key nightly --label service=backup
beacon resolve backup.failed --key nightly
beacon snapshot backup --fire 'backup.space used_percent=96' --stale-after 30m
beacon snapshot backup --unobserved "The bucket's status page didn't load."
beacon silence add --label service=dashboard --for 30m --reason "Deploying dashboard"
```

`send`, `resolve` and `snapshot` take `--spool DIR` (else `BEACON_SPOOL_DIR`): what Beacon can't
take is queued there, as "The spool" says, and the command prints `queued  <kind>  in <dir>: <why>`
(`--json`: `{"queued": true, "spool", "why", "dropped"}`) and exits `0`. While Beacon doesn't
answer, the kinds come from the copy the spool keeps of what Beacon last gave; with none yet, the
command fails as it would without a spool.

Exit status: `0` done (or queued); `1` refused, by a local check or by Beacon (every problem printed to stderr
as `at: message` and `  fix: …`); `2` the command line is wrong; `3` Beacon didn't answer, or
answered `5xx`, after the retries above. `--json` prints Beacon's answer to stdout. The command
retries like the emitters, with one Idempotency-Key per invocation.

## What the apps do

The web and iPhone clients are not generated: they read `spec/api.md`. In short:

- Poll `GET /v1/inbox` with `If-None-Match` while active (15 s), and on a push or after an action.
- Post presence (`POST /v1/presence`) every 30 s while active, and `active: false` on leaving.
  Web is active while the tab is visible and focused and the person has moved the pointer, pressed
  a key or touched the page within the last 5 minutes; when that lapses it posts `active: false`
  (`spec/api.md`, `POST /v1/presence`).
- Mark seen (`POST /v1/seen`) as `spec/delivery.md`, "Seen and done", says.
- Show one unread dot per row (`seen` false), and the dot while `dot` is true; no numbers, no app
  icon badge.
- Show `held` items as an in-app banner, once each, and mark them seen when shown, with their
  own `client`. A web client's seen doesn't stop an urgent push (`spec/delivery.md`, "Seen and
  done"), so the phone still rings for one shown only in a browser.
- Draw each source with its `icon` and `source_name`, and every text as plain text.
- Web: Needs you, Alerts (open, then resolved), Activity (by day, then by source; records behind a
  filter); Today's "Since you were last here" from `since_last_visit`.
- iPhone: register the device (`POST /v1/devices`) and the notification categories (from
  `GET /v1/kinds`, `spec/apns.md`) at launch. On opening, after the first inbox, remove each
  delivered notification of an alert or action kind whose `data.item` is in neither `needs_you` nor
  `alerts.open`: it is done, dismissed, snoozed or resolved. Notifications of updates stay. While in
  the foreground, show nothing for a push whose `context` is on screen, and only a list entry for
  the rest.
