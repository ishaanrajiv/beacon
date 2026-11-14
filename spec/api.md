# The HTTP API

Everything is JSON over plain HTTP/1.1. Beacon listens on loopback (`spec/config.md`, `listen`);
clients reach it through a reverse proxy on a private network. `GET /v1` returns the route list
below as data, and a test in `crates/server` will fail if a route or parameter is missing from
this file.

## Conventions

- **Times** are integer milliseconds since 1970 UTC (`1794571200000`), except in
  `POST /v1/live-activity`, which keeps the unix seconds of the service it replaces.
- **Durations** in request bodies are strings: a whole number and a unit, `90s`, `10m`, `2h`, `7d`.
- **Ids** of items and silences are positive integers, assigned by Beacon, increasing.
- **JSON bodies** must be objects, at most `limits.body_bytes` (1 MB). Unknown fields in a request
  body are an error, with the name you probably meant, except in `POST /v1/trail` and
  `POST /v1/live-activity`, which keep the contracts of other programs and ignore fields they don't
  read.
- **Lists** are newest first unless a route says otherwise.
- **`version`** is one counter for the whole server. It goes up whenever anything a client shows
  changes: an item created or changed (state, text, seen, push status), a silence opened, changed or
  ended, a source observed or unobserved, a visit starting. Each item carries the `version` of its
  last change.

### Errors

Every error answer has the same body. `error` is the first problem; `problems` lists every one
when there are several (a declarations file, an event with three bad facts).

```json
{
  "error": {
    "at": "event kind",
    "message": "`backup.faild` is not a declared kind",
    "fix": "did you mean `backup.failed`? Declare a new kind in kinds.toml and run `beacon publish`"
  },
  "problems": [{"at": "event kind", "message": "…", "fix": "…"}]
}
```

| Status | Means |
|---|---|
| `200` | Done. |
| `201` | Created (a silence). |
| `304` | Not modified (`GET /v1/inbox`, `GET /v1/kinds` with `If-None-Match`). No body. |
| `400` | The body or a parameter can't be read: not JSON, a wrong type, a field out of range, an unknown field. |
| `403` | The request broke an access rule (below). |
| `404` | No such route, item, silence, source or device. |
| `405` | The route exists, with another method. The `Allow` header lists the methods. |
| `409` | A conflict: an Idempotency-Key reused with another body, a snapshot older than one already taken, an action that doesn't apply to the item's state, a reserved source. |
| `410` | `POST /v1/live-activity` only: Apple says the activity's token is dead. |
| `413` | The body is over `limits.body_bytes`: by its Content-Length, or for a chunked body as soon as the chunks so far and the next chunk's size pass it. |
| `422` | Readable, but not declared: an undeclared kind, or facts and slots that don't fit their kind. |
| `502` | `POST /v1/live-activity` only: Apple didn't take the update. |
| `503` | Storage failed (retry), or for `POST /v1/live-activity`, no APNs key is configured. |

### Access

Beacon has one user. Its rules keep other web pages and other machines from acting for them.

| Access | Who may call | Routes |
|---|---|---|
| `read` | anyone who can reach Beacon | every `GET` |
| `client` | a same-origin JSON request, or a `local` one | presence, item actions, seen, silences, devices |
| `producer` | a `local` request, or one with `Authorization: Bearer <auth.producer_token>` when that is set | registry writes, events, snapshots |
| `local` | a request made straight to Beacon from the same machine | `POST /v1/trail`, `POST /v1/live-activity` |

- **Same-origin JSON**: `Content-Type: application/json`; `Sec-Fetch-Site` absent or not
  `cross-site`; and `Origin` absent, equal to `http://<Host>` or `https://<Host>` (the request's
  own `Host` header), or listed in `origins` (`spec/config.md`). The iPhone app sends no `Origin`
  and passes.
