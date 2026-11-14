# Configuration

`beacon serve --config beacon.toml`. Every field is optional: with no file, Beacon listens on
loopback, keeps its data in `./beacon-data`, and can't push (no APNs key). An unknown field is an
error with the name you probably meant, so a typo is caught at start. Durations are a whole number
and a unit (`90s`, `10m`, `2h`, `7d`). The parser will be `crates/core/src/config.rs`.

```toml
listen = "127.0.0.1:8210"       # address and port. Loopback by default: put a proxy in front to reach it from elsewhere.
data_dir = "./beacon-data"      # the SQLite database, beacon.db
devices_db = "./beacon-data/devices.db"   # the devices and their push tokens; default data_dir/devices.db
readable_db = false                       # true: beacon.db mode 644, readable by other local users
timezone = "UTC"                # IANA name: quiet hours, the catch-up, the budget's day, and how times render
push = true                     # false: decide and record everything, send nothing (push.reason "push_off")
origins = []                    # Origin values a browser may send mutations from, beside the request's own host
banned_words = []               # words no declared copy may contain, checked at publish: your app's name, the machine's name
retention = "90d"               # closed and one-off items are deleted this long after (spec/delivery.md, "Retention")
idempotency = "24h"             # how long an Idempotency-Key is remembered

[limits]
body_bytes = 1048576            # largest request body

[presence]
heartbeat = "30s"               # what clients are told to send at (POST /v1/presence answers it)
active_window = "75s"           # a device is active this long after its last heartbeat

[quiet_hours]
start = "23:00"                 # local time, HH:MM. Equal start and end turn quiet hours off.
end = "08:00"                   # also when the morning catch-up goes out

[delivery]
hold = "90s"                    # an action or update waits this long while you're active
alert_hold = "120s"             # an alert waits this long while you're active
flap_window = "30m"
flap_openings = 3               # more openings than this within flap_window is flapping
flap_stable = "15m"             # a flapping alert settles after this long without a change
budget = 5                      # non-alert pushes a day before beacon.budget is recorded
snooze = "1h"                   # a snooze with no until
snapshot_stale_after = "15m"    # a snapshot's stale_after when it sends none
late = "1h"                     # a Trail notice older than this is recorded, not pushed

[apns]
key_path = "/etc/beacon/AuthKey_ABC123DEFG.p8"   # unset: no pushes, each fails "unconfigured"
key_id = "ABC123DEFG"           # default: from the file name AuthKey_<key id>.p8
team_id = "TEAM123456"          # required with key_path
topic = "com.example.lab"       # the app's bundle id; required with key_path
retry = ["10s", "30s", "2m", "5m"]   # waits between attempts after a retryable failure
give_up = "1h"                  # no attempt starts this long after the push was queued

[inbox]
resolved_days = 7               # resolved alerts listed in GET /v1/inbox
page = 50                       # the default activity page
since_last_visit_items = 5      # items listed in since_last_visit

[trail]
open = "/"                      # the link every trail.* item opens
stale_after = "11m"             # Trail re-posts its snapshot every 5 minutes

[sink]
trail_url = "http://127.0.0.1:8200"   # unset: nothing is copied to Trail. Plain http: Trail runs beside Beacon
source = "beacon"               # the Trail source the copies are recorded under

[auth]
producer_token = "…"            # when set, producer routes also accept Authorization: Bearer <token> from other machines
```

