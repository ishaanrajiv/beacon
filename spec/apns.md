# Apple push

How Beacon talks to Apple's push service (APNs) for the one iPhone app it serves. The code will be
`crates/apns`, the only crate allowed `reqwest` (blocking, HTTP/2, rustls), `ring` and `base64`.
Device tokens are secrets: they never appear in logs, errors, history or API answers beyond their
last six characters.

## Configuration

From `[apns]` in `beacon.toml` (`spec/config.md`):

| Field | |
|---|---|
| `key_path` | The `.p8` signing key Apple issued (PKCS#8 PEM, P-256). Read once at start. |
| `key_id` | The key's 10-character id. Default: taken from the file name `AuthKey_<key id>.p8`. |
| `team_id` | The 10-character Apple team id: the token's issuer. |
| `topic` | The app's bundle id, like `com.example.lab`. |

With no key, Beacon still decides and records everything; each push fails with `unconfigured` and
`GET /v1/health` says `"apns": "unconfigured"`.

## Provider tokens

Token-based auth: a JWT signed with the key, sent as `authorization: bearer <jwt>`.

- Header `{"alg": "ES256", "kid": "<key_id>"}`, claims `{"iss": "<team_id>", "iat": <unix seconds>}`.
- Each part base64url without padding; the signature is ES256 over `<header>.<claims>`, the raw 64
  bytes `r || s` (ring's `ECDSA_P256_SHA256_FIXED_SIGNING`), base64url without padding.
- Reused for 40 minutes, then signed again. Apple refuses tokens older than an hour and throttles
  ones refreshed more often than every 20 minutes.
- An answer of `403` with reason `ExpiredProviderToken` or `InvalidProviderToken` drops the cached
  token; the request is retried once at once with a new one, and that retry doesn't count as an
  attempt.

## Connection

- HTTP/2 over TLS to `https://api.push.apple.com` (`production`) or
  `https://api.sandbox.push.apple.com` (`sandbox`), port 443, path `/3/device/<token>`. A device's
  environment is the one it registered with (`POST /v1/devices`).
- One client, kept open and reused; timeout 15 s per request.
- Sends happen on one worker thread, one request at a time: Beacon pushes a few times a day.

## A notification

Headers:

| Header | Value |
|---|---|
| `apns-push-type` | `alert` |
| `apns-topic` | `topic` |
| `apns-priority` | `10`, or `5` for a passive push |
| `apns-expiration` | now + 1 hour, in unix seconds: Apple stops trying after that |
| `apns-collapse-id` | the item's `collapse_id` (at most 64 bytes; Beacon's are 19 or fewer) |

Body:

```json
{
  "aps": {
    "alert": {"title": "Backup failed", "body": "The disk is full. The last good backup was 02:00."},
    "interruption-level": "time-sensitive",
    "sound": "default",
    "thread-id": "backup",
    "category": "backup.failed",
    "relevance-score": 1.0
  },
  "url": "/system",
  "data": {"item": "4182", "kind": "backup.failed", "key": "nightly", "collapse_id": "b1-29fd692930019189"}
}
```

| Field | Rule |
|---|---|
| `aps.alert.title`, `.body` | The item's rendered title and body. |
| `aps.interruption-level` | `time-sensitive` (urgent), `active` (normal), `passive` (silent). Urgent never asks for critical alerts: the app has no entitlement. |
| `aps.sound` | `"default"`, left out for `passive`. |
| `aps.thread-id` | The item's source. |
| `aps.category` | The item's kind. Left out of a recovery push, so a recovered alert shows no buttons. |
| `aps.relevance-score` | Alert `1.0`, action `0.8`, update `0.5`, recovery `0.3`. |
| `url` | The item's link, or left out when it has none. |
| `data` | String values only: `item` (the id), `kind`, `key`, `collapse_id`. |

No `badge` (the app shows a dot in its own UI, never an icon badge), no `mutable-content` (no
Notification Service Extension), no `content-available`.

**4 KB.** The body is at most 4096 bytes. If it is over: the body text is cut first, by bytes at a
character boundary, ending in `…`, repeating while JSON escaping keeps it over; then `url` is
dropped; then the title is cut to 40 characters. `category`, `thread-id` and `data` are never cut.

### Categories

`aps.category` is the kind's name, so the app must have registered a category of that name to show
its buttons. The app fetches `GET /v1/kinds` at launch (and when its `ETag` changes) and registers
one `UNNotificationCategory` per kind:

- identifier: the kind's name;
- actions: the kind's built-in actions, in its declared order, each identified by its action id
  (`done`, `acknowledge`, `snooze`, `dismiss`, `silence_1h`) with its label. Custom actions are not
  notification buttons;
- `hiddenPreviewsBodyPlaceholder`: the kind's `preview`, shown when previews are hidden on a
  locked phone.

When a button is pressed the app posts `POST /v1/items/{data.item}/actions` with that action id
and `"client": "ios"`. Tapping the notification opens `url` and posts `POST /v1/seen` for the item.

## Results and retries

Each attempt has one of three results, recorded in the item's `attempts`:

| Result | When |
|---|---|
| `accepted` | `200` |
| `rejected` | Apple answered with an error |
| `uncertain` | no answer: a timeout, a dropped connection, a TLS error. Apple may or may not have it. |

Apple's error `reason` is taken from the JSON body when it is letters only (at most 64); otherwise
the reason is `HTTP <status>`.

| Answer | Then |
|---|---|
| `200` | Taken for this device. |
| `410` (`Unregistered`), or `400` with `BadDeviceToken` or `DeviceTokenNotForTopic` | A dead token (below). |
| `403` with `ExpiredProviderToken` or `InvalidProviderToken` | New provider token, retry at once (see above). |
| `429`, any `5xx`, or `uncertain` | Retry: after 10 s, then 30 s, then 2 min, then 5 min. Five attempts at most. |
| anything else (`400` `PayloadTooLarge`, `BadTopic`, `413`…) | Rejected for good. |

- **No replay after an hour.** No attempt starts more than `apns.give_up` (1 h) after the push was
  queued: past that it is failed with `expired`. A push that was in flight when Beacon stopped is
  `uncertain` and retried at start, within the same hour.
- A push is sent to each registered device separately. The item is `sent` when any device took it,
  `failed` (`rejected` or `expired`) when every device has finished without taking it, and
  `failed` with `no_devices` when none is registered.

### Dead tokens

`BadDeviceToken` can mean the token belongs to the other environment (a debug build registered as
production, or the reverse). So:

1. On `BadDeviceToken`, try the same push once in the other environment. If that is taken, the
   device's environment is changed to it.
2. If that also fails as dead, or the first answer was `410` or `Unregistered` or
   `DeviceTokenNotForTopic`, the device is deleted. The app registers again at its next launch.

## Live Activities

`POST /v1/live-activity` (`spec/api.md`) sends at once, outside the queue: no retries, no history.

| Header | Value |
|---|---|
| `apns-push-type` | `liveactivity` |
| `apns-topic` | `<topic>.push-type.liveactivity` |
| `apns-priority` | the request's `priority` (`5` or `10`, default `10`) |
| `apns-expiration` | now + 1 hour |

Body: `{"aps": {"timestamp", "event", "content-state", "dismissal-date"?, "stale-date"?, "alert"?}}`,
from the request's `timestamp`, `event`, `contentState`, `dismissalDate`, `staleDate` and `alert`;
`alert` gains `"sound": "default"`. Over 4 KB is refused (`400`), never trimmed. The environment
is the request's, else that of the most recently updated device, then the other on
`BadDeviceToken`. A dead-token answer is `410` to the caller and deletes nothing (the token is the
activity's, not a device's).