- **Local**: the peer address is loopback, the request has no `X-Forwarded-For` and no `Forwarded`
  header (so it didn't come through the proxy), and `Content-Type: application/json`.
- A `DELETE` has no body, so it needs no `Content-Type`; its other rules still hold. (A browser
  can't send a cross-origin `DELETE` without asking first, and Beacon never says yes.)
- A `producer` token is compared whole; with `auth.producer_token` unset, only `local` requests
  may produce.
- A request that fails its rule gets `403` with a `fix` naming the rule.

## Routes

`GET /v1` returns them as `{"version": "0.1.0", "routes": [{"method", "path", "summary", "access", "params": [{"name", "in", "summary"}]}]}`,
where `in` is `path`, `query` or `header`. `GET /` answers the same, so a reverse proxy's check of
the bare prefix (`/beacon/`) gets a `200` rather than a `404`.

| Method | Path | Access | Summary |
|---|---|---|---|
| `GET` | `/` | read | The same as GET /v1, so a proxy's check of the bare prefix gets an answer. |
| `GET` | `/v1` | read | This list. |
| `GET` | `/v1/health` | read | Whether Beacon is up, and whether it can push. |
| `GET` | `/v1/registry` | read | Every source: name, icon, kind count, whether it is observed. |
| `GET` | `/v1/registry/{source}` | read | One source's declarations. |
| `PUT` | `/v1/registry/{source}` | producer | Publish a source's declarations. |
| `DELETE` | `/v1/registry/{source}` | producer | Remove a source's declarations. |
| `GET` | `/v1/kinds` | read | Every kind, flat, with its source's name and icon: what the apps register notification categories from. |
| `POST` | `/v1/events` | producer | One notification: open, update or resolve an alert or action, or record an update or record. |
| `POST` | `/v1/snapshots` | producer | A source's complete set of open alerts and actions; everything absent resolves. |
| `POST` | `/v1/trail` | local | Trail's alert webhook: its snapshot and notice bodies. |
| `POST` | `/v1/presence` | client | A client's heartbeat: active or not, and what it is viewing. |
| `GET` | `/v1/inbox` | read | Needs you, alerts, activity, held items, the dot, and since your last visit. |
| `GET` | `/v1/items/{id}` | read | One item, with its facts, slots, history and push attempts. |
| `POST` | `/v1/items/{id}/actions` | client | Press a button: done, acknowledge, snooze, dismiss, silence_1h or a custom action. |
| `POST` | `/v1/seen` | client | Mark items seen. |
| `GET` | `/v1/silences` | read | Active silences, or all recent ones. |
| `POST` | `/v1/silences` | client | Open a silence. |
| `GET` | `/v1/silences/{id}` | read | One silence. |
| `PATCH` | `/v1/silences/{id}` | client | Move a silence's end, or change its reason. |
| `DELETE` | `/v1/silences/{id}` | client | End a silence now. |
| `GET` | `/v1/devices` | read | Registered iPhones, with their tokens cut short. |
| `POST` | `/v1/devices` | client | Register an APNs device token. |
| `DELETE` | `/v1/devices/{token}` | client | Forget a device. |
| `POST` | `/v1/live-activity` | local | Send one Live Activity update or end straight to Apple. |

## The server

### `GET /v1/health`

```json
{"ok": true, "version": "0.1.0", "started": 1794571200000, "push": true, "apns": "ok", "devices": 1, "unreadable_sources": 0}
```

| Field | |
|---|---|
| `ok` | false, with status `503`, when the database can't be read |
| `push` | `push` in beacon.toml: false means decisions are made and recorded but nothing is sent |
| `apns` | `ok`, or `unconfigured` when there is no signing key |
| `devices` | registered devices |
| `unreadable_sources` | published sources whose stored declarations, or some of whose kinds, Beacon can't read back (`spec/declarations.md`, "Reading back what was stored"); `GET /v1/registry` says which |

## The registry

`spec/declarations.md` has the format and how publishing works.

### `GET /v1/registry`

```json
{
  "sources": [
    {
      "source": "backup",
      "name": "Backups",
      "icon": "archive",
      "kinds": 4,
      "builtin": false,
      "published": 1794571200000,
      "observed": true,
      "reason": null,
      "last_snapshot": 1794571100000,
      "stale_after": "30m",
      "problem": null
    }
  ]
}
```

Sorted by `source`. `beacon` and `trail` are listed with `builtin: true` and `published: null`.
`observed` is false while the source is unobserved (`spec/delivery.md`, "Sources"), and `reason`
says why. `last_snapshot` and `stale_after` are null for a source that has never posted a snapshot.
`problem` is null, or `{at, message, fix}` when Beacon can't read back what was published for the
source, or some of its kinds (`spec/declarations.md`, "Reading back what was stored"): `kinds`
counts only those it can read (0 when it can read none, and then `name` is the source and `icon`
`bell`), and the fix says to publish it again.

### `GET /v1/registry/{source}`

The declarations in their JSON form (`spec/declarations.md`, "The JSON form"). `404` for a source
nobody published.

### `PUT /v1/registry/{source}`

Body: the declarations in their JSON form. `beacon publish` sends this.

- `200` `{"ok": true, "source": "backup", "kinds": 4}`: stored, replacing the source's earlier
  declarations entirely.
- `400` with every problem, when they fail a check (`spec/declarations.md`), including the
  server's `banned_words`.
- `409` when `{source}` is `beacon` or `trail`, or differs from the body's `source`.

### `DELETE /v1/registry/{source}`

`200` `{"ok": true}`, or `404`. The source's items stay; new events for its kinds are refused.

### `GET /v1/kinds`

Every kind Beacon knows, built-in ones included, sorted by name. Sends an `ETag`; with a matching
`If-None-Match`, `304`.

```json
{
  "kinds": [
    {
      "kind": "backup.failed",
      "source": "backup",
      "source_name": "Backups",
      "icon": "archive",
      "class": "alert",
      "push": "urgent",
      "preview": "Backup problem",
      "actions": [{"id": "acknowledge", "label": "Acknowledge", "url": null}, {"id": "silence_1h", "label": "Silence 1h", "url": null}],
      "time_bound": false,
      "context": false,
      "one_off": false,
      "categories": []
    }
  ]
}
```

The iPhone app registers one notification category per kind from this (`spec/apns.md`,
"Categories"). `categories` lists the app's own categories a kind's pushes carry instead (the
first by default); the app registers those itself. A custom action's `url` is its template, as
declared.

## Producers

### `POST /v1/events`

One notification of a declared kind.

```json
{
  "kind": "backup.failed",
  "key": "nightly",
  "facts": {"last_ok": 1794535200000},
  "slots": {"reason": "The disk is full"},
  "labels": {"service": "backup"}
}
```

| Field | Type | Default | Rule |
|---|---|---|---|
| `kind` | string | required | A declared kind. Otherwise `422`, with the closest kind in the fix. |
| `source` | string | the kind's | If sent, must equal the kind's source (`422`). |
| `key` | string | `""` | 0 to 128 printable ASCII characters, no spaces. Which alert or action this is, across events: `(kind, key)` names one. |
| `facts` | object | `{}` | Every required fact of the kind, of its type; no undeclared ones (`422`). |
| `slots` | object | `{}` | Every slot of the kind, each a string; no undeclared ones (`422`). Longer text is cut, not refused. |
| `labels` | object of strings | `{}` | At most 16. Names snake_case (at most 32 characters), values at most 128 characters. Silences match on them. |
| `state` | string | `"firing"` for an alert, `"open"` for an action | Alert: `"firing"` or `"resolved"`. Action: `"open"` or `"resolved"`. Update and record: must be absent (`400`). |
| `context` | string or null | null | Required (1 to 128 characters) when the kind declares `context = true`; must be null or absent otherwise (`400`). Like `assistant:thread:42`. |
| `url` | string or null | the kind's rendered `open` | Overrides the link. Starts with `/` or `https://`, at most 1024 characters. |
| `category` | string or null | the kind's first `categories`, else the kind's name | One of the kind's declared `categories` (`spec/declarations.md`): the `aps.category` of this item's pushes, so the app shows that category's buttons. Any other value is `422`, with the closest declared one in the fix. Kept with the item; a later event for the same alert or action may change it. |

With `"state": "resolved"`, `facts` and `slots` may be left out: the item keeps its latest ones.
When sent, they are checked and replace them.

What happens (`spec/delivery.md` has the state machines):

| Class, state | No open item for (kind, key) | An open item |
|---|---|---|
| alert, firing | opens one, or reopens the last one if it resolved within the flap window | updates its facts, slots, text and labels; no push |
| alert, resolved | nothing (`outcome: "ignored"`) | resolves it |
| action, open | opens one | updates it; no push |
| action, resolved | nothing (`"ignored"`) | resolves it |
| update, record | records a new item every time | — |

"Open" means state `open` or `dismissed` for an alert, `open` or `snoozed` for an action.

**Idempotency-Key** (header, optional): 1 to 128 printable ASCII characters. Beacon keeps each
key, scoped to the kind's source, for 24 hours. A request with a key it has seen and the same body
gets the first answer again, with `"repeat": true`, and changes nothing. The same key with a
different body is `409`. Generated emitters send one with every call (`spec/clients.md`).

`200`:

```json
{"outcome": "opened", "item": { "…": "an Item" }, "repeat": false}
```

`outcome` is `opened`, `reopened`, `updated`, `resolved`, `recorded` or `ignored` (`item` is null
then). The item shows what was decided about its push at once (`push.status` may be `held`).
`updated` is also the answer when the event changes nothing about an open item; then nothing is
written (`spec/delivery.md`, "Alerts").

`422` for an undeclared kind:

```json
{
  "error": {
    "at": "event kind",
    "message": "`backup.faild` is not a declared kind",
    "fix": "did you mean `backup.failed`? Declare a new kind in kinds.toml and run `beacon publish`"
  },
  "problems": [ … ]
}
```

and for values, one problem per fact or slot, `at` like `event facts.last_ok`.

### `POST /v1/snapshots`

A source's complete set of what is open, for the kinds it covers. Whatever was open and isn't in
the list has recovered. A source that checks something on a schedule (a backup, a login, a usage
limit) posts one after each check, so a restart or a missed post is put right at the next one.

```json
{
  "source": "backup",
  "observed": true,
  "stale_after": "30m",
  "sent_at": 1794571200000,
  "firing": [
    {"kind": "backup.space", "key": "", "facts": {"used_percent": 96}, "slots": {}, "labels": {"service": "backup"}}
  ]
}
```

| Field | Type | Default | Rule |
|---|---|---|---|
| `source` | string | required | A published source (`422` otherwise). Not `beacon` or `trail`. |
| `observed` | bool | `true` | false: the source couldn't see what it watches (see below). |
| `reason` | string | required when `observed` is false | 1 to 100 characters, a sentence for a person: "The usage page didn't load." Becomes the `reason` slot of `beacon.source_stopped` (or, for a source with no alert kind, `beacon.source_unobserved`). |
| `stale_after` | duration | `[delivery] snapshot_stale_after` (`15m`) | `1m` to `7d`. With no snapshot for this long, the source becomes unobserved. |
| `sent_at` | integer ms | none | When the source made it. If sent: `409` when older than the newest snapshot already taken from this source, more than 5 minutes old, or more than 1 minute in the future. |
| `kinds` | list of kind names | every `alert` and `action` kind of the source | The kinds this snapshot is complete for. Each must be an alert or action kind of the source (`422`). |
| `firing` | list | `[]` | Entries `{kind, key, facts, slots, labels, context, url, category}`, each as in `POST /v1/events` (no `state`). Every `kind` must be in `kinds`; each `(kind, key)` at most once (`400`). |

With `observed: true`, in one transaction:

1. Each entry opens or updates its item, exactly as an event with state `firing` (alert) or `open`
   (action) would.
2. Every alert in state `open` or `dismissed`, and every action in state `open` or `snoozed`, of a
   covered kind of this source, that has no entry, resolves. It doesn't matter whether an event or
   a snapshot opened it.
3. An action you closed (done or dismissed) whose `(kind, key)` is still listed stays closed. It
   opens again as a new item only after a snapshot without it, followed by one with it.
4. The source is observed again (if it wasn't, its open `beacon.source_stopped` resolves, or with
   none open Beacon records `beacon.source_observed`), its
   `last_snapshot` is now and its `stale_after` is the one given.

With `observed: false`, nothing opens or resolves (`firing` is ignored): the source is unobserved
with `reason`, and its alerts are muted until a snapshot with `observed: true`
(`spec/delivery.md`, "Sources"). A source that declares an alert kind opens a
`beacon.source_stopped` alert saying so.

The snapshot is all or nothing: if any entry fails a check, `422` with every problem
(`at` like `snapshot firing[2] facts.used_percent`), and nothing changes.

`200`:

```json
{"ok": true, "opened": [4183], "reopened": [], "updated": [4101], "resolved": [4077], "observed": true}
```

### `POST /v1/trail`

Trail's alert webhook (Trail's `spec/alerts.md`): point Trail's `alerts.webhook` here and nothing
else in Trail changes. Local access only. Every item it makes belongs to source `trail`
(`spec/declarations.md`, "Beacon's own kinds").

**`snapshot`** (`{"type": "snapshot", "sent_at", "firing": [{key, source, alert, severity, title, body, labels, value, since}]}`):

| Trail severity | Kind | What a firing entry does |
|---|---|---|
| `severe` | `trail.severe` (alert, urgent) | Opens or updates the alert keyed by Trail's `key`. Absent from a later snapshot: it resolves. |
| `warning` | `trail.warning` (update, silent) | Records one update when the key appears; nothing while it stays; again only after a snapshot without it. |
| `info` | `trail.info` (record) | The same, as a record. |

- An empty `firing` list resolves every open `trail.severe` alert.
- Trail's keys and notice ids are used as item keys as they are (up to 200 and 256 characters, any
  text): the key rule of `POST /v1/events` doesn't apply to them.
