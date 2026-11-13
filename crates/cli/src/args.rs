//! Reading a command line: positionals, `--option value` and `--flag`, checked against what the
//! command takes. An unknown option is refused with the one probably meant.

use crate::Fail;
use beacon_core::Problem;
use beacon_core::problem::closest;
use std::collections::{BTreeMap, BTreeSet};

/// The address Beacon listens on when nothing says otherwise (`spec/config.md`, `listen`).
pub const DEFAULT_URL: &str = "http://127.0.0.1:8300";

#[derive(Debug, Default)]
pub struct Args {
    pub positional: Vec<String>,
    values: BTreeMap<String, Vec<String>>,
    flags: BTreeSet<String>,
}

impl Args {
    /// Parses `raw` for `command`, which takes the `options` (with a value) and `flags` named.
    pub fn parse(command: &str, raw: Vec<String>, options: &[&str], flags: &[&str]) -> Result<Args, Fail> {
        let mut args = Args::default();
        let mut parser = lexopt::Parser::from_args(raw);
        let fail = |message: String, fix: String| {
            Fail::usage(Problem::new(format!("beacon {command}"), message, fix))
        };
        loop {
            let arg = match parser.next() {
                Ok(Some(arg)) => arg,
                Ok(None) => break,
                Err(e) => {
                    return Err(fail(
                        e.to_string(),
                        format!("`beacon help` shows how to write `{command}`"),
                    ));
                }
            };
            match arg {
                lexopt::Arg::Value(value) => args.positional.push(value.to_string_lossy().into_owned()),
                lexopt::Arg::Long(name) => {
                    let name = name.to_string();
                    if flags.contains(&name.as_str()) {
                        args.flags.insert(name);
                    } else if options.contains(&name.as_str()) {
                        let value = parser.value().map_err(|_| {
                            fail(
                                format!("`--{name}` needs a value"),
                                format!("write `--{name} <value>`"),
                            )
                        })?;
                        args.values
                            .entry(name)
                            .or_default()
                            .push(value.to_string_lossy().into_owned());
                    } else {
                        let known: Vec<&str> = options.iter().chain(flags).copied().collect();
                        let hint = closest(&name, known.iter().copied())
                            .map(|c| format!("did you mean `--{c}`? "))
                            .unwrap_or_default();
                        let listed = if known.is_empty() {
                            "none".to_string()
                        } else {
                            known
                                .iter()
                                .map(|k| format!("--{k}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        return Err(fail(
                            format!("`--{name}` is not an option of `beacon {command}`"),
                            format!("{hint}its options: {listed}"),
                        ));
                    }
                }
                lexopt::Arg::Short(letter) => {
                    return Err(fail(
                        format!("`-{letter}` is not an option"),
                        "options are written in full, like `--server`".to_string(),
                    ));
                }
            }
        }
        Ok(args)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).and_then(|v| v.last()).map(String::as_str)
    }

    pub fn values(&self, name: &str) -> &[String] {
        self.values.get(name).map_or(&[], Vec::as_slice)
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    /// Beacon's address: `--server`, else `BEACON_URL`, else loopback.
    pub fn server(&self) -> String {
        self.value("server")
            .map(str::to_string)
            .or_else(|| std::env::var("BEACON_URL").ok().filter(|v| !v.is_empty()))
            .unwrap_or_else(|| DEFAULT_URL.to_string())
            .trim_end_matches('/')
            .to_string()
    }

    /// Fails unless there are at least as many positionals as `names`, saying which is missing.
    pub fn need(&self, command: &str, names: &[&str]) -> Result<(), Fail> {
        if self.positional.len() >= names.len() {
            return Ok(());
        }
        let missing = names[self.positional.len()];
        let usage = names
            .iter()
            .map(|n| format!("<{n}>"))
            .collect::<Vec<_>>()
            .join(" ");
        Err(Fail::usage(Problem::new(
            format!("beacon {command}"),
            format!("missing <{missing}>"),
            format!("write `beacon {command} {usage}`"),
        )))
    }

    /// Fails when there are more positionals than `most`.
    pub fn at_most(&self, command: &str, most: usize) -> Result<(), Fail> {
        match self.positional.get(most) {
            None => Ok(()),
            Some(extra) => Err(Fail::usage(Problem::new(
                format!("beacon {command}"),
                format!("`{extra}` is one argument too many"),
                "quote a value that has spaces, and write options as `--name value`",
            ))),
        }
    }
}

/// Splits `name=value`.
pub fn pair<'a>(at: &str, item: &'a str, example: &str) -> Result<(&'a str, &'a str), Fail> {
    item.split_once('=').ok_or_else(|| {
        Fail::usage(Problem::new(
            at,
            format!("`{item}` is not name=value"),
            format!("write it as name=value, like {example}"),
        ))
    })
}