| Field | Range |
|---|---|
| `listen` | an address and port |
| `timezone` | an IANA zone name Beacon can load, or `UTC` |
| `origins` | full origins, like `https://lab.example.com` (scheme, host, optional port; no path) |
| `banned_words` | each a single word |
| `retention` | `1d` to `3650d` |
| `idempotency` | `1h` to `7d` |
| `limits.body_bytes` | 1 KB to 16 MB |
| `presence.heartbeat` | `5s` to `5m`, less than `active_window` |
| `presence.active_window` | `10s` to `15m` |
| `quiet_hours.start`, `.end` | `HH:MM`, 24-hour |
| `delivery.hold`, `.alert_hold` | `1s` to `30m` |
| `delivery.flap_window`, `.flap_stable` | `1m` to `1d` |
| `delivery.flap_openings` | 1 to 100 |
| `delivery.budget` | 0 to 1000 |
| `delivery.snooze` | `1m` to `30d` |
| `delivery.snapshot_stale_after`, `trail.stale_after` | `1m` to `7d` |
| `delivery.late` | `1m` to `7d` |
| `apns.retry` | 0 to 10 durations, each `1s` to `1h` |
| `apns.give_up` | `1m` to `1d` |
| `inbox.resolved_days` | 1 to 90 |
| `inbox.page` | 1 to 200 |
| `inbox.since_last_visit_items` | 0 to 50 |
| `trail.open`, link | starts with `/` or `https://` |
| `sink.trail_url` | `http://` (the copy speaks plain HTTP) |
| `apns.key_id`, `apns.team_id` | 10 capital letters and digits |
| `auth.producer_token` | at least 16 printable characters, no spaces |

A value out of range stops Beacon at start with a `Problem` naming the field and a value that
works.

## Storage

`data_dir/beacon.db`: SQLite in write-ahead-log mode with `synchronous = FULL`, so an accepted
event survives a power cut. The folder is created with mode 700 and the databases 600. With
`readable_db = true`, `beacon.db` and its log files are 644 and a folder Beacon creates is 755, so
another local user can read it; `devices_db` stays 600 either way. The mode is set on every start. Back it up
like any SQLite file (`sqlite3 beacon.db ".backup copy.db"`, or copy it while Beacon is stopped).

