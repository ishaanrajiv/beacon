//! The routes (`spec/api.md`): find the route, check its access rule, read and check the request,
//! call the store, send what it returns. The store owns every decision; this layer owns the
//! wire: JSON bodies, unknown fields with the name you probably meant, and status codes.

use crate::App;
use crate::http::{Handler, Reply, Request};
use crate::push;
use crate::routes::{Access, ROUTES, matches};
use beacon_core::Problem;
use beacon_core::builtin;
use beacon_core::decl::{Action, Check, Declarations};
use beacon_core::delivery::SilenceMatch;
use beacon_core::duration::Duration;
use beacon_core::model::Class;
use beacon_core::problem::closest;
use beacon_store::{Event, Heartbeat, InboxQuery, NewSilence, SeenTarget, Snapshot, SnapshotEntry};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) struct Api {
    pub app: Arc<App>,
}

impl Handler for Api {
    fn body_limit(&self) -> usize {
        self.app.config.body_bytes
    }

    fn handle(&self, request: Request) -> Reply {
        let reply = route(&self.app, &request);
        if request.method != "GET" && reply.status < 300 {
            self.app.wake();
        }
        reply
    }
}

type Answer = Result<Reply, Reply>;

/// Finds the route and runs it.
fn route(app: &App, req: &Request) -> Reply {
    let path = if req.path.len() > 1 {
        req.path.trim_end_matches('/')
    } else {
        req.path.as_str()
    };
    let mut allowed = Vec::new();
    for route in ROUTES {
        let Some(parts) = matches(route.path, path) else {
            continue;
        };
        if route.method != req.method {
            allowed.push(route.method);
            continue;
        }
        if let Err(reply) = access(app, req, route.access) {
            return reply;
        }
        let answer = run(app, req, route.method, route.path, &parts);
        return answer.unwrap_or_else(|reply| reply);
    }
    if !allowed.is_empty() {
        return Reply::problem(
            405,
            "request method",
            format!("{} {path} is not a route", req.method),
            format!("{path} takes {}", allowed.join(", ")),
        )
        .with("Allow", allowed.join(", "));
    }
    let paths: Vec<&str> = ROUTES.iter().map(|r| r.path).collect();
    let fix = match closest(path, paths.iter().copied()) {
        Some(near) => format!("did you mean {near}? GET /v1 lists every route"),
        None => "GET /v1 lists every route".to_string(),
    };
    Reply::problem(404, "request path", format!("no route {path}"), fix)
}

