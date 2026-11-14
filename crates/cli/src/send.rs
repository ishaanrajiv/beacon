//! `beacon send`, `beacon resolve` and `beacon snapshot`: an event or a snapshot from a shell
//! script, checked against the kind's declaration before anything is sent.

use crate::args::{Args, pair};
use crate::http::{Answer, Http};
use crate::{Done, Fail, out};
use beacon_core::Problem;
use beacon_core::decl::{self, Check, Declarations, FactBase, Kind};
use beacon_core::duration::Duration;
use beacon_core::names::source_of;
use beacon_core::problem::closest;
use serde_json::{Map, Value, json};
use std::path::Path;

/// Where a command learns the kinds: a local folder (`--decls`), else the server's registry.
pub struct Kinds<'a> {
    folders: &'a [String],
    http: &'a Http,
}

impl<'a> Kinds<'a> {
    pub fn new(args: &'a Args, http: &'a Http) -> Kinds<'a> {
        Kinds {
            folders: args.values("decls"),
            http,
        }
    }

    /// The declarations of `source`.
    pub fn source(&self, source: &str) -> Result<Declarations, Fail> {
        if !self.folders.is_empty() {
            let mut found = Vec::new();
            for folder in self.folders {
                let decl = decl::load(Path::new(folder), &Check::default()).map_err(Fail::refused)?;
                if decl.source == source {
                    return Ok(decl);
                }
                found.push(decl.source);
            }
            let hint = closest(source, found.iter().map(String::as_str))
                .map(|c| format!("did you mean `{c}`? "))
                .unwrap_or_default();
            return Err(Fail::refused(vec![Problem::new(
                "--decls",
                format!(
                    "no folder given declares source `{source}` (they declare {})",
                    found.join(", ")
                ),
                format!("{hint}add the folder of `{source}`'s kinds.toml with --decls"),
            )]));
        }
        let answer = self
            .http
            .request("GET", &format!("/v1/registry/{source}"), None)?;
        if answer.status == 404 {
            return Err(Fail::refused(vec![Problem::new(
                format!("source {source}"),
                format!("Beacon has no declarations for `{source}`"),
                format!(
                    "run `beacon publish <folder>` for `{source}`, or check the kind's name; --decls <folder> checks against a local kinds.toml instead"
                ),
            )]));
        }
        let answer = answer.into_result()?;
        // The server checked them when they were published; read them as tolerantly as it reads
        // its own, so a newer server's fields don't stop the command.
        Declarations::from_stored(&answer.json())
            .map(|stored| stored.decl)
            .map_err(|why| {
                Fail::refused(vec![Problem::new(
                    format!("source {source}"),
                    format!("Beacon's declarations for `{source}` can't be read: {why}"),
                    "check --server points at Beacon, or pass --decls <folder> to check against a local kinds.toml",
                )])
            })
    }
}

/// The kind named `name` in `decl`, or the problem that names the closest one.
pub fn find<'d>(decl: &'d Declarations, name: &str, at: &str) -> Result<&'d Kind, Fail> {
    decl.kinds.get(name).ok_or_else(|| {
        let near = closest(name, decl.kinds.keys().map(String::as_str))
            .map(|c| format!("did you mean `{c}`? "))
            .unwrap_or_default();
        Fail::refused(vec![Problem::new(
            at,
            format!("`{name}` is not a declared kind"),
            format!("{near}Declare a new kind in kinds.toml and run `beacon publish`"),
        )])
    })
}

/// How a value of each type is written, for fixes.
fn example(name: &str, base: FactBase) -> String {
    match base {
        FactBase::String => format!("{name}=\"some text\""),
        FactBase::Int => format!("{name}=42"),
        FactBase::Float => format!("{name}=95.1"),
        FactBase::Duration => format!("{name}=5m (or 90s, 2h, 7d, or whole milliseconds)"),
        FactBase::Time => {
            format!("{name}=now (or -15m, +2h, whole milliseconds since 1970, or 2026-11-13T02:00:00+00:00)")
        }
    }
}