`devices_db` (default `data_dir/devices.db`): the registered devices and their APNs tokens, and
nothing else. Tokens are secrets, so they never enter `beacon.db`: a push attempt keeps only the
token's last six characters, and the push queue looks devices up when it sends. `beacon.db` can
then be opened read-only by another program (an assistant answering "what did Beacon send
today?") without exposing a token. Back up both files. A `beacon.db` from before devices moved
(schema version 1) has its devices moved into `devices_db` at start, with the old rows overwritten.

### Reading the database

Another program may read `beacon.db` directly, read-only (with `readable_db = true` when it runs
as another user; DuckDB's SQLite reader, or `sqlite3 -readonly`). It reads the views below, never
the tables: the tables are internal and change between versions. A `_v1` view only gains columns,
at the end; one that would lose, rename or retype a column becomes a `_v2` view beside it, and the
`_v1` stays. The views are made again at every start. Every column is `INT` (an integer; times are
milliseconds since 1970 UTC) or `TEXT`, as SQLite reports them, so DuckDB types them without
`sqlite_all_varchar`. A null is a value the item doesn't have.

`items_v1`, one row per item:

| Column | Type | |
|---|---|---|
| `id` | INT | the item's id (`GET /v1/items/{id}`) |
| `source`, `source_name` | TEXT | the source, and its display name |
| `kind`, `key` | TEXT | the kind and the key |
| `class` | TEXT | `alert`, `action`, `update` or `record` |
| `state` | TEXT | the lifecycle state (`spec/delivery.md`); null for updates and records |
| `push_level`, `push_status`, `push_reason` | TEXT | as `GET /v1/items/{id}` gives them |
| `title`, `body` | TEXT | the words as shown now |
| `url`, `context` | TEXT | the link it opens, and its context |
| `labels` | TEXT | a JSON object of strings |
| `service` | TEXT | its `service` label (`spec/declarations.md`) |
| `seen` | INT | 1 once seen, else 0 |
| `created`, `updated` | INT | made, and last changed |
| `opened`, `acknowledged`, `dismissed`, `snoozed_until`, `done`, `resolved` | INT | when each happened (alerts and actions) |

`history_v1`, one row per change of an item (`GET /v1/items/{id}`, `history`), with the item as
that change left it:

| Column | Type | |
|---|---|---|
| `id` | INT | the change's id, in order |
| `item` | INT | the item's id |
| `at` | INT | when |
| `what` | TEXT | the step, like `opened`, `resolved`, `push_sent` |
| `client` | TEXT | the client that did it, when one did |
| `source`, `kind`, `class`, `service` | TEXT | the item's |
| `state`, `title`, `push_level`, `push_status`, `push_reason` | TEXT | the item right after the change |

```sql
-- DuckDB: how many items each service raised, and the latest
ATTACH 'beacon.db' AS beacon (TYPE sqlite, READ_ONLY);
SELECT service, kind, count(*) AS items, to_timestamp(max(created) / 1000) AS latest
FROM beacon.items_v1 GROUP BY ALL;
```

### Going back a version

`beacon.db` carries a schema version (`PRAGMA user_version`; now 3). A Beacon refuses to open a
database of a newer version than it knows, so going back to an older Beacon after an upgrade that
changed the schema needs the database as it was. Before it moves a database to a newer schema,
Beacon keeps a consistent copy of it (SQLite's `VACUUM INTO`) at
`data_dir/previous/v<old version>/beacon.db`, mode 600, written under another name and renamed so
it is whole or absent. If the copy can't be made (the folder isn't writable, the disk is full),
Beacon doesn't migrate: it stops at start with the problem, and the database is unchanged.

To go back to the Beacon that wrote version N:

1. Stop Beacon.
2. Move `data_dir/beacon.db` aside (and its `-wal` and `-shm` files), and copy
   `data_dir/previous/vN/beacon.db` to `data_dir/beacon.db`.
3. Start the older Beacon. `devices_db` needs nothing: it is the same in every version since 2.

What happened after the upgrade (new items, history, silences) is not in the copy: it is the
database as it was at the upgrade. The copy stays until you delete it; the next upgrade from the
same version replaces it. A copy of a version 1 database still holds push tokens (they left
`beacon.db` in version 2), so delete `previous/v1` once you won't go back that far.

| Version | What changed | Copy kept |
|---|---|---|
| 1 | the first schema | none |
| 2 | devices and their tokens move to `devices_db`; items keep their notification category | `previous/v1` |
| 3 | each history row keeps what its item was right after the change, for the copy to Trail | `previous/v2` |

## The copy to Trail

With `[sink] trail_url` set, Beacon copies every item change to Trail as an event, so the history
of notifications sits beside a system's metrics. It sends batches to Trail's `POST /v1/ingest`,
in order, from a cursor kept in its database, retrying with backoff while Trail doesn't answer or
answers `408`, `429` or `5xx`; each event's id is `beacon-<history row id>`, so a resend is stored
once. The copy is outside the notification's path: a Trail outage never delays a push.

Each event says what the item was right after that change (its `state`, `title`, push level,
status and reason), as the change's history row recorded it, not what the item is when the copy
runs: a copy sent late, after the item resolved, still says it opened. (Changes recorded before
schema version 3 carry the item as it was when Beacon was upgraded.)

Any other `4xx` will never succeed. A batch Trail refuses is sent again one change at a time, in
order; a single change it refuses is dropped, logged and counted (`sink_dropped` in
`GET /v1/health`), and the copies behind it go on.

The event is `<sink.source>.notification.recorded`. Its `item_source` is the item's source (Trail
reserves `source` for the sender). Declare it in the Trail declarations of whoever runs Beacon:

```toml
[events."beacon.notification.recorded"]
description = "A notification was made or changed state in Beacon"
retention = "forever"
[events."beacon.notification.recorded".props]
item = "int"
what = "string"            # a history step (spec/api.md, GET /v1/items/{id}), like opened, resolved, push_sent
kind = "string"
class = { enum = ["alert", "action", "update", "record"] }
item_source = "string"
state = "string?"          # the item's state after the change; absent for updates and records
title = "string"
push_level = { enum = ["urgent", "normal", "silent", "none"] }
push_status = { enum = ["none", "held", "queued", "sent", "failed", "suppressed"] }
push_reason = "string?"
```

## Running it

One process, no async runtime outside the push sender. Beacon is small: a server with nothing to
do should hold well under 20 MB (`scripts/check` measures it). A systemd unit, a container, or
`beacon serve` in a terminal all work; it needs only its config, its data folder and, to push, the
key file.

Beacon is crash-only: there is no shutdown step. Every accepted write is committed before it is
answered, owed work is a time stored beside what caused it, and a push that was in flight is
retried at start (within `apns.give_up`), so SIGTERM's default action, ending the process at once,
loses nothing Beacon answered for. That is how it stops; a service manager needs no stop command.
