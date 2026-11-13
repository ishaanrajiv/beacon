//! The server over HTTP: started on an ephemeral port with a temporary data folder, a fake clock
//! and a fake Apple, and driven the way producers and the apps drive it.

use beacon_apns::{Environment, Response, Transport};
use beacon_core::config;
use beacon_core::duration::{HOUR, MINUTE, SECOND};
use beacon_server::{Clock, Options, Server};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

/// 2026-11-13 12:00 UTC.
const T0: i64 = 1_794_571_200_000;
const TOKEN: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";

const KINDS: &str = r#"
source = "lab"
name = "Lab"
icon = "server"

[kinds."lab.down"]
class = "alert"
push = "normal"
title = "Website is down"
body = "{cause}."
slots = { cause = 80 }
open = "/system"
actions = ["acknowledge", "dismiss", "silence_1h"]
throttle = "none"
recovered = "Website is back"

[kinds."lab.note"]
class = "update"
push = "normal"
title = "A note"
body = "{text}"
slots = { text = 100 }

[kinds."lab.approve"]
class = "action"
push = "normal"
title = "Approval needed"
body = "{what}"
slots = { what = 80 }
actions = ["done", "dismiss"]
context = true
categories = ["APPROVAL", "APPROVAL_LONG"]
"#;

// ---------- the fakes ----------

struct FakeClock(AtomicI64);

impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
struct Sent {
    environment: Environment,
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

/// Apple, in process: records each request and answers from a script (200 when it runs out).
#[derive(Clone, Default)]
struct Apple {
    sent: Arc<Mutex<Vec<Sent>>>,
    script: Arc<Mutex<VecDeque<(u16, &'static str)>>>,
}

impl Transport for Apple {
    fn post(
        &self,
        environment: Environment,
        path: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Result<Response, String> {
        self.sent.lock().unwrap().push(Sent {
            environment,
            path: path.to_string(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_ascii_lowercase(), v.clone()))
                .collect(),
            body: serde_json::from_slice(body).unwrap(),
        });
        let (status, reason) = self.script.lock().unwrap().pop_front().unwrap_or((200, ""));
        let body = if reason.is_empty() {
            Vec::new()
        } else {
            json!({ "reason": reason }).to_string().into_bytes()
        };
        Ok(Response { status, body })
    }
}

impl Apple {
    fn take(&self) -> Vec<Sent> {
        std::mem::take(&mut *self.sent.lock().unwrap())
    }