fn run(app: &App, req: &Request, method: &str, pattern: &str, parts: &[String]) -> Answer {
    let part = |i: usize| parts.get(i).map_or("", String::as_str);
    match (method, pattern) {
        ("GET", "/" | "/v1") => Ok(Reply::ok(json!({"version": crate::VERSION, "routes": ROUTES}))),
        ("GET", "/v1/health") => Ok(health(app)),
        ("GET", "/v1/registry") => {
            let sources = app.store.sources().map_err(store_error)?;
            Ok(Reply::ok(json!({"sources": sources})))
        }
        ("GET", "/v1/registry/{source}") => {
            let source = part(0);
            match app.store.declarations(source).map_err(store_error)? {
                Some(decl) => Ok(Reply::ok(decl.to_json())),
                None => Err(Reply::problem(
                    404,
                    "registry source",
                    format!("nobody published source `{source}`"),
                    "GET /v1/registry lists the sources; `beacon publish` adds one",
                )),
            }
        }
        ("PUT", "/v1/registry/{source}") => publish(app, req, part(0)),
        ("DELETE", "/v1/registry/{source}") => {
            if app.store.unpublish(part(0)).map_err(store_error)? {
                Ok(Reply::ok(json!({"ok": true})))
            } else {
                Err(Reply::problem(
                    404,
                    "registry source",
                    format!("nobody published source `{}`", part(0)),
                    "GET /v1/registry lists the sources",
                ))
            }
        }
        ("GET", "/v1/kinds") => kinds(app, req),
        ("POST", "/v1/events") => events(app, req),
        ("POST", "/v1/snapshots") => snapshots(app, req),
        ("POST", "/v1/trail") => trail(app, req),
        ("POST", "/v1/presence") => presence(app, req),
        ("GET", "/v1/inbox") => inbox(app, req),
        ("GET", "/v1/items/{id}") => {
            let id = id(part(0), "item")?;
            match app.store.item_detail(id, app.now()).map_err(store_error)? {
                Some(detail) => Ok(Reply::ok(to_json(&detail))),
                None => Err(no_item(id)),
            }
        }
        ("POST", "/v1/items/{id}/actions") => press(app, req, part(0)),
        ("POST", "/v1/seen") => seen(app, req),
        ("GET", "/v1/silences") => {
            let all = matches!(req.param("all"), Some("1" | "true"));
            let silences = app.store.silences(all, app.now()).map_err(store_error)?;
            Ok(Reply::ok(json!({"silences": silences})))
        }
        ("POST", "/v1/silences") => open_silence(app, req),
        ("GET", "/v1/silences/{id}") => {
            let id = id(part(0), "silence")?;
            match app.store.silence(id, app.now()).map_err(store_error)? {
                Some(view) => Ok(Reply::ok(to_json(&view))),
                None => Err(no_silence(id)),
            }
        }
        ("PATCH", "/v1/silences/{id}") => change_silence(app, req, part(0)),
        ("DELETE", "/v1/silences/{id}") => {
            let id = id(part(0), "silence")?;
            let view = app.store.end_silence(id, app.now()).map_err(store_error)?;
            Ok(Reply::ok(to_json(&view)))
        }
        ("GET", "/v1/devices") => {
            let devices = app.store.devices().map_err(store_error)?;
            Ok(Reply::ok(json!({"devices": devices})))
        }
        ("POST", "/v1/devices") => register(app, req),
        ("DELETE", "/v1/devices/{token}") => {
            if app.store.forget_device(part(0), app.now()).map_err(store_error)? {
                Ok(Reply::ok(json!({"ok": true})))
            } else {
                Err(Reply::problem(
                    404,
                    "device",
                    "no device has that token",
                    "GET /v1/devices lists them; the app registers again at its next launch",
                ))
            }
        }
        ("POST", "/v1/live-activity") => push::live_activity(app, req),
        _ => Err(Reply::problem(
            404,
            "request path",
            "no such route",
            "GET /v1 lists every route",
        )),
    }
}

// ---------- access ----------

/// A request made straight to Beacon from this machine: a loopback peer and no proxy headers.
fn is_local(req: &Request) -> bool {
    req.peer.ip().is_loopback()
        && req.header("x-forwarded-for").is_none()
        && req.header("forwarded").is_none()
}

/// The body is JSON, or the method has none (a DELETE). Browsers can't send JSON across origins
/// without asking first, which Beacon never allows.
fn json_typed(req: &Request) -> bool {
    req.method == "DELETE"
        || req.header("content-type").is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
        })
}

fn same_origin(app: &App, req: &Request) -> bool {
    let cross = req
        .header("sec-fetch-site")
        .is_some_and(|v| v.eq_ignore_ascii_case("cross-site"));
    let origin_ok = match req.header("origin") {
        None => true,
        Some(origin) => {
            let host = req.header("host").unwrap_or_default();
            (!host.is_empty() && (origin == format!("http://{host}") || origin == format!("https://{host}")))
                || app.config.origins.iter().any(|o| o == origin)
        }
    };
    json_typed(req) && !cross && origin_ok
}

