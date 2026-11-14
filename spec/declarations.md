# Declarations

Every notification Beacon shows is an instance of a **kind** that a service declared ahead of
time. The kind fixes the notification's class, how it may push, and its copy. A producer sends
facts and slots; Beacon writes the words. A service keeps its declarations in one file,
`kinds.toml`, in a folder beside its code (by convention `<service>/beacon/kinds.toml`).

- `beacon check <folder>` validates it, `beacon gen` writes typed emitters from it
  (`spec/clients.md`), and `beacon publish` loads it into the server (below).
- The parser and every check are in `crates/core/src/decl.rs`; rendering is in
  `crates/core/src/render.rs`. `examples/homelab-ish/*/beacon/kinds.toml` are complete examples.

## The file

```toml
source = "backup"          # required: the service's name
name = "Backups"           # required: what people see as the source's name
icon = "archive"           # optional, default "bell": one of the icons below
service = "backup"         # optional, default the source: the label `service` on every item

[kinds."backup.failed"]    # one table per kind
class = "alert"
push = "urgent"
description = "The last backup run failed."
title = "Backup failed"
body = "{reason}. The last good backup was {last_ok}."
facts = { last_ok = "time" }
slots = { reason = 80 }
open = "/system"
actions = ["acknowledge", "silence_1h"]
preview = "Backup problem"
recovered = "Backups are working again"
repeat = ["2h", "6h"]
```

### Top level

| Field | Type | Required | Rule |
|---|---|---|---|
| `source` | string | yes | Lowercase letters, digits and hyphens, starting with a letter, at most 32 characters. `beacon` is reserved. `trail` adds kinds for Trail's rules ("Kinds for Trail's rules"). |
| `name` | string | yes | 1 to 24 characters. Shown beside every item from this source. |
| `icon` | string | no, `bell` | One of the icon names below. |
| `service` | string | no, the source | The service this source's items are about: lowercase letters and digits, words joined by `_` or `-`, starting with a letter, at most 64 characters (`backup`, `mail-worker`). Every item of the source gets it as the label `service` unless the event or snapshot entry set that label itself, so a deploy's silence matching `labels.service` (`spec/api.md`, "Silences") mutes everything the service sends. For `trail` it is ignored: each Trail item's `service` is Trail's `service` label, else Trail's `source`. |
| `kinds` | table | yes, at least one | `[kinds."<source>.<name>"]` tables. |

Any other top-level field is an error, with the name you probably meant.

### Icons

A source picks one. The iPhone app draws the SF Symbol; the web client draws its own icon for
each name. The set is closed: a new icon is a change to Beacon (`crates/core/src/model.rs`, `Icon`).

| Name | SF Symbol | For |
|---|---|---|
| `archive` | `archivebox` | backups, storage |
| `bell` | `bell` | the default; Beacon itself |
| `book` | `book` | documents, reading, a library |
| `calendar` | `calendar` | reminders, schedules |
| `code` | `chevron.left.forwardslash.chevron.right` | code, pull requests, coding agents |
| `cpu` | `cpu` | hardware |
| `gauge` | `gauge.with.dots.needle.33percent` | usage limits, quotas, monitoring |
| `globe` | `globe` | network, internet |
| `house` | `house` | home devices |
| `key` | `key` | sign-ins, credentials |
| `server` | `server.rack` | apps and services |
| `ship` | `shippingbox` | deploys, releases |
| `shield` | `checkmark.shield` | security |
| `sparkles` | `sparkles` | an assistant |
| `thermometer` | `thermometer.medium` | temperatures |

### A kind

The table's name is the kind's name: the source, a dot, then one or more snake_case parts joined
by dots (`backup.failed`, `assistant.task.result`), at most 96 characters. Its source part must
equal the file's `source`.

