# Configuration

`beacon serve --config beacon.toml`. Every field is optional: with no file, Beacon listens on
loopback, keeps its data in `./beacon-data`, and can't push (no APNs key). An unknown field is an
error with the name you probably meant, so a typo is caught at start. Durations are a whole number
and a unit (`90s`, `10m`, `2h`, `7d`). The parser will be `crates/core/src/config.rs`.

```toml
listen = "127.0.0.1:8300"       # address and port. Loopback by default: put a proxy in front to reach it from elsewhere.
data_dir = "./beacon-data"      # the SQLite database (beacon.db) and nothing else
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
trail_url = "http://127.0.0.1:8200"   # unset: nothing is copied to Trail
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
| `sink.trail_url` | `http://` or `https://` |

A value out of range stops Beacon at start with a `Problem` naming the field and a value that
works.

## Storage

`data_dir/beacon.db`: SQLite in write-ahead-log mode with `synchronous = FULL`, so an accepted
event survives a power cut. The folder is created with mode 700 and the database 600. Back it up
like any SQLite file (`sqlite3 beacon.db ".backup copy.db"`, or copy it while Beacon is stopped).

## The copy to Trail

With `[sink] trail_url` set, Beacon copies every item change to Trail as an event, so the history
of notifications sits beside a system's metrics. It sends batches to Trail's `POST /v1/ingest`,
in order, from a cursor kept in its database, retrying with backoff while Trail is unreachable;
each event's id is `beacon-<history row id>`, so a resend is stored once. The copy is outside the
notification's path: a Trail outage never delays a push.

The event is `<sink.source>.notification.recorded`. Declare it in the Trail declarations of
whoever runs Beacon:

```toml
[events."beacon.notification.recorded"]
description = "A notification was made or changed state in Beacon"
retention = "forever"
[events."beacon.notification.recorded".props]
item = "int"
what = "string"            # a history step (spec/api.md, GET /v1/items/{id}), like opened, resolved, push_sent
kind = "string"
class = { enum = ["alert", "action", "update", "record"] }
source = "string"
state = "string?"          # the item's state after the change; absent for updates and records
title = "string"
push_level = { enum = ["urgent", "normal", "silent", "none"] }
push_status = { enum = ["none", "held", "queued", "sent", "failed", "suppressed"] }
push_reason = "string?"
```

## Running it

One process, no async runtime outside the push sender. Beacon is small: a server with nothing to
do should hold well under 20 MB. A systemd unit, a container, or `beacon serve` in a terminal all
work; it needs only its config, its data folder and, to push, the key file.