fn bearer_ok(app: &App, req: &Request) -> bool {
    let Some(token) = &app.config.producer_token else {
        return false;
    };
    let Some(sent) = req
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
    else {
        return false;
    };
    let (a, b) = (sent.trim().as_bytes(), token.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn access(app: &App, req: &Request, access: Access) -> Result<(), Reply> {
    let local = is_local(req) && json_typed(req);
    let (ok, rule) = match access {
        Access::Read => (true, ""),
        Access::Client => (
            same_origin(app, req) || local,
            "this route takes a same-origin JSON request: send Content-Type: application/json, and from a browser only from Beacon's own origin or one listed in `origins` in beacon.toml",
        ),
        Access::Producer => (
            local || bearer_ok(app, req),
            if app.config.producer_token.is_some() {
                "this route takes a request from this machine straight to Beacon, or one with Authorization: Bearer <auth.producer_token>; send Content-Type: application/json"
            } else {
                "this route takes a request from this machine straight to Beacon (not through a proxy) with Content-Type: application/json; set auth.producer_token in beacon.toml to allow others"
            },
        ),
        Access::Local => (
            local,
            "this route takes a request from this machine straight to Beacon's port, not through a proxy (no X-Forwarded-For or Forwarded), with Content-Type: application/json",
        ),
    };
    if ok {
        Ok(())
    } else {
        Err(Reply::problem(
            403,
            "request access",
            format!(
                "{} {} is {} access only",
                req.method,
                req.path,
                json!(access).as_str().unwrap_or("")
            ),
            rule,
        ))
    }
}

// ---------- reading requests ----------

pub(crate) fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

pub(crate) fn store_error(e: beacon_store::Error) -> Reply {
    Reply::problems(e.kind.status(), &e.problems)
}

fn no_item(id: i64) -> Reply {
    Reply::problem(
        404,
        "item",
        format!("no item {id}"),
        "check the id; GET /v1/inbox lists items",
    )
}

fn no_silence(id: i64) -> Reply {
    Reply::problem(
        404,
        "silence",
        format!("no silence {id}"),
        "GET /v1/silences?all=1 lists them",
    )
}

fn id(text: &str, what: &str) -> Result<i64, Reply> {
    text.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(|| {
        Reply::problem(
            404,
            what,
            format!("`{text}` is not an id"),
            format!("{what} ids are positive whole numbers"),
        )
    })
}

/// The body as a JSON object.
pub(crate) fn object(req: &Request, at: &str) -> Result<Map<String, Value>, Reply> {
    let value: Value = serde_json::from_slice(&req.body).map_err(|e| {
        Reply::problem(
            400,
            at,
            format!("the body is not JSON: {e}"),
            "send a JSON object with Content-Type: application/json",
        )
    })?;
    match value {
        Value::Object(map) => Ok(map),
        other => Err(Reply::problem(
            400,
            at,
            format!("the body is {}, not an object", kind_of(&other)),
            "send a JSON object, like {\"kind\": \"backup.failed\"}",
        )),
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// Unknown fields, each with the name the writer probably meant.
pub(crate) fn unknown(map: &Map<String, Value>, at: &str, known: &[&str]) -> Vec<Problem> {
    map.keys()
        .filter(|key| !known.contains(&key.as_str()))
        .map(|key| {
            let fix = match closest(key, known.iter().copied()) {
                Some(near) => format!("did you mean `{near}`?"),
                None => format!("the fields are {}", known.join(", ")),
            };
            Problem::new(
                format!("{at} {key}"),
                format!("`{key}` is not a field of this body"),
                fix,
            )
        })
        .collect()
}

pub(crate) fn bad(problems: Vec<Problem>) -> Reply {
    Reply::problems(400, &problems)
}

/// Reads a checked object into `T`; a wrong type is `400`.
pub(crate) fn read<T: DeserializeOwned>(map: Map<String, Value>, at: &str) -> Result<T, Reply> {
    serde_json::from_value(Value::Object(map)).map_err(|e| {
        Reply::problem(
            400,
            at,
            format!("a field can't be read: {e}"),
            "check each field's type against spec/api.md",
        )
    })
}

/// Reads a body: an object with only `known` fields, into `T`.
fn body<T: DeserializeOwned>(
    req: &Request,
    at: &str,
    known: &[&str],
) -> Result<(T, Map<String, Value>), Reply> {
    let map = object(req, at)?;
    let problems = unknown(&map, at, known);
    if !problems.is_empty() {
        return Err(bad(problems));
    }
    Ok((read(map.clone(), at)?, map))
}

// ---------- the server ----------

fn health(app: &App) -> Reply {
    let devices = app.store.devices();
    let unreadable = app.store.unreadable();
    let ok = app.store.version().is_ok() && devices.is_ok() && unreadable.is_ok();
    Reply::json(
        if ok { 200 } else { 503 },
        json!({
            "ok": ok,
            "version": crate::VERSION,
            "started": app.started,
            "push": app.config.settings.push,
            "apns": if app.apns.is_some() { "ok" } else { "unconfigured" },
            "devices": devices.map_or(0, |d| d.len()),
            "unreadable_sources": unreadable.map_or(0, |u| u.len()),
        }),
    )
}

// ---------- the registry ----------

fn publish(app: &App, req: &Request, source: &str) -> Answer {
    let keep = match req.param("keep") {
        None | Some("false" | "0") => false,
        Some("true" | "1") => true,
        Some(other) => {
            return Err(Reply::problem(
                400,
                "registry keep",
                format!("`{other}` is not true or false"),
                "send ?keep=true to keep kinds the body lacks, or leave keep out to replace them",
            ));
        }
    };
    let value: Value = Value::Object(object(req, "declarations")?);
    let check = Check {
        banned_words: app.config.banned_words.clone(),
        ..Check::default()
    };
    let decl = Declarations::from_json(value, &check).map_err(bad)?;
    if decl.source != source {
        return Err(Reply::problem(
            409,
            "registry source",
            format!(
                "the path names `{source}` but the body is source `{}`",
                decl.source
            ),
            format!("PUT it to /v1/registry/{}", decl.source),
        ));
    }
    let published = app
        .store
        .publish_with(&decl, keep, app.now())
        .map_err(store_error)?;
    Ok(Reply::ok(json!({
        "ok": true,
        "source": decl.source,
        "kinds": published.kinds,
        "kept": published.kept,
        "closed": published.closed.len(),
    })))
}

/// FNV-1a 64, for the kinds' ETag.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn kinds(app: &App, req: &Request) -> Answer {
    let mut kinds = Vec::new();
    for decl in app.store.all_declarations().map_err(store_error)? {
        for (name, kind) in &decl.kinds {
            let actions: Vec<Value> = kind
                .actions
                .iter()
                .map(|action| {
                    let url = match action {
                        Action::Custom(custom) => custom.url.clone(),
                        Action::Builtin(_) => None,
                    };
                    json!({"id": action.id(), "label": action.label(), "url": url})
                })
                .collect();
            kinds.push(json!({
                "kind": name,
                "source": decl.source,
                "source_name": decl.name,
                "icon": decl.icon,
                "class": kind.class,
                "push": kind.push,
                "preview": kind.preview,
                "actions": actions,
                "time_bound": kind.time_bound,
                "context": kind.context,
                "one_off": kind.one_off,
                "categories": kind.categories,
            }));
        }
    }
    kinds.sort_by(|a, b| a["kind"].as_str().cmp(&b["kind"].as_str()));
    let body = json!({"kinds": kinds});
    let etag = format!("W/\"k{:016x}\"", fnv(body.to_string().as_bytes()));
    if req.header("if-none-match") == Some(etag.as_str()) {
        return Ok(Reply::json(304, Value::Null).with("ETag", etag));
    }
    Ok(Reply::ok(body).with("ETag", etag))
}

// ---------- producers ----------

const EVENT_FIELDS: [&str; 10] = [
    "kind", "source", "key", "facts", "slots", "labels", "state", "context", "url", "category",
];
const ENTRY_FIELDS: [&str; 8] = [
    "kind", "key", "facts", "slots", "labels", "context", "url", "category",
];
const SNAPSHOT_FIELDS: [&str; 7] = [
    "source",
    "observed",
    "reason",
    "stale_after",
    "sent_at",
    "kinds",
    "firing",
];

fn events(app: &App, req: &Request) -> Answer {
    let (event, _) = body::<Event>(req, "event", &EVENT_FIELDS)?;
    let key = req.header("idempotency-key");
    let answer = app
        .store
        .apply_event(&event, key, app.now())
        .map_err(store_error)?;
    Ok(Reply::ok(to_json(&answer)))
}

fn snapshots(app: &App, req: &Request) -> Answer {
    let map = object(req, "snapshot")?;
    let mut problems = unknown(&map, "snapshot", &SNAPSHOT_FIELDS);
    if let Some(Value::Array(firing)) = map.get("firing") {
        for (i, entry) in firing.iter().enumerate() {
            if let Value::Object(entry) = entry {
                problems.extend(unknown(entry, &format!("snapshot firing[{i}]"), &ENTRY_FIELDS));
            }
        }
    }
    if !problems.is_empty() {
        return Err(bad(problems));
    }
    let snapshot: Snapshot = read(map, "snapshot")?;
    let answer = app
        .store
        .apply_snapshot(&snapshot, app.now())
        .map_err(store_error)?;
    Ok(Reply::ok(to_json(&answer)))
}

/// Trail's labels as Beacon keeps them: snake_case names, values cut to 128, at most 16 with
/// `trail_source` and `trail_alert` first. Others are dropped rather than refusing the body.
fn trail_labels(labels: Option<&Value>, source: &str, alert: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if !source.is_empty() {
        out.insert("trail_source".to_string(), cut(source, 128));
    }
    if !alert.is_empty() {
        out.insert("trail_alert".to_string(), cut(alert, 128));
    }
    if let Some(Value::Object(labels)) = labels {
        for (name, value) in labels {
            if out.len() >= 16 {
                break;
            }
            if let Some(value) = value.as_str()
                && beacon_core::names::is_ident(name)
                && !out.contains_key(name)
            {
                out.insert(name.clone(), cut(value, 128));
            }
        }
    }
    out
}

fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Checks the fields of one Trail entry or notice (`spec/api.md`, `POST /v1/trail`, "Validation").
fn trail_fields(at: &str, v: &Map<String, Value>, key_field: &str, key_max: usize) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut need = |ok: bool, field: &str, rule: &str| {
        if !ok {
            problems.push(Problem::new(
                format!("{at} {field}"),
                rule.to_string(),
                "send what Trail's spec/alerts.md describes",
            ));
        }
    };
    let key = v.get(key_field).and_then(Value::as_str);
    need(
        key.is_some_and(|k| !k.is_empty() && k.chars().count() <= key_max),
        key_field,
        &format!("a non-empty string of at most {key_max} characters"),
    );
    need(
        matches!(
            v.get("severity").and_then(Value::as_str),
            Some("info" | "warning" | "severe")
        ),
        "severity",
        "one of info, warning, severe",
    );
    need(
        v.get("title")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.is_empty() && t.chars().count() <= 500),
        "title",
        "a non-empty string of at most 500 characters",
    );
    need(
        match v.get("body") {
            None | Some(Value::Null) => true,
            Some(Value::String(b)) => b.chars().count() <= 4096,
            _ => false,
        },
        "body",
        "a string of at most 4096 characters, or absent",
    );
    need(
        match v.get("labels") {
            None | Some(Value::Null) => true,
            Some(Value::Object(l)) => l.values().all(Value::is_string),
            _ => false,
        },
        "labels",
        "an object of strings, or absent",
    );
    problems
}

fn trail_kind(severity: &str, notice: bool) -> &'static str {
    match (severity, notice) {
        ("severe", false) => builtin::TRAIL_SEVERE,
        ("severe", true) => builtin::TRAIL_SEVERE_NOTICE,
        ("warning", _) => builtin::TRAIL_WARNING,
        _ => builtin::TRAIL_INFO,
    }
}