    fn answer(&self, status: u16, reason: &'static str) {
        self.script.lock().unwrap().push_back((status, reason));
    }
}

struct Lab {
    server: Server,
    clock: Arc<FakeClock>,
    apple: Apple,
    _dir: TempDir,
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir() -> TempDir {
    static N: AtomicI64 = AtomicI64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "beacon-server-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

fn start_with(extra: &str, apns: bool) -> Lab {
    start_full(extra, apns, false)
}

fn start_full(extra: &str, apns: bool, jobs: bool) -> Lab {
    let dir = temp_dir();
    let mut text = format!(
        "listen = \"127.0.0.1:0\"\ndata_dir = \"{}\"\n{extra}\n",
        dir.0.join("data").display()
    );
    if apns {
        let key = dir.0.join("AuthKey_ABC123DEFG.p8");
        std::fs::write(&key, beacon_apns::new_key_pem().unwrap()).unwrap();
        text.push_str(&format!(
            "[apns]\nkey_path = \"{}\"\nteam_id = \"TEAM123456\"\ntopic = \"com.example.lab\"\n",
            key.display()
        ));
    }
    let config = config::parse(&text).unwrap_or_else(|p| panic!("{p:?}"));
    let clock = Arc::new(FakeClock(AtomicI64::new(T0)));
    let apple = Apple::default();
    let server = Server::start(Options {
        config,
        clock: clock.clone(),
        transport: Some(Box::new(apple.clone())),
        jobs,
    })
    .unwrap_or_else(|p| panic!("{p:?}"));
    let lab = Lab {
        server,
        clock,
        apple,
        _dir: dir,
    };
    let decl = beacon_core::decl::parse(KINDS, &Default::default()).unwrap();
    assert_eq!(lab.put("/v1/registry/lab", &decl.to_json()).status, 200);
    lab
}

fn start() -> Lab {
    start_with("", true)
}

// ---------- a small HTTP client ----------

#[derive(Debug)]
struct Res {
    status: u16,
    headers: Vec<(String, String)>,
    json: Value,
}

impl Res {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn raw(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&Value>) -> Res {
    let mut stream = TcpStream::connect(addr).unwrap();
    let text = body.map(Value::to_string).unwrap_or_default();
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n{text}", text.len()));
    stream.write_all(request.as_bytes()).unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body).unwrap()
    };
    Res {
        status,
        headers,
        json,
    }
}

const JSON: (&str, &str) = ("Content-Type", "application/json");

impl Lab {
    fn addr(&self) -> SocketAddr {
        self.server.addr()
    }
    fn get(&self, path: &str) -> Res {
        raw(self.addr(), "GET", path, &[], None)
    }
    fn post(&self, path: &str, body: &Value) -> Res {
        raw(self.addr(), "POST", path, &[JSON], Some(body))
    }
    fn put(&self, path: &str, body: &Value) -> Res {
        raw(self.addr(), "PUT", path, &[JSON], Some(body))
    }
    fn at(&self, now: i64) {
        self.clock.0.store(now, Ordering::SeqCst);
    }
    fn later(&self, ms: i64) {
        self.clock.0.fetch_add(ms, Ordering::SeqCst);
    }
    fn tick(&self) {
        self.server.tick();
    }
    fn item(&self, id: &Value) -> Value {
        self.get(&format!("/v1/items/{id}")).json["item"].clone()
    }
    fn device(&self) {
        let r = self.post(
            "/v1/devices",
            &json!({"token": TOKEN, "environment": "production", "name": "iPhone"}),
        );
        assert_eq!(r.status, 200, "{:?}", r.json);
    }
    fn down(&self) -> Res {
        self.post(
            "/v1/events",
            &json!({"kind": "lab.down", "slots": {"cause": "No answer"}}),
        )
    }
    fn up(&self) -> Res {
        self.post("/v1/events", &json!({"kind": "lab.down", "state": "resolved"}))
    }
}

// ---------- the tests ----------

#[test]
fn an_alert_pushes_and_its_recovery_replaces_the_banner() {
    let lab = start();
    lab.device();
    let r = lab.down();
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["outcome"], "opened");
    let id = r.json["item"]["id"].clone();
    assert_eq!(r.json["item"]["push"]["status"], "queued");
    lab.tick();
    let sent = lab.apple.take();
    assert_eq!(sent.len(), 1);
    let push = &sent[0];
    assert_eq!(push.environment, Environment::Production);
    assert_eq!(push.path, format!("/3/device/{TOKEN}"));
    assert_eq!(push.headers["apns-push-type"], "alert");
    assert_eq!(push.headers["apns-topic"], "com.example.lab");
    assert_eq!(push.headers["apns-priority"], "10");
    assert!(push.headers["authorization"].starts_with("bearer "));
    let collapse = push.headers["apns-collapse-id"].clone();
    assert!(collapse.starts_with("b1-"));
    let aps = &push.body["aps"];
    assert_eq!(aps["alert"]["title"], "Website is down");
    assert_eq!(aps["alert"]["body"], "No answer.");
    assert_eq!(aps["interruption-level"], "active");
    assert_eq!(aps["sound"], "default");
    assert_eq!(aps["thread-id"], "lab");
    assert_eq!(aps["category"], "lab.down");
    assert_eq!(push.body["url"], "/system");
    assert_eq!(push.body["data"]["item"], id.to_string());
    assert_eq!(push.body["data"]["collapse_id"], collapse.as_str());
    let item = lab.item(&id);
    assert_eq!(item["push"]["status"], "sent");
    assert_eq!(item["collapse_id"], collapse.as_str());