| Field | Type | Default | Rule |
|---|---|---|---|
| `class` | `"alert"` \| `"action"` \| `"update"` \| `"record"` | required | See "Classes". |
| `push` | `"urgent"` \| `"normal"` \| `"silent"` \| `"none"` | required, except `record` (`"none"`) | `urgent` only on `alert`. `record` is always `none`. `alert` is never `none`. |
| `description` | string | `""` | At most 200 characters. One sentence; becomes the generated emitter's doc comment. |
| `title` | template | required | See "Copy". Placeholders: facts and slots. |
| `body` | template | `""` | See "Copy". Placeholders: facts and slots. |
| `facts` | table of name → type | `{}` | Typed values the producer sends. See "Facts". |
| `slots` | table of name → integer | `{}` | Model-written text, each with its most characters (1 to 1000). See "Slots". |
| `open` | template | none | Where tapping the item goes. Starts with `/` (a page of your app, resolved by the client against its own base) or `https://`. At most 512 characters. Placeholders: facts only. |
| `actions` | list | `[]` | Buttons. See "Actions". |
| `time_bound` | bool | `false` | `action` and `update` only. It passes quiet hours: reminders, results of tasks you scheduled. |
| `repeat` | list of durations | `[]` | `alert` only. See "Repeats". |
| `throttle` | duration, or `"none"` | `"10m"` | At most one push per kind and key per this long; at least `1m`. `"none"` never throttles. |
| `preview` | string | the title, when the title has no placeholders | Required when `push` isn't `none` and the title has placeholders. 1 to 40 characters, no placeholders. What a locked phone shows when previews are hidden. |
| `recovered` | template | see "Recovery" | `alert` only, not `one_off`. The title once the alert recovers. Placeholders: facts, slots, `{lasted}`. |
| `recovered_body` | template | `"Lasted {lasted}."` on a recovering alert | `alert` only, not `one_off`. The body once it recovers. |
| `context` | bool | `false` | Not on `record`. Events of this kind carry a viewing context (`spec/api.md`, `POST /v1/events`), like `assistant:thread:42`, and aren't pushed while a client is viewing it. |
| `one_off` | bool | `false` | `alert` only. Nothing will report this alert's recovery (a failed deploy): acknowledging or dismissing it resolves it. It must offer `acknowledge` or `dismiss`, and has no `recovered` and no `repeat`. |
| `categories` | list of strings | `[]` | Not on `record`. At most 4, each `^[A-Z][A-Z0-9_]{0,39}$`, no repeats. Notification categories the app registered itself (approval buttons, say): the first is the `aps.category` of this kind's pushes instead of the kind's name, and an event may pick another of them with its `category` field (`spec/api.md`, `POST /v1/events`). Empty: the category is the kind's name (`spec/apns.md`, "Categories"). |

Durations are a whole number and a unit: `90s`, `10m`, `2h`, `7d`. Any other field in a kind is an
error, with the name you probably meant.

## Classes

Beacon sorts notifications by what you have to do, not by how alarming they sound.

| Class | Meaning | Lives in | Lifecycle (`spec/delivery.md`) |
|---|---|---|---|
| `alert` | Something is broken | Needs you while open, and Alerts | open → resolved; can flap, repeat, be acknowledged or dismissed |
| `action` | Needs your decision | Needs you until done, dismissed, snoozed or resolved | open → done / dismissed / snoozed / resolved |
| `update` | Worth knowing | Activity, and Today's "Since you were last here" | none: one-off |
| `record` | Audit trail only | Activity, behind a filter | none: one-off, never pushed |

## Push levels

| Level | On the phone | Allowed on |
|---|---|---|
| `urgent` | Time Sensitive: breaks through Focus, passes quiet hours | `alert` |
| `normal` | A banner with sound | `alert`, `action`, `update` |
| `silent` | Passive: no sound, no lit screen, lands in the notification list | `alert`, `action`, `update` |
| `none` | Never pushed | `action`, `update`, `record` |

Guidance for whoever declares a kind: default to `update` with `push = "silent"`. Use `alert` and
`urgent` only for something you'd want to be woken for at 3am, and `action` only for something that
needs a decision.

## Facts

