//! `beacon silence add|extend|end|list`: mute pushes for what a silence matches
//! (`spec/api.md`, "Silences").

use crate::args::Args;
use crate::http::Http;
use crate::send::{labels, read_time};
use crate::{Done, Fail, out};
use beacon_core::Problem;
use beacon_core::duration::Duration;
use serde_json::{Map, Value, json};

const HELP: &str = "write `beacon silence add|extend|end|list`; `beacon help` shows each";

pub fn silence(mut raw: Vec<String>, now_ms: i64) -> Done {
    if raw.is_empty() {
        return Err(Fail::usage(Problem::new(
            "beacon silence",
            "which silence command?",
            HELP,
        )));
    }
    let command = raw.remove(0);
    match command.as_str() {
        "add" => add(raw, now_ms),
        "extend" => extend(raw, now_ms),
        "end" => end(raw),
        "list" => list(raw),
        other => {
            let near = beacon_core::problem::closest(other, ["add", "extend", "end", "list"])
                .map(|c| format!("did you mean `beacon silence {c}`? "))
                .unwrap_or_default();
            Err(Fail::usage(Problem::new(
                "beacon silence",
                format!("`{other}` is not a silence command"),
                format!("{near}{HELP}"),
            )))
        }
    }
}

/// `--for D` or `--until TIME`, exactly one, as the body fields `for` or `until`.
fn when(at: &str, args: &Args, body: &mut Map<String, Value>, now_ms: i64) -> Result<(), Fail> {
    match (args.value("for"), args.value("until")) {
        (Some(_), Some(_)) | (None, None) => Err(Fail::usage(Problem::new(
            at,
            "a silence needs exactly one of --for and --until",
            "add --for 30m, or --until 2026-11-13T18:00:00+00:00",
        ))),
        (Some(length), None) => {
            Duration::parse(length).map_err(|message| {
                Fail::usage(Problem::new(
                    format!("{at} --for"),
                    message,
                    format!("write {}", Duration::FORMS),
                ))
            })?;
            body.insert("for".into(), json!(length));
            Ok(())
        }
        (None, Some(until)) => {
            let ms = read_time(until, now_ms).ok_or_else(|| {
                Fail::usage(Problem::new(
                    format!("{at} --until"),
                    format!("`{until}` doesn't read as a time"),
                    "write +2h, whole milliseconds since 1970, or 2026-11-13T18:00:00+00:00",
                ))
            })?;
            body.insert("until".into(), json!(ms));
            Ok(())
        }
    }
}

fn id(at: &str, args: &Args) -> Result<u64, Fail> {
    let text = &args.positional[0];
    text.parse().map_err(|_| {
        Fail::usage(Problem::new(
            at,
            format!("`{text}` is not a silence id"),
            "use the number `beacon silence add` printed, or one from `beacon silence list`",
        ))
    })
}

fn time(ms: Option<i64>) -> String {
    ms.and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map_or_else(
            || "-".to_string(),
            |t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
        )
}

/// One silence on a line: id, until, match, reason.
fn line(silence: &Value) -> String {
    let mut matched = Vec::new();
    if let Some(m) = silence.get("match") {
        for field in ["source", "kind", "key"] {
            if let Some(value) = m.get(field).and_then(Value::as_str) {
                matched.push(format!("{field}={value}"));
            }
        }
        if let Some(labels) = m.get("labels").and_then(Value::as_object) {
            for (name, value) in labels {
                matched.push(format!("label.{name}={}", value.as_str().unwrap_or_default()));
            }
        }
    }
    format!(
        "{}  until {}  {}  {}",
        silence.get("id").and_then(Value::as_i64).unwrap_or_default(),
        time(silence.get("until").and_then(Value::as_i64)),
        matched.join(" "),
        silence.get("reason").and_then(Value::as_str).unwrap_or_default()
    )
}

/// `beacon silence add [--source S] [--kind K [--key K]] [--label n=v]... (--for D | --until T) --reason R`
fn add(raw: Vec<String>, now_ms: i64) -> Done {
    let at = "beacon silence add";
    let args = Args::parse(
        "silence add",
        raw,
        &[
            "source", "kind", "key", "label", "for", "until", "reason", "server",
        ],
        &[],
    )?;
    args.at_most("silence add", 0)?;
    let mut matched = Map::new();
    for field in ["source", "kind", "key"] {
        if let Some(value) = args.value(field) {
            matched.insert(field.into(), json!(value));
        }
    }
    let labels = labels(at, &args)?;
    if !labels.is_empty() {
        matched.insert("labels".into(), Value::Object(labels));
    }
    if matched.is_empty() {
        return Err(Fail::usage(Problem::new(
            at,
            "a silence must match something",
            "add --source, --kind or --label, like --label service=dashboard",
        )));
    }
    if matched.contains_key("key") && !matched.contains_key("kind") {
        return Err(Fail::usage(Problem::new(
            format!("{at} --key"),
            "a key belongs to a kind",
            "add --kind <kind> beside --key",
        )));
    }
    let Some(reason) = args.value("reason") else {
        return Err(Fail::usage(Problem::new(
            at,
            "a silence needs a reason, shown beside it",
            "add --reason \"Deploying dashboard\"",
        )));
    };
    let mut body = Map::new();
    body.insert("match".into(), Value::Object(matched));
    when(at, &args, &mut body, now_ms)?;
    body.insert("reason".into(), json!(reason));
    body.insert("client".into(), json!("api"));
    let http = Http::new(args.server());
    let answer = http
        .request("POST", "/v1/silences", Some(&Value::Object(body)))?
        .into_result()?;
    out(&answer
        .json()
        .get("id")
        .and_then(Value::as_i64)
        .unwrap_or_default()
        .to_string());
    Ok(())
}

/// `beacon silence extend <id> (--for D | --until T)`
fn extend(raw: Vec<String>, now_ms: i64) -> Done {
    let at = "beacon silence extend";
    let args = Args::parse("silence extend", raw, &["for", "until", "server"], &[])?;
    args.need("silence extend", &["id"])?;
    args.at_most("silence extend", 1)?;
    let id = id(at, &args)?;
    let mut body = Map::new();
    when(at, &args, &mut body, now_ms)?;
    let http = Http::new(args.server());
    let answer = http
        .request("PATCH", &format!("/v1/silences/{id}"), Some(&Value::Object(body)))?
        .into_result()?;
    out(&line(&answer.json()));
    Ok(())
}

/// `beacon silence end <id>`
fn end(raw: Vec<String>) -> Done {
    let args = Args::parse("silence end", raw, &["server"], &[])?;
    args.need("silence end", &["id"])?;
    args.at_most("silence end", 1)?;
    let id = id("beacon silence end", &args)?;
    let http = Http::new(args.server());
    http.request("DELETE", &format!("/v1/silences/{id}"), None)?
        .into_result()?;
    out(&format!("ended silence {id}"));
    Ok(())
}

/// `beacon silence list [--all]`
fn list(raw: Vec<String>) -> Done {
    let args = Args::parse("silence list", raw, &["server"], &["all", "json"])?;
    args.at_most("silence list", 0)?;
    let http = Http::new(args.server());
    let path = if args.flag("all") {
        "/v1/silences?all=1"
    } else {
        "/v1/silences"
    };
    let answer = http.request("GET", path, None)?.into_result()?;
    if args.flag("json") {
        out(&answer.body);
        return Ok(());
    }
    let value = answer.json();
    let silences = value
        .get("silences")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if silences.is_empty() {
        out("no active silences");
    }
    for silence in &silences {
        out(&line(silence));
    }
    Ok(())
}