fn trail_slots(v: &Map<String, Value>) -> Map<String, Value> {
    let text = |field: &str| v.get(field).and_then(Value::as_str).unwrap_or_default();
    let mut slots = Map::new();
    slots.insert("title".into(), json!(text("title")));
    slots.insert("body".into(), json!(text("body")));
    slots
}

/// Trail's webhook. Trail retries 408, 429 and 5xx and drops anything else, so a body Beacon
/// can't use is 400, never 422.
fn trail(app: &App, req: &Request) -> Answer {
    let map = object(req, "trail")?;
    let now = app.now();
    let positive = |field: &str| map.get(field).and_then(Value::as_i64).filter(|n| *n > 0);
    let answer = match map.get("type").and_then(Value::as_str) {
        Some("snapshot") => {
            let mut problems = Vec::new();
            let sent_at = positive("sent_at");
            if sent_at.is_none() {
                problems.push(Problem::new(
                    "trail sent_at",
                    "a positive whole number of ms",
                    "send Trail's snapshot as it is",
                ));
            }
            let firing = match map.get("firing") {
                Some(Value::Array(list)) => list.clone(),
                _ => {
                    problems.push(Problem::new(
                        "trail firing",
                        "a list of firing entries",
                        "send [] when nothing fires",
                    ));
                    Vec::new()
                }
            };
            let mut entries = Vec::new();
            for (i, entry) in firing.iter().enumerate() {
                let at = format!("trail firing[{i}]");
                let Value::Object(v) = entry else {
                    problems.push(Problem::new(
                        at,
                        "an entry is an object",
                        "send Trail's entries as they are",
                    ));
                    continue;
                };
                problems.extend(trail_fields(&at, v, "key", 200));
                let text = |f: &str| v.get(f).and_then(Value::as_str).unwrap_or_default().to_string();
                entries.push(SnapshotEntry {
                    kind: trail_kind(&text("severity"), false).to_string(),
                    key: text("key"),
                    slots: trail_slots(v),
                    labels: trail_labels(v.get("labels"), &text("source"), &text("alert")),
                    ..SnapshotEntry::default()
                });
            }
            if !problems.is_empty() {
                return Err(bad(problems));
            }
            // Trail may list one key twice; the store would refuse that.
            let mut seen = std::collections::BTreeSet::new();
            entries.retain(|e| seen.insert((e.kind.clone(), e.key.clone())));
            app.store
                .apply_trail_snapshot(sent_at.unwrap_or_default(), entries, now)
                .map_err(trail_error)?;
            json!({"ok": true})
        }
        Some("notice") => {
            let mut problems = trail_fields("trail", &map, "id", 256);
            let ts = positive("ts");
            if ts.is_none() {
                problems.push(Problem::new(
                    "trail ts",
                    "a positive whole number of ms",
                    "send Trail's notice as it is",
                ));
            }
            if !problems.is_empty() {
                return Err(bad(problems));
            }
            let text = |f: &str| map.get(f).and_then(Value::as_str).unwrap_or_default().to_string();
            let id = text("id");
            let event = Event {
                kind: trail_kind(&text("severity"), true).to_string(),
                key: id.clone(),
                slots: Some(trail_slots(&map)),
                facts: Some(Map::new()),
                labels: trail_labels(map.get("labels"), &text("source"), &text("alert")),
                ..Event::default()
            };
            let late = ts.is_some_and(|ts| now - ts > app.config.settings.late);
            let answer = app
                .store
                .apply_trail_notice(&id, &event, late, now)
                .map_err(trail_error)?;
            json!({"ok": true, "item": answer.item.map(|i| i.id), "repeat": answer.repeat})
        }
        other => {
            return Err(Reply::problem(
                400,
                "trail type",
                format!("`{}` is not a Trail webhook type", other.unwrap_or("")),
                "send `snapshot` or `notice`",
            ));
        }
    };
    Ok(Reply::ok(answer))
}