    lab.later(5 * MINUTE);
    let r = lab.up();
    assert_eq!(r.json["outcome"], "resolved");
    lab.tick();
    let sent = lab.apple.take();
    assert_eq!(sent.len(), 1, "one recovery push");
    let recovery = &sent[0];
    assert_eq!(
        recovery.headers["apns-collapse-id"], collapse,
        "the recovery replaces the banner"
    );
    assert_eq!(recovery.headers["apns-priority"], "5");
    let aps = &recovery.body["aps"];
    assert_eq!(aps["alert"]["title"], "Website is back");
    assert_eq!(aps["interruption-level"], "passive");
    assert!(aps.get("sound").is_none());
    assert!(
        aps.get("category").is_none(),
        "a recovered alert shows no buttons"
    );
    let detail = lab.get(&format!("/v1/items/{id}")).json;
    let history: Vec<&str> = detail["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["what"].as_str().unwrap())
        .collect();
    assert!(history.contains(&"recovery_sent"), "{history:?}");
    assert_eq!(
        detail["attempts"][0]["device"],
        format!("…{}", &TOKEN[TOKEN.len() - 6..])
    );
    assert_eq!(detail["attempts"][0]["status"], "accepted");
}

#[test]
fn categories_and_context_reach_the_payload() {
    let lab = start();
    lab.device();
    let kinds = lab.get("/v1/kinds");
    let approve = kinds.json["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["kind"] == "lab.approve")
        .unwrap()
        .clone();
    assert_eq!(approve["categories"], json!(["APPROVAL", "APPROVAL_LONG"]));
    let body = json!({"kind": "lab.approve", "key": "7", "slots": {"what": "Deploy web"}, "context": "assistant:thread:42"});
    assert_eq!(lab.post("/v1/events", &body).status, 200);
    let mut chosen = body.clone();
    chosen["key"] = json!("8");
    chosen["category"] = json!("APPROVAL_LONG");
    assert_eq!(lab.post("/v1/events", &chosen).status, 200);
    chosen["key"] = json!("9");
    chosen["category"] = json!("APPROVE");
    let r = lab.post("/v1/events", &chosen);
    assert_eq!(r.status, 422);
    assert!(
        r.json["error"]["fix"]
            .as_str()
            .unwrap()
            .contains("did you mean `APPROVAL`")
    );
    lab.tick();
    let sent = lab.apple.take();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].body["aps"]["category"], "APPROVAL");
    assert_eq!(sent[1].body["aps"]["category"], "APPROVAL_LONG");
    assert_eq!(sent[0].body["data"]["context"], "assistant:thread:42");
    assert_eq!(sent[0].body["aps"]["relevance-score"], 0.8);
}

#[test]
fn the_kinds_send_an_etag() {
    let lab = start();
    let first = lab.get("/v1/kinds");
    let etag = first.header("ETag").unwrap().to_string();
    let again = raw(lab.addr(), "GET", "/v1/kinds", &[("If-None-Match", &etag)], None);
    assert_eq!(again.status, 304);
    let mut decl = beacon_core::decl::parse(KINDS, &Default::default())
        .unwrap()
        .to_json();
    decl["kinds"]["lab.note"]["push"] = json!("silent");
    assert_eq!(lab.put("/v1/registry/lab", &decl).status, 200);
    let changed = raw(lab.addr(), "GET", "/v1/kinds", &[("If-None-Match", &etag)], None);
    assert_eq!(changed.status, 200);
}

#[test]
fn a_push_held_while_active_goes_out_if_unseen() {
    let lab = start();
    lab.device();
    let r = lab.post(
        "/v1/presence",
        &json!({"client": "web", "device": "web-1", "active": true}),
    );
    assert_eq!(r.json["active_until"], T0 + 75 * SECOND);
    assert_eq!(r.json["heartbeat"], "30s");
    let r = lab.down();
    let id = r.json["item"]["id"].clone();
    assert_eq!(r.json["item"]["push"]["status"], "held");
    assert_eq!(r.json["item"]["push"]["reason"], "active");
    assert_eq!(r.json["item"]["push"]["release_at"], T0 + 120 * SECOND);
    let inbox = lab.get("/v1/inbox").json;
    assert_eq!(inbox["held"][0]["id"], id);
    lab.tick();
    assert!(lab.apple.take().is_empty());
    lab.later(121 * SECOND);
    lab.tick();
    assert_eq!(lab.apple.take().len(), 1, "unseen when the hold ended: it pushes");
    assert_eq!(lab.item(&id)["push"]["status"], "sent");
}

#[test]
fn seeing_a_held_item_suppresses_its_push() {
    let lab = start();
    lab.device();
    lab.post(
        "/v1/presence",
        &json!({"client": "ios", "device": "ios-1", "active": true}),
    );
    let id = lab.down().json["item"]["id"].clone();
    let r = lab.post("/v1/seen", &json!({"ids": [id], "client": "ios"}));
    assert_eq!(r.json, json!({"ok": true, "seen": 1}));
    let item = lab.item(&id);
    assert_eq!(item["push"]["status"], "suppressed");
    assert_eq!(item["push"]["reason"], "seen");
    lab.later(10 * MINUTE);
    lab.tick();
    assert!(lab.apple.take().is_empty());
    let r = lab.post("/v1/seen", &json!({"through": 1, "ids": [1]}));
    assert_eq!(r.status, 400);
}

#[test]
fn quiet_hours_hold_until_the_morning() {
    let lab = start();
    lab.device();
    lab.at(T0 + 13 * HOUR); // 01:00 UTC
    let r = lab.post(
        "/v1/events",
        &json!({"kind": "lab.note", "slots": {"text": "Overnight"}}),
    );
    let id = r.json["item"]["id"].clone();
    assert_eq!(r.json["item"]["push"]["reason"], "quiet_hours");
    lab.tick();
    assert!(lab.apple.take().is_empty());
    lab.at(T0 + 20 * HOUR); // 08:00
    lab.tick();
    assert_eq!(
        lab.apple.take().len(),
        1,
        "the one held item goes out at the end of quiet hours"
    );
    assert_eq!(lab.item(&id)["push"]["status"], "sent");
}

#[test]
fn a_second_push_within_the_throttle_is_suppressed() {
    let lab = start();
    let note = json!({"kind": "lab.note", "key": "k", "slots": {"text": "One"}});
    assert_eq!(
        lab.post("/v1/events", &note).json["item"]["push"]["status"],
        "queued"
    );
    lab.later(MINUTE);
    let second = lab.post("/v1/events", &note).json;
    assert_eq!(second["item"]["push"]["reason"], "throttled");
}

#[test]
fn a_flapping_alert_stops_pushing() {
    let lab = start();
    let mut last = Value::Null;
    for _ in 0..4 {
        last = lab.down().json;
        lab.later(MINUTE);
        lab.up();
        lab.later(MINUTE);
    }
    assert_eq!(last["outcome"], "reopened");
    assert_eq!(last["item"]["alert"]["flapping"], true);
    assert_eq!(last["item"]["push"]["reason"], "flapping");
    let inbox = lab.get("/v1/inbox?class=update").json;
    let kinds: Vec<&str> = inbox["activity"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"beacon.flapping"), "{kinds:?}");
}

#[test]
fn a_silence_mutes_and_its_end_decides_again() {
    let lab = start();
    lab.device();
    let body = json!({"match": {"source": "lab"}, "for": "30m", "reason": "Deploying"});
    let r = raw(
        lab.addr(),
        "POST",
        "/v1/silences",
        &[JSON, ("Idempotency-Key", "deploy-1")],
        Some(&body),
    );
    assert_eq!(r.status, 201, "{:?}", r.json);
    let silence = r.json["id"].clone();
    assert_eq!(r.json["until"], T0 + 30 * MINUTE);
    let again = raw(
        lab.addr(),
        "POST",
        "/v1/silences",
        &[JSON, ("Idempotency-Key", "deploy-1")],
        Some(&body),
    );
    assert_eq!(
        again.json["id"], silence,
        "the same key and body open one silence"
    );
    let other = json!({"match": {"source": "lab"}, "for": "1h", "reason": "Deploying"});
    let conflict = raw(
        lab.addr(),
        "POST",
        "/v1/silences",
        &[JSON, ("Idempotency-Key", "deploy-1")],
        Some(&other),
    );
    assert_eq!(conflict.status, 409);
    assert_eq!(
        lab.get("/v1/silences").json["silences"].as_array().unwrap().len(),
        1
    );
    let id = lab.down().json["item"]["id"].clone();
    assert_eq!(lab.item(&id)["push"]["reason"], "silenced");
    let typo = lab.post(
        "/v1/silences",
        &json!({"match": {"sorce": "lab"}, "for": "1h", "reason": "x"}),
    );
    assert_eq!(typo.status, 400);
    assert_eq!(typo.json["error"]["fix"], "did you mean `source`?");
    let ended = raw(
        lab.addr(),
        "DELETE",
        &format!("/v1/silences/{silence}"),
        &[],
        None,
    );
    assert_eq!(ended.status, 200);
    assert_eq!(ended.json["active"], false);
    assert_eq!(
        lab.item(&id)["push"]["status"],
        "queued",
        "unmuted when the silence ended"
    );
}

#[test]
fn the_silence_action_answers_its_until() {
    let lab = start();
    let id = lab.down().json["item"]["id"].clone();
    let r = lab.post(
        &format!("/v1/items/{id}/actions"),
        &json!({"action": "silence_1h", "client": "ios"}),
    );
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["silence"]["until"], T0 + HOUR);
    let r = lab.post(&format!("/v1/items/{id}/actions"), &json!({"action": "done"}));
    assert_eq!(r.status, 409);
    assert!(r.json["error"]["fix"].as_str().unwrap().contains("`acknowledge`"));
    assert_eq!(
        lab.post("/v1/items/999/actions", &json!({"action": "done"}))
            .status,
        404
    );
}

