# Delivery

How Beacon decides, for each notification, whether and when it reaches the phone; and how alerts
and actions move through their states. Everything here is a pure function of the item, the
stored state, presence and a clock, so every rule is tested with a fake clock. The decision and
the state machines live in `crates/core` (`delivery.rs`, `lifecycle.rs`, `quiet.rs`); `crates/store`
runs them in the transaction that caused them, and the server runs the jobs that wake them.

## The numbers

Every one but `throttle` is set in `beacon.toml` (`spec/config.md`); `throttle` is per kind, in
`kinds.toml`, so that the published declarations say everything about a kind.

| Setting | Default | What |
|---|---|---|
| `presence.heartbeat` | `30s` | How often an active client posts presence |
| `presence.active_window` | `75s` | A device is active this long after its last heartbeat |
| `delivery.hold` | `90s` | An action or update waits this long while you're active |
| `delivery.alert_hold` | `120s` | An alert waits this long while you're active |
| `quiet_hours.start`, `.end` | `23:00`, `08:00` | Local time in `timezone`; only urgent and time-bound pushes pass |
| catch-up | at `quiet_hours.end` | One push standing in for what quiet hours held |
| `throttle` (per kind, in kinds.toml) | `10m` | At most one push per kind and key this often |
| `delivery.flap_window` | `30m` | Openings counted over this window; a resolved alert firing again within it reopens the same item |
| `delivery.flap_openings` | `3` | More openings than this in the window is flapping |
| `delivery.flap_stable` | `15m` | A flapping alert settles after this long without opening or resolving |
| `delivery.budget` | `5` | Non-alert pushes a day before Beacon records `beacon.budget` |
| `delivery.snooze` | `1h` | A snooze with no `until` |
| `delivery.snapshot_stale_after` | `15m` | A source's snapshots are stale after this, unless the snapshot says otherwise |
| `trail.stale_after` | `11m` | The same, for Trail's webhook |
| `delivery.late` | `1h` | A Trail notice older than this is recorded, not pushed |
| `apns.retry` | `10s, 30s, 2m, 5m` | Waits between attempts after a retryable failure (`spec/apns.md`) |
| `apns.give_up` | `1h` | No attempt starts this long after a push was queued |
| `retention` | `90d` | Closed and one-off items are deleted this long after they closed or were made |
| `idempotency` | `24h` | How long an Idempotency-Key is remembered |
| `inbox.resolved_days` | `7` | Resolved alerts shown in the inbox |

## The decision

`decide(item, trigger, state, presence, now) → decision`, run when something might push:

| Trigger | When |
|---|---|
| `opened` | An alert or action opened, or an update was recorded |
| `reopened` | A resolved alert fired again within the flap window |
| `released` | A hold (`push.status: held`, reason `active`) reached its `release_at` |
| `woke` | A snoozed action's time came |
| `unmuted` | A silence that muted an open alert ended, or its source was observed again |
| `settled` | A flapping alert settled while firing |
| `caught_up` | The end of quiet hours, for the one item quiet hours held (see "Catch-up") |
| `repeat` | An unacknowledged alert's next repeat came due |

`state` is what the decision reads: the active silences, which sources are unobserved, the time of
the last push sent per `(kind, key)`, whether the item's last push on the phone is its recovery,
the alert's flapping flag, and the server's `push` setting.
`presence` is every device with its last heartbeat and `viewing`. The decision is one of: **send**
(`push.status` becomes `queued`), **hold** until a time (`held`, with a reason), **none**, or
**suppress** (with a reason).

The steps, in order. The first that applies decides. A step marked *skipped for* doesn't run for
those triggers.

1. **Muted?**
   - A silence matches the item (`spec/api.md`, "Silences") → suppress, `silenced`.
   - The item is an alert and its source is unobserved ("Sources") → suppress, `unobserved`.
2. **Declared quiet?** The class is `record` or the kind's push is `none` → none, `declared`.
3. **Flapping?** The item is an alert that is flapping ("Alerts") → suppress, `flapping`.
4. **Being viewed?** *Skipped for `repeat`.* The item has a `context`, and an active device's
   `viewing` equals it → suppress, `viewing`. The client shows it in place.