fn trail_error(e: beacon_store::Error) -> Reply {
    let status = match e.kind.status() {
        422 => 400,
        other => other,
    };
    Reply::problems(status, &e.problems)
}

// ---------- clients ----------

fn presence(app: &App, req: &Request) -> Answer {
    let (beat, _) = body::<Heartbeat>(req, "presence", &["client", "device", "active", "viewing"])?;
    let active_until = app.store.heartbeat(&beat, app.now()).map_err(store_error)?;
    let heartbeat = Duration::from_ms(app.config.heartbeat).map(|d| d.to_string());
    Ok(Reply::ok(
        json!({"ok": true, "heartbeat": heartbeat, "active_until": active_until}),
    ))
}

fn inbox(app: &App, req: &Request) -> Answer {
    let mut query = InboxQuery {
        limit: app.config.inbox_page,
        ..InboxQuery::default()
    };
    let mut problems = Vec::new();
    if let Some(sources) = req.param("source").filter(|s| !s.is_empty()) {
        query.sources = sources.split(',').map(|s| s.trim().to_string()).collect();
    }
    if let Some(classes) = req.param("class").filter(|s| !s.is_empty()) {
        query.classes.clear();
        for name in classes.split(',').map(str::trim) {
            match Class::parse(name) {
                Some(class) => query.classes.push(class),
                None => {
                    let names = Class::ALL.map(Class::as_str);
                    let fix = match closest(name, names) {
                        Some(near) => format!("did you mean `{near}`?"),
                        None => format!("send classes from {}", names.join(", ")),
                    };
                    problems.push(Problem::new(
                        "inbox class",
                        format!("`{name}` is not a class"),
                        fix,
                    ));
                }
            }
        }
    }
    let number = |name: &str, problems: &mut Vec<Problem>| -> Option<i64> {
        let text = req.param(name)?;
        match text.parse::<i64>() {
            Ok(n) if n >= 0 => Some(n),
            _ => {
                problems.push(Problem::new(
                    format!("inbox {name}"),
                    format!("`{text}` is not a whole number"),
                    format!("send {name} as a number from an earlier answer"),
                ));
                None
            }
        }
    };
    query.before = number("before", &mut problems);
    if let Some(since) = number("since", &mut problems) {
        query.since = since;
    }
    if let Some(limit) = number("limit", &mut problems) {
        if (1..=200).contains(&limit) {
            query.limit = u32::try_from(limit).unwrap_or(50);
        } else {
            problems.push(Problem::new(
                "inbox limit",
                format!("{limit} is outside 1 to 200"),
                "send a limit from 1 to 200",
            ));
        }
    }
    if !problems.is_empty() {
        return Err(bad(problems));
    }
    let version = app.store.version().map_err(store_error)?;
    let etag = format!("W/\"{version}\"");
    if req.header("if-none-match") == Some(etag.as_str()) {
        return Ok(Reply::json(304, Value::Null).with("ETag", etag));
    }
    let inbox = app.store.inbox(&query, app.now()).map_err(store_error)?;
    let etag = format!("W/\"{}\"", inbox.version);
    Ok(Reply::ok(to_json(&inbox)).with("ETag", etag))
}