fn whole_ms(text: &str) -> Option<i64> {
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

/// Reads a time: whole milliseconds, `now`, `-15m` / `+2h` from now, or RFC 3339 with an offset.
pub fn read_time(text: &str, now_ms: i64) -> Option<i64> {
    if text == "now" {
        return Some(now_ms);
    }
    if let Some(ms) = whole_ms(text) {
        return Some(ms);
    }
    if let Some(ago) = text.strip_prefix('-') {
        return Duration::parse(ago).ok().map(|d| now_ms - d.ms());
    }
    if let Some(ahead) = text.strip_prefix('+') {
        return Duration::parse(ahead).ok().map(|d| now_ms + d.ms());
    }
    text.parse::<jiff::Timestamp>().ok().map(|t| t.as_millisecond())
}

/// Reads one fact's value as its type.
fn read_fact(base: FactBase, text: &str, now_ms: i64) -> Option<Value> {
    match base {
        FactBase::String => Some(Value::String(text.to_string())),
        FactBase::Int => text.parse::<i64>().ok().map(Value::from),
        FactBase::Float => text
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number),
        FactBase::Duration => whole_ms(text)
            .or_else(|| Duration::parse(text).ok().map(Duration::ms))
            .map(Value::from),
        FactBase::Time => read_time(text, now_ms).map(Value::from),
    }
}

/// Facts and slots read from `name=value` words, checked against the kind.
pub struct Values {
    pub facts: Map<String, Value>,
    pub slots: Map<String, Value>,
}

