//! The `beacon` command (`spec/clients.md`). `beacon help` lists what it does; each part lives in
//! its own module:
//!
//! - `args`: reading the command line, with the option you probably meant when one is unknown.
//! - `http`: one request to Beacon, retried like the emitters.
//! - `decls`: `check`, `gen`, `publish`.
//! - `send`: `send`, `resolve`, `snapshot`, checked against the kind before anything is sent.
//! - `silence`: `silence add`, `extend`, `end`, `list`.
//! - `serve`: runs the server.

mod args;
mod decls;
mod http;
mod send;
mod serve;
mod silence;

#[cfg(test)]
mod tests;

use beacon_core::Problem;
use beacon_core::problem::closest;
use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "\
beacon: decides which notifications deserve your attention, and delivers them.

Run the server
  beacon serve [--config beacon.toml]

Declare what a service sends (spec/declarations.md)
  beacon check <folder>...                  validate each folder's kinds.toml
  beacon gen <folder> --lang python|typescript [--out FILE] [--check]
                                            write typed emitters; --check fails if FILE is stale
  beacon publish <folder>... [--server URL] check, then load the declarations into Beacon

Send from a script (checked against the published kinds, or a local folder with --decls)
  beacon send <kind> [name=value]... [--key K] [--resolve] [--label name=value]...
              [--context C] [--open LINK] [--decls FOLDER] [--server URL] [--json]
  beacon resolve <kind> [--key K] [--context C] [--decls FOLDER] [--server URL] [--json]
  beacon snapshot <source> [--fire '<kind> [key=K] name=value...']... [--kinds K,K]
              [--stale-after D] [--unobserved REASON] [--label name=value]...
              [--decls FOLDER] [--server URL] [--json]

Silence pushes
  beacon silence add [--source S] [--kind K [--key K]] [--label name=value]...
              (--for D | --until TIME) --reason R [--server URL]
  beacon silence extend <id> (--for D | --until TIME) [--server URL]
  beacon silence end <id> [--server URL]
  beacon silence list [--all] [--server URL] [--json]

--server defaults to BEACON_URL, else http://127.0.0.1:8210; BEACON_TOKEN is sent as a bearer token.
Values: text for a string fact or slot (reason=\"\" sends an empty slot); a number for int and
float; 90s, 5m, 2h, 7d or whole milliseconds for a duration; now, -15m, +2h, milliseconds since
1970 or 2026-11-13T02:00:00+00:00 for a time.
Exit status: 0 done, 1 refused (each problem on stderr with its fix), 2 the command line is wrong,
3 Beacon didn't answer, or answered 5xx, after three attempts.";

const COMMANDS: [&str; 9] = [
    "serve", "check", "gen", "publish", "send", "resolve", "snapshot", "silence", "help",
];

/// Prints a line to stdout. A closed pipe (`beacon silence list | head`) is not an error.
pub(crate) fn out(text: &str) {
    let _ = writeln!(std::io::stdout(), "{text}");
}

/// Why a command failed, with the exit status that says so (`spec/clients.md`).
#[derive(Debug)]
pub(crate) struct Fail {
    pub code: u8,
    pub problems: Vec<Problem>,
}

impl Fail {
    /// Refused, by a local check or by Beacon: exit 1.
    pub fn refused(problems: Vec<Problem>) -> Fail {
        Fail { code: 1, problems }
    }

    /// The command line is wrong: exit 2.
    pub fn usage(problem: Problem) -> Fail {
        Fail {
            code: 2,
            problems: vec![problem],
        }
    }

    /// Beacon didn't answer, or answered 5xx, after the retries: exit 3.
    pub fn unreachable(problems: Vec<Problem>) -> Fail {
        Fail { code: 3, problems }
    }
}

/// What a command returns.
pub(crate) type Done = Result<(), Fail>;

/// Runs one command line (without the program name). `now_ms` is the clock for relative times.
pub(crate) fn run(mut raw: Vec<String>, now_ms: i64) -> Done {
    if raw.is_empty() {
        out(USAGE);
        return Err(Fail::usage(Problem::new(
            "beacon",
            "which command?",
            "`beacon help` lists them",
        )));
    }
    let command = raw.remove(0);
    match command.as_str() {
        "help" | "--help" | "-h" => {
            out(USAGE);
            Ok(())
        }
        "--version" | "version" => {
            out(env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "serve" => serve::serve(raw),
        "check" => decls::check(raw),
        "gen" => decls::generate(raw),
        "publish" => decls::publish(raw),
        "send" => send::send(raw, now_ms),
        "resolve" => send::resolve(raw, now_ms),
        "snapshot" => send::snapshot(raw, now_ms),
        "silence" => silence::silence(raw, now_ms),
        other => {
            let hint = closest(other, COMMANDS)
                .map(|c| format!("did you mean `beacon {c}`? "))
                .unwrap_or_default();
            Err(Fail::usage(Problem::new(
                "beacon",
                format!("`{other}` is not a command"),
                format!("{hint}`beacon help` lists them"),
            )))
        }
    }
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    match run(raw, now_ms) {
        Ok(()) => ExitCode::SUCCESS,
        Err(fail) => {
            for problem in &fail.problems {
                eprintln!("{problem}");
            }
            ExitCode::from(fail.code)
        }
    }
}