fn client_of(map: &Map<String, Value>, at: &str, allowed: &[&str]) -> Result<String, Reply> {
    match map.get("client") {
        None | Some(Value::Null) => Ok("api".into()),
        Some(Value::String(c)) if allowed.contains(&c.as_str()) => Ok(c.clone()),
        Some(other) => Err(Reply::problem(
            400,
            &format!("{at} client"),
            format!("{other} is not a client"),
            format!("send one of {}", allowed.join(", ")),
        )),
    }
}

fn press(app: &App, req: &Request, id_text: &str) -> Answer {
    let id = id(id_text, "item")?;
    let map = object(req, "action")?;
    let problems = unknown(&map, "action", &["action", "until", "client"]);
    if !problems.is_empty() {
        return Err(bad(problems));
    }
    let Some(action) = map.get("action").and_then(Value::as_str) else {
        return Err(Reply::problem(
            400,
            "action action",
            "`action` is required",
            "send the id of one of the item's actions, like \"done\"",
        ));
    };
    let until = match map.get("until") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_i64().ok_or_else(|| {
            Reply::problem(
                400,
                "action until",
                "until is a time in ms",
                "send a whole number of ms, or leave it out",
            )
        })?),
    };
    let client = client_of(&map, "action", &["web", "ios", "api"])?;
    let answer = app
        .store
        .press(id, action, until, &client, app.now())
        .map_err(store_error)?;
    Ok(Reply::ok(to_json(&answer)))
}