/// Reads `items` for `kind`. With `partial` (a resolve), nothing is required.
pub fn read_values(
    at: &str,
    kind_name: &str,
    kind: &Kind,
    items: &[&str],
    partial: bool,
    now_ms: i64,
) -> Result<Values, Fail> {
    let mut values = Values {
        facts: Map::new(),
        slots: Map::new(),
    };
    let mut problems = Vec::new();
    // Names given, read or not: one that didn't read is reported once, not also as missing.
    let mut given = std::collections::BTreeSet::new();
    for item in items {
        let (name, text) = pair(at, item, "reason=\"The disk is full\"")?;
        given.insert(name);
        if let Some(fact) = kind.facts.get(name) {
            match read_fact(fact.base, text, now_ms) {
                Some(Value::String(s)) if s.chars().count() > decl::FACT_STRING_MAX => {
                    problems.push(Problem::new(
                        format!("{at} {name}"),
                        format!(
                            "this is {} characters; a fact is a short value of at most {}",
                            s.chars().count(),
                            decl::FACT_STRING_MAX
                        ),
                        "shorten it; longer text belongs in a slot",
                    ))
                }
                Some(value) => {
                    values.facts.insert(name.to_string(), value);
                }
                None => problems.push(Problem::new(
                    format!("{at} {name}"),
                    format!(
                        "`{text}` doesn't read as {} {} fact",
                        if fact.base == FactBase::Int { "an" } else { "a" },
                        fact.base.name()
                    ),
                    format!("write {}", example(name, fact.base)),
                )),
            }
        } else if kind.slots.contains_key(name) {
            values
                .slots
                .insert(name.to_string(), Value::String(text.to_string()));
        } else {
            let names: Vec<&str> = kind
                .facts
                .keys()
                .chain(kind.slots.keys())
                .map(String::as_str)
                .collect();
            problems.push(Problem::new(
                format!("{at} {name}"),
                format!("`{kind_name}` has no fact or slot `{name}`"),
                match closest(name, names.iter().copied()) {
                    Some(near) => format!("did you mean `{near}`?"),
                    None if names.is_empty() => "it takes no values; leave them out".to_string(),
                    None => format!("it takes {}", names.join(", ")),
                },
            ));
        }
    }
    if !(partial && given.is_empty()) {
        for (name, fact) in &kind.facts {
            if !fact.optional && !given.contains(name.as_str()) {
                problems.push(Problem::new(
                    format!("{at} {name}"),
                    format!("`{kind_name}` needs the fact `{name}` ({})", fact.base.name()),
                    format!("add {}", example(name, fact.base)),
                ));
            }
        }
        for name in kind.slots.keys() {
            if !given.contains(name.as_str()) {
                problems.push(Problem::new(
                    format!("{at} {name}"),
                    format!("`{kind_name}` needs the slot `{name}`"),
                    format!("add {name}=\"…\"; {name}=\"\" sends an empty slot"),
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(values)
    } else {
        Err(Fail::refused(problems))
    }
}

/// `--label name=value`, repeated.
pub fn labels(at: &str, args: &Args) -> Result<Map<String, Value>, Fail> {
    let mut labels = Map::new();
    for item in args.values("label") {
        let (name, value) = pair(at, item, "--label service=backup")?;
        labels.insert(name.to_string(), Value::String(value.to_string()));
    }
    Ok(labels)
}

/// Checks `--context` against the kind: required when it declares `context = true`, refused otherwise.
fn check_context(at: &str, kind_name: &str, kind: &Kind, context: Option<&str>) -> Result<(), Fail> {
    match (kind.context, context) {
        (true, None) => Err(Fail::refused(vec![Problem::new(
            at,
            format!("`{kind_name}` needs a viewing context"),
            "add --context <what it is about>, like --context assistant:thread:42",
        )])),
        (false, Some(_)) => Err(Fail::refused(vec![Problem::new(
            at,
            format!("`{kind_name}` doesn't declare `context = true`"),
            "leave out --context, or declare `context = true` on the kind and run `beacon publish`",
        )])),
        _ => Ok(()),
    }
}

fn print_answer(args: &Args, kind: &str, answer: &Answer) {
    if args.flag("json") {
        out(&answer.body);
        return;
    }
    let value = answer.json();
    let outcome = value.get("outcome").and_then(Value::as_str).unwrap_or("sent");
    match value.pointer("/item/id").and_then(Value::as_i64) {
        Some(id) => out(&format!("{outcome}  {kind}  item {id}")),
        None => out(&format!("{outcome}  {kind}")),
    }
}

/// `beacon send <kind> [name=value]... [--key K] [--resolve] [--label n=v]... [--context C] [--open LINK]`
pub fn send(raw: Vec<String>, now_ms: i64) -> Done {
    let args = Args::parse(
        "send",
        raw,
        &["key", "label", "context", "open", "decls", "server"],
        &["resolve", "json"],
    )?;
    args.need("send", &["kind"])?;
    event(&args, "send", args.flag("resolve"), now_ms)
}

/// `beacon resolve <kind> [--key K] [--context C]`
pub fn resolve(raw: Vec<String>, now_ms: i64) -> Done {
    let args = Args::parse("resolve", raw, &["key", "context", "decls", "server"], &["json"])?;
    args.need("resolve", &["kind"])?;
    args.at_most("resolve", 1)?;
    event(&args, "resolve", true, now_ms)
}

fn event(args: &Args, command: &str, resolve: bool, now_ms: i64) -> Done {
    let kind_name = args.positional[0].as_str();
    let at = format!("beacon {command} {kind_name}");
    let http = Http::new(args.server());
    let decl = Kinds::new(args, &http).source(source_of(kind_name))?;
    let kind = find(&decl, kind_name, &at)?;
    if resolve && !kind.class.has_lifecycle() {
        return Err(Fail::refused(vec![Problem::new(
            &at,
            format!(
                "`{kind_name}` is an {}, which has no state to resolve",
                kind.class.as_str()
            ),
            "send it without --resolve: each one is recorded on its own",
        )]));
    }
    let items: Vec<&str> = args.positional[1..].iter().map(String::as_str).collect();
    let values = read_values(&at, kind_name, kind, &items, resolve, now_ms)?;
    let context = args.value("context");
    check_context(&at, kind_name, kind, context)?;

    let mut body = Map::new();
    body.insert("kind".into(), json!(kind_name));
    body.insert("key".into(), json!(args.value("key").unwrap_or("")));
    if !values.facts.is_empty() || !resolve {
        body.insert("facts".into(), Value::Object(values.facts));
    }
    if !values.slots.is_empty() || !resolve {
        body.insert("slots".into(), Value::Object(values.slots));
    }
    let labels = labels(&at, args)?;
    if !labels.is_empty() {
        body.insert("labels".into(), Value::Object(labels));
    }
    if let Some(context) = context {
        body.insert("context".into(), json!(context));
    }
    if let Some(open) = args.value("open") {
        body.insert("url".into(), json!(open));
    }
    if resolve {
        body.insert("state".into(), json!("resolved"));
    }
    let answer = http
        .request("POST", "/v1/events", Some(&Value::Object(body)))?
        .into_result()?;
    print_answer(args, kind_name, &answer);
    Ok(())
}

/// Splits a `--fire` value into words: spaces separate, and '…' or "…" keep spaces in one word.
pub fn words(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if let Some(q) = quote {
        return Err(format!("a {q} is not closed"));
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

/// `beacon snapshot <source> [--fire '<kind> [key=K] name=value...']... [--kinds K,K] [--stale-after D] [--unobserved REASON]`
pub fn snapshot(raw: Vec<String>, now_ms: i64) -> Done {
    let args = Args::parse(
        "snapshot",
        raw,
        &[
            "fire",
            "kinds",
            "stale-after",
            "unobserved",
            "label",
            "decls",
            "server",
        ],
        &["json"],
    )?;
    args.need("snapshot", &["source"])?;
    args.at_most("snapshot", 1)?;
    let source = args.positional[0].as_str();
    let at = format!("beacon snapshot {source}");
    let http = Http::new(args.server());

    let mut body = Map::new();
    body.insert("source".into(), json!(source));
    if let Some(stale) = args.value("stale-after") {
        Duration::parse(stale).map_err(|message| {
            Fail::usage(Problem::new(
                format!("{at} --stale-after"),
                message,
                format!("write {}, from 1m to 7d", Duration::FORMS),
            ))
        })?;
        body.insert("stale_after".into(), json!(stale));
    }
    body.insert("sent_at".into(), json!(now_ms));

    if let Some(reason) = args.value("unobserved") {
        if !args.values("fire").is_empty() {
            return Err(Fail::usage(Problem::new(
                &at,
                "--unobserved and --fire don't go together: an unobserved source says nothing about what is firing",
                "leave out --fire, or --unobserved",
            )));
        }
        body.insert("observed".into(), json!(false));
        body.insert("reason".into(), json!(reason));
    } else {
        let decl = Kinds::new(&args, &http).source(source)?;
        let labels = labels(&at, &args)?;
        let mut firing = Vec::new();
        let mut problems = Vec::new();
        for (i, fire) in args.values("fire").iter().enumerate() {
            let here = format!("{at} --fire[{i}]");
            match entry(&here, &decl, fire, &labels, now_ms) {
                Ok(entry) => firing.push(entry),
                Err(fail) => problems.extend(fail.problems),
            }
        }
        if let Some(kinds) = args.value("kinds") {
            let mut list = Vec::new();
            for name in kinds.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                match find(&decl, name, &format!("{at} --kinds")) {
                    Ok(kind) if kind.class.has_lifecycle() => list.push(json!(name)),
                    Ok(kind) => problems.push(Problem::new(
                        format!("{at} --kinds"),
                        format!(
                            "`{name}` is an {}; a snapshot covers alerts and actions",
                            kind.class.as_str()
                        ),
                        "leave it out of --kinds",
                    )),
                    Err(fail) => problems.extend(fail.problems),
                }
            }
            body.insert("kinds".into(), Value::Array(list));
        }
        if !problems.is_empty() {
            return Err(Fail::refused(problems));
        }
        body.insert("observed".into(), json!(true));
        body.insert("firing".into(), Value::Array(firing));
    }

    let answer = http
        .request("POST", "/v1/snapshots", Some(&Value::Object(body)))?
        .into_result()?;
    if args.flag("json") {
        out(&answer.body);
    } else {
        let value = answer.json();
        let count = |field: &str| value.get(field).and_then(Value::as_array).map_or(0, Vec::len);
        out(&format!(
            "{source}: {} opened, {} reopened, {} updated, {} resolved",
            count("opened"),
            count("reopened"),
            count("updated"),
            count("resolved")
        ));
    }
    Ok(())
}

/// One `--fire` entry: `<kind> [key=K] [context=C] [url=U] name=value...`.
fn entry(
    at: &str,
    decl: &Declarations,
    fire: &str,
    labels: &Map<String, Value>,
    now_ms: i64,
) -> Result<Value, Fail> {
    let words = words(fire).map_err(|message| {
        Fail::usage(Problem::new(
            at,
            message,
            "close the quote: --fire 'backup.failed reason=\"The disk is full\"'",
        ))
    })?;
    let Some((kind_name, rest)) = words.split_first() else {
        return Err(Fail::usage(Problem::new(
            at,
            "an empty --fire",
            "write --fire '<kind> name=value…', like --fire 'backup.space used_percent=96'",
        )));
    };
    let kind = find(decl, kind_name, at)?;
    if !kind.class.has_lifecycle() {
        return Err(Fail::refused(vec![Problem::new(
            at,
            format!(
                "`{kind_name}` is an {}; a snapshot lists alerts and actions",
                kind.class.as_str()
            ),
            format!("send it with `beacon send {kind_name}` instead"),
        )]));
    }
    let mut entry = Map::new();
    entry.insert("kind".into(), json!(kind_name));
    entry.insert("key".into(), json!(""));
    let mut items = Vec::new();
    let mut context = None;
    for word in rest {
        match word.split_once('=') {
            Some(("key", key)) => {
                entry.insert("key".into(), json!(key));
            }
            Some(("context", value)) => context = Some(value),
            Some(("url", value)) => {
                entry.insert("url".into(), json!(value));
            }
            _ => items.push(word.as_str()),
        }
    }
    check_context(at, kind_name, kind, context)?;
    if let Some(context) = context {
        entry.insert("context".into(), json!(context));
    }
    let values = read_values(at, kind_name, kind, &items, false, now_ms)?;
    entry.insert("facts".into(), Value::Object(values.facts));
    entry.insert("slots".into(), Value::Object(values.slots));
    if !labels.is_empty() {
        entry.insert("labels".into(), Value::Object(labels.clone()));
    }
    Ok(Value::Object(entry))
}
