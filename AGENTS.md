# Working on Beacon

Beacon is one small Rust server that decides which notifications deserve your attention and
delivers them to your own iPhone app through Apple's push service. It is maintained by coding
agents, so its rules are checks that fail with a message saying what to do. Run `scripts/check`
before every commit.

## This repository is public

Anyone can read everything here: the code, every commit in the history, commit messages, branch
names, pull requests and their comments. Write as if a stranger reads it, because one will.

- **Never commit a secret**, even for a moment: keys, tokens, passwords, certificates, `.env`
  files, Apple signing keys (`.p8`) or real team ids, bundle ids and device tokens. Removing it
  in a later commit does not help; the history keeps it. If one is pushed, say so, and treat it
  as leaked: it has to be revoked and replaced, not just deleted.
- **Nothing about the machine Beacon runs on, or its owner.** No real hostnames, IP addresses,
  domains, usernames, home or data paths (`/home/…`, `/srv/…`), names of other services or
  projects, or people. Use `example.com`, loopback addresses and made-up names instead.
- **No real data.** No databases, logs, journal lines, exports or screenshots from a running
  Beacon. Tests and examples use data made up for them.
- **The deployment lives elsewhere.** Config, secrets and anything specific to where Beacon is run
  belong in that deployment's own (private) repository, not here. If a task needs something
  private to be written down, stop and ask rather than putting it here.
- **CI's secret scan is a backstop, not a check you can rely on.** It looks for things shaped like
  keys; it can't tell a real hostname or a personal detail from an example.

## Layout

| Path | What it is |
|---|---|
| `crates/core` | Types and pure functions: classes, push levels, declarations (`kinds.toml`), templates and rendering, Beacon's own kinds, the alert and action lifecycles, the delivery decision, quiet hours. No sockets, no database. |
| `crates/store` | SQLite: items, alerts, silences, presence, the registry, push attempts in `beacon.db`; devices and their tokens in `devices.db`. Each method applies a core transition and records its decision in one transaction; `crates/store/README.md` maps routes and jobs to methods. |
| `crates/server` | The HTTP/1.1 server on `std::net` (`src/httpd.rs`, shared with Trail), the API routes, the background jobs. |
| `crates/apns` | Apple push: provider tokens, HTTP/2, payloads, retries. The only crate with reqwest, ring and base64. |
| `crates/codegen` | `beacon gen`: typed emitters for Python and TypeScript. Each generated file is a client below plus one function per kind. |
| `crates/cli` | The `beacon` binary: check, gen, publish, send, resolve, snapshot, silence; `serve` once the server lands. |
| `clients/python`, `clients/node` | The stdlib clients every generated file copies between its `runtime` markers, with their suites against a fake Beacon. `tests/gen/` holds files generated from `examples/`. |
| `crates/core/builtin/` | The kinds Beacon owns: `beacon.*` and what Trail's webhook becomes, `trail.*`. |
| `spec/` | The contracts: declarations, API, delivery, Apple push, config, clients. |
| `examples/homelab-ish/` | Made-up services' `kinds.toml` files; the tests load them. |

## Rules

- **The spec and the code change together.** A change to a wire format, a file format, a default
  number or a config field updates its file in `spec/` in the same commit. The spec is what other
  agents build clients against; a behaviour it doesn't describe doesn't exist.
- **No new dependencies.** `Cargo.toml` lists every crate Beacon may use, and `scripts/check` fails
  on any other, and on reqwest, ring or base64 anywhere but `crates/apns`. There is no async
  runtime outside `crates/apns`, no SQL engine but SQLite, and no logging framework, on purpose.
- **Every error says how to fix it.** Return a `Problem { at, message, fix }`
  (`crates/core/src/problem.rs`). Name the field, say what is wrong with the value, and give a value
  that would work. Use `closest` to suggest the name the writer probably meant. The API sends the
  same three fields (`spec/api.md`, "Errors").
- **No `unwrap`, `expect`, `panic!` or `unsafe`** outside tests; the lints deny them. Return the error.
- **Declared, not free text.** Every notification is an instance of a kind a service declared.
  Producers send facts and slots; Beacon writes the words from the kind's templates. Don't add a
  route that takes a finished title.
- **Nothing is trusted to memory alone.** Work that is owed (a held push, a repeat, the morning
  catch-up, a retry) survives a kill at any line: it is in SQLite in the same transaction as what
  caused it, or found again at start from what is stored.
- **Time comes in, never from inside.** Functions that decide anything take the clock as an
  argument, so every case is tested with a fake clock.
- **Behaviour has a test.** A bug fix starts with the test that fails. Every validation message
  has a test that triggers it.
- **Nothing private in the repository.** It is public: see "This repository is public" above.
  Examples use made-up services (`backup`, `assistant`, `ship`),
  `example.com` and loopback addresses.
- **The transport is shared.** `crates/server/src/httpd.rs` (reading requests under limits and
  timeouts, the connection cap, the slots, writing answers) is kept byte for byte the same as
  Trail's copy, and a check outside both repositories fails when they differ. Change it in both
  in the same piece of work. It names neither server and uses only `std` (and `serde_json` in its
  tests); what is Beacon's own (the error body, which requests take a slot, the numbers) goes in
  through `httpd::Config` and `httpd::Handler`, from `lib.rs` and `api.rs`.
- **Heavy jobs are capped.** Build with `CARGO_BUILD_JOBS=4` and `nice`: the machine this is
  written on runs other services. `scripts/check` sets the cap.

## Commands

```sh
scripts/check                 # format, lint, tests, allowed dependencies, nothing private, client suites, release build, binary size, idle memory
scripts/check --fast          # Rust only: no client suites, no pyright/tsc over generated code, no release build
cargo test -p beacon-core     # declarations, templates, rendering, the decision, the lifecycles
cargo test -p beacon-store    # the store against SQLite, with a fake clock
cargo test -p beacon-server   # the API over HTTP with a fake clock and a fake Apple
cargo test -p beacon-apns     # provider tokens, payloads, retries, against a fake transport
cargo test -p beacon-codegen  # generated code; runs pyright and tsc through npx when it is on PATH
BEACON_BLESS=1 cargo test -p beacon-codegen   # rewrite clients/*/tests/gen after changing a client or the generator
```

`beacon serve --config beacon.toml` runs `beacon_server::run` (`crates/cli/src/serve.rs`). The
server's tests (`cargo test -p beacon-server`) start it on an ephemeral port with a fake clock and a
fake Apple (`crates/server/tests/http.rs`); nothing in the tests calls Apple.