fn seen(app: &App, req: &Request) -> Answer {
    let map = object(req, "seen")?;
    let problems = unknown(&map, "seen", &["through", "ids", "client"]);
    if !problems.is_empty() {
        return Err(bad(problems));
    }
    let client = client_of(&map, "seen", &["web", "ios", "api"])?;
    let target = match (map.get("through"), map.get("ids")) {
        (Some(through), None) => SeenTarget::Through(through.as_i64().ok_or_else(|| {
            Reply::problem(
                400,
                "seen through",
                "through is an item id",
                "send the newest id the client shows",
            )
        })?),
        (None, Some(Value::Array(ids))) => SeenTarget::Ids(
            ids.iter()
                .map(Value::as_i64)
                .collect::<Option<Vec<i64>>>()
                .ok_or_else(|| {
                    Reply::problem(400, "seen ids", "ids is a list of item ids", "send [4182, 4177]")
                })?,
        ),
        _ => {
            return Err(Reply::problem(
                400,
                "seen",
                "send exactly one of `through` and `ids`",
                "send {\"through\": <newest id>} or {\"ids\": [<id>, …]}",
            ));
        }
    };
    let count = app
        .store
        .mark_seen(&target, &client, app.now())
        .map_err(store_error)?;
    Ok(Reply::ok(json!({"ok": true, "seen": count})))
}

/// `for` or `until` (exactly one when `required`) as an end time.
fn end_of(map: &Map<String, Value>, at: &str, now: i64, required: bool) -> Result<Option<i64>, Reply> {
    match (map.get("for"), map.get("until")) {
        (Some(_), Some(_)) => Err(Reply::problem(
            400,
            &format!("{at} for"),
            "send `for` or `until`, not both",
            "keep one: `for` like \"30m\", or `until` in ms",
        )),
        (Some(Value::String(text)), None) => Duration::parse(text)
            .map(|d| Some(now + d.ms()))
            .map_err(|m| Reply::problem(400, &format!("{at} for"), m, format!("write {}", Duration::FORMS))),
        (Some(_), None) => Err(Reply::problem(
            400,
            &format!("{at} for"),
            "for is a duration string",
            "send like \"30m\"",
        )),
        (None, Some(v)) => v.as_i64().map(Some).ok_or_else(|| {
            Reply::problem(
                400,
                &format!("{at} until"),
                "until is a time in ms",
                "send a whole number of ms",
            )
        }),
        (None, None) if required => Err(Reply::problem(
            400,
            &format!("{at} for"),
            "a silence needs an end",
            "send `for` like \"30m\", or `until` in ms",
        )),
        (None, None) => Ok(None),
    }
}

