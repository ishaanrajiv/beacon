# Beacon

One small server that decides which notifications deserve your attention, and delivers them to
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

Status: pre-0.1. The formats in `spec/` will change.

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