- Each item's slots are Trail's `title` (cut to 40) and `body` (cut to 150). Its labels are
  `trail_source` (Trail's `source`) and `trail_alert` (Trail's `alert`), then Trail's `labels`:
  a label whose name isn't snake_case is dropped, values are cut to 128 characters, and at most 16
  are kept. A notice's labels are the same. Its link is `[trail] open`.
- Trail listing one key twice in a snapshot counts once.
- A snapshot also observes source `trail`, with `stale_after` = `[trail] stale_after` (`11m`:
  Trail re-posts every 5 minutes). If Trail stops posting, `trail` becomes unobserved, its
  alerts are muted, and `beacon.source_stopped` (key `trail`) opens and pushes.

**`notice`** (`{"type": "notice", "id", "source", "alert", "severity", "title", "body", "ts", "event"}`):

| Trail severity | Kind |
|---|---|
| `severe` | `trail.severe_notice` (a one-off alert, urgent), key = the notice `id` |
| `warning` | `trail.warning`, key = the notice `id` |
| `info` | `trail.info`, key = the notice `id` |

- Recorded once per notice `id`: a repeat answers `200` with `"repeat": true` and changes nothing.
- A notice whose `ts` is more than `delivery.late` (an hour) old is recorded and not pushed:
  whatever the decision would have been, its push is suppressed `late` (a `trail.info` record stays
  `none`).
