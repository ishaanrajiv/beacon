use super::*;
use crate::decl::{Check, Declarations, load, parse};
use crate::duration::{DAY, HOUR, MINUTE};
use jiff::tz::{TimeZone, offset};
use serde_json::json;

fn example(name: &str) -> Declarations {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/homelab-ish")
        .join(name)
        .join("beacon");
    load(&dir, &Check::default()).unwrap()
}

fn map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// 2026-11-13 12:00 UTC, a Friday.
const NOON: i64 = 1_794_571_200_000;

fn utc() -> Clock {
    Clock {
        now_ms: NOON,
        tz: TimeZone::UTC,
    }
}

#[test]
fn renders_an_alert() {
    let backup = example("backup");
    let kind = &backup.kinds["backup.failed"];
    let facts = map(json!({"last_ok": NOON - 10 * HOUR}));
    let slots = map(json!({"reason": "The disk\nis full"}));
    assert!(check_values("event", "backup.failed", kind, &facts, &slots).is_empty());
    let rendered = render(kind, &facts, &slots, &utc());
    assert_eq!(rendered.title, "Backup failed");
    assert_eq!(rendered.body, "The disk is full. The last good backup was 02:00.");
    assert_eq!(rendered.url.as_deref(), Some("/system"));

    let (title, body) = render_recovery(kind, &facts, &slots, 3 * MINUTE + 20_000, &utc()).unwrap();
    assert_eq!(title, "Backups are working again");
    assert_eq!(body, "Lasted 3 min.");
    assert_eq!(
        render_recovery(&example("ship").kinds["ship.failed"], &facts, &slots, 1, &utc()),
        None
    );
}

#[test]
fn slots_are_cut_at_a_word_and_links_encode_facts() {
    let assistant = example("assistant");
    let kind = &assistant.kinds["assistant.task_result"];
    let facts = map(json!({"task_id": "weekly digest/2"}));
    let slots =
        map(json!({"task": "Summarise the week's reading list and notes", "summary": "Three articles."}));
    assert!(check_values("event", "assistant.task_result", kind, &facts, &slots).is_empty());
    let rendered = render(kind, &facts, &slots, &utc());
    assert_eq!(rendered.title, "Summarise the week's… finished");
    assert!(rendered.title.chars().count() <= 40);
    assert_eq!(
        rendered.url.as_deref(),
        Some("/assistant/tasks/weekly%20digest%2F2")
    );
    assert_eq!(
        link("/assistant/tasks/{task_id}", kind, &facts, &utc()),
        "/assistant/tasks/weekly%20digest%2F2"
    );
}

#[test]
fn titles_and_bodies_are_capped_whatever_the_facts() {
    let ship = example("ship");
    let kind = &ship.kinds["ship.failed"];
    let facts = map(json!({"app": "the very long application name that goes on and on"}));
    let slots = map(json!({"cause": "x ".repeat(80)}));
    let rendered = render(kind, &facts, &slots, &utc());
    assert!(rendered.title.chars().count() <= 40, "{}", rendered.title);
    assert!(rendered.title.ends_with('…'));
    assert!(rendered.body.chars().count() <= 150);
}

#[test]
fn values_are_cleaned_and_empty_ones_leave_no_gaps() {
    let text = "source = \"demo\"\nname = \"Demo\"\n[kinds.\"demo.done\"]\nclass = \"update\"\npush = \"silent\"\ntitle = \"Job done\"\nbody = \"{who} finished {what} today.\"\nfacts = { who = \"string?\" }\nslots = { what = 40 }\n";
    let decl = parse(text, &Check::default()).unwrap();
    let kind = &decl.kinds["demo.done"];
    let facts = map(json!({}));
    let slots = map(json!({"what": "the\u{202E} \t job"}));
    assert!(check_values("event", "demo.done", kind, &facts, &slots).is_empty());
    assert_eq!(
        render(kind, &facts, &slots, &utc()).body,
        "finished the job today."
    );
    let facts = map(json!({"who": null}));
    assert!(check_values("event", "demo.done", kind, &facts, &slots).is_empty());
}