5. **You're active?** *Skipped for `released` and `repeat`.* A device is active → hold,
   `active`, until now + `delivery.alert_hold` (120 s) for an alert, now + `delivery.hold` (90 s)
   for an action or update. The item shows in the inbox's `held` list for the in-app banner.
   - push `urgent` → only an `ios` device counts (the phone in hand shows it in the app). Web
     presence never holds an urgent push: a tab can be open on the app with nobody in front of it.
   - any other push → any device counts.
6. **Quiet hours?** Now is within `[quiet_hours.start, quiet_hours.end)` in `timezone`:
   - push `urgent` → go on;
   - the kind is `time_bound` → go on;
   - trigger `repeat` → skip this repeat (it is not held);
   - otherwise → hold, `quiet_hours`, until the next `quiet_hours.end`.
7. **Throttled?** *Skipped for `repeat`, `woke` and `caught_up`.* A push for the same `(kind, key)`
   was sent (decided `send`, whether or not Apple has taken it yet) within the kind's `throttle` →
   suppress, `throttled`. A kind with `throttle = "none"`
   is never throttled. A reopened alert whose last push a device has (sent) or may have (an
   attempt `accepted` or `uncertain`) is its recovery is not throttled either, whether it is
   decided as it reopens or later (a hold `released`, a silence ended): the phone says it is
   back, and the firing text must replace that.
8. **Send.** With `push = false` in beacon.toml → suppress, `push_off` (everything above is still
   decided and recorded, so the decisions can be compared before push is turned on). Otherwise →
   send ("Sending").

**When a hold ends** (`released`): if the item has been seen → suppress, `seen`; for an urgent
push, only a seen from a client other than `web` counts. If it has left
`open` (an alert resolved or dismissed, an action done, dismissed, snoozed or resolved) →
suppress, `closed`. Otherwise decide again with trigger `released`, which skips step 5: an item
you didn't see in the hold pushes even though you are still active.

At any time, a held item that is seen is suppressed (`seen`) at once (an urgent one only when
seen on something other than a web client), and one that leaves `open`
is suppressed (`closed`) at once; it doesn't wait for `release_at`.

## Sending

A send goes to every registered device (`spec/apns.md`), with:

| | |
|---|---|
| title, body | the item's rendered text |
| interruption level | `urgent` → `time-sensitive`, `normal` → `active`, `silent` → `passive` |
| sound | `default`, except `passive` |
| `thread-id` | the item's source, so each source is its own group on the phone |
| `apns-collapse-id` | the item's `collapse_id` |
| `category` | the event's chosen `category`, else the kind's first declared `categories`, else the item's kind |
| `url`, `data` | the item's link; its id, kind, key and collapse id, and its context when it has one |

`collapse_id` (`crates/core/src/ids.rs`):

- An alert or action: `b1-` and 16 lowercase hex digits of FNV-1a 64 over `<kind>\n<key>`. Every
  push about the same alert or action replaces the last one's banner: a recovery, a repeat, a
  snooze ending.
- An update: `i<id>`.
- `beacon.flapping` carries the flapping alert's collapse id, so it replaces that alert's banner.

The item's `push.status` is `queued` until a device takes it (`sent`) or every device has failed
for good (`failed`, with `rejected`, `expired`, `no_devices` or `unconfigured`).

**A queued push never outlives what it says.** The phone must not end up showing the firing text
of an alert or action that has closed:

- When an item leaves `open` (an alert resolved or dismissed, an action done, dismissed, snoozed
  or resolved) while its push is `queued` (waiting for its first attempt or a retry), the push
  makes no more attempts. If no attempt was `accepted` or `uncertain`, no device has it: the
  item's push is suppressed (`closed`) and it needs no recovery. If one was, a device may have
  it: a resolved alert that isn't flapping or one-off gets its **recovery** push, which replaces
  that banner through the shared collapse id; anything else is suppressed (`closed`), and the
  iPhone app removes the notification when it next opens (`spec/clients.md`).