- `event` is not read.

Validation: `type` is `snapshot` or `notice`; `sent_at`/`ts` positive integers; each entry's
`key` (or the notice's `id`) a non-empty string of at most 200 (256 for `id`) characters;
`severity` one of `info`, `warning`, `severe`; `title` a non-empty string of at most 500
characters; `body` a string of at most 4096 characters or absent; `labels` an object of strings
or absent. `source`, `alert`, `value` and `since` are read where said above and otherwise ignored.

Trail retries `408`, `429` and `5xx`, and treats every other status as "never send this body
again". So:

| Status | When | Trail then |
|---|---|---|
| `200` `{"ok": true}` (a notice adds `"item"` and `"repeat"`) | taken, including a repeated notice | moves on |
| `400` | malformed | drops it |
| `403` | not local | drops it |
| `409` | a snapshot older than the last one taken from Trail, more than 5 minutes old, or more than 1 minute in the future | replaces it with a fresh one at its next pass; the notices behind it still go |
| `503` | storage failed | retries |

`422` is never sent: every Trail severity maps to a built-in kind.

## Clients

### `POST /v1/presence`

A client's heartbeat. Beacon holds pushes while you are active somewhere (`spec/delivery.md`).

```json
{"client": "web", "device": "web-3f9a1c", "active": true, "viewing": "assistant:thread:42"}
```

| Field | Type | Rule |
|---|---|---|
| `client` | `"web"` \| `"ios"` | required |
| `device` | string | required, 1 to 64 of `A–Z a–z 0–9 _ -`. Stable per browser profile or per app install. |
| `active` | bool | required |
| `viewing` | string or null | at most 128 characters: the context on screen, like `assistant:thread:42`. Ignored when `active` is false. |

`200` `{"ok": true, "heartbeat": "30s", "active_until": 1794571275000}` (`active_until` null when
`active` is false).

What each client must do:

- **Web:** active while the tab is visible and focused (`document.visibilityState === "visible"`
  and `document.hasFocus()`) **and** the person has moved the pointer, pressed a key or touched
  the page within the last 5 minutes (`pointermove`, `pointerdown`, `keydown`, `wheel`,
  `touchstart` on the page). Post at once on becoming active, every `heartbeat` (30 s) while
  active, and `active: false` on becoming hidden or blurred, or when 5 minutes pass without input.
  A tab left open on the app with nobody in front of it is not active.
- **iOS:** active while the app is in the foreground (scene phase active). Post at once on
  entering the foreground, every 30 s while there, and `active: false` on leaving it.
- Post again at once whenever `viewing` changes.
- No idle detection from the operating system, on either: the web's 5 minutes are input on the
  page itself.

`client` matters: an urgent push is held only while an `ios` device is active, because a web tab
can be open with nobody looking at it (`spec/delivery.md`, step 5).

A device counts as active for `presence.active_window` (75 s) after its last heartbeat with
`active: true`, or until a heartbeat with `active: false`. A **visit** is a run of time with at
least one active device and no gap longer than the active window; Beacon records when each visit
ends, for `since_last_visit`.

### `GET /v1/inbox`

What a client shows: the notification centre, the dot, Today's roll-up, and the in-app banner.
Reading it changes nothing.

| Parameter | Default | |
|---|---|---|
| `source` | all | Activity only: a source, or several separated by commas. |
| `class` | `update` | Activity only: classes separated by commas. `update,record` shows records too. |
| `before` | none | Activity only: the previous page's `activity.next`. |
| `limit` | 50 | Activity page size, 1 to 200. |
| `since` | 0 | `held` only: a `version` from an earlier answer; lists held items changed after it. |

Sends `ETag: W/"<version>"`. With `If-None-Match` equal to it, `304` and no body. An ETag belongs
to its URL: a client keeps one per query.

**Polling.** Clients poll; there is no stream. Poll every 15 s while the client is active (as for
presence), at once on becoming active, when a push arrives, and after the client's own action.
Don't poll while hidden or in the background.

```json
{
  "version": 90211,
  "now": 1794571200000,
  "dot": true,
  "needs_you": [ "Item", "…" ],
  "alerts": {"open": [ "Item" ], "resolved": [ "Item" ]},
  "activity": {"items": [ "Item" ], "next": 4100},
  "held": [ "Item" ],
  "since_last_visit": {
    "since": 1794540000000,
    "count": 4,
    "sources": [{"source": "ship", "name": "Deploys", "icon": "ship", "count": 3}],
    "items": [ "Item" ]
  }
}
```

| Field | What it holds |
|---|---|
| `needs_you` | Alerts in state `open`, newest opened first, then actions in state `open`, newest first. At most 100. Dismissed alerts and snoozed actions are left out. |
| `alerts.open` | Alerts in state `open` or `dismissed`, newest opened first. At most 100. |
| `alerts.resolved` | Alerts resolved within `inbox.resolved_days` (7), newest resolved first. At most 50. |
| `activity.items` | Items of the `class` filter (default `update`), newest created first, one page. Alerts and actions live in Needs you and Alerts, so Activity shows each item once. |
| `activity.next` | The id to pass as `before` for the next page; null on the last page. |
| `held` | Items whose push is held because you're active (`push.status: "held"`, `push.reason: "active"`) and whose `version` is above `since`: what the in-app banner shows. |
| `dot` | `needs_you` isn't empty. The only badge: a dot, never a number, and no app icon badge. |
| `since_last_visit` | Updates created since the end of your previous visit (`since`; null when there was none, and then the last 24 hours). `count` in all, `sources` with a count each (most first), `items` the 5 newest. Today's "Since you were last here". |

### The item

Every route that returns notifications returns this shape.

```json
{
  "id": 4182,
  "version": 90211,
  "kind": "backup.failed",
  "class": "alert",
  "source": "backup",
  "source_name": "Backups",
  "icon": "archive",
  "key": "nightly",
  "title": "Backup failed",
  "body": "The disk is full. The last good backup was 02:00.",
  "preview": "Backup problem",
  "url": "/system",
  "context": null,
  "labels": {"service": "backup"},
  "created": 1794571200000,
  "updated": 1794571260000,
  "state": "open",
  "times": {
    "opened": 1794571200000,
    "acknowledged": null,
    "dismissed": null,
    "snoozed_until": null,
    "done": null,
    "resolved": null
  },
  "alert": {
    "openings": 1,
    "flapping": false,
    "one_off": false,
    "repeats_sent": 0,
    "next_repeat": 1794578400000
  },
  "actions": [
    {"id": "acknowledge", "label": "Acknowledge", "url": null},
    {"id": "silence_1h", "label": "Silence 1h", "url": null}
  ],
  "seen": false,
  "collapse_id": "b1-29fd692930019189",
  "push": {
    "level": "urgent",
    "status": "sent",
    "reason": null,
    "at": 1794571201000,
    "release_at": null,
    "covered_by": null
  }
}
```

| Field | Type | |
|---|---|---|
| `id` | int | |
| `version` | int | The server `version` of the item's last change. |
| `kind`, `class`, `source` | string | `class` is `alert`, `action`, `update` or `record`. |
| `source_name`, `icon` | string | From the source's declarations when the item was made. |
| `key` | string | `""` when none was sent. |
| `title`, `body` | string | Rendered (`spec/declarations.md`, "Rendering"). A resolved alert shows its recovery text. Plain text. |
| `preview` | string or null | The kind's hidden-preview text. |
| `url` | string or null | Where tapping goes. A relative link (`/system`) is resolved by the client against its own base. |
| `context` | string or null | The event's viewing context. |
| `labels` | object of strings | |
| `created`, `updated` | int ms | `updated` changes with any change to the item. |
| `state` | string or null | Alert: `open`, `dismissed`, `resolved`. Action: `open`, `snoozed`, `done`, `dismissed`, `resolved`. Update and record: null. |
| `times` | object or null | Alerts and actions: when each step happened, null if it hasn't (`opened` is the latest opening). Updates and records: null. |
| `alert` | object or null | Alerts only. `openings`: openings within the flap window; `flapping`; `one_off`; `repeats_sent`; `next_repeat`: when the next repeat is due, or null. |
| `actions` | list | The buttons that apply now (below), each `{id, label, url}`; `url` is the rendered link of a custom action, or null. |
| `seen` | bool | You have seen the item in its current state (`spec/delivery.md`, "Seen and done"). |
| `collapse_id` | string | The `apns-collapse-id` its pushes carry, which is also the delivered notification's identifier on the phone. The iPhone app uses it to remove delivered notifications of items that are done. |
| `push.level` | string | The kind's push level, or `none` for a record. |
| `push.status` | string | `none`, `held`, `queued`, `sent`, `failed` or `suppressed`. |
| `push.reason` | string or null | Why it is held, suppressed or failed (below). Null for `queued` and `sent`. |
| `push.at` | int ms or null | When the status last changed. |
| `push.release_at` | int ms or null | While `held`: when Beacon decides again. |
| `push.covered_by` | int or null | While suppressed with `catch_up`: the `beacon.catch_up` item that stood in for it. |

`push.reason` values:

| Status | Reasons |
|---|---|
| `none` | `declared` (a record, or a kind with push `none`) |
| `held` | `active` (you're active somewhere), `quiet_hours` |
| `suppressed` | `silenced`, `unobserved`, `flapping`, `viewing` (a client showed its context), `seen` (seen while held), `closed` (done, dismissed, snoozed or resolved while held, or while queued: see `spec/delivery.md`, "Sending"), `throttled`, `catch_up`, `late`, `push_off` |
| `failed` | `no_devices`, `rejected` (Apple refused it for every device), `expired` (not taken within an hour), `unconfigured` (no signing key) |

The buttons that apply now (`actions`), from the kind's declared list:

| Item | Shows |
|---|---|
| alert, `open` | all declared, except `acknowledge` once acknowledged |
| alert, `dismissed` | the same, except `dismiss` |
| action, `open` | all declared |
| action, `snoozed` | all declared, except `snooze` |
| update | all declared |
| record; or alert `resolved`; or action `done`, `dismissed`, `resolved` | custom actions that have a `url` only |

### `GET /v1/items/{id}`

The item, plus:

```json
{
  "item": { "…": "the Item" },
  "facts": {"last_ok": 1794535200000},
  "slots": {"reason": "The disk is full"},
  "acted": [{"action": "review", "at": 1794571300000, "client": "web"}],
  "history": [
    {"at": 1794571200000, "what": "opened", "client": null},
    {"at": 1794571201000, "what": "push_sent", "client": null},
    {"at": 1794571300000, "what": "seen", "client": "ios"}
  ],
  "attempts": [
    {"at": 1794571201000, "device": "…3f9a1c", "environment": "production", "attempt": 1, "status": "accepted", "reason": null}
  ]
}
```

`history[].what` is one of `recorded` (an update or record made), `opened`, `reopened`, `updated`, `resolved`, `acknowledged`,
`dismissed`, `done`, `snoozed`, `woke` (a snooze ended), `seen`, `action` (a custom action; see
`acted`), `push_held`, `push_released`, `push_sent`, `push_failed`, `push_suppressed`, `repeated`,
`recovery_sent`, `flapping`, `settled`. `attempts[].status` is `accepted`, `rejected` or
`uncertain` (`spec/apns.md`). `device` is the token's last six characters. `404` for an unknown id.

### `POST /v1/items/{id}/actions`

```json
{"action": "snooze", "until": 1794578400000, "client": "ios"}
```

| Field | Type | Rule |
|---|---|---|
| `action` | string | required: one of the item's current `actions`, by id |
| `until` | int ms | `snooze` only, optional: default now + `delivery.snooze` (1 h). Between 1 minute and 30 days ahead. |
| `client` | `"web"` \| `"ios"` \| `"api"` | optional, default `"api"`: recorded in history |

| Action | Effect |
|---|---|
| `done` | An open or snoozed action becomes `done`. |
| `acknowledge` | An open or dismissed alert is acknowledged: no more repeats. A one-off alert resolves. |
| `snooze` | An open action becomes `snoozed` until `until`, then opens again and is decided afresh. |
| `dismiss` | An open or snoozed action becomes `dismissed`. An open alert becomes `dismissed`; a one-off alert resolves instead. |
| `silence_1h` | Opens a silence for one hour matching the item's source, kind, and key when it has one, with reason "Silenced from a notification". The item doesn't change. |
| a custom action | Recorded in `acted`. Without a `url`, an `action` item also becomes `done`. |

A held push of an item that leaves `open` is suppressed (`closed`).

`200` `{"item": Item, "silence": Silence or null}`: `silence` is the one `silence_1h` opened, with
its `until`, so the client can say until when. Pressing an action whose effect already holds
(done on a done item, snooze on a snoozed one, acknowledge on an acknowledged alert or a resolved
one-off; a second tap or a notification button that fired twice) is `200` with
the item unchanged. An action the item doesn't offer now is `409`, with the item's current actions
in the fix. `404` for an unknown item.

### `POST /v1/seen`

```json
{"through": 4182}
```

or `{"ids": [4182, 4177]}`: exactly one of the two. `through` marks every item with an id up to
it; `ids` at most 500 items. Optional `"client"`: `"web"`, `"ios"` or `"api"` (default), recorded in
each item's history. `200` `{"ok": true, "seen": 3}`: how many were newly marked. A held
push of a newly seen item is suppressed (`seen`), except that `"client": "web"` never suppresses
an urgent push. An `ios` or `api` mark on an item that only a web client has seen, whose urgent
push is held, is recorded and suppresses it, though it doesn't count as newly marked. When a client marks seen is in
`spec/delivery.md`, "Seen and done".

### Silences

A silence mutes pushes for what it matches until it ends. Items are still recorded and shown.

```json
{
  "id": 12,
  "match": {"source": null, "kind": null, "key": null, "labels": {"service": "dashboard"}},
  "reason": "Deploying dashboard",
  "created": 1794571200000,
  "until": 1794573000000,
  "ended": null,
  "active": true,
  "created_by": "api",
  "matched": 2
}
```

- An item matches when every field given in `match` equals the item's: `source`, `kind`, `key`, and
  each label in `labels`. At least one must be given.
- `active` is true while now is before `until` and it hasn't been ended. `ended` is when it was
  ended early. `matched` counts the items it muted.
- When a silence ends, each alert it muted that is still open is decided afresh, as if it had just
  opened (`spec/delivery.md`). Muted actions and updates are not pushed later.

#### `GET /v1/silences`

`{"silences": [Silence]}`: the active ones, soonest-ending first. `?all=1` (parameter `all`) adds
those that ended in the last 7 days, newest first.

#### `POST /v1/silences`

```json
{"match": {"labels": {"service": "dashboard"}}, "for": "30m", "reason": "Deploying dashboard"}
```

| Field | Rule |
|---|---|
| `match` | required: an object of `source`, `kind`, `key`, `labels`, at least one given. `key` needs `kind`. |
| `for` or `until` | exactly one: a duration from `1m` to `30d`, or a time in the next 30 days |
| `reason` | required, 1 to 100 characters: shown beside the silence |
| `client` | optional, `"web"`, `"ios"` or `"api"` (default): becomes `created_by` |

`201` with the Silence.

**Idempotency-Key** (header, optional), as for `POST /v1/events`: the same key and body within
`idempotency` (24 h) answers the silence the first request opened, and opens no other; the same
key with another body is `409`. The body is compared as sent, so a retried `"for": "30m"` matches
even though its end would now be later. Keys for silences are their own scope, apart from every
source's.

A deploy script opens one before it deploys and moves its end after:

```sh
id=$(curl -s -X POST -H 'Content-Type: application/json' http://127.0.0.1:8210/v1/silences \
  -d '{"match": {"labels": {"service": "dashboard"}}, "for": "30m", "reason": "Deploying dashboard"}' | jq .id)
# … deploy …
curl -s -X PATCH -H 'Content-Type: application/json' "http://127.0.0.1:8210/v1/silences/$id" -d '{"for": "5m"}'
```

(`beacon silence add` and `beacon silence extend` do the same, `spec/clients.md`.)

#### `GET /v1/silences/{id}`

The Silence, or `404`.

#### `PATCH /v1/silences/{id}`

`{"for": "5m"}` (from now) or `{"until": <ms>}`, and/or `{"reason": "…"}`. A time in the past
ends it now. `200` with the Silence; `409` for one already ended.

#### `DELETE /v1/silences/{id}`

Ends it now. `200` with the Silence (ending one already ended changes nothing); `404`.

### Devices

#### `POST /v1/devices`

```json
{"token": "3f9a…(64 to 400 lowercase hex)", "environment": "production", "name": "iPhone"}
```

| Field | Rule |
|---|---|
| `token` | required: the APNs device token, 64 to 400 lowercase hex characters |
| `environment` | required: `"sandbox"` (a debug build) or `"production"` |
| `name` | optional, at most 64 characters |

Adds the device, or updates its environment and name. The iPhone app sends this on every launch.
`200` `{"ok": true}`.

#### `GET /v1/devices`

```json
{"devices": [{"device": "…3f9a1c", "environment": "production", "name": "iPhone", "created": 1794000000000, "updated": 1794571200000, "last_push": 1794571201000, "last_status": "accepted"}]}
```

Tokens are never returned or logged whole: `device` is the last six characters. They are kept
only in `devices.db` (`spec/config.md`, "Storage"), never in `beacon.db`, so the main database can
be shared read-only.

#### `DELETE /v1/devices/{token}`

`200` `{"ok": true}`, or `404`.

### `POST /v1/live-activity`

One Live Activity update for the iPhone app, sent at once (the same contract as the service
Beacon replaces). Local access only. Nothing is queued, retried or kept as an item: the next
update supersedes a lost one.

```json
{
  "token": "<the activity's push token, lowercase hex>",
  "event": "update",
  "contentState": {"step": "Reading the logs", "progress": 0.4},
  "timestamp": 1794571200,
  "dismissalDate": 1794574800,
  "staleDate": 1794572100,
  "alert": {"title": "Still working", "body": "Two steps left"},
  "priority": 10,
  "environment": "production"
}
```

| Field | Rule |
|---|---|
| `token` | required: the activity's push token, 64 to 400 lowercase hex characters |
| `event` | required: `"update"` or `"end"` |
| `contentState` | required: an object, passed to the app as it is (it decodes it into its activity's content state) |
| `timestamp` | required: unix **seconds**, more than 0 |
| `dismissalDate`, `staleDate` | optional: unix seconds, more than 0 |
| `alert` | optional: `{title, body}`, both strings |
| `priority` | optional: `5` or `10` (default `10`) |
| `environment` | optional: `"sandbox"` or `"production"` |

Beacon sends it to Apple with `apns-push-type: liveactivity`, topic `<apns.topic>.push-type.liveactivity`,
the priority given, and `apns-expiration` an hour ahead, as
`{"aps": {"timestamp", "event", "content-state", "dismissal-date"?, "stale-date"?, "alert"?: {"title", "body", "sound": "default"}}}`
(`spec/apns.md`). The environment is the one given; else that of the most recently updated
registered device, then the other one when Apple answers `BadDeviceToken`.

| Status | When |
|---|---|
| `200` `{"ok": true}` | Apple took it |
| `400` | a bad field, or the payload is over 4 KB |
| `403` | not local |
| `410` | Apple says the token is dead: the activity ended |
| `502` | Apple didn't take it, or didn't answer |
| `503` | no APNs signing key is configured |

Error bodies use Beacon's error shape; callers act on the status.