#[test]
fn checks_values_against_the_kind() {
    let backup = example("backup");
    let kind = &backup.kinds["backup.failed"];
    let cases: Vec<(Value, Value, &str)> = vec![
        (json!({}), json!({"reason": "x"}), "needs the fact `last_ok`"),
        (
            json!({"last_ok": "yesterday"}),
            json!({"reason": "x"}),
            "a time fact can't be a string",
        ),
        (
            json!({"last_ok": 1.5}),
            json!({"reason": "x"}),
            "a time fact can't be 1.5",
        ),
        (
            json!({"last_ok": NOON, "lastok": 1}),
            json!({"reason": "x"}),
            "did you mean `last_ok`?",
        ),
        (json!({"last_ok": NOON}), json!({}), "needs the slot `reason`"),
        (
            json!({"last_ok": NOON}),
            json!({"reason": 3}),
            "a slot is text, and this is a number",
        ),
        (
            json!({"last_ok": NOON}),
            json!({"reason": "x", "detail": "y"}),
            "declares no slot `detail`",
        ),
    ];
    for (facts, slots, expected) in cases {
        let found = check_values(
            "event",
            "backup.failed",
            kind,
            &map(facts.clone()),
            &map(slots.clone()),
        );
        let text = found
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains(expected),
            "expected {expected:?} for {facts} {slots}, got:\n{text}"
        );
        assert!(
            found
                .iter()
                .all(|p| p.at.starts_with("event ") && !p.fix.is_empty())
        );
    }

    let stale = &backup.kinds["backup.stale"];
    let found = check_values(
        "event",
        "backup.stale",
        stale,
        &map(json!({"since": -5, "last_ok": NOON})),
        &Map::new(),
    );
    assert!(
        found[0].message.contains("a duration fact can't be -5"),
        "{found:?}"
    );

    let space = &backup.kinds["backup.space"];
    let found = check_values(
        "event",
        "backup.space",
        space,
        &map(json!({"used_percent": 95.5})),
        &Map::new(),
    );
    assert!(
        found[0].message.contains("an int fact can't be 95.5"),
        "{found:?}"
    );

    let ship = example("ship");
    let deployed = &ship.kinds["ship.deployed"];
    let long = "x".repeat(201);
    let found = check_values(
        "event",
        "ship.deployed",
        deployed,
        &map(json!({"app": long})),
        &map(json!({"summary": ""})),
    );
    assert!(
        found[0].message.contains("longer text goes in a slot"),
        "{found:?}"
    );

    let finished = &backup.kinds["backup.finished"];
    let found = check_values(
        "snapshot firing[0]",
        "backup.finished",
        finished,
        &map(json!({"size": "1 GB", "took": 1, "x": 1})),
        &map(json!({"y": "z"})),
    );
    let text = found
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("snapshot firing[0] facts.x"), "{text}");
    assert!(text.contains("its facts are size, took"), "{text}");
    assert!(text.contains("send no slots"), "{text}");
}

#[test]
fn formats_numbers_and_durations() {
    assert_eq!(format_float(95.13), "95.1");
    assert_eq!(format_float(2.0), "2");
    assert_eq!(format_float(150.4), "150");
    assert_eq!(format_float(-0.01), "0");
    assert_eq!(format_duration(0), "under a minute");
    assert_eq!(format_duration(59_999), "under a minute");
    assert_eq!(format_duration(4 * MINUTE + 59_000), "4 min");
    assert_eq!(format_duration(2 * HOUR), "2 h");
    assert_eq!(format_duration(HOUR + 5 * MINUTE), "1 h 5 min");
    assert_eq!(format_duration(DAY), "1 day");
    assert_eq!(format_duration(3 * DAY + HOUR), "3 days");
}

#[test]
fn formats_times_beside_now() {
    let clock = utc();
    assert_eq!(format_time(NOON + 4 * HOUR + 45 * MINUTE, &clock), "16:45");
    assert_eq!(format_time(NOON - 14 * HOUR, &clock), "yesterday 22:00");
    assert_eq!(format_time(NOON + 21 * HOUR, &clock), "tomorrow 09:00");
    assert_eq!(format_time(NOON - 3 * DAY, &clock), "Tue 12:00");
    assert_eq!(format_time(NOON - 30 * DAY, &clock), "14 Oct");
    assert_eq!(format_time(NOON - 400 * DAY, &clock), "9 Oct 2025");
    // The same moment is today in one zone and yesterday in another.
    let east = Clock {
        now_ms: NOON,
        tz: TimeZone::fixed(offset(13)),
    };
    assert_eq!(format_time(NOON - 2 * HOUR, &clock), "10:00");
    assert_eq!(format_time(NOON - 2 * HOUR, &east), "yesterday 23:00");
}

#[test]
fn trail_kinds_render_their_slots() {
    let builtins = crate::builtin::declarations().unwrap();
    let kind = &builtins[1].kinds["trail.severe"];
    let slots = map(json!({"title": "The email queue is backing up", "body": "150 jobs are waiting."}));
    assert!(check_values("trail", "trail.severe", kind, &Map::new(), &slots).is_empty());
    let rendered = render(kind, &Map::new(), &slots, &utc());
    assert_eq!(rendered.title, "The email queue is backing up");
    assert_eq!(rendered.url, None);
    let (title, body) = render_recovery(kind, &Map::new(), &slots, 4 * MINUTE, &utc()).unwrap();
    assert_eq!(title, "Back to normal");
    assert_eq!(body, "The email queue is backing up · lasted 4 min");
}