- A push the sender has in flight at that moment has no attempt recorded yet, so it is cancelled
  as above. When Apple's answer comes back it is recorded; if the push was the item's own,
  Apple took it or gave no clear answer, and the item is an alert that has stayed resolved with
  nothing newer queued, the recovery goes out then. One sender sends one push at a time, so the
  recovery always follows the push it replaces.
- A newer push about the same item (a repeat, a recovery, a reopening's push) cancels an older
  one still queued: they share a collapse id, and the older would only put stale text back. A
  reopening cancels a recovery still queued even when the reopening itself doesn't push.
- A recovery in flight when its alert reopens is cancelled the same way. If Apple took it or
  gave no clear answer and the reopening was suppressed `throttled` (nothing said then that a
  device had the recovery), the open alert is decided again (`reopened`), now unthrottled.
- The sender lists every push due and sends them one at a time, so it checks that each is still
  `queued` just before sending it: one cancelled while an earlier push was sending is skipped.
- A resolving alert also gets its recovery when its own push was never `sent` but a device has,
  or may have, an earlier firing push of the same item: one cancelled when the alert was
  dismissed, or one sent before a reopening that didn't push. What the phone shows is the
  item's latest push that a device has or may have.

## Alerts

An alert item is one incident of one `(kind, key)`. Its state is `open`, `dismissed` or
`resolved`; on top of that it may be **acknowledged** (repeats stop) and **flapping** (pushes
stop).

| From | Input | To | Effects |
|---|---|---|---|
| — (no item, or the last resolved `flap_window` or more ago) | fire | `open` | A new item. `openings = 1`. `seen = false`. Repeats scheduled from now. Decide (`opened`). |
| `resolved`, within `flap_window` of resolving | fire | `open` | The same item reopens: `times.opened` now, `times.resolved`, `acknowledged` and `dismissed` cleared, `seen = false`, text back to the firing text, repeats rescheduled. Count the openings within the window; more than `flap_openings` (3) → flapping (below). Decide (`reopened`). |
| `open`, `dismissed` | fire | unchanged | Facts, slots, text and labels updated. No push. When nothing it shows or stores would change (facts, slots, labels, rendered text and buttons, link, category, context), nothing is written: no history row, no new `version`, no copy to Trail; the answer is still `updated` with the item as it is. |
| `open`, `dismissed` | resolve (an event, or absent from its source's snapshot) | `resolved` | `times.resolved` now. Text becomes the recovery text, with `{lasted}` from `times.opened` to now. `seen = false`. A held push is suppressed (`closed`); a queued one stops ("Sending"). Repeats cancelled. If the item's push was `sent`, or a device may have a firing push of it ("Sending"), and it isn't flapping, a **recovery** push: the recovery text, same collapse id, `passive`, no category; the item's `push.status` follows it (`queued`, then `sent`). It is not subject to silences, quiet hours, presence or throttle; only to `push = false` (suppressed, `push_off`). |
| `open`, `dismissed` | its kind removed by a publish (`spec/api.md`, `PUT /v1/registry/{source}`) | `resolved` | `times.resolved` now, history `kind_removed`. It keeps its text and `seen`: nothing recovered. A held push is suppressed (`closed`); a queued one stops. The recovery push goes out on the same terms as above (its text is the item's, unchanged), so a banner a device has is replaced. |
| `resolved` | resolve | unchanged | Nothing (`outcome: "ignored"`). |
| `open`, `dismissed` | acknowledge | unchanged | `acknowledged` now. Repeats cancelled. A one-off alert resolves instead (no recovery text: it keeps its own). |
| `open` | dismiss | `dismissed` | Leaves Needs you; stays in Alerts. Repeats cancelled. A held push is suppressed (`closed`). A one-off alert resolves instead. |
| `open`, `dismissed` | silence_1h | unchanged | A silence opens. |

- **Flapping.** When a reopening makes the openings within `flap_window` more than
  `flap_openings`, the alert becomes flapping, and Beacon records one `beacon.flapping` update
  (key `<kind>/<key>`, facts `openings`, `window`, `stable`, slot `alert` = the alert's firing
  title), decided like any update. While flapping, the alert's own decisions are suppressed
  (`flapping`) and it sends no recovery push. It **settles** once it has neither opened nor resolved
  for `flap_stable` (15 m): flapping clears, and if it is open it is decided again (`settled`); if
  it is resolved nothing is sent.
- **Repeats.** While an alert is `open`, not acknowledged, not flapping, and its kind declares
  `repeat`, the next repeat is due `repeat[n]` after the previous push (the first after it opened).
  A due repeat is decided with trigger `repeat`; whether it sends or not, `repeats_sent` goes up and
  the next is scheduled, until the list runs out. A repeat that doesn't send (muted, skipped in
  quiet hours) leaves the item's `push` as it was: the last push it sent still stands. Becoming
  flapping cancels the next repeat; settling while open and unacknowledged schedules it again,
  `repeat[repeats_sent]` from then.
- **One-off** alerts (`one_off = true`) have no recovery: acknowledge or dismiss resolves them, as
  does an event with `state: "resolved"`.
- The plan's "pending" is not a state: an alert waiting to push is `open` with `push.status: held`.

## Actions

| From | Input | To | Effects |
|---|---|---|---|
| — (no item in `open` or `snoozed` for the `(kind, key)`) | open | `open` | A new item. `seen = false`. Decide (`opened`). |
| `open`, `snoozed` | open | unchanged | Facts, slots, text and labels updated. No push. Unchanged, nothing is written, as for alerts. |
| `open`, `snoozed` | done | `done` | `times.done`. A held push is suppressed (`closed`); a queued one stops ("Sending"). |
| `open`, `snoozed` | dismiss | `dismissed` | `times.dismissed`. The same. |
| `open` | snooze until T | `snoozed` | `times.snoozed_until = T`. The same. |
| `snoozed` | T passes | `open` | `seen = false`. Decide (`woke`). |
| `open`, `snoozed` | resolve (an event, or absent from its source's snapshot) | `resolved` | `times.resolved`. The same. `seen` unchanged: you didn't miss anything. |
| `open`, `snoozed` | its kind removed by a publish | `resolved` | The same, with history `kind_removed`. |
| `done`, `dismissed`, `resolved` | done, dismiss, snooze | unchanged | `200` when the effect already holds, else `409` (`spec/api.md`). |
| `open`, `snoozed` | a custom action without a url | `done` | Recorded in `acted`. |

A snapshot keeps an action you closed closed while it still lists it (`spec/api.md`,
`POST /v1/snapshots`). Producers send a key that names the thing (a reminder's id, an approval's
id): `(kind, key)` names one action, so a second event with the same key updates the first.

## Updates and records

Recorded once, never change state. Updates are decided (`opened`); records never push. Only
`seen` and `push` change afterwards.

## Seen and done

- **Seen** means you have seen the item in its current state. Clients post `POST /v1/seen`:
  - when the notification centre is on screen (web: the page is visible; iOS: the sheet is shown):
    `through` the newest item id, and again whenever a refresh brings newer ones;
  - when the in-app banner shows an item, and when you open an item from a push or a list;
  - when a client is showing a context (an assistant thread), for the items with that context.
  Counting items in Today's "Since you were last here" doesn't mark them seen.
- Seen goes back to false when the producer changes the item's state: an alert opens, reopens or
  resolves; a snoozed action wakes. Text updates don't change it.
- A held push of an item that is seen is suppressed. Seen never resolves anything.
- A seen posted with `"client": "web"` never suppresses an urgent push: the item reads as seen,
  but its held push still goes out unless the phone (or the API) marks it seen too.
- **Done** is an action you handled: `POST /v1/items/{id}/actions` with `done`. It differs from
  `dismissed` (you won't do it) and `resolved` (its source closed it). All three leave Needs you.
- When the iPhone app opens, it removes the delivered notifications of alerts and actions that are
  no longer open (`spec/clients.md`, "What the apps do").
- **The dot** shows while `needs_you` is non-empty: open alerts and open actions.

## Catch-up

At `quiet_hours.end` each day, for the items held with `quiet_hours`:

1. Each item that has been seen is suppressed (`seen`); each alert or action that left `open` is
   suppressed (`closed`).
2. None left: nothing. One left: it is decided again with trigger `caught_up` (presence applies: if
   you're active, it is held as usual).
3. Two or more: Beacon records one `beacon.catch_up` update with fact `count` and slot `titles`
   (their titles, oldest first, joined by `; `, cut to 150), and decides it (`opened`). Its push
   level is `normal` if any of the items was `urgent` or `normal`, else `silent`. Each item is
   suppressed (`catch_up`) with `push.covered_by` set to the catch-up's id.

There is no digest at other times: updates roll up on Today's page only.

## Sources

A source is **observed** until it says otherwise or goes quiet:

- A snapshot with `observed: false` and a `reason`, or no snapshot for `stale_after` after the last
  one (checked at least every 30 seconds), makes it **unobserved**. A source that has never posted
  a snapshot is never stale.
- Becoming unobserved, for a source that declares at least one alert kind (and for `trail`),
  opens one `beacon.source_stopped` alert (key = the source, fact `source_name`, slot `reason`;
  "It stopped reporting." when stale), push `normal`, decided like any alert (`opened`, or
  `reopened` within the flap window). Its source is `beacon`, which is always observed, so the rule
  it reports on never mutes it; a silence can.
- A source with no alert kind records one `beacon.source_unobserved` update instead (the same key,
  fact and slot). Its push is `none`; clients show it on their system page.
- While unobserved, the source's alerts don't push (`unobserved`), and nothing about them changes:
  not hearing from a source is not a recovery. Its actions and updates push as usual.
- A snapshot with `observed: true` makes it observed again. Its open `beacon.source_stopped`
  resolves ("Checking {source_name} again", with how long it lasted, and a recovery push if its
  push was sent); with none open, Beacon records `beacon.source_observed`. Each of its alerts
  still open whose push was suppressed `unobserved` is decided again (`unmuted`).
- Source `trail` follows the same rules through `POST /v1/trail`. Source `beacon` is always observed.

## The budget

Beacon counts the pushes sent (decided `send`) for actions and updates (not alerts) each local day. The send that
makes the count `delivery.budget + 1` (the 6th) records one `beacon.budget` update for that day:
fact `count`, fact `source_name` and slot `kind_title` naming the kind with the most such pushes
that day (on a tie, the one that pushed last) by its source's name and its latest item's title. Its
push is `none`. Nothing is suppressed by the budget: it is a signal to fix a noisy kind.

## Idempotency

- `POST /v1/events` with an `Idempotency-Key`: the same key and body within `idempotency` (24 h)
  answers the first answer again and changes nothing; the same key with another body is `409`.
  Keys are scoped to the kind's source.
- Trail notices are recorded once per notice `id`, for as long as their item is kept.
- Snapshots are idempotent by nature: posting the same one twice changes nothing the second time.
  An entry (or event) that would change nothing about an open item writes nothing: no history
  row, no `version`, so clients' `If-None-Match` still gets `304`.
- Item actions whose effect already holds answer `200` and change nothing; `POST /v1/seen` and
  presence are idempotent.
- A push retried after a lost answer from Apple may show twice; the collapse id makes the phone
  replace the first rather than add one.

## Background work

Everything that is owed is a time stored in SQLite with what caused it, so a restart picks it up:
held pushes (`release_at`), snoozes (`snoozed_until`), repeats (`next_repeat`), silence ends
(`until`), flap settling (the last opening or resolution), stale sources (`last_snapshot` +
`stale_after`), the catch-up (the next `quiet_hours.end`), retries (`spec/apns.md`) and retention
(daily). A job runs what is due at least once a second. Work that was due while Beacon was down
runs at start, in time order, except that a push more than `apns.give_up` (1 h) old is failed
(`expired`), not sent.

## Retention

Deleted with their history and push attempts, once a day: alerts and actions closed (resolved,
done, dismissed) more than `retention` (90 days) ago, and updates and records made more than
`retention` ago. Open items are kept however old. Silences that ended more than 7 days ago,
Idempotency-Keys older than `idempotency`, devices' presence older than a day, and budget counts
older than 7 days are deleted too.