A fact is a short typed value: a name, a number, a moment. Written `name = "type"`, or
`name = "type?"` for an optional fact the producer may leave out (it renders as nothing).

| Type | JSON the producer sends | Rendered as |
|---|---|---|
| `string` | a string of at most 200 characters | the text, cleaned (below) |
| `int` | a whole number | its digits: `196` |
| `float` | a number | one decimal under 100, none from 100: `95.1`, `150`; a trailing `.0` dropped |
| `duration` | whole milliseconds, 0 or more | `under a minute`, `4 min`, `2 h`, `1 h 5 min`, `1 day`, `3 days` |
| `time` | milliseconds since 1970 UTC | in the server's timezone, beside now: `16:45` today, `yesterday 16:45`, `tomorrow 09:00`, `Tue 16:45` within six days either way, else `12 Nov`, or `12 Nov 2025` in another year |

Rules: a fact's name is snake_case, starting with a letter, at most 32 characters. It may not be
`lasted`, nor `key`, `labels`, `url`, `context` or `category` (the generated emitters' own arguments), nor contain `password`, `passwd`, `secret`, `token`, `otp`, `api_key`, `apikey`,
`authorization`, `cookie` or `credential` (notifications are kept for months and shown on a lock
screen). Every declared fact must appear in some template (`title`, `body`, `open`, `recovered`,
`recovered_body`, or a custom action's `url`).

A time is rendered relative to the moment the event is rendered (when it arrives), and the text is
stored: it does not change later.

## Slots

A slot is text someone (often a model) wrote: an assistant's question, a task's summary, the reason a
backup failed. Written `name = <most characters>`. The producer must send every slot (`""` when
there is nothing to say). A longer value is not refused: it is cut at a word boundary and ends in
`…`, within the declared length (`crates/core/src/template.rs`, `truncate_words`). A first word
longer than half the limit is cut mid-word.

Slot names follow the fact rules, may not repeat a fact's name, and must appear in a template.
A slot may not appear in `open` or a custom action's `url`.

## Templates

`title`, `body`, `open`, `recovered`, `recovered_body` and a custom action's `url` are templates:
text with `{name}` placeholders. `{{` and `}}` are a literal brace. A `{` without its `}`, an empty
`{}`, a name that isn't snake_case, or a `}` that closes nothing is an error.

Each placeholder must be a declared fact or slot of the kind. `{lasted}` (how long the alert was
open, rendered as a duration) is available in `recovered` and `recovered_body` only.

### Rendering

`crates/core/src/render.rs`:

1. Each value is cleaned: control characters and invisible layout characters (zero-width marks,
   bidirectional overrides) are removed, and every run of whitespace, newlines included, becomes
   one space. Values are plain text: clients show titles and bodies as text, never as HTML or
   Markdown.
2. Facts are formatted by type (above); slots are cut to their length.
3. The template is filled in, then cleaned again (so an empty value leaves no double space).
4. The title is cut to 40 characters and the body to 150, at a word boundary with `…`. Validation
   (below) makes this rare: it bounds the fixed text and the slots, and only fact values can make
   a title run long.
5. In `open` and a custom action's `url`, each value is percent-encoded (everything but
   `A–Z a–z 0–9 - . _ ~`).

## Copy

Beacon writes the text from the kind. The rules, and which are checked at publish:

- **Title = thing + state**, 40 characters or fewer, no ending punctuation: "Backup failed",
  "Dashboard is down", "PR #196 is ready".
- **Body = impact or cause, then the next step or a time**, 150 characters or fewer, written to you.
- **Never:** the app's name, source ids, commit SHAs, shell commands, ISO timestamps, live values
  in titles.
- **Recovery** is the same row and the same banner: "<thing> is back", with how long it lasted.
- **Hidden-preview text** is fixed per kind ("Approval needed").

Checked on a published kind (each a `Problem` with a fix):

| Check | Applies to |
|---|---|
| Fixed text plus every slot's length is at most 40 | `title`, `recovered` |
| Fixed text plus every slot's length is at most 150 | `body`, `recovered_body` |
| Doesn't end with `.` `!` `?` `;` `,` | `title`, `recovered` |
| Has no `:` (no "Cleared:" or "Backup:" prefix) | `title`, `recovered` |
| Starts with a capital letter, when it starts with fixed text | `title`, `recovered` |
| Has fixed words of its own, not only placeholders | `title`, `recovered` (Beacon's own kinds are exempt) |
| No `float`, `duration` or `time` fact, and no `{lasted}`: a live value doesn't belong in a title | `title`, `recovered` |
| No backticks; no `sudo `, `systemctl `, `$ `, `&& `, `\| grep` | every text field's fixed text, and `preview` |
| No ISO date (`2026-11-13`) | the same |
| No run of 7 or more hex characters with both digits and letters (a commit SHA) | the same |
| No word from `banned_words` in `beacon.toml` (`spec/config.md`), whole words, ignoring case | the same. Use it for the app's own name and the machine's name. |

Values are checked at the event (`spec/api.md`), not here: a fact can't hold more than 200
characters, and slots are cut to fit.

## Actions

`actions` lists the buttons an item shows, in order, at most 4. Each is one of Beacon's own, or a
custom table.

| Action | Label | On | What it does (`spec/delivery.md`) |
|---|---|---|---|
| `done` | Done | `action` | Closes the item: you handled it. |
| `acknowledge` | Acknowledge | `alert` | "I'm on it": stops repeats. A one-off alert resolves. |
| `snooze` | Snooze | `action` | Hides the item until a time (default 1 hour), then it opens and pushes again. |
| `dismiss` | Dismiss | `alert`, `action` | An action closes; a firing alert leaves Needs you but stays in Alerts until it resolves. A one-off alert resolves. |
| `silence_1h` | Silence 1h | `alert`, `action`, `update` | Opens a silence for one hour matching this item's source, kind and key (`spec/api.md`, "Silences"). |

A custom action is `{ id = "review", label = "Review", url = "/code/pulls/{number}/review" }`:

| Field | Rule |
|---|---|
| `id` | snake_case, at most 32 characters, not a built-in action's name nor `silence` |
| `label` | 1 to 20 characters: a short verb ("Review", "Sign in") |
| `url` | optional link template: starts with `/` or `https://`, facts only |

With a `url` the client opens the link and tells Beacon it was pressed, which records it on the
item and changes nothing else. Without one, pressing it is a choice Beacon records; on an `action`
item it also marks the item done. Beacon never calls a producer back: a producer that cares reads
the item (`GET /v1/items/{id}`, `acted`). Custom actions appear in the web and iOS apps, not as
buttons on a notification (iOS needs every notification button registered by the app ahead of time).

A `record` has no actions. Each action may be listed once.

## Repeats

`repeat = ["30m", "2h", "6h"]` re-pushes an open alert that nobody has acknowledged: 30 minutes
after it opened, then 2 hours after that, then 6 hours after that, then never again. Each interval
is at least `5m` and at least as long as the one before; at most 8. Only an `alert` with push
`urgent` or `normal` that offers `acknowledge` and isn't `one_off` may repeat.

## Recovery

An alert that isn't `one_off` recovers: its row and its banner change to the recovery text.

- `recovered` defaults from the title when the title is "<thing> is <state>" or
  "<things> are <state>": "Dashboard is down" recovers as "Dashboard is back", "Backups are
  overdue" as "Backups are back". Any other title needs `recovered` declared, or `one_off = true`.
- `recovered_body` defaults to `"Lasted {lasted}."`.
- Both may use the alert's latest facts and slots, and `{lasted}`.

## Beacon's own kinds

`crates/core/builtin/beacon.toml` and `crates/core/builtin/trail.toml` are always loaded, with the
same parser and checks, except that their sources are reserved and their titles may be a slot on
its own. A service can't publish source `beacon`; source `trail` can only be added to (below).
`spec/delivery.md` says when each is posted.

| Kind | Class | Push | Title | When |
|---|---|---|---|---|
| `beacon.source_stopped` | alert | normal | "Can't check {source_name}" | A source that declares an alert kind (or `trail`) posted `observed: false`, or its snapshots went stale: its alerts are on hold. Key: the source. Body: "{reason} Its alerts are on hold until it reports again." Recovers as "Checking {source_name} again", "Its alerts are back on. Lasted {lasted}." when the source is observed again. Actions `acknowledge`, `silence_1h`. Its source is `beacon`, which is always observed, so the rule it reports on never mutes it; a silence can. |
| `beacon.source_unobserved` | update | none | "Can't check {source_name}" | A source with no alert kind posted `observed: false`, or its snapshots went stale. Shown on the client's system page. |
| `beacon.source_observed` | record | none | "Checking {source_name} again" | An unobserved source with no open `beacon.source_stopped` reported again. |
| `beacon.flapping` | update | normal | "An alert keeps flapping" | An alert opened more than 3 times in 30 minutes. Body: "{alert} opened {openings} times in {window}. It stays quiet until it settles for {stable}." |
| `beacon.catch_up` | update | normal | "{count} things came in overnight" | At the end of quiet hours, standing in for two or more held items. Body: their titles. |
| `beacon.budget` | update | none | "More pushes than usual today" | The 6th non-alert push of a day. Body: "{count} so far. Most were {kind_title} from {source_name}." |
| `trail.severe` | alert | urgent | Trail's title | A severe Trail metric rule is firing. Recovers as "Back to normal", "{title} · lasted {lasted}". |
| `trail.severe_notice` | alert, one-off | urgent | Trail's title | A severe Trail event rule matched. |
| `trail.warning` | update | silent | Trail's title | A warning rule started firing, or matched an event. |
| `trail.info` | record | none | Trail's title | An info rule started firing, or matched an event. |

Trail's kinds carry Trail's title and body in slots `title` (40) and `body` (150). Their link is
`[trail] open` in `beacon.toml`.

### Kinds for Trail's rules

The four built-in Trail kinds give every severe rule one push level and Trail's free text. To give
a rule its own class, push level, copy, facts, actions and repeats, declare a kind for it in a
folder with `source = "trail"` and name it in the rule's `kind` (`spec/api.md`, `POST /v1/trail`).
`examples/homelab-ish/trail/beacon/kinds.toml` declares an urgent and a normal alert.

- Its kinds are named `trail.<name>`, may not take a built-in's name (`trail.severe`,
  `trail.severe_notice`, `trail.warning`, `trail.info`), and are checked like any other kind, copy
  rules included (a title needs words of its own). `beacon check`, `beacon gen` and
  `beacon publish` take the folder; `PUT /v1/registry/trail` stores it.
- The built-in kinds are always there as well. `GET /v1/registry/trail` and `GET /v1/kinds` list
  both; `DELETE /v1/registry/trail` removes only the published ones.
- `name` and `icon` are required by the file format and **ignored**: Trail's items keep Beacon's
  ("Monitoring", `gauge`).
- A kind without `open` gets `[trail] open`, like the built-in ones.
- Facts are filled by name from Trail's post: `value` and `since` from a firing entry's `value`
  (a number) and `since` (a time), `alert` from Trail's `alert`, `trail_source` from Trail's
  `source`, and any other name from the entry's `labels`, or for a notice from its event's
  `props` (a notice has no `value` or `since` of its own, so those come from `props` too). Slots
  `title` and `body` take Trail's title and body; any other slot is read like a fact, as text.
  Declare a fact optional (`"string?"`) when a rule may not send it.
- A metric rule (a snapshot entry) names an alert, action, update or record that isn't a one-off
  alert; an event rule (a notice) names a one-off alert, an update or a record. Anything else
  falls back to the severity's built-in kind.

## Reaching the server

Like Trail, declarations live in the service's repository and are published to the server, which
keeps them:

1. `beacon publish <folder>…` reads each folder's `kinds.toml`, runs every check locally, and sends
   each source with `PUT /v1/registry/{source}`. The body is the declarations as JSON: the same
   fields as the TOML with every default filled in (below).
2. The server checks them again (with its `banned_words`), stores them in SQLite in one
   transaction, and answers `200`. A source's new declarations replace its old ones entirely.
   What it stores is the JSON form below stamped with `"format": 1`, so a later version can tell
   what wrote it; each item keeps the kind it was made with the same way.
3. An event or snapshot entry of a kind the registry doesn't have is refused with `422`, and the
   fix names the closest declared kind and says to declare and publish it.
4. Items already stored keep the text, class and push they were given. Republishing never
   rewrites history. An alert or action still open whose kind a publish removed can never be
   resolved by its producer (events for that kind are refused), so the publish closes it: it
   resolves with history step `kind_removed` and keeps its text (`spec/api.md`,
   `PUT /v1/registry/{source}`).

A deploy pipeline publishes in two steps, so neither the old code still running nor the new code
about to start can send a kind the server doesn't know: `beacon publish --keep` before it restarts
the service (the new kinds are added, and kinds the folder no longer has stay), then a plain
`beacon publish` once the new code runs (the source's declarations become exactly the folder's).

### Reading back what was stored

The checks run when declarations are published, and stay as strict as above. Reading back what
Beacon itself stored (at start, after each publish, and each item's kind) is tolerant, so a
rollback, or an upgrade that changed a field, never takes a source down:

- A field this version doesn't know is ignored, at every level (the source, a kind, a custom
  action), and so is `format`.
- A missing field takes its default (the table above; `name` defaults to the source, `icon` to
  `bell`, `push` to `none` on a record). Stored declarations aren't checked again.
- A kind that still can't be read (no `class` or `title`, a value this version can't read) is kept
  out of the registry, and so is a source that can't be read at all (not an object, no `source`).
  Neither is dropped silently: the source is listed by `GET /v1/registry` with a `problem`
  (`spec/api.md`), `GET /v1/health` counts it in `unreadable_sources`, the server says so once on
  stderr at start, and an event, snapshot or snapshot `kinds` entry naming what can't be read is
  refused (`422`) with a fix that says to publish the source again. Its other kinds work as usual.
