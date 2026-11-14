//! The command against a fake Beacon: what each command sends, and every way it refuses.

use crate::send::{read_time, words};
use crate::{Fail, run};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 2026-11-13 12:00 UTC.
const NOW: i64 = 1_794_571_200_000;

#[derive(Debug, Clone)]
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A server that records each request and answers from a script, then with `200 {}`.
struct Fake {
    url: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Fake {
    fn start(script: Vec<(u16, Value)>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            let mut script = script.into_iter();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let mut headers = Vec::new();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                        headers.push((name.to_string(), value.trim().to_string()));
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
                seen.lock().unwrap().push(Request {
                    method,
                    path,
                    headers,
                    body,
                });
                let (status, answer) = script.next().unwrap_or((200, json!({})));
                let text = answer.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        Fake { url, requests }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    fn last(&self) -> Request {
        self.requests().pop().expect("a request")
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn example(source: &str) -> String {
    root()
        .join("examples/homelab-ish")
        .join(source)
        .join("beacon")
        .display()
        .to_string()
}

fn beacon(args: &[&str]) -> Result<(), Fail> {
    run(args.iter().map(|a| a.to_string()).collect(), NOW)
}

/// Runs a command that must fail, and gives its exit status and every problem as one text.
fn refused(args: &[&str]) -> (u8, String) {
    match beacon(args) {
        Ok(()) => panic!("{args:?} should fail"),
        Err(fail) => (
            fail.code,
            fail.problems
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("beacon-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------- the command line ----------

#[test]
fn an_unknown_command_or_option_is_a_usage_error() {
    let (code, text) = refused(&["sned"]);
    assert_eq!(code, 2);
    assert!(text.contains("did you mean `beacon send`?"), "{text}");
    let (code, text) = refused(&["send", "backup.failed", "--kye", "x"]);
    assert_eq!(code, 2);
    assert!(text.contains("did you mean `--key`?"), "{text}");
    let (code, _) = refused(&["silence", "ad"]);
    assert_eq!(code, 2);
}

#[test]
fn serve_refuses_a_config_it_cant_use() {
    let (code, text) = refused(&["serve", "--config", "/nonexistent/beacon.toml"]);
    assert_eq!(code, 1);
    assert!(text.contains("can't read it"), "{text}");
    let dir = std::env::temp_dir().join(format!("beacon-cli-serve-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("beacon.toml");
    std::fs::write(&path, "lisen = \"127.0.0.1:1\"\n").unwrap();
    let (code, text) = refused(&["serve", "--config", path.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(text.contains("did you mean `listen`?"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fire_values_split_into_words() {
    assert_eq!(
        words(r#"backup.failed reason="The disk is full" last_ok=-1d"#).unwrap(),
        ["backup.failed", "reason=The disk is full", "last_ok=-1d"]
    );
    assert_eq!(words("a '' b").unwrap(), ["a", "", "b"]);
    assert!(words("a 'b").is_err());
}

#[test]
fn times_read_as_the_spec_says() {
    assert_eq!(read_time("now", NOW), Some(NOW));
    assert_eq!(read_time("-15m", NOW), Some(NOW - 15 * 60_000));
    assert_eq!(read_time("+2h", NOW), Some(NOW + 2 * 3_600_000));
    assert_eq!(read_time("1794535200000", NOW), Some(1_794_535_200_000));
    assert_eq!(
        read_time("2026-11-13T02:00:00+00:00", NOW),
        Some(1_794_535_200_000)
    );
    assert_eq!(
        read_time("2026-11-13T02:00:00", NOW),
        None,
        "a time must say its offset"
    );
    assert_eq!(read_time("yesterday", NOW), None);
}

// ---------- check, gen, publish ----------

#[test]
fn check_reads_every_folder() {
    beacon(&[
        "check",
        &example("backup"),
        &example("ship"),
        &example("assistant"),
    ])
    .unwrap();
    let (code, text) = refused(&["check", &example("backup"), &example("backup")]);
    assert_eq!(code, 1);
    assert!(text.contains("source `backup` is also declared in"), "{text}");
    let (code, text) = refused(&["check", "/nonexistent/folder"]);
    assert_eq!(code, 1);
    assert!(text.contains("can't read it"), "{text}");
}

#[test]
fn check_runs_the_generators_checks_too() {
    let dir = scratch("clash");
    std::fs::write(
        dir.join("kinds.toml"),
        "source = \"ship\"\nname = \"Deploys\"\n[kinds.\"ship.pr_ready\"]\nclass = \"record\"\ntitle = \"Pull request ready\"\n[kinds.\"ship.pr.ready\"]\nclass = \"record\"\ntitle = \"Pull request is ready\"\n",
    )
    .unwrap();
    let (code, text) = refused(&["check", &dir.display().to_string()]);
    assert_eq!(code, 1);
    assert!(
        text.contains("both give the Python function `ship_pr_ready`"),
        "{text}"
    );
}

#[test]
fn gen_writes_and_then_checks_the_file() {
    let dir = scratch("gen");
    let file = dir.join("backup_kinds.py").display().to_string();
    let folder = example("backup");
    let (code, text) = refused(&["gen", &folder, "--lang", "python", "--out", &file, "--check"]);
    assert_eq!(code, 1);
    assert!(text.contains("does not exist"), "{text}");
    beacon(&["gen", &folder, "--lang", "python", "--out", &file]).unwrap();
    beacon(&["gen", &folder, "--lang", "python", "--out", &file, "--check"]).unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.starts_with("# Generated by `beacon gen`"));

    std::fs::write(&file, text.replace("fnv1a64 ", "fnv1a64 0")).unwrap();
    let (code, problem) = refused(&["gen", &folder, "--lang", "python", "--out", &file, "--check"]);
    assert_eq!(code, 1);
    assert!(
        problem.contains("generated from other declarations of `backup`"),
        "{problem}"
    );
    assert!(
        problem.contains(&format!("run `beacon gen {folder} --lang python --out {file}`")),
        "{problem}"
    );

    let (code, _) = refused(&["gen", &folder, "--lang", "swift"]);
    assert_eq!(code, 2);
    let (code, _) = refused(&["gen", &folder, "--lang", "ts", "--check"]);
    assert_eq!(code, 2);
}

#[test]
fn publish_puts_each_source_in_its_json_form() {
    let fake = Fake::start(vec![(200, json!({"ok": true})), (200, json!({"ok": true}))]);
    beacon(&[
        "publish",
        &example("backup"),
        &example("ship"),
        "--server",
        &fake.url,
    ])
    .unwrap();
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "PUT");
    assert_eq!(requests[0].path, "/v1/registry/backup");
    assert_eq!(requests[1].path, "/v1/registry/ship");
    assert_eq!(requests[0].body["kinds"]["backup.failed"]["throttle"], "10m");
    assert_eq!(requests[0].header("content-type"), Some("application/json"));
}

#[test]
fn a_trail_folder_is_checked_generated_and_published() {
    let trail = example("trail");
    beacon(&["check", &trail]).unwrap();
    beacon(&["gen", &trail, "--lang", "typescript"]).unwrap();
    let fake = Fake::start(vec![]);
    beacon(&["publish", &trail, "--server", &fake.url]).unwrap();
    assert_eq!(fake.last().path, "/v1/registry/trail");
    assert!(fake.last().body["kinds"]["trail.disk_full"].is_object());
}

#[test]
fn publish_keep_asks_beacon_to_keep_the_other_kinds() {
    let fake = Fake::start(vec![(
        200,
        json!({"ok": true, "kinds": 5, "kept": ["backup.old"], "closed": 0}),
    )]);
    beacon(&["publish", &example("backup"), "--keep", "--server", &fake.url]).unwrap();
    assert_eq!(fake.last().path, "/v1/registry/backup?keep=true");
    let fake = Fake::start(vec![]);
    beacon(&["publish", &example("backup"), "--server", &fake.url]).unwrap();
    assert_eq!(fake.last().path, "/v1/registry/backup");
}

#[test]
fn publish_reports_what_beacon_refused() {
    let problem = json!({"at": "kinds.\"backup.failed\".title", "message": "`Backups` is a banned word", "fix": "leave it out"});
    let fake = Fake::start(vec![(400, json!({"error": problem, "problems": [problem]}))]);
    let (code, text) = refused(&["publish", &example("backup"), "--server", &fake.url]);
    assert_eq!(code, 1);
    assert!(text.contains("banned word"), "{text}");
    assert!(text.contains("fix: leave it out"), "{text}");
}

// ---------- send and resolve ----------

#[test]
fn send_checks_against_a_local_folder_and_posts_the_event() {
    let fake = Fake::start(vec![(200, json!({"outcome": "opened", "item": {"id": 4183}}))]);
    let backup = example("backup");
    beacon(&[
        "send",
        "backup.failed",
        "reason=The disk is full",
        "last_ok=-1d",
        "--key",
        "nightly",
        "--label",
        "service=backup",
        "--decls",
        &backup,
        "--server",
        &fake.url,
    ])
    .unwrap();
    let request = fake.last();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/events");
    assert_eq!(
        request.body,
        json!({
            "kind": "backup.failed",
            "key": "nightly",
            "facts": {"last_ok": NOW - 86_400_000},
            "slots": {"reason": "The disk is full"},
            "labels": {"service": "backup"},
        })
    );
    let key = request.header("idempotency-key").unwrap();
    assert_eq!(key.len(), 32);
    assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[test]
fn send_reads_the_kinds_from_the_server() {
    let decl = beacon_core::decl::load(Path::new(&example("ship")), &Default::default()).unwrap();
    let fake = Fake::start(vec![
        (200, decl.to_json()),
        (200, json!({"outcome": "recorded", "item": {"id": 9}})),
    ]);
    beacon(&[
        "send",
        "ship.drift",
        "count=3",
        "oldest=2026-11-13T02:00:00+00:00",
        "--server",
        &fake.url,
    ])
    .unwrap();
    let requests = fake.requests();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/v1/registry/ship");
    assert_eq!(
        requests[1].body["facts"],
        json!({"count": 3, "oldest": 1_794_535_200_000_i64})
    );
}

#[test]
fn an_unpublished_source_says_to_publish_it() {
    let fake = Fake::start(vec![(
        404,
        json!({"error": {"at": "source", "message": "no", "fix": ""}}),
    )]);
    let (code, text) = refused(&["send", "mail.sent", "--server", &fake.url]);
    assert_eq!(code, 1);
    assert!(text.contains("Beacon has no declarations for `mail`"), "{text}");
    assert!(text.contains("beacon publish"), "{text}");
}

#[test]
fn send_refuses_what_the_kind_does_not_declare() {
    let backup = example("backup");
    let d = ["--decls", backup.as_str(), "--server", "http://127.0.0.1:9"];
    let with = |args: &[&str]| refused(&[args, &d[..]].concat());

    let (code, text) = with(&["send", "backup.faild", "reason=x", "last_ok=now"]);
    assert_eq!(code, 1);
    assert!(text.contains("`backup.faild` is not a declared kind"), "{text}");
    assert!(
        text.contains(
            "did you mean `backup.failed`? Declare a new kind in kinds.toml and run `beacon publish`"
        ),
        "{text}"
    );

    let (_, text) = with(&["send", "backup.failed", "reason=x", "last_okay=now"]);
    assert!(
        text.contains("`backup.failed` has no fact or slot `last_okay`"),
        "{text}"
    );
    assert!(text.contains("did you mean `last_ok`?"), "{text}");
    assert!(text.contains("needs the fact `last_ok` (time)"), "{text}");

    let (_, text) = with(&["send", "backup.failed", "last_ok=now"]);
    assert!(text.contains("needs the slot `reason`"), "{text}");
    assert!(text.contains("reason=\"\" sends an empty slot"), "{text}");

    let (_, text) = with(&["send", "backup.failed", "reason=", "last_ok=yesterday"]);
    assert!(text.contains("`yesterday` doesn't read as a time fact"), "{text}");
    assert!(text.contains("fix: write last_ok=now"), "{text}");
    assert!(!text.contains("needs the fact"), "reported once: {text}");

    let (_, text) = with(&["send", "backup.space", "used_percent=lots"]);
    assert!(text.contains("`lots` doesn't read as an int fact"), "{text}");

    let (code, text) = with(&["send", "backup.failed", "reason"]);
    assert_eq!(code, 2);
    assert!(text.contains("is not name=value"), "{text}");
}

#[test]
fn context_is_required_where_declared_and_refused_elsewhere() {
    let assistant = example("assistant");
    let fake = Fake::start(vec![]);
    let (_, text) = refused(&[
        "send",
        "assistant.approval",
        "thread=42",
        "question=?",
        "--decls",
        &assistant,
    ]);
    assert!(text.contains("needs a viewing context"), "{text}");
    let (_, text) = refused(&[
        "send",
        "assistant.reminder",
        "text=x",
        "--context",
        "c",
        "--decls",
        &assistant,
    ]);
    assert!(text.contains("doesn't declare `context = true`"), "{text}");
    beacon(&[
        "send",
        "assistant.approval",
        "thread=42",
        "question=Delete the old logs?",
        "--context",
        "assistant:thread:42",
        "--open",
        "/assistant/threads/42",
        "--decls",
        &assistant,
        "--server",
        &fake.url,
    ])
    .unwrap();
    let body = fake.last().body;
    assert_eq!(body["context"], "assistant:thread:42");
    assert_eq!(body["url"], "/assistant/threads/42");
}

#[test]
fn resolve_sends_the_state_and_no_values() {
    let backup = example("backup");
    let fake = Fake::start(vec![(200, json!({"outcome": "resolved", "item": {"id": 4183}}))]);
    beacon(&[
        "resolve",
        "backup.failed",
        "--key",
        "nightly",
        "--decls",
        &backup,
        "--server",
        &fake.url,
    ])
    .unwrap();
    assert_eq!(
        fake.last().body,
        json!({"kind": "backup.failed", "key": "nightly", "state": "resolved"})
    );

    let (code, text) = refused(&["resolve", "backup.finished", "--decls", &backup]);
    assert_eq!(code, 1);
    assert!(text.contains("has no state to resolve"), "{text}");
}

// ---------- snapshot ----------

#[test]
fn snapshot_sends_each_fire_as_an_entry() {
    let backup = example("backup");
    let fake = Fake::start(vec![(
        200,
        json!({"ok": true, "opened": [4183], "resolved": [4077]}),
    )]);
    beacon(&[
        "snapshot",
        "backup",
        "--fire",
        "backup.space used_percent=96",
        "--fire",
        "backup.failed key=nightly reason='The disk is full' last_ok=-1d",
        "--kinds",
        "backup.space,backup.failed",
        "--stale-after",
        "30m",
        "--label",
        "service=backup",
        "--decls",
        &backup,
        "--server",
        &fake.url,
    ])
    .unwrap();
    let request = fake.last();
    assert_eq!(request.path, "/v1/snapshots");
    assert_eq!(
        request.body,
        json!({
            "source": "backup",
            "observed": true,
            "stale_after": "30m",
            "sent_at": NOW,
            "kinds": ["backup.space", "backup.failed"],
            "firing": [
                {"kind": "backup.space", "key": "", "facts": {"used_percent": 96}, "slots": {}, "labels": {"service": "backup"}},
                {"kind": "backup.failed", "key": "nightly", "facts": {"last_ok": NOW - 86_400_000}, "slots": {"reason": "The disk is full"}, "labels": {"service": "backup"}},
            ],
        })
    );
}

#[test]
fn snapshot_refuses_records_and_unknown_kinds() {
    let backup = example("backup");
    let (code, text) = refused(&[
        "snapshot",
        "backup",
        "--fire",
        "backup.finished size=1 took=1",
        "--kinds",
        "backup.finished,backup.spase",
        "--decls",
        &backup,
    ]);
    assert_eq!(code, 1);
    assert!(text.contains("a snapshot lists alerts and actions"), "{text}");
    assert!(text.contains("a snapshot covers alerts and actions"), "{text}");
    assert!(text.contains("did you mean `backup.space`?"), "{text}");
}

#[test]
fn an_unobserved_snapshot_needs_no_declarations() {
    let fake = Fake::start(vec![(200, json!({"ok": true, "observed": false}))]);
    beacon(&[
        "snapshot",
        "backup",
        "--unobserved",
        "The bucket's status page didn't load.",
        "--server",
        &fake.url,
    ])
    .unwrap();
    let request = fake.last();
    assert_eq!(
        request.body,
        json!({"source": "backup", "observed": false, "reason": "The bucket's status page didn't load.", "sent_at": NOW})
    );
    assert_eq!(fake.requests().len(), 1);
}

// ---------- silences ----------

#[test]
fn silence_add_extend_end_and_list() {
    let silence = json!({"id": 12, "match": {"source": null, "kind": null, "key": null, "labels": {"service": "dashboard"}}, "reason": "Deploying dashboard", "until": NOW + 1_800_000});
    let fake = Fake::start(vec![
        (201, silence.clone()),
        (200, silence.clone()),
        (200, silence.clone()),
        (200, json!({"silences": [silence]})),
    ]);
    let s = fake.url.as_str();
    beacon(&[
        "silence",
        "add",
        "--label",
        "service=dashboard",
        "--for",
        "30m",
        "--reason",
        "Deploying dashboard",
        "--server",
        s,
    ])
    .unwrap();
    beacon(&["silence", "extend", "12", "--until", "+5m", "--server", s]).unwrap();
    beacon(&["silence", "end", "12", "--server", s]).unwrap();
    beacon(&["silence", "list", "--all", "--server", s]).unwrap();
    let requests = fake.requests();
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/silences");
    assert_eq!(
        requests[0].body,
        json!({"match": {"labels": {"service": "dashboard"}}, "for": "30m", "reason": "Deploying dashboard", "client": "api"})
    );
    assert_eq!(
        (requests[1].method.as_str(), requests[1].path.as_str()),
        ("PATCH", "/v1/silences/12")
    );
    assert_eq!(requests[1].body, json!({"until": NOW + 300_000}));
    assert_eq!(
        (requests[2].method.as_str(), requests[2].path.as_str()),
        ("DELETE", "/v1/silences/12")
    );
    assert_eq!(
        (requests[3].method.as_str(), requests[3].path.as_str()),
        ("GET", "/v1/silences?all=1")
    );
}

#[test]
fn silence_add_refuses_an_incomplete_silence() {
    let (code, text) = refused(&["silence", "add", "--for", "30m", "--reason", "x"]);
    assert_eq!(code, 2);
    assert!(text.contains("must match something"), "{text}");
    let (_, text) = refused(&[
        "silence", "add", "--key", "nightly", "--for", "30m", "--reason", "x",
    ]);
    assert!(text.contains("a key belongs to a kind"), "{text}");
    let (_, text) = refused(&["silence", "add", "--source", "backup", "--reason", "x"]);
    assert!(text.contains("exactly one of --for and --until"), "{text}");
    let (_, text) = refused(&["silence", "add", "--source", "backup", "--for", "30m"]);
    assert!(text.contains("needs a reason"), "{text}");
    let (code, text) = refused(&["silence", "end", "twelve"]);
    assert_eq!(code, 2);
    assert!(text.contains("not a silence id"), "{text}");
}

// ---------- retries and exit status ----------

#[test]
fn a_5xx_is_retried_with_one_key_then_exits_3() {
    let fake = Fake::start(vec![
        (503, json!({})),
        (502, json!({})),
        (
            500,
            json!({"error": {"at": "storage", "message": "busy", "fix": "retry"}}),
        ),
    ]);
    let (code, text) = refused(&[
        "snapshot",
        "backup",
        "--unobserved",
        "down",
        "--server",
        &fake.url,
    ]);
    assert_eq!(code, 3);
    assert!(text.contains("storage: busy"), "{text}");
    let requests = fake.requests();
    assert_eq!(requests.len(), 3);
    let keys: std::collections::BTreeSet<_> = requests
        .iter()
        .filter_map(|r| r.header("idempotency-key"))
        .collect();
    assert_eq!(keys.len(), 1);
}

#[test]
fn a_refusal_is_not_retried_and_exits_1() {
    let problem = json!({"at": "snapshot firing[0] facts.used_percent", "message": "an int fact can't be a string", "fix": "send a whole number"});
    let fake = Fake::start(vec![(422, json!({"error": problem, "problems": [problem]}))]);
    let (code, text) = refused(&[
        "snapshot",
        "backup",
        "--unobserved",
        "down",
        "--server",
        &fake.url,
    ]);
    assert_eq!(code, 1);
    assert!(
        text.contains(
            "snapshot firing[0] facts.used_percent: an int fact can't be a string\n  fix: send a whole number"
        ),
        "{text}"
    );
    assert_eq!(fake.requests().len(), 1);
}

#[test]
fn no_answer_exits_3() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let (code, text) = refused(&["silence", "list", "--server", &url]);
    assert_eq!(code, 3);
    assert!(text.contains("no answer from Beacon"), "{text}");
}

// ---------- the spool ----------

/// The request files a spool holds, oldest first.
fn spooled(dir: &Path) -> Vec<Value> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".json") && !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
        .iter()
        .map(|n| serde_json::from_slice(&std::fs::read(dir.join(n)).unwrap()).unwrap())
        .collect()
}

/// An address nothing listens on.
const NOBODY: &str = "http://127.0.0.1:9";

#[test]
fn a_send_beacon_cant_take_is_spooled_and_flush_sends_it_with_its_key() {
    let dir = scratch("spool-send");
    let spool = dir.join("spool");
    let spool = spool.to_str().unwrap();
    let fake = Fake::start(vec![(503, json!({})), (503, json!({})), (503, json!({}))]);
    let backup = example("backup");
    let send = |server: &str| {
        beacon(&[
            "send",
            "backup.space",
            "used_percent=96",
            "--decls",
            &backup,
            "--spool",
            spool,
            "--server",
            server,
        ])
    };
    send(&fake.url).unwrap();
    let [file] = spooled(Path::new(spool)).try_into().unwrap();
    assert_eq!(file["path"], "/v1/events");
    assert_eq!(file["queued_at"], NOW);
    assert_eq!(file["body"]["kind"], "backup.space");
    let key = fake.requests()[0].header("idempotency-key").unwrap().to_string();
    assert_eq!(file["key"], key.as_str());

    // No answer at all is queued too, behind the first.
    send(NOBODY).unwrap();
    assert_eq!(spooled(Path::new(spool)).len(), 2);
    let (code, text) = refused(&["flush", "--spool", spool, "--server", NOBODY]);
    assert_eq!(code, 3);
    assert!(text.contains("2 still wait"), "{text}");

    // Beacon is back: the oldest goes first, with the key it was first sent with.
    let fake = Fake::start(vec![]);
    beacon(&["flush", "--spool", spool, "--server", &fake.url]).unwrap();
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].header("idempotency-key"), Some(key.as_str()));
    assert_eq!(requests[0].body["facts"], json!({"used_percent": 96}));
    assert!(spooled(Path::new(spool)).is_empty());
}

#[test]
fn a_flush_drops_a_refusal_and_whatever_is_too_old_and_reads_any_clients_files() {
    let dir = scratch("spool-flush");
    // Written as the Python and TypeScript emitters write them (spec/clients.md, "The spool").
    let file = |name: &str, path: &str, queued_at: i64, body: Value| {
        let request = json!({"path": path, "key": "0".repeat(32), "queued_at": queued_at, "body": body});
        std::fs::write(dir.join(name), request.to_string()).unwrap();
    };
    file(
        "001794484700000-000000-aaaaaaaa.json",
        "/v1/events",
        NOW - 86_400_001,
        json!({"kind": "backup.space", "key": "old"}),
    );
    file(
        "001794571100000-000000-bbbbbbbb.json",
        "/v1/snapshots",
        NOW - 100_000,
        json!({"source": "backup", "observed": true, "firing": [], "sent_at": NOW - 300_001}),
    );
    file(
        "001794571190000-000000-cccccccc.json",
        "/v1/events",
        NOW - 10_000,
        json!({"kind": "backup.faild", "key": ""}),
    );
    file(
        "001794571199000-000000-dddddddd.json",
        "/v1/events",
        NOW - 1_000,
        json!({"kind": "backup.space", "key": "new"}),
    );
    std::fs::write(dir.join("001794571199500-000000-eeeeeeee.json"), "not json").unwrap();
    let fake = Fake::start(vec![(422, json!({})), (200, json!({}))]);
    let dir_text = dir.to_str().unwrap();
    beacon(&["flush", "--spool", dir_text, "--server", &fake.url, "--json"]).unwrap();
    let requests = fake.requests();
    assert_eq!(requests.len(), 2, "only the two still in time are sent");
    assert_eq!(requests[0].body["kind"], "backup.faild");
    assert_eq!(requests[1].body["key"], "new");
    assert_eq!(
        requests[1].header("idempotency-key"),
        Some("0".repeat(32).as_str())
    );
    assert!(spooled(&dir).is_empty());
}

#[test]
fn a_refusal_is_never_spooled() {
    let dir = scratch("spool-refused");
    let fake = Fake::start(vec![(
        422,
        json!({"error": {"at": "event", "message": "no", "fix": ""}}),
    )]);
    let (code, _) = refused(&[
        "send",
        "backup.space",
        "used_percent=96",
        "--decls",
        &example("backup"),
        "--spool",
        dir.to_str().unwrap(),
        "--server",
        &fake.url,
    ]);
    assert_eq!(code, 1);
    assert!(spooled(&dir).is_empty());
    let (code, _) = refused(&["flush"]);
    assert_eq!(code, 2, "flush needs a spool");
}

#[test]
fn with_a_spool_the_kinds_beacon_last_gave_check_a_send_while_it_is_away() {
    let dir = scratch("spool-kinds");
    let spool = dir.to_str().unwrap();
    let decl = beacon_core::decl::load(Path::new(&example("ship")), &Default::default()).unwrap();
    let fake = Fake::start(vec![(200, decl.to_json()), (200, json!({"outcome": "recorded"}))]);
    let send = |server: &str| {
        beacon(&[
            "send",
            "ship.drift",
            "count=3",
            "oldest=-1h",
            "--spool",
            spool,
            "--server",
            server,
        ])
    };
    send(&fake.url).unwrap();
    send(NOBODY).unwrap();
    let [file] = spooled(&dir).try_into().unwrap();
    assert_eq!(file["body"]["facts"]["count"], 3);
    // Not a value it can check: refused here, not queued.
    let (code, _) = refused(&[
        "send",
        "ship.drift",
        "count=many",
        "--spool",
        spool,
        "--server",
        NOBODY,
    ]);
    assert_eq!(code, 1);
}

#[test]
fn a_newer_snapshot_replaces_a_spooled_one_of_the_same_kinds() {
    let dir = scratch("spool-snapshot");
    let spool = dir.to_str().unwrap();
    let backup = example("backup");
    let snapshot = |fire: &str, kinds: &str| {
        let mut args = vec!["snapshot", "backup", "--decls", &backup];
        if !fire.is_empty() {
            args.extend(["--fire", fire]);
        }
        if !kinds.is_empty() {
            args.extend(["--kinds", kinds]);
        }
        args.extend(["--spool", spool, "--server", NOBODY]);
        beacon(&args).unwrap();
    };
    snapshot("backup.space used_percent=96", "");
    snapshot("backup.space used_percent=97", "backup.space");
    snapshot("", "");
    let files = spooled(&dir);
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["body"]["kinds"], json!(["backup.space"]));
    assert_eq!(files[1]["body"]["firing"], json!([]));
}

#[test]
fn a_snapshot_whose_replacement_cant_be_written_is_kept() {
    let dir = scratch("spool-replace-fails");
    let spool = dir.to_str().unwrap();
    let backup = example("backup");
    let snapshot = |fire: &[&str]| {
        let mut args = vec!["snapshot", "backup", "--decls", &backup];
        args.extend(fire);
        args.extend(["--spool", spool, "--server", NOBODY]);
        args.into_iter().map(str::to_string).collect::<Vec<_>>()
    };
    let first = snapshot(&["--fire", "backup.space used_percent=96"]);
    beacon(&first.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
    crate::spool::fault::fail_next_write();
    let second = snapshot(&[]);
    let (code, text) = refused(&second.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(code, 3);
    assert!(text.contains("can't be written"), "{text}");
    let [kept] = spooled(&dir).try_into().unwrap();
    assert_eq!(kept["body"]["firing"].as_array().map(Vec::len), Some(1));
}

/// The request files' names, in order.
fn spool_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json") && !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

fn spooled_keys(dir: &Path) -> Vec<String> {
    spooled(dir)
        .iter()
        .map(|f| f["body"]["key"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn request_text(key: &str) -> String {
    json!({"path": "/v1/events", "key": "0".repeat(32), "queued_at": NOW, "body": {"kind": "backup.space", "key": key}})
        .to_string()
}

/// A request file as another process would have left it.
fn another_process_queued(dir: &Path, name: &str, key: &str) {
    std::fs::write(dir.join(name), request_text(key)).unwrap();
}

/// `beacon send backup.space --key <key>` while Beacon is away, through the spool in `dir`.
fn send_while_away(dir: &Path, key: &str) {
    beacon(&[
        "send",
        "backup.space",
        "used_percent=96",
        "--key",
        key,
        "--decls",
        &example("backup"),
        "--spool",
        dir.to_str().unwrap(),
        "--server",
        NOBODY,
    ])
    .unwrap();
}

#[test]
fn a_new_spool_file_sorts_after_any_already_there_whatever_wrote_it() {
    let dir = scratch("spool-ahead");
    // Another process's clock is ahead of this one, and it named its file the old way.
    another_process_queued(&dir, &format!("{:015}-000003-0a1b2c3d.json", NOW + 5000), "ahead");
    send_while_away(&dir, "mine");
    assert_eq!(spooled_keys(&dir), ["ahead", "mine"]);
    assert_eq!(
        spool_names(&dir).last().unwrap(),
        &format!("{:015}-000004.json", NOW + 5000)
    );
}

#[test]
fn within_a_millisecond_the_spool_sequence_goes_on_from_the_highest_there() {
    let dir = scratch("spool-sequence");
    another_process_queued(&dir, &format!("{NOW:015}-000500.json"), "theirs");
    send_while_away(&dir, "mine");
    assert_eq!(
        spool_names(&dir),
        [format!("{NOW:015}-000500.json"), format!("{NOW:015}-000501.json")]
    );
    // Past the last sequence, the next millisecond.
    another_process_queued(&dir, &format!("{NOW:015}-999999.json"), "full");
    send_while_away(&dir, "next");
    assert_eq!(
        spool_names(&dir).last().unwrap(),
        &format!("{:015}-000000.json", NOW + 1)
    );
    assert_eq!(spooled_keys(&dir), ["theirs", "mine", "full", "next"]);
}

#[test]
fn a_spool_name_another_process_takes_first_is_not_overwritten() {
    let dir = scratch("spool-collision");
    crate::spool::fault::take_next_name(request_text("theirs"));
    send_while_away(&dir, "mine");
    assert_eq!(spooled_keys(&dir), ["theirs", "mine"]);
}

#[test]
fn a_request_larger_than_the_whole_spool_is_not_queued_and_evicts_nothing() {
    let dir = scratch("spool-oversized");
    send_while_away(&dir, "k0");
    send_while_away(&dir, "k1");
    crate::spool::fault::limit_bytes(10);
    let backup = example("backup");
    let spool = dir.to_str().unwrap();
    let (code, text) = refused(&[
        "send",
        "backup.space",
        "used_percent=96",
        "--key",
        "big",
        "--decls",
        &backup,
        "--spool",
        spool,
        "--server",
        NOBODY,
    ]);
    assert_eq!(code, 3);
    assert!(text.contains("larger than the spool's limit"), "{text}");
    assert_eq!(spooled_keys(&dir), ["k0", "k1"]);
}

/// Run again in a child under `umask 022` by the test after it: this process can't set its own.
#[cfg(unix)]
#[test]
fn the_spool_folder_and_its_files_are_private_to_their_user() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let dir = scratch("spool-private");
    let spool = dir.join("spool");
    let decl = beacon_core::decl::load(Path::new(&example("ship")), &Default::default()).unwrap();
    let fake = Fake::start(vec![
        (200, decl.to_json()),
        (503, json!({})),
        (503, json!({})),
        (503, json!({})),
    ]);
    beacon(&[
        "send",
        "ship.drift",
        "count=3",
        "oldest=-1h",
        "--spool",
        spool.to_str().unwrap(),
        "--server",
        &fake.url,
    ])
    .unwrap();
    assert_eq!(mode(&spool), 0o700);
    // A request file is a hard link to the temporary file it was written as: one mode for both.
    let [name] = spool_names(&spool).try_into().unwrap();
    assert_eq!(mode(&spool.join(name)), 0o600);
    assert_eq!(mode(&spool.join(".kinds-ship.json")), 0o600);
    // A folder that is already there keeps its mode.
    let shared = dir.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o750)).unwrap();
    send_while_away(&shared, "shared");
    assert_eq!(mode(&shared), 0o750);
}

#[cfg(unix)]
#[test]
fn the_spool_is_private_under_a_umask_of_022() {
    let test = "tests::the_spool_folder_and_its_files_are_private_to_their_user";
    let run = std::process::Command::new("sh")
        .args(["-c", "umask 022 && exec \"$0\" --exact \"$1\""])
        .arg(std::env::current_exe().unwrap())
        .arg(test)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&run.stdout).into_owned() + &String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{text}");
    assert!(text.contains("1 passed"), "{text}");
}