fn open_silence(app: &App, req: &Request) -> Answer {
    let map = object(req, "silence")?;
    let mut problems = unknown(&map, "silence", &["match", "for", "until", "reason", "client"]);
    if let Some(Value::Object(m)) = map.get("match") {
        problems.extend(unknown(m, "silence match", &["source", "kind", "key", "labels"]));
    }
    if !problems.is_empty() {
        return Err(bad(problems));
    }
    let now = app.now();
    let matcher: SilenceMatch = match map.get("match") {
        Some(m @ Value::Object(_)) => serde_json::from_value(m.clone()).map_err(|e| {
            Reply::problem(
                400,
                "silence match",
                format!("can't be read: {e}"),
                "send {\"labels\": {\"service\": \"web\"}}",
            )
        })?,
        _ => {
            return Err(Reply::problem(
                400,
                "silence match",
                "`match` is required: an object",
                "send {\"match\": {\"source\": \"backup\"}, …}",
            ));
        }
    };
    let until = end_of(&map, "silence", now, true)?.unwrap_or(now);
    let reason = match map.get("reason") {
        Some(Value::String(r)) => r.clone(),
        _ => {
            return Err(Reply::problem(
                400,
                "silence reason",
                "`reason` is required: 1 to 100 characters",
                "say why, like \"Deploying dashboard\"",
            ));
        }
    };
    let client = client_of(&map, "silence", &["web", "ios", "api"])?;
    let new = NewSilence {
        matcher,
        until,
        reason,
        client,
    };
    let view = match req.header("idempotency-key") {
        Some(key) => {
            let canonical = Value::Object(map).to_string();
            app.store
                .open_silence_once(&new, key, &canonical, now)
                .map_err(store_error)?
                .0
        }
        None => app.store.open_silence(&new, now).map_err(store_error)?,
    };
    Ok(Reply::json(201, to_json(&view)))
}

fn change_silence(app: &App, req: &Request, id_text: &str) -> Answer {
    let id = id(id_text, "silence")?;
    let map = object(req, "silence")?;
    let problems = unknown(&map, "silence", &["for", "until", "reason"]);
    if !problems.is_empty() {
        return Err(bad(problems));
    }
    let now = app.now();
    let until = end_of(&map, "silence", now, false)?;
    let reason = match map.get("reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(r)) => Some(r.as_str()),
        Some(_) => {
            return Err(Reply::problem(
                400,
                "silence reason",
                "reason is a string",
                "send 1 to 100 characters",
            ));
        }
    };
    if until.is_none() && reason.is_none() {
        return Err(Reply::problem(
            400,
            "silence",
            "nothing to change",
            "send `for` or `until`, and/or `reason`",
        ));
    }
    let view = app
        .store
        .change_silence(id, until, reason, now)
        .map_err(store_error)?;
    Ok(Reply::ok(to_json(&view)))
}

fn register(app: &App, req: &Request) -> Answer {
    let map = object(req, "device")?;
    let problems = unknown(&map, "device", &["token", "environment", "name"]);
    if !problems.is_empty() {
        return Err(bad(problems));
    }
    let text = |field: &str| map.get(field).and_then(Value::as_str);
    let (Some(token), Some(environment)) = (text("token"), text("environment")) else {
        return Err(Reply::problem(
            400,
            "device",
            "`token` and `environment` are required strings",
            "send {\"token\": \"<hex>\", \"environment\": \"production\"}",
        ));
    };
    app.store
        .register_device(token, environment, text("name"), app.now())
        .map_err(store_error)?;
    Ok(Reply::ok(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_suggest_the_closest() {
        let map: Map<String, Value> = serde_json::from_str(r#"{"knd": 1, "zzzzzz": 2}"#).unwrap();
        let problems = unknown(&map, "event", &EVENT_FIELDS);
        assert_eq!(problems.len(), 2);
        assert_eq!(problems[0].fix, "did you mean `kind`?");
        assert!(problems[1].fix.starts_with("the fields are"));
    }

    #[test]
    fn trail_labels_are_kept_valid() {
        let labels = json!({"Bad Name": "x", "queue": "y".repeat(300)});
        let out = trail_labels(Some(&labels), "web", "errors");
        assert_eq!(out["trail_source"], "web");
        assert_eq!(out["trail_alert"], "errors");
        assert_eq!(out["queue"].len(), 128);
        assert!(!out.contains_key("Bad Name"));
    }
}