- An item whose stored kind can't be read still loads: it keeps its text, push level and buttons,
  and is said once on stderr.

### The JSON form

`GET /v1/registry/{source}` returns, and `PUT /v1/registry/{source}` takes, this shape. Every
field is present when Beacon writes it. When reading it, Beacon fills a missing field with its
default, except that a missing `throttle` means `"10m"` and `null` means `"none"`.

```json
{
  "source": "backup",
  "name": "Backups",
  "icon": "archive",
  "service": "backup",
  "kinds": {
    "backup.failed": {
      "class": "alert",
      "push": "urgent",
      "description": "The last backup run failed.",
      "title": "Backup failed",
      "body": "{reason}. The last good backup was {last_ok}.",
      "facts": {"last_ok": "time"},
      "slots": {"reason": 80},
      "open": "/system",
      "actions": ["acknowledge", "silence_1h"],
      "time_bound": false,
      "repeat": ["2h", "6h"],
      "throttle": "10m",
      "preview": "Backup problem",
      "recovered": "Backups are working again",
      "recovered_body": "Lasted {lasted}.",
      "context": false,
      "one_off": false,
      "categories": []
    }
  }
}
```

A custom action is an object in `actions`: `{"id": "review", "label": "Review", "url": "/code/pulls/{number}/review"}`
(`url` null or absent when it has none). `open`, `preview`, `recovered` and `recovered_body` are
null when the kind has none. `beacon publish` leaves `service` out when it is the source's name
(missing means the source), so a server from before `service` takes the body; the declarations
hash of a generated emitter (`spec/clients.md`) is over that same JSON. Durations are written in the largest unit that divides them (`"2h"`,
not `"120m"`).