#[test]
fn snapshots_resolve_what_is_absent_and_an_unobserved_source_mutes_its_alerts() {
    let lab = start();
    let firing = json!({"source": "lab", "firing": [{"kind": "lab.down", "slots": {"cause": "Timeout"}}]});
    let r = lab.post("/v1/snapshots", &firing);
    assert_eq!(r.status, 200, "{:?}", r.json);
    let id = r.json["opened"][0].clone();
    let r = lab.post("/v1/snapshots", &json!({"source": "lab", "firing": []}));
    assert_eq!(r.json["resolved"], json!([id]));
    let r = lab.post(
        "/v1/snapshots",
        &json!({"source": "lab", "observed": false, "reason": "The probe didn't load."}),
    );
    assert_eq!(r.json["observed"], false);
    let registry = lab.get("/v1/registry").json;
    let lab_source = registry["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == "lab")
        .unwrap()
        .clone();
    assert_eq!(lab_source["observed"], false);
    lab.later(31 * MINUTE);
    let muted = lab.down().json["item"].clone();
    assert_eq!(muted["push"]["reason"], "unobserved");
    let r = lab.post(
        "/v1/snapshots",
        &json!({"source": "lab", "firing": [{"kind": "lab.down", "slots": {"cause": "Timeout"}}]}),
    );
    assert_eq!(r.json["observed"], true);
    assert_eq!(
        lab.item(&muted["id"])["push"]["status"],
        "queued",
        "decided again once observed"
    );
    let bad = lab.post(
        "/v1/snapshots",
        &json!({"source": "lab", "firing": [{"kind": "lab.down", "slotz": {}}]}),
    );
    assert_eq!(bad.status, 400);
    assert_eq!(bad.json["error"]["at"], "snapshot firing[0] slotz");
}

#[test]
fn trails_webhook_becomes_trail_items() {
    let lab = start();
    let snapshot = json!({
        "type": "snapshot",
        "sent_at": T0,
        "firing": [
            {"key": "web/errors", "source": "web", "alert": "errors", "severity": "severe", "title": "Errors are high", "body": "12 a minute", "labels": {"queue": "email"}, "value": 12, "since": T0},
            {"key": "web/slow", "source": "web", "alert": "slow", "severity": "warning", "title": "Pages are slow"}
        ]
    });
    let r = lab.post("/v1/trail", &snapshot);
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json, json!({"ok": true}));
    let inbox = lab.get("/v1/inbox").json;
    let severe = &inbox["alerts"]["open"][0];
    assert_eq!(severe["kind"], "trail.severe");
    assert_eq!(severe["key"], "web/errors");
    assert_eq!(severe["title"], "Errors are high");
    assert_eq!(
        severe["labels"],
        json!({"queue": "email", "trail_alert": "errors", "trail_source": "web"})
    );
    assert_eq!(severe["url"], "/");
    let warning = &inbox["activity"]["items"][0];
    assert_eq!(warning["kind"], "trail.warning");
    let stale = lab.post(
        "/v1/trail",
        &json!({"type": "snapshot", "sent_at": T0 - HOUR, "firing": []}),
    );
    assert_eq!(stale.status, 409);
    lab.later(SECOND);
    let empty = lab.post(
        "/v1/trail",
        &json!({"type": "snapshot", "sent_at": T0 + SECOND, "firing": []}),
    );
    assert_eq!(empty.status, 200);
    assert!(
        lab.get("/v1/inbox").json["alerts"]["open"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let notice = json!({"type": "notice", "id": "rule-1/evt-9", "source": "web", "alert": "crash", "severity": "severe", "title": "The app crashed", "body": "Twice", "ts": T0, "event": {}});
    let first = lab.post("/v1/trail", &notice).json;
    assert_eq!(first["repeat"], false);
    let item = lab.item(&first["item"]);
    assert_eq!(item["kind"], "trail.severe_notice");
    assert_eq!(item["alert"]["one_off"], true);
    let again = lab.post("/v1/trail", &notice).json;
    assert_eq!(
        (again["repeat"].clone(), again["item"].clone()),
        (json!(true), first["item"].clone())
    );
    let mut late = notice.clone();
    late["id"] = json!("rule-1/evt-10");
    late["ts"] = json!(T0 - 2 * HOUR);
    let late = lab.post("/v1/trail", &late).json;
    assert_eq!(lab.item(&late["item"])["push"]["reason"], "late");
    let malformed = lab.post(
        "/v1/trail",
        &json!({"type": "notice", "id": "", "severity": "loud", "title": "x", "ts": 1}),
    );
    assert_eq!(malformed.status, 400);
    assert_eq!(lab.post("/v1/trail", &json!({"type": "other"})).status, 400);
    let proxied = raw(
        lab.addr(),
        "POST",
        "/v1/trail",
        &[JSON, ("X-Forwarded-For", "192.0.2.1")],
        Some(&notice),
    );
    assert_eq!(proxied.status, 403);
}

#[test]
fn live_activities_pass_straight_through() {
    let lab = start();
    lab.device();
    let token = "ab".repeat(40);
    let body = json!({
        "token": token,
        "event": "update",
        "contentState": {"step": "Reading the logs", "progress": 0.4},
        "timestamp": 1_794_571_200,
        "staleDate": 1_794_572_100,
        "alert": {"title": "Still working", "body": "Two steps left"},
        "priority": 5,
        "extra": "ignored"
    });
    let r = lab.post("/v1/live-activity", &body);
    assert_eq!(r.status, 200, "{:?}", r.json);
    let sent = lab.apple.take();
    assert_eq!(sent.len(), 1);
    let push = &sent[0];
    assert_eq!(push.path, format!("/3/device/{token}"));
    assert_eq!(push.headers["apns-push-type"], "liveactivity");
    assert_eq!(
        push.headers["apns-topic"],
        "com.example.lab.push-type.liveactivity"
    );
    assert_eq!(push.headers["apns-priority"], "5");
    assert!(!push.headers.contains_key("apns-collapse-id"));
    assert_eq!(
        push.body,
        json!({"aps": {
            "timestamp": 1_794_571_200,
            "event": "update",
            "content-state": {"step": "Reading the logs", "progress": 0.4},
            "stale-date": 1_794_572_100,
            "alert": {"title": "Still working", "body": "Two steps left", "sound": "default"}
        }})
    );
    lab.apple.answer(400, "BadDeviceToken");
    let r = lab.post("/v1/live-activity", &body);
    assert_eq!(r.status, 200);
    let sent = lab.apple.take();
    assert_eq!(
        sent.iter().map(|s| s.environment).collect::<Vec<_>>(),
        [Environment::Production, Environment::Sandbox],
        "the latest device's environment, then the other"
    );
    lab.apple.answer(410, "Unregistered");
    assert_eq!(lab.post("/v1/live-activity", &body).status, 410);
    assert_eq!(
        lab.get("/v1/devices").json["devices"].as_array().unwrap().len(),
        1,
        "an activity's dead token deletes no device"
    );
    lab.apple.answer(500, "InternalServerError");
    assert_eq!(lab.post("/v1/live-activity", &body).status, 502);
    let mut big = body.clone();
    big["contentState"] = json!({"text": "x".repeat(5000)});
    assert_eq!(lab.post("/v1/live-activity", &big).status, 400);
    let mut bad = body.clone();
    bad["event"] = json!("start");
    assert_eq!(lab.post("/v1/live-activity", &bad).status, 400);
    let proxied = raw(
        lab.addr(),
        "POST",
        "/v1/live-activity",
        &[JSON, ("Forwarded", "for=192.0.2.1")],
        Some(&body),
    );
    assert_eq!(proxied.status, 403);

    let unconfigured = start_with("", false);
    assert_eq!(unconfigured.post("/v1/live-activity", &body).status, 503);
}

#[test]
fn without_a_key_pushes_fail_unconfigured() {
    let lab = start_with("", false);
    lab.device();
    let id = lab.down().json["item"]["id"].clone();
    lab.tick();
    let item = lab.item(&id);
    assert_eq!(
        (item["push"]["status"].clone(), item["push"]["reason"].clone()),
        (json!("failed"), json!("unconfigured"))
    );
    assert_eq!(lab.get("/v1/health").json["apns"], "unconfigured");
}

#[test]
fn dead_tokens_are_pruned_and_retries_are_scheduled() {
    let lab = start();
    lab.device();
    lab.apple.answer(503, "ServiceUnavailable");
    let id = lab.down().json["item"]["id"].clone();
    lab.tick();
    assert_eq!(lab.apple.take().len(), 1);
    assert_eq!(lab.item(&id)["push"]["status"], "queued", "a 503 is retried");
    lab.later(10 * SECOND);
    lab.apple.answer(410, "Unregistered");
    lab.tick();
    assert_eq!(lab.apple.take().len(), 1);
    let item = lab.item(&id);
    assert_eq!(
        (item["push"]["status"].clone(), item["push"]["reason"].clone()),
        (json!("failed"), json!("rejected"))
    );
    assert!(
        lab.get("/v1/devices").json["devices"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the dead token is deleted"
    );
    let health = lab.get("/v1/health").json;
    assert_eq!(health["devices"], 0);
    lab.later(MINUTE);
    let note = lab
        .post("/v1/events", &json!({"kind": "lab.note", "slots": {"text": "x"}}))
        .json["item"]["id"]
        .clone();
    lab.tick();
    assert_eq!(lab.item(&note)["push"]["reason"], "no_devices");
}

#[test]
fn a_device_that_moved_environment_is_updated() {
    let lab = start();
    lab.device();
    lab.apple.answer(400, "BadDeviceToken");
    lab.down();
    lab.tick();
    let sent = lab.apple.take();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].environment, Environment::Sandbox);
    assert_eq!(
        lab.get("/v1/devices").json["devices"][0]["environment"],
        "sandbox"
    );
}

/// The keys of the spec's examples: the inbox and the item.
#[test]
fn the_inbox_has_the_specs_shape_and_an_etag() {
    let lab = start();
    lab.down();
    lab.post(
        "/v1/events",
        &json!({"kind": "lab.note", "slots": {"text": "Hello"}}),
    );
    lab.post(
        "/v1/presence",
        &json!({"client": "web", "device": "web-1", "active": true}),
    );
    let r = lab.get("/v1/inbox");
    assert_eq!(r.status, 200);
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    let mut top = keys(&r.json);
    top.sort();
    assert_eq!(
        top,
        [
            "activity",
            "alerts",
            "dot",
            "held",
            "needs_you",
            "now",
            "since_last_visit",
            "version"
        ]
    );
    assert_eq!(r.json["dot"], true);
    assert_eq!(keys(&r.json["since_last_visit"]).len(), 4);
    let item = &r.json["needs_you"][0];
    let mut item_keys = keys(item);
    item_keys.sort();
    let mut spec = vec![
        "id",
        "version",
        "kind",
        "class",
        "source",
        "source_name",
        "icon",
        "key",
        "title",
        "body",
        "preview",
        "url",
        "context",
        "labels",
        "created",
        "updated",
        "state",
        "times",
        "alert",
        "actions",
        "seen",
        "collapse_id",
        "push",
    ];
    spec.sort_unstable();
    assert_eq!(item_keys, spec);
    let mut push = keys(&item["push"]);
    push.sort();
    assert_eq!(
        push,
        ["at", "covered_by", "level", "reason", "release_at", "status"]
    );
    assert_eq!(keys(&item["times"]).len(), 6);
    assert_eq!(keys(&item["alert"]).len(), 5);
    let etag = r.header("ETag").unwrap().to_string();
    assert_eq!(etag, format!("W/\"{}\"", r.json["version"]));
    let same = raw(lab.addr(), "GET", "/v1/inbox", &[("If-None-Match", &etag)], None);
    assert_eq!(same.status, 304);
    assert_eq!(same.json, Value::Null);
    lab.post("/v1/seen", &json!({"through": 100}));
    let changed = raw(lab.addr(), "GET", "/v1/inbox", &[("If-None-Match", &etag)], None);
    assert_eq!(changed.status, 200);
    assert_eq!(lab.get("/v1/inbox?limit=0").status, 400);
    let typo = lab.get("/v1/inbox?class=updte");
    assert_eq!(typo.status, 400);
    assert_eq!(typo.json["error"]["fix"], "did you mean `update`?");
    let page = lab.get("/v1/inbox?class=update,record&limit=1&source=lab").json;
    assert_eq!(page["activity"]["items"].as_array().unwrap().len(), 1);
}

#[test]
fn access_rules_hold() {
    let lab = start_with("[auth]\nproducer_token = \"0123456789abcdef0123\"", true);
    let event = json!({"kind": "lab.note", "slots": {"text": "x"}});
    let proxied = [JSON, ("X-Forwarded-For", "192.0.2.1")];
    let r = raw(lab.addr(), "POST", "/v1/events", &proxied, Some(&event));
    assert_eq!(r.status, 403);
    assert!(r.json["error"]["fix"].as_str().unwrap().contains("Bearer"));
    let with_token = [
        JSON,
        ("X-Forwarded-For", "192.0.2.1"),
        ("Authorization", "Bearer 0123456789abcdef0123"),
    ];
    assert_eq!(
        raw(lab.addr(), "POST", "/v1/events", &with_token, Some(&event)).status,
        200
    );
    let wrong_token = [
        JSON,
        ("X-Forwarded-For", "192.0.2.1"),
        ("Authorization", "Bearer 0123456789abcdef0124"),
    ];
    assert_eq!(
        raw(lab.addr(), "POST", "/v1/events", &wrong_token, Some(&event)).status,
        403
    );
    let untyped = raw(
        lab.addr(),
        "POST",
        "/v1/events",
        &[("Content-Type", "text/plain")],
        Some(&event),
    );
    assert_eq!(untyped.status, 403);

    // Client routes: same-origin JSON through the proxy, refused from another site.
    let beat = json!({"client": "web", "device": "web-1", "active": false});
    let host = lab.addr().to_string();
    let own = format!("http://{host}");
    let same = [
        JSON,
        ("X-Forwarded-For", "192.0.2.1"),
        ("Origin", own.as_str()),
        ("Sec-Fetch-Site", "same-origin"),
    ];
    assert_eq!(
        raw(lab.addr(), "POST", "/v1/presence", &same, Some(&beat)).status,
        200
    );
    let cross = [
        JSON,
        ("X-Forwarded-For", "192.0.2.1"),
        ("Origin", "https://evil.example.com"),
    ];
    assert_eq!(
        raw(lab.addr(), "POST", "/v1/presence", &cross, Some(&beat)).status,
        403
    );
    let site = [
        JSON,
        ("X-Forwarded-For", "192.0.2.1"),
        ("Sec-Fetch-Site", "cross-site"),
    ];
    assert_eq!(
        raw(lab.addr(), "POST", "/v1/presence", &site, Some(&beat)).status,
        403
    );
    let app = [JSON, ("X-Forwarded-For", "192.0.2.1")];
    assert_eq!(
        raw(lab.addr(), "POST", "/v1/presence", &app, Some(&beat)).status,
        200,
        "the iPhone app sends no Origin"
    );
    // Reads are open.
    assert_eq!(raw(lab.addr(), "GET", "/v1/inbox", &proxied, None).status, 200);
}

#[test]
fn routes_unknown_fields_and_methods_say_what_to_do() {
    let lab = start();
    let index = lab.get("/v1").json;
    assert_eq!(index["version"], beacon_server::VERSION);
    let routes = index["routes"].as_array().unwrap();
    assert_eq!(routes.len(), beacon_server::ROUTES.len());
    assert!(routes.iter().any(|r| {
        r["path"] == "/v1/inbox"
            && r["params"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["name"] == "since" && p["in"] == "query")
    }));
    let missing = lab.get("/v1/inbx");
    assert_eq!(missing.status, 404);
    assert!(
        missing.json["error"]["fix"]
            .as_str()
            .unwrap()
            .contains("did you mean /v1/inbox?")
    );
    let method = raw(lab.addr(), "DELETE", "/v1/inbox", &[], None);
    assert_eq!(method.status, 405);
    assert_eq!(method.header("Allow"), Some("GET"));
    let typo = lab.post("/v1/events", &json!({"knd": "lab.note"}));
    assert_eq!(typo.status, 400);
    assert_eq!(typo.json["error"]["at"], "event knd");
    assert_eq!(typo.json["error"]["fix"], "did you mean `kind`?");
    let undeclared = lab.post("/v1/events", &json!({"kind": "lab.nte"}));
    assert_eq!(undeclared.status, 422);
    assert!(
        undeclared.json["error"]["fix"]
            .as_str()
            .unwrap()
            .contains("did you mean `lab.note`?")
    );
    assert_eq!(lab.post("/v1/events", &json!([1])).status, 400);
    let not_json = raw(lab.addr(), "POST", "/v1/events", &[JSON], None);
    assert_eq!(not_json.status, 400);
    assert_eq!(lab.get("/v1/items/nope").status, 404);
    assert_eq!(lab.get("/v1/items/12345").status, 404);
    let reserved = lab.put(
        "/v1/registry/other",
        &beacon_core::decl::parse(KINDS, &Default::default())
            .unwrap()
            .to_json(),
    );
    assert_eq!(reserved.status, 409);
    assert_eq!(lab.get("/v1/registry/lab").json["source"], "lab");
    assert_eq!(lab.get("/v1/registry/nobody").status, 404);
    let health = lab.get("/v1/health").json;
    assert_eq!(
        (
            health["ok"].clone(),
            health["apns"].clone(),
            health["push"].clone()
        ),
        (json!(true), json!("ok"), json!(true))
    );
}

#[test]
fn a_body_over_the_limit_is_413() {
    let lab = start_with("[limits]\nbody_bytes = 4096", true);
    let big = json!({"kind": "lab.note", "slots": {"text": "x".repeat(5000)}});
    assert_eq!(lab.post("/v1/events", &big).status, 413);
}

#[test]
fn idempotent_events_answer_again() {
    let lab = start();
    let body = json!({"kind": "lab.note", "slots": {"text": "x"}});
    let first = raw(
        lab.addr(),
        "POST",
        "/v1/events",
        &[JSON, ("Idempotency-Key", "a1")],
        Some(&body),
    )
    .json;
    let again = raw(
        lab.addr(),
        "POST",
        "/v1/events",
        &[JSON, ("Idempotency-Key", "a1")],
        Some(&body),
    )
    .json;
    assert_eq!(again["repeat"], true);
    assert_eq!(again["item"]["id"], first["item"]["id"]);
}

#[test]
fn devices_register_and_are_forgotten_and_tokens_stay_out_of_beacon_db() {
    let lab = start();
    lab.device();
    let list = lab.get("/v1/devices").json;
    assert_eq!(
        list["devices"][0]["device"],
        format!("…{}", &TOKEN[TOKEN.len() - 6..])
    );
    assert!(!list.to_string().contains(TOKEN));
    let bad = lab.post(
        "/v1/devices",
        &json!({"token": "XYZ", "environment": "production"}),
    );
    assert_eq!(bad.status, 400);
    let gone = raw(lab.addr(), "DELETE", &format!("/v1/devices/{TOKEN}"), &[], None);
    assert_eq!(gone.status, 200);
    let again = raw(lab.addr(), "DELETE", &format!("/v1/devices/{TOKEN}"), &[], None);
    assert_eq!(again.status, 404);
}

#[test]
fn the_job_thread_sends_a_push_without_being_asked_and_stops() {
    let lab = start_full("", true, true);
    lab.device();
    lab.down();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while lab.apple.sent.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        lab.apple.take().len(),
        1,
        "the job thread woke for the queued push"
    );
    let Lab { server, .. } = lab;
    server.stop();
}
