# Beacon

A self-hosted notification server that decides which events deserve a push, and sends them to
your own iPhone app through Apple's push service. It is for one person running their own
services, where most events are worth recording and few are worth a sound.

- **Sorted by what you have to do.** Every notification belongs to a declared kind with a class
  (alert, action, update, record) that fixes where it shows and how it may push.
- **Quiet while you're looking.** Clients report presence; a push is held while you're active and
  dropped if you saw it.
- **Alerts with a lifecycle.** Pending, firing, resolved, with flap detection, silences, and a
  recovery that replaces its own banner.
- **Declared, then checked.** Services declare their kinds in TOML; Beacon generates typed emitters,
  so an undeclared kind is a type error.

> **Alpha, a work in progress.** Beacon is built mainly for my own use, on my own machines. Expect
> things to change or break between commits: the formats in `spec/`, the config, the API and the
> database can all change without a migration. Pin a commit if you depend on it.

## No iPhone app included

Beacon delivers to an iPhone app, but this repository doesn't contain one. Mine lives elsewhere
and isn't published. To receive pushes you need your own app: it registers its device token with
`POST /v1/devices`, reports presence, and shows what `GET /v1` serves ([`spec/api.md`](spec/api.md),
[`spec/apns.md`](spec/apns.md)). Sending through Apple's push service also needs a paid Apple
Developer account, for the `.p8` signing key, team id and bundle id below. Without them Beacon
still records and serves notifications; it just doesn't push.

## Running it

```sh
beacon serve --config beacon.toml     # every field optional; with none it listens on 127.0.0.1:8210
```

A minimal `beacon.toml` that pushes:

```toml
data_dir = "/var/lib/beacon"
[apns]
key_path = "/etc/beacon/AuthKey_ABC123DEFG.p8"
team_id = "TEAM123456"
topic = "com.example.lab"
```

`spec/config.md` has every field; `spec/api.md` every route.

## Security

Beacon has one user and trusts whoever can reach its port. Anyone who can reach it can read every
notification, and a same-origin request can register a device, mark items seen or silence alerts
([`spec/api.md`](spec/api.md), "Access"). Only producing (sending events, declaring kinds) is
limited, to the same machine or a holder of `auth.producer_token`. There is no TLS and no login.
Keep it on loopback, behind a reverse proxy on a private network you trust such as a VPN, and
don't expose it to the internet. It has had no outside security review.

## License

MIT or Apache-2.0, at your option.
