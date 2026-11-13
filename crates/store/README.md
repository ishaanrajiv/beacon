# beacon-store

Beacon's SQLite store: `data_dir/beacon.db`, write-ahead log, `synchronous = FULL`. Every write is
one transaction that applies a transition from `beacon-core` (`lifecycle`, `delivery`) and records
the decision it led to, so owed work (a held push, a repeat, a snooze, the catch-up, a retry) is in
the database with what caused it. The store never reads the clock: every method takes `now`
(ms since 1970 UTC). The server is a thin layer: parse the request, call one method, serialize what
it returns. Errors carry `kind.status()` (400, 404, 409, 422, 503) and problems in the API's shape.

```rust
let store = Store::open(&data_dir.join("beacon.db"), settings)?; // Settings from beacon.toml
store.set_trail_open(&config.trail.open)?;
```

## By route

| Route | Method |
|---|---|
| `GET /v1` (`version`) | `version() -> i64` |
| `GET /v1/registry` | `sources() -> Vec<SourceView>` |
| `GET /v1/registry/{source}`, `GET /v1/kinds` | `declarations(source) -> Option<Declarations>`, `all_declarations()` |
| `PUT /v1/registry/{source}` | `publish(&Declarations, now)` (run the checks with `banned_words` first) |
| `DELETE /v1/registry/{source}` | `unpublish(source) -> bool` |
| `POST /v1/events` | `apply_event(&Event, idempotency_key, now) -> EventAnswer` |
| `POST /v1/snapshots` | `apply_snapshot(&Snapshot, now) -> SnapshotAnswer` |
| `POST /v1/trail` snapshot | `apply_trail_snapshot(sent_at, Vec<SnapshotEntry>, now)` (map Trail's entries to `trail.*` kinds first) |
| `POST /v1/trail` notice | `apply_trail_notice(id, &Event, late, now) -> EventAnswer` |
| `POST /v1/presence` | `heartbeat(&Heartbeat, now) -> Option<active_until>` |
| `GET /v1/inbox` | `inbox(&InboxQuery, now) -> Inbox` (ETag: `W/"<version>"`) |
| `GET /v1/items/{id}` | `item_detail(id, now) -> Option<ItemDetail>`; `item(id, now)` for the bare item |
| `POST /v1/items/{id}/actions` | `press(id, action, until, client, now) -> PressAnswer` |
| `POST /v1/seen` | `mark_seen(&SeenTarget, client, now) -> u32` |
| `GET /v1/silences[/{id}]` | `silences(all, now)`, `silence(id, now)` |
| `POST /v1/silences` | `open_silence(&NewSilence, now) -> SilenceView` (turn `for` into `until`) |
| `PATCH /v1/silences/{id}` | `change_silence(id, until, reason, now)` |
| `DELETE /v1/silences/{id}` | `end_silence(id, now)` |
| `GET/POST/DELETE /v1/devices` | `devices()`, `register_device(token, environment, name, now)`, `forget_device(token, now)` |

`Event`, `Snapshot`, `Heartbeat` derive `Deserialize` with `deny_unknown_fields`; the server still
owns "did you mean" for unknown fields and the body limits.

## Background jobs

| Job | Method |
|---|---|
| Owed work, at least once a second and at start | `run_due(now) -> Ran`: holds released, the catch-up, snoozes woken, repeats, flapping alerts settled, silences ended, stale sources, pushes given up (`expired`), daily retention. Each piece runs in its own transaction, oldest first. |
| What is due, without running it | `due(now) -> Vec<(at, Due)>`; `next_due(now) -> Option<at>` to sleep until |
| The push sender | `pushes_due(now) -> Vec<PushJob>` (each with every device), then `push_attempted(push, &[Attempt], PushNext, now)` with `Sent`, `Failed(reason)` or `RetryAt(t)`; `forget_device` for a dead token |

## Layout

`lib.rs` (open, migrate, transactions), `rows.rs` (an item's row), `engine.rs` (deciding and
recording inside a transaction), `producers.rs`, `clients.rs`, `reads.rs`, `jobs.rs`,
`schema.sql` (version 1; a later version adds a migration beside it). `cargo test -p beacon-store`
runs every case against an in-memory database, plus one file database across a restart.
