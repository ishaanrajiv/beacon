//! The store against SQLite, with a fake clock: events, snapshots, presses, holds, the catch-up,
//! silences, sources, idempotency, the inbox, the push queue, retention, and a restart.

use crate::*;
use beacon_core::decl::{Check, parse};
use beacon_core::delivery::{Settings, SilenceMatch};
use beacon_core::duration::{DAY, HOUR, MINUTE, SECOND};
use beacon_core::model::{Class, ItemState, PushLevel, PushReason as R, PushStatus as S};
use serde_json::{Value, json};

/// 2026-11-13 12:00 UTC.
const T0: i64 = 1_794_571_200_000;
/// 01:00 that night: quiet hours (23:00–08:00 UTC).
const NIGHT: i64 = T0 + 13 * HOUR;
/// 08:00 the next morning.
const MORNING: i64 = T0 + 20 * HOUR;

const BACKUP: &str = include_str!("../../../examples/homelab-ish/backup/beacon/kinds.toml");
const SHIP: &str = include_str!("../../../examples/homelab-ish/ship/beacon/kinds.toml");
const ASSISTANT: &str = include_str!("../../../examples/homelab-ish/assistant/beacon/kinds.toml");
/// A made-up monitor with a normal alert that repeats, for repeats in quiet hours.
const MONITOR: &str = r#"
source = "monitor"
name = "Monitor"
icon = "gauge"

[kinds."monitor.down"]
class = "alert"
push = "normal"
title = "Website is down"
body = "{cause}."
slots = { cause = 80 }
actions = ["acknowledge", "dismiss", "silence_1h"]
repeat = ["30m", "2h"]
throttle = "none"
"#;

fn store_with(settings: Settings) -> Store {
    let store = Store::open_in_memory(settings).unwrap();
    for text in [BACKUP, SHIP, ASSISTANT, MONITOR] {
        store
            .publish(&parse(text, &Check::default()).unwrap(), T0 - DAY)
            .unwrap();
    }
    store
}

fn store() -> Store {
    store_with(Settings::default())
}

fn event(kind: &str, key: &str, facts: Value, slots: Value) -> Event {
    Event {
        kind: kind.into(),
        key: key.into(),
        facts: facts.as_object().cloned(),
        slots: slots.as_object().cloned(),
        ..Event::default()
    }
}

fn failed() -> Event {
    event(
        "backup.failed",
        "nightly",
        json!({"last_ok": T0 - 10 * HOUR}),
        json!({"reason": "The disk is full"}),
    )
}

fn down() -> Event {
    event("monitor.down", "", json!({}), json!({"cause": "No answer"}))
}

fn resolved(mut e: Event) -> Event {
    e.state = Some(EventState::Resolved);
    e.facts = None;
    e.slots = None;
    e
}

fn ready(n: i64) -> Event {
    event(
        "ship.pr_ready",
        &format!("{n}"),
        json!({"number": n}),
        json!({"summary": "Adds a thing"}),
    )
}

fn reminder(key: &str) -> Event {
    event(
        "assistant.reminder",
        key,
        json!({}),
        json!({"text": "Water the plants"}),
    )
}

fn item(answer: &EventAnswer) -> beacon_core::item::Item {
    answer.item.clone().unwrap()
}

fn history(store: &Store, id: i64) -> Vec<String> {
    store
        .item_detail(id, T0)
        .unwrap()
        .unwrap()
        .history
        .into_iter()
        .map(|h| h.what)
        .collect()
}

fn active(store: &Store, now: i64, viewing: Option<&str>) {
    store
        .heartbeat(
            &Heartbeat {
                client: "web".into(),
                device: "web-1".into(),
                active: true,
                viewing: viewing.map(Into::into),
            },
            now,
        )
        .unwrap();
}

fn inactive(store: &Store, now: i64) {
    store
        .heartbeat(
            &Heartbeat {
                client: "web".into(),
                device: "web-1".into(),
                active: false,
                viewing: None,
            },
            now,
        )
        .unwrap();
}

fn sent(store: &Store, now: i64) {
    for job in store.pushes_due(now).unwrap() {
        store.push_attempted(job.id, &[], PushNext::Sent, now).unwrap();
    }
}

// ---------- the database ----------

#[test]
fn a_file_database_uses_wal_and_survives_a_restart_with_its_owed_work() {
    let dir = std::env::temp_dir().join(format!("beacon-store-{}-{}", std::process::id(), T0));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("beacon.db");
    let _ = std::fs::remove_file(&path);
    let id = {
        let store = Store::open(&path, Settings::default()).unwrap();
        store
            .publish(&parse(SHIP, &Check::default()).unwrap(), T0)
            .unwrap();
        active(&store, T0, None);
        let answer = store.apply_event(&ready(1), None, T0).unwrap();
        assert_eq!(item(&answer).push.status, S::Held);
        item(&answer).id
    };
    // Killed while the push was held: the hold is found again after the restart.
    let store = Store::open(&path, Settings::default()).unwrap();
    assert_eq!(store.declarations("ship").unwrap().unwrap().source, "ship");
    let ran = store.run_due(T0 + 2 * MINUTE).unwrap();
    assert!(ran.ran.iter().any(|(_, work)| *work == Due::Release(id)));
    assert_eq!(store.item(id, T0).unwrap().unwrap().push.status, S::Queued);
    let mode: String = store
        .lock()
        .unwrap()
        .conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------- events and alerts ----------

#[test]
fn an_alert_opens_queues_its_push_and_is_sent() {
    let store = store();
    let answer = store.apply_event(&failed(), None, T0).unwrap();
    assert_eq!(answer.outcome, EventOutcome::Opened);
    let it = item(&answer);
    assert_eq!(it.title, "Backup failed");
    assert_eq!(it.body, "The disk is full. The last good backup was 02:00.");
    assert_eq!(it.state, Some(ItemState::Open));
    assert_eq!(it.collapse_id, "b1-29fd692930019189");
    assert_eq!(it.push.status, S::Queued);
    assert_eq!(it.push.level, PushLevel::Urgent);
    assert_eq!(it.alert.as_ref().unwrap().next_repeat, Some(T0 + 2 * HOUR));
    assert_eq!(
        it.actions.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
        ["acknowledge", "silence_1h"]
    );
    let jobs = store.pushes_due(T0).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].category.as_deref(), Some("backup.failed"));
    assert_eq!(jobs[0].thread, "backup");
    assert_eq!(jobs[0].collapse_id, it.collapse_id);
    assert_eq!(jobs[0].data["id"], it.id);
    store
        .push_attempted(
            jobs[0].id,
            &[Attempt {
                token: "ab".repeat(32),
                environment: "production".into(),
                status: "accepted".into(),
                reason: None,
            }],
            PushNext::Sent,
            T0 + SECOND,
        )
        .unwrap();
    let detail = store.item_detail(it.id, T0).unwrap().unwrap();
    assert_eq!(detail.item.push.status, S::Sent);
    assert_eq!(detail.attempts.len(), 1);
    assert_eq!(detail.attempts[0].device, format!("…{}", "ab".repeat(3)));
    assert_eq!(history(&store, it.id), ["opened", "push_sent"]);
    assert!(store.pushes_due(T0 + MINUTE).unwrap().is_empty());
}

#[test]
fn firing_again_updates_without_a_push() {
    let store = store();
    let first = item(&store.apply_event(&failed(), None, T0).unwrap());
    let mut again = failed();
    again.slots = json!({"reason": "The disk broke"}).as_object().cloned();
    let answer = store.apply_event(&again, None, T0 + MINUTE).unwrap();
    assert_eq!(answer.outcome, EventOutcome::Updated);
    assert_eq!(item(&answer).id, first.id);
    assert_eq!(
        item(&answer).body,
        "The disk broke. The last good backup was 02:00."
    );
    assert_eq!(store.pushes_due(T0 + MINUTE).unwrap().len(), 1);
}

#[test]
fn resolving_a_sent_alert_queues_a_passive_recovery() {
    let store = store();
    let it = item(&store.apply_event(&failed(), None, T0).unwrap());
    sent(&store, T0);
    let answer = store
        .apply_event(&resolved(failed()), None, T0 + 90 * MINUTE)
        .unwrap();
    assert_eq!(answer.outcome, EventOutcome::Resolved);
    let after = item(&answer);
    assert_eq!(after.state, Some(ItemState::Resolved));
    assert_eq!(after.title, "Backups are working again");
    assert_eq!(after.body, "Lasted 1 h 30 min.");
    assert!(!after.seen);
    assert_eq!(after.alert.as_ref().unwrap().next_repeat, None);
    let jobs = store.pushes_due(T0 + 90 * MINUTE).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].purpose, "recovery");
    assert_eq!(jobs[0].level, PushLevel::Silent);
    assert_eq!(jobs[0].category, None);
    assert_eq!(jobs[0].collapse_id, it.collapse_id);
    store
        .push_attempted(jobs[0].id, &[], PushNext::Sent, T0 + 91 * MINUTE)
        .unwrap();
    assert_eq!(
        history(&store, it.id),
        ["opened", "push_sent", "resolved", "recovery_sent"]
    );
    // Resolving again is ignored.
    let again = store
        .apply_event(&resolved(failed()), None, T0 + 2 * HOUR)
        .unwrap();
    assert_eq!(again.outcome, EventOutcome::Ignored);
    assert!(again.item.is_none());
}

#[test]
fn an_alert_that_never_pushed_recovers_quietly() {
    let store = store();
    active(&store, T0, None);
    store.apply_event(&failed(), None, T0).unwrap();
    let after = item(&store.apply_event(&resolved(failed()), None, T0 + MINUTE).unwrap());
    // The hold was suppressed `closed`; nothing was sent, so no recovery.
    assert_eq!(after.push.status, S::Suppressed);
    assert_eq!(after.push.reason, Some(R::Closed));
    assert!(store.pushes_due(T0 + MINUTE).unwrap().is_empty());
}

#[test]
fn a_recovery_with_push_off_is_suppressed() {
    let store = store_with(Settings {
        push: false,
        ..Settings::default()
    });
    let it = item(&store.apply_event(&failed(), None, T0).unwrap());
    assert_eq!(it.push.reason, Some(R::PushOff));
    assert!(store.pushes_due(T0).unwrap().is_empty());
}

#[test]
fn reopening_within_the_flap_window_then_flapping_then_settling() {
    let store = store();
    let first = item(&store.apply_event(&down(), None, T0).unwrap());
    sent(&store, T0);
    let mut now = T0;
    let mut outcomes = Vec::new();
    for _ in 0..4 {
        now += MINUTE;
        store.apply_event(&resolved(down()), None, now).unwrap();
        sent(&store, now);
        now += MINUTE;
        let answer = store.apply_event(&down(), None, now).unwrap();
        outcomes.push(answer.outcome);
        assert_eq!(item(&answer).id, first.id);
        sent(&store, now);
    }
    assert_eq!(outcomes, [EventOutcome::Reopened; 4]);
    let it = store.item(first.id, now).unwrap().unwrap();
    assert!(it.alert.as_ref().unwrap().flapping);
    assert_eq!(it.alert.as_ref().unwrap().openings, 5);
    assert_eq!(it.push.reason, Some(R::Flapping));
    // One beacon.flapping, carrying the alert's collapse id.
    let flaps: Vec<_> = store
        .inbox(&InboxQuery::default(), now)
        .unwrap()
        .activity
        .items
        .into_iter()
        .filter(|i| i.kind == "beacon.flapping")
        .collect();
    assert_eq!(flaps.len(), 1);
    assert_eq!(flaps[0].collapse_id, first.collapse_id);
    assert_eq!(flaps[0].key, "monitor.down/");
    assert_eq!(
        flaps[0].body,
        "Website is down opened 4 times in 30 min. It stays quiet until it settles for 15 min."
    );
    // Settles 15 minutes after the last opening, while firing: decided again.
    assert!(
        !store
            .run_due(now + 14 * MINUTE)
            .unwrap()
            .ran
            .iter()
            .any(|(_, w)| matches!(w, Due::Settle(_)))
    );
    store.run_due(now + 15 * MINUTE).unwrap();
    let it = store.item(first.id, now + 15 * MINUTE).unwrap().unwrap();
    assert!(!it.alert.as_ref().unwrap().flapping);
    assert_eq!(it.push.status, S::Queued);
    assert!(history(&store, first.id).contains(&"settled".to_string()));
}

#[test]
fn firing_after_the_flap_window_opens_a_new_item() {
    let store = store();
    let first = item(&store.apply_event(&failed(), None, T0).unwrap());
    store.apply_event(&resolved(failed()), None, T0 + MINUTE).unwrap();
    let second = store.apply_event(&failed(), None, T0 + 31 * MINUTE).unwrap();
    assert_eq!(second.outcome, EventOutcome::Opened);
    assert_ne!(item(&second).id, first.id);
}

#[test]
fn repeats_go_out_until_the_list_runs_out_or_it_is_acknowledged() {
    let store = store();
    let it = item(&store.apply_event(&failed(), None, T0).unwrap());
    sent(&store, T0);
    store.run_due(T0 + 2 * HOUR).unwrap();
    let after = store.item(it.id, T0 + 2 * HOUR).unwrap().unwrap();
    assert_eq!(after.alert.as_ref().unwrap().repeats_sent, 1);
    assert_eq!(after.alert.as_ref().unwrap().next_repeat, Some(T0 + 8 * HOUR));
    assert_eq!(after.push.status, S::Queued);
    sent(&store, T0 + 2 * HOUR);
    store
        .press(it.id, "acknowledge", None, "ios", T0 + 3 * HOUR)
        .unwrap();
    let after = store.item(it.id, T0 + 3 * HOUR).unwrap().unwrap();
    assert_eq!(after.alert.as_ref().unwrap().next_repeat, None);
    assert!(after.actions.iter().all(|a| a.id != "acknowledge"));
    assert!(
        store
            .run_due(T0 + 9 * HOUR)
            .unwrap()
            .ran
            .iter()
            .all(|(_, w)| !matches!(w, Due::Repeat(_)))
    );
}

#[test]
fn a_repeat_in_quiet_hours_is_skipped_but_counted() {
    let store = store();
    // monitor.down is normal, repeating after 30m then 2h. Opened at 22:00.
    let opened = T0 + 10 * HOUR;
    let it = item(&store.apply_event(&down(), None, opened).unwrap());
    assert_eq!(it.push.status, S::Queued);
    sent(&store, opened);
    // 22:30: the first repeat sends.
    store.run_due(opened + 30 * MINUTE).unwrap();
    let mid = store.item(it.id, opened).unwrap().unwrap();
    assert_eq!(mid.alert.as_ref().unwrap().repeats_sent, 1);
    assert_eq!(mid.push.status, S::Queued);
    sent(&store, opened + 30 * MINUTE);
    // 00:30: the second comes due in quiet hours and is skipped, not held.
    let night = opened + 150 * MINUTE;
    let ran = store.run_due(night).unwrap();
    assert!(ran.ran.iter().any(|(_, w)| *w == Due::Repeat(it.id)));
    let after = store.item(it.id, night).unwrap().unwrap();
    assert_eq!(after.alert.as_ref().unwrap().repeats_sent, 2);
    assert_eq!(after.alert.as_ref().unwrap().next_repeat, None);
    assert_eq!(
        after.push.status,
        S::Sent,
        "a skipped repeat changes nothing about the push"
    );
    assert!(store.pushes_due(night).unwrap().is_empty());
}

#[test]
fn one_off_alerts_resolve_when_acknowledged() {
    let store = store();
    let e = event(
        "ship.failed",
        "web-42",
        json!({"app": "web"}),
        json!({"cause": "Tests failed"}),
    );
    let it = item(&store.apply_event(&e, None, T0).unwrap());
    assert_eq!(it.alert.as_ref().unwrap().next_repeat, None);
    let answer = store
        .press(it.id, "acknowledge", None, "web", T0 + MINUTE)
        .unwrap();
    assert_eq!(answer.item.state, Some(ItemState::Resolved));
    assert_eq!(
        answer.item.title, "Deploy of web failed",
        "a one-off keeps its own text"
    );
    // A second tap: the effect holds.
    let again = store
        .press(it.id, "acknowledge", None, "web", T0 + 2 * MINUTE)
        .unwrap();
    assert_eq!(again.item, answer.item);
}

// ---------- validation ----------

#[test]
fn an_undeclared_kind_is_422_with_the_closest_name() {
    let store = store();
    let err = store
        .apply_event(&event("backup.faild", "", json!({}), json!({})), None, T0)
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Undeclared);
    assert_eq!(err.kind.status(), 422);
    assert_eq!(err.problem().message, "`backup.faild` is not a declared kind");
    assert!(err.problem().fix.contains("`backup.failed`"));
}

#[test]
fn facts_and_slots_that_dont_fit_are_422_each() {
    let store = store();
    let err = store
        .apply_event(
            &event(
                "backup.failed",
                "",
                json!({"last_ok": "x", "extra": 1}),
                json!({}),
            ),
            None,
            T0,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Undeclared);
    assert_eq!(err.problems.len(), 3, "{err}");
    assert!(err.problems.iter().any(|p| p.at == "event facts.last_ok"));
}

#[test]
fn shape_problems_are_400() {
    let store = store();
    let mut e = failed();
    e.key = "has space".into();
    e.context = Some("x".into());
    e.url = Some("http://example.com".into());
    e.state = Some(EventState::Open);
    let err = store.apply_event(&e, None, T0).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Invalid);
    let at: Vec<&str> = err.problems.iter().map(|p| p.at.as_str()).collect();
    assert_eq!(at, ["event state", "event key", "event context", "event url"]);
    // A context kind needs one.
    let e = event(
        "assistant.question",
        "t1",
        json!({"thread": "1"}),
        json!({"question": "Ok?"}),
    );
    assert_eq!(
        store.apply_event(&e, None, T0).unwrap_err().problem().at,
        "event context"
    );
    // A source that isn't the kind's.
    let mut e = failed();
    e.source = Some("ship".into());
    assert_eq!(
        store.apply_event(&e, None, T0).unwrap_err().kind,
        ErrorKind::Undeclared
    );
}

// ---------- idempotency ----------

#[test]
fn an_idempotency_key_answers_the_first_answer_again() {
    let store = store();
    let first = store.apply_event(&ready(1), Some("k-1"), T0).unwrap();
    assert!(!first.repeat);
    let again = store.apply_event(&ready(1), Some("k-1"), T0 + HOUR).unwrap();
    assert!(again.repeat);
    assert_eq!(again.item, first.item);
    assert_eq!(
        store
            .inbox(&InboxQuery::default(), T0)
            .unwrap()
            .activity
            .items
            .len(),
        1
    );
    let err = store.apply_event(&ready(2), Some("k-1"), T0 + HOUR).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
    // Keys are scoped to the kind's source.
    assert!(!store.apply_event(&reminder("r"), Some("k-1"), T0).unwrap().repeat);
    // Forgotten after a day.
    assert!(
        !store
            .apply_event(&ready(1), Some("k-1"), T0 + DAY)
            .unwrap()
            .repeat
    );
    assert_eq!(
        store
            .apply_event(&ready(1), Some("bad key"), T0)
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
}

// ---------- holds ----------

#[test]
fn a_hold_while_active_releases_and_pushes_if_unseen() {
    let store = store();
    active(&store, T0, None);
    let it = item(&store.apply_event(&ready(1), None, T0).unwrap());
    assert_eq!(it.push.status, S::Held);
    assert_eq!(it.push.reason, Some(R::Active));
    assert_eq!(it.push.release_at, Some(T0 + 90 * SECOND));
    let inbox = store.inbox(&InboxQuery::default(), T0).unwrap();
    assert_eq!(inbox.held.len(), 1);
    // `since` filters by version.
    let later = InboxQuery {
        since: inbox.version,
        ..InboxQuery::default()
    };
    assert!(store.inbox(&later, T0).unwrap().held.is_empty());
    // Still active at release time: it pushes anyway.
    active(&store, T0 + 60 * SECOND, None);
    store.run_due(T0 + 90 * SECOND).unwrap();
    let after = store.item(it.id, T0).unwrap().unwrap();
    assert_eq!(after.push.status, S::Queued);
    assert_eq!(history(&store, it.id), ["recorded", "push_held", "push_released"]);
}

#[test]
fn a_held_item_seen_or_closed_is_suppressed_at_once() {
    let store = store();
    active(&store, T0, None);
    let a = item(&store.apply_event(&ready(1), None, T0).unwrap());
    let b = item(&store.apply_event(&reminder("r1"), None, T0).unwrap());
    let c = item(&store.apply_event(&failed(), None, T0).unwrap());
    assert_eq!(c.push.release_at, Some(T0 + 120 * SECOND));
    assert_eq!(
        store
            .mark_seen(&SeenTarget::Ids(vec![a.id]), "web", T0 + SECOND)
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .mark_seen(&SeenTarget::Ids(vec![a.id]), "web", T0 + SECOND)
            .unwrap(),
        0
    );
    store.press(b.id, "snooze", None, "web", T0 + SECOND).unwrap();
    store
        .press(c.id, "acknowledge", None, "web", T0 + SECOND)
        .unwrap();
    let a = store.item(a.id, T0).unwrap().unwrap();
    assert_eq!((a.push.status, a.push.reason), (S::Suppressed, Some(R::Seen)));
    let b = store.item(b.id, T0).unwrap().unwrap();
    assert_eq!((b.push.status, b.push.reason), (S::Suppressed, Some(R::Closed)));
    // Acknowledging doesn't leave open: still held, then pushed.
    let c2 = store.item(c.id, T0).unwrap().unwrap();
    assert_eq!(c2.push.status, S::Held);
    store.run_due(T0 + 3 * MINUTE).unwrap();
    assert_eq!(store.item(c.id, T0).unwrap().unwrap().push.status, S::Queued);
    assert!(store.inbox(&InboxQuery::default(), T0).unwrap().held.is_empty());
}

#[test]
fn an_item_viewed_in_its_context_is_suppressed() {
    let store = store();
    active(&store, T0, Some("assistant:thread:42"));
    let mut e = event(
        "assistant.question",
        "q1",
        json!({"thread": "42"}),
        json!({"question": "Go ahead?"}),
    );
    e.context = Some("assistant:thread:42".into());
    let it = item(&store.apply_event(&e, None, T0).unwrap());
    assert_eq!(it.push.reason, Some(R::Viewing));
    assert_eq!(it.url.as_deref(), Some("/assistant/threads/42"));
}

// ---------- quiet hours and the catch-up ----------

#[test]
fn quiet_hours_hold_until_the_morning_and_one_item_is_decided_again() {
    let store = store();
    let it = item(&store.apply_event(&ready(1), None, NIGHT).unwrap());
    assert_eq!(it.push.status, S::Held);
    assert_eq!(it.push.reason, Some(R::QuietHours));
    assert_eq!(it.push.release_at, Some(MORNING));
    // Urgent and time-bound pass.
    assert_eq!(
        item(&store.apply_event(&failed(), None, NIGHT).unwrap())
            .push
            .status,
        S::Queued
    );
    assert_eq!(
        item(&store.apply_event(&reminder("r"), None, NIGHT).unwrap())
            .push
            .status,
        S::Queued
    );
    assert!(
        store
            .run_due(MORNING - 1)
            .unwrap()
            .ran
            .iter()
            .all(|(_, w)| *w != Due::CatchUp)
    );
    store.run_due(MORNING).unwrap();
    let after = store.item(it.id, MORNING).unwrap().unwrap();
    assert_eq!(after.push.status, S::Queued);
}

#[test]
fn the_catch_up_stands_in_for_several_held_items() {
    let store = store();
    let a = item(&store.apply_event(&ready(1), None, NIGHT).unwrap());
    let b = item(&store.apply_event(&ready(2), None, NIGHT + MINUTE).unwrap());
    let mut deploy = event(
        "ship.deployed",
        "web",
        json!({"app": "web"}),
        json!({"summary": "v2"}),
    );
    deploy.key = "web".into();
    let c = item(&store.apply_event(&deploy, None, NIGHT + 2 * MINUTE).unwrap());
    let d = item(&store.apply_event(&ready(4), None, NIGHT + 3 * MINUTE).unwrap());
    store
        .mark_seen(&SeenTarget::Ids(vec![d.id]), "ios", NIGHT + 4 * MINUTE)
        .unwrap();
    store.run_due(MORNING).unwrap();
    let inbox = store.inbox(&InboxQuery::default(), MORNING).unwrap();
    let summary = inbox
        .activity
        .items
        .iter()
        .find(|i| i.kind == "beacon.catch_up")
        .unwrap();
    assert_eq!(summary.title, "3 things came in overnight");
    assert_eq!(summary.body, "PR #1 is ready; PR #2 is ready; web is deployed");
    assert_eq!(summary.push.level, PushLevel::Normal);
    assert_eq!(summary.push.status, S::Queued);
    for id in [a.id, b.id, c.id] {
        let it = store.item(id, MORNING).unwrap().unwrap();
        assert_eq!(it.push.reason, Some(R::CatchUp));
        assert_eq!(it.push.covered_by, Some(summary.id));
    }
    assert_eq!(
        store.item(d.id, MORNING).unwrap().unwrap().push.reason,
        Some(R::Seen)
    );
}

// ---------- throttle, budget ----------

#[test]
fn a_second_push_within_the_throttle_is_suppressed() {
    let store = store();
    let mut e = event(
        "ship.deployed",
        "web",
        json!({"app": "web"}),
        json!({"summary": "v2"}),
    );
    assert_eq!(
        item(&store.apply_event(&e, None, T0).unwrap()).push.status,
        S::Queued
    );
    e.slots = json!({"summary": "v3"}).as_object().cloned();
    assert_eq!(
        item(&store.apply_event(&e, None, T0 + 9 * MINUTE).unwrap())
            .push
            .reason,
        Some(R::Throttled)
    );
    assert_eq!(
        item(&store.apply_event(&e, None, T0 + 19 * MINUTE).unwrap())
            .push
            .status,
        S::Queued
    );
}

#[test]
fn the_sixth_non_alert_push_of_a_day_records_the_budget_once() {
    let store = store();
    for n in 0..7 {
        store.apply_event(&ready(n), None, T0 + n * MINUTE).unwrap();
    }
    store.apply_event(&failed(), None, T0 + HOUR).unwrap();
    let budget: Vec<_> = store
        .inbox(&InboxQuery::default(), T0 + HOUR)
        .unwrap()
        .activity
        .items
        .into_iter()
        .filter(|i| i.kind == "beacon.budget")
        .collect();
    assert_eq!(budget.len(), 1);
    assert_eq!(budget[0].title, "More pushes than usual today");
    assert_eq!(budget[0].body, "6 so far. Most were PR #5 is ready from Deploys.");
    assert_eq!(budget[0].push.status, S::None);
}

// ---------- presses ----------

#[test]
fn actions_snooze_wake_and_close() {
    let store = store();
    let it = item(&store.apply_event(&reminder("r1"), None, T0).unwrap());
    sent(&store, T0);
    store.mark_seen(&SeenTarget::Through(it.id), "web", T0).unwrap();
    let snoozed = store
        .press(it.id, "snooze", Some(T0 + 2 * HOUR), "ios", T0 + MINUTE)
        .unwrap()
        .item;
    assert_eq!(snoozed.state, Some(ItemState::Snoozed));
    assert!(
        store
            .inbox(&InboxQuery::default(), T0)
            .unwrap()
            .needs_you
            .is_empty()
    );
    // An event for it updates the snoozed item.
    let update = store.apply_event(&reminder("r1"), None, T0 + HOUR).unwrap();
    assert_eq!(update.outcome, EventOutcome::Updated);
    store.run_due(T0 + 2 * HOUR).unwrap();
    let woke = store.item(it.id, T0).unwrap().unwrap();
    assert_eq!(woke.state, Some(ItemState::Open));
    assert!(!woke.seen);
    assert_eq!(woke.push.status, S::Queued);
    let done = store
        .press(it.id, "done", None, "ios", T0 + 3 * HOUR)
        .unwrap()
        .item;
    assert_eq!(done.state, Some(ItemState::Done));
    // Done again: 200, unchanged. Snooze now: 409, naming what it offers.
    assert_eq!(
        store
            .press(it.id, "done", None, "ios", T0 + 4 * HOUR)
            .unwrap()
            .item,
        done
    );
    let err = store
        .press(it.id, "snooze", None, "ios", T0 + 4 * HOUR)
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
    assert_eq!(err.problem().fix, "it offers no actions now");
    assert_eq!(
        store.press(999, "done", None, "ios", T0).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        store
            .press(it.id, "done", Some(T0 + HOUR), "ios", T0)
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    assert_eq!(
        history(&store, it.id),
        [
            "opened",
            "push_sent",
            "seen",
            "snoozed",
            "updated",
            "woke",
            "done"
        ]
    );
}

#[test]
fn silence_1h_opens_a_silence_for_the_item() {
    let store = store();
    let it = item(&store.apply_event(&failed(), None, T0).unwrap());
    let answer = store.press(it.id, "silence_1h", None, "ios", T0).unwrap();
    let silence = answer.silence.unwrap();
    assert_eq!(silence.until, T0 + HOUR);
    assert_eq!(silence.reason, "Silenced from a notification");
    assert_eq!(silence.created_by, "ios");
    assert_eq!(
        silence.matcher,
        SilenceMatch {
            source: Some("backup".into()),
            kind: Some("backup.failed".into()),
            key: Some("nightly".into()),
            ..SilenceMatch::default()
        }
    );
    assert_eq!(answer.item.state, Some(ItemState::Open));
}

#[test]
fn a_custom_action_is_recorded_in_acted() {
    let store = store();
    let it = item(&store.apply_event(&ready(7), None, T0).unwrap());
    assert_eq!(it.actions[0].url.as_deref(), Some("/code/pulls/7/review"));
    store.press(it.id, "review", None, "web", T0 + MINUTE).unwrap();
    let detail = store.item_detail(it.id, T0).unwrap().unwrap();
    assert_eq!(detail.acted.len(), 1);
    assert_eq!(detail.acted[0].action, "review");
    assert_eq!(detail.acted[0].client.as_deref(), Some("web"));
}

// ---------- silences ----------

#[test]
fn a_silence_mutes_then_its_end_decides_the_alert_again() {
    let store = store();
    let silence = store
        .open_silence(
            &NewSilence {
                matcher: SilenceMatch {
                    labels: [("service".to_string(), "backup".to_string())].into(),
                    ..SilenceMatch::default()
                },
                until: T0 + 30 * MINUTE,
                reason: "Deploying backup".into(),
                client: "api".into(),
            },
            T0,
        )
        .unwrap();
    assert!(silence.active);
    let mut e = failed();
    e.labels = [("service".to_string(), "backup".to_string())].into();
    let it = item(&store.apply_event(&e, None, T0 + MINUTE).unwrap());
    assert_eq!(it.push.reason, Some(R::Silenced));
    assert_eq!(store.silence(silence.id, T0).unwrap().unwrap().matched, 1);
    // Ending it early decides the alert again.
    let ended = store.end_silence(silence.id, T0 + 5 * MINUTE).unwrap();
    assert!(!ended.active);
    assert_eq!(ended.ended, Some(T0 + 5 * MINUTE));
    assert_eq!(store.item(it.id, T0).unwrap().unwrap().push.status, S::Queued);
    assert_eq!(
        store
            .change_silence(silence.id, Some(T0 + HOUR), None, T0 + 6 * MINUTE)
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert!(store.silences(false, T0 + 6 * MINUTE).unwrap().is_empty());
    assert_eq!(store.silences(true, T0 + 6 * MINUTE).unwrap().len(), 1);
}

#[test]
fn a_silence_that_runs_out_unmutes_through_the_job() {
    let store = store();
    let silence = store
        .open_silence(
            &NewSilence {
                matcher: SilenceMatch {
                    kind: Some("monitor.down".into()),
                    ..SilenceMatch::default()
                },
                until: T0 + 10 * MINUTE,
                reason: "Planned".into(),
                client: "web".into(),
            },
            T0,
        )
        .unwrap();
    let it = item(&store.apply_event(&down(), None, T0).unwrap());
    assert_eq!(it.push.reason, Some(R::Silenced));
    let moved = store
        .change_silence(silence.id, Some(T0 + 20 * MINUTE), Some("Longer"), T0 + MINUTE)
        .unwrap();
    assert_eq!(moved.until, T0 + 20 * MINUTE);
    assert_eq!(moved.reason, "Longer");
    store.run_due(T0 + 19 * MINUTE).unwrap();
    assert_eq!(store.item(it.id, T0).unwrap().unwrap().push.status, S::Suppressed);
    let ran = store.run_due(T0 + 20 * MINUTE).unwrap();
    assert!(ran.ran.iter().any(|(_, w)| *w == Due::SilenceEnd(silence.id)));
    assert_eq!(store.item(it.id, T0).unwrap().unwrap().push.status, S::Queued);
}

#[test]
fn silence_checks() {
    let store = store();
    let new = |matcher: SilenceMatch, until: i64, reason: &str| NewSilence {
        matcher,
        until,
        reason: reason.into(),
        client: "api".into(),
    };
    let kind = SilenceMatch {
        kind: Some("x.y".into()),
        ..SilenceMatch::default()
    };
    for bad in [
        new(SilenceMatch::default(), T0 + HOUR, "r"),
        new(
            SilenceMatch {
                key: Some("k".into()),
                ..SilenceMatch::default()
            },
            T0 + HOUR,
            "r",
        ),
        new(kind.clone(), T0 + SECOND, "r"),
        new(kind.clone(), T0 + 31 * DAY, "r"),
        new(kind.clone(), T0 + HOUR, ""),
    ] {
        assert_eq!(
            store.open_silence(&bad, T0).unwrap_err().kind,
            ErrorKind::Invalid,
            "{bad:?}"
        );
    }
}

// ---------- snapshots and sources ----------

fn snapshot(firing: Vec<SnapshotEntry>) -> Snapshot {
    Snapshot {
        source: "backup".into(),
        observed: true,
        reason: None,
        stale_after: None,
        sent_at: None,
        kinds: None,
        firing,
    }
}

fn space(percent: i64) -> SnapshotEntry {
    SnapshotEntry {
        kind: "backup.space".into(),
        facts: json!({"used_percent": percent}).as_object().cloned().unwrap(),
        ..SnapshotEntry::default()
    }
}

#[test]
fn a_snapshot_opens_updates_and_resolves() {
    let store = store();
    let open = store.apply_event(&failed(), None, T0).unwrap();
    let answer = store
        .apply_snapshot(&snapshot(vec![space(96)]), T0 + MINUTE)
        .unwrap();
    assert_eq!(answer.opened.len(), 1);
    assert_eq!(
        answer.resolved,
        [item(&open).id],
        "an event-opened alert absent from the snapshot resolves"
    );
    let answer = store
        .apply_snapshot(&snapshot(vec![space(97)]), T0 + 2 * MINUTE)
        .unwrap();
    assert_eq!(answer.updated.len(), 1);
    // The same snapshot twice changes nothing.
    let v = store.version().unwrap();
    let again = store
        .apply_snapshot(&snapshot(vec![space(97)]), T0 + 2 * MINUTE)
        .unwrap();
    assert!(again.opened.is_empty() && again.resolved.is_empty());
    assert_eq!(again.updated.len(), 1);
    assert!(store.version().unwrap() >= v);
    let answer = store.apply_snapshot(&snapshot(vec![]), T0 + 3 * MINUTE).unwrap();
    assert_eq!(answer.resolved.len(), 1);
}

#[test]
fn a_snapshot_is_all_or_nothing() {
    let store = store();
    let mut bad = space(1);
    bad.key = "b".into();
    bad.facts = json!({"used_percent": "lots"}).as_object().cloned().unwrap();
    let err = store
        .apply_snapshot(&snapshot(vec![space(96), bad]), T0)
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Undeclared);
    assert_eq!(err.problem().at, "snapshot firing[1] facts.used_percent");
    assert!(
        store
            .inbox(&InboxQuery::default(), T0)
            .unwrap()
            .alerts
            .open
            .is_empty()
    );
    let err = store
        .apply_snapshot(&snapshot(vec![space(1), space(2)]), T0)
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Invalid);
    let mut s = snapshot(vec![]);
    s.kinds = Some(vec!["backup.finished".into()]);
    assert_eq!(
        store.apply_snapshot(&s, T0).unwrap_err().kind,
        ErrorKind::Undeclared
    );
    let mut s = snapshot(vec![]);
    s.source = "trail".into();
    assert_eq!(
        store.apply_snapshot(&s, T0).unwrap_err().kind,
        ErrorKind::Undeclared
    );
    let mut s = snapshot(vec![]);
    s.source = "nobody".into();
    assert_eq!(
        store.apply_snapshot(&s, T0).unwrap_err().kind,
        ErrorKind::Undeclared
    );
}

#[test]
fn snapshot_sent_at_must_be_fresh_and_in_order() {
    let store = store();
    let mut s = snapshot(vec![]);
    s.sent_at = Some(T0);
    store.apply_snapshot(&s, T0).unwrap();
    s.sent_at = Some(T0 - SECOND);
    assert_eq!(
        store.apply_snapshot(&s, T0).unwrap_err().kind,
        ErrorKind::Conflict
    );
    s.sent_at = Some(T0 - 6 * MINUTE);
    assert_eq!(
        store.apply_snapshot(&s, T0).unwrap_err().kind,
        ErrorKind::Conflict
    );
    s.sent_at = Some(T0 + 2 * MINUTE);
    assert_eq!(
        store.apply_snapshot(&s, T0).unwrap_err().kind,
        ErrorKind::Conflict
    );
}

#[test]
fn a_closed_action_stays_closed_while_the_snapshot_lists_it() {
    let store = store();
    let entry = SnapshotEntry {
        kind: "assistant.reminder".into(),
        key: "r1".into(),
        slots: json!({"text": "Call"}).as_object().cloned().unwrap(),
        ..SnapshotEntry::default()
    };
    let snap = |firing: Vec<SnapshotEntry>| Snapshot {
        source: "assistant".into(),
        kinds: Some(vec!["assistant.reminder".into()]),
        ..snapshot(firing)
    };
    let first = store.apply_snapshot(&snap(vec![entry.clone()]), T0).unwrap();
    let id = first.opened[0];
    store.press(id, "done", None, "ios", T0 + MINUTE).unwrap();
    let again = store
        .apply_snapshot(&snap(vec![entry.clone()]), T0 + 2 * MINUTE)
        .unwrap();
    assert!(again.opened.is_empty());
    assert_eq!(store.item(id, T0).unwrap().unwrap().state, Some(ItemState::Done));
    // A snapshot without it, then one with it: a new item.
    store.apply_snapshot(&snap(vec![]), T0 + 3 * MINUTE).unwrap();
    let fresh = store.apply_snapshot(&snap(vec![entry]), T0 + 4 * MINUTE).unwrap();
    assert_eq!(fresh.opened.len(), 1);
    assert_ne!(fresh.opened[0], id);
}

#[test]
fn an_unobserved_source_mutes_its_alerts_until_it_reports_again() {
    let store = store();
    let mut s = snapshot(vec![]);
    s.observed = false;
    s.reason = Some("The bucket didn't answer.".into());
    let answer = store.apply_snapshot(&s, T0).unwrap();
    assert!(!answer.observed);
    let source = store
        .sources()
        .unwrap()
        .into_iter()
        .find(|s| s.source == "backup")
        .unwrap();
    assert!(!source.observed);
    assert_eq!(source.reason.as_deref(), Some("The bucket didn't answer."));
    let it = item(&store.apply_event(&failed(), None, T0 + MINUTE).unwrap());
    assert_eq!(it.push.reason, Some(R::Unobserved));
    // Its actions and updates push as usual (ship is another source; assistant too).
    assert_eq!(
        item(&store.apply_event(&reminder("r"), None, T0).unwrap())
            .push
            .status,
        S::Queued
    );
    // Unobserved twice records one notice.
    store.apply_snapshot(&s, T0 + MINUTE).unwrap();
    store
        .apply_snapshot(
            &snapshot(vec![SnapshotEntry {
                kind: "backup.failed".into(),
                key: "nightly".into(),
                facts: json!({"last_ok": T0}).as_object().cloned().unwrap(),
                slots: json!({"reason": "Still full"}).as_object().cloned().unwrap(),
                ..SnapshotEntry::default()
            }]),
            T0 + 2 * MINUTE,
        )
        .unwrap();
    let after = store.item(it.id, T0).unwrap().unwrap();
    assert_eq!(after.state, Some(ItemState::Open));
    assert_eq!(after.push.status, S::Queued, "decided again (unmuted)");
    let all = InboxQuery {
        classes: vec![Class::Update, Class::Record],
        ..InboxQuery::default()
    };
    let kinds: Vec<String> = store
        .inbox(&all, T0 + 3 * MINUTE)
        .unwrap()
        .activity
        .items
        .into_iter()
        .map(|i| i.kind)
        .filter(|k| k.starts_with("beacon."))
        .collect();
    assert_eq!(kinds, ["beacon.source_observed", "beacon.source_unobserved"]);
}

#[test]
fn a_source_goes_stale_without_snapshots() {
    let store = store();
    let mut s = snapshot(vec![]);
    s.stale_after = beacon_core::duration::Duration::from_ms(5 * MINUTE);
    store.apply_snapshot(&s, T0).unwrap();
    store.run_due(T0 + 4 * MINUTE).unwrap();
    assert!(store.sources().unwrap().iter().all(|s| s.observed));
    store.run_due(T0 + 5 * MINUTE).unwrap();
    let source = store
        .sources()
        .unwrap()
        .into_iter()
        .find(|s| s.source == "backup")
        .unwrap();
    assert!(!source.observed);
    assert_eq!(source.reason.as_deref(), Some("It stopped reporting."));
    let unobserved = store
        .inbox(&InboxQuery::default(), T0 + 5 * MINUTE)
        .unwrap()
        .activity
        .items
        .into_iter()
        .find(|i| i.kind == "beacon.source_unobserved")
        .unwrap();
    assert_eq!(unobserved.title, "Can't check Backups");
    assert_eq!(unobserved.push.status, S::None);
}

// ---------- Trail ----------

fn trail_entry(kind: &str, key: &str, title: &str) -> SnapshotEntry {
    SnapshotEntry {
        kind: kind.into(),
        key: key.into(),
        slots: json!({"title": title, "body": "From Trail"})
            .as_object()
            .cloned()
            .unwrap(),
        ..SnapshotEntry::default()
    }
}

#[test]
fn trail_snapshots_open_severe_alerts_and_record_warnings_once_per_appearance() {
    let store = store();
    store.set_trail_open("/trail").unwrap();
    let firing = vec![
        trail_entry("trail.severe", "cpu hot", "CPU is hot"),
        trail_entry("trail.warning", "disk 80", "Disk is filling"),
    ];
    let first = store.apply_trail_snapshot(T0, firing.clone(), T0).unwrap();
    assert_eq!(first.opened.len(), 2);
    let severe = store.item(first.opened[0], T0).unwrap().unwrap();
    assert_eq!(severe.url.as_deref(), Some("/trail"));
    assert_eq!(severe.key, "cpu hot", "Trail's keys are kept as they are");
    let second = store
        .apply_trail_snapshot(T0 + MINUTE, firing.clone(), T0 + MINUTE)
        .unwrap();
    assert!(second.opened.is_empty());
    assert_eq!(second.updated.len(), 1);
    let gone = store
        .apply_trail_snapshot(T0 + 2 * MINUTE, vec![], T0 + 2 * MINUTE)
        .unwrap();
    assert_eq!(gone.resolved, [severe.id]);
    let back = store
        .apply_trail_snapshot(T0 + 3 * MINUTE, firing, T0 + 3 * MINUTE)
        .unwrap();
    assert_eq!(back.opened.len(), 1, "the warning again");
    assert_eq!(back.reopened, [severe.id]);
}

#[test]
fn trail_notices_are_recorded_once_and_late_ones_not_pushed() {
    let store = store();
    let notice = |key: &str| {
        event(
            "trail.severe_notice",
            key,
            json!({}),
            json!({"title": "Login failed", "body": "Three times"}),
        )
    };
    let first = store
        .apply_trail_notice("n-1", &notice("n-1"), false, T0)
        .unwrap();
    assert_eq!(first.outcome, EventOutcome::Opened);
    assert_eq!(item(&first).push.status, S::Queued);
    let again = store
        .apply_trail_notice("n-1", &notice("n-1"), false, T0 + MINUTE)
        .unwrap();
    assert!(again.repeat);
    assert_eq!(item(&again).id, item(&first).id);
    let late = store
        .apply_trail_notice("n-2", &notice("n 2 with spaces"), true, T0)
        .unwrap();
    assert_eq!(item(&late).push.reason, Some(R::Late));
}

// ---------- the inbox ----------

#[test]
fn the_inbox_lists_needs_you_alerts_and_activity() {
    let store = store();
    let alert = item(&store.apply_event(&failed(), None, T0).unwrap());
    let action = item(&store.apply_event(&reminder("r1"), None, T0 + MINUTE).unwrap());
    let other = item(&store.apply_event(&down(), None, T0 + 2 * MINUTE).unwrap());
    store
        .press(other.id, "dismiss", None, "web", T0 + 3 * MINUTE)
        .unwrap();
    // Four updates: with the action, five pushes, inside the budget.
    for n in 0..4 {
        store.apply_event(&ready(n), None, T0 + n * SECOND).unwrap();
    }
    let inbox = store.inbox(&InboxQuery::default(), T0 + HOUR).unwrap();
    assert!(inbox.dot);
    let ids: Vec<i64> = inbox.needs_you.iter().map(|i| i.id).collect();
    assert_eq!(ids, [alert.id, action.id], "the dismissed alert left Needs you");
    let open: Vec<i64> = inbox.alerts.open.iter().map(|i| i.id).collect();
    assert_eq!(open, [other.id, alert.id]);
    // Activity pages by id.
    let page = InboxQuery {
        limit: 2,
        ..InboxQuery::default()
    };
    let first = store.inbox(&page, T0).unwrap().activity;
    assert_eq!(first.items.len(), 2);
    let next = first.next.unwrap();
    let mut rest = Vec::new();
    let mut before = Some(next);
    while let Some(b) = before {
        let a = store
            .inbox(
                &InboxQuery {
                    before: Some(b),
                    ..page.clone()
                },
                T0,
            )
            .unwrap()
            .activity;
        rest.extend(a.items.into_iter().map(|i| i.id));
        before = a.next;
    }
    assert_eq!(rest.len(), 2);
    let by_source = InboxQuery {
        sources: vec!["assistant".into()],
        ..InboxQuery::default()
    };
    assert!(store.inbox(&by_source, T0).unwrap().activity.items.is_empty());
    // Resolved alerts show for 7 days.
    store.apply_event(&resolved(failed()), None, T0 + HOUR).unwrap();
    assert_eq!(
        store
            .inbox(&InboxQuery::default(), T0 + 7 * DAY)
            .unwrap()
            .alerts
            .resolved
            .len(),
        1
    );
    assert!(
        store
            .inbox(&InboxQuery::default(), T0 + 8 * DAY)
            .unwrap()
            .alerts
            .resolved
            .is_empty()
    );
}

#[test]
fn since_last_visit_counts_updates_after_the_previous_visit() {
    let store = store();
    // No visit yet: the last 24 hours.
    store.apply_event(&ready(1), None, T0 - 2 * DAY).unwrap();
    store.apply_event(&ready(2), None, T0 - HOUR).unwrap();
    let summary = store.inbox(&InboxQuery::default(), T0).unwrap().since_last_visit;
    assert_eq!((summary.since, summary.count), (None, 1));
    active(&store, T0, None);
    active(&store, T0 + 30 * SECOND, None);
    inactive(&store, T0 + MINUTE);
    store.apply_event(&ready(3), None, T0 + HOUR).unwrap();
    let mut deploy = event(
        "ship.deployed",
        "web",
        json!({"app": "web"}),
        json!({"summary": "v2"}),
    );
    deploy.key = "web".into();
    store.apply_event(&deploy, None, T0 + HOUR).unwrap();
    // A new visit starts: since is when the last one ended.
    active(&store, T0 + 2 * HOUR, None);
    let summary = store
        .inbox(&InboxQuery::default(), T0 + 2 * HOUR)
        .unwrap()
        .since_last_visit;
    assert_eq!(summary.since, Some(T0 + MINUTE));
    assert_eq!(summary.count, 2);
    assert_eq!(summary.sources.len(), 1);
    assert_eq!(summary.sources[0].name, "Deploys");
    assert_eq!(summary.items.len(), 2);
}

// ---------- the push queue ----------

#[test]
fn a_push_retries_then_fails_or_expires() {
    let store = store();
    let a = item(&store.apply_event(&failed(), None, T0).unwrap());
    let job = &store.pushes_due(T0).unwrap()[0];
    store
        .push_attempted(job.id, &[], PushNext::RetryAt(T0 + 10 * SECOND), T0)
        .unwrap();
    assert!(store.pushes_due(T0 + 5 * SECOND).unwrap().is_empty());
    let job = &store.pushes_due(T0 + 10 * SECOND).unwrap()[0];
    assert_eq!(job.attempts, 1);
    store
        .push_attempted(job.id, &[], PushNext::Failed(R::NoDevices), T0 + 10 * SECOND)
        .unwrap();
    let a = store.item(a.id, T0).unwrap().unwrap();
    assert_eq!((a.push.status, a.push.reason), (S::Failed, Some(R::NoDevices)));
    let mut deploy = event(
        "ship.deployed",
        "web",
        json!({"app": "web"}),
        json!({"summary": "v2"}),
    );
    deploy.key = "web".into();
    let b = item(&store.apply_event(&deploy, None, T0).unwrap());
    assert!(
        store.pushes_due(T0 + HOUR).unwrap().is_empty(),
        "not attempted an hour after it was queued"
    );
    store.run_due(T0 + HOUR).unwrap();
    let b = store.item(b.id, T0).unwrap().unwrap();
    assert_eq!((b.push.status, b.push.reason), (S::Failed, Some(R::Expired)));
}

#[test]
fn devices_register_update_and_are_forgotten() {
    let store = store();
    let token = "0a".repeat(32);
    store
        .register_device(&token, "sandbox", Some("iPhone"), T0)
        .unwrap();
    store
        .register_device(&token, "production", None, T0 + MINUTE)
        .unwrap();
    let devices = store.devices().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].environment, "production");
    assert_eq!(devices[0].name.as_deref(), Some("iPhone"));
    assert_eq!(devices[0].device, "…0a0a0a");
    store.apply_event(&failed(), None, T0).unwrap();
    assert_eq!(
        store.pushes_due(T0).unwrap()[0].devices,
        [(token.clone(), "production".to_string())]
    );
    assert_eq!(
        store
            .register_device("XYZ", "production", None, T0)
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    assert_eq!(
        store
            .register_device(&token, "staging", None, T0)
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    assert!(store.forget_device(&token, T0).unwrap());
    assert!(!store.forget_device(&token, T0).unwrap());
}

// ---------- retention ----------

#[test]
fn retention_deletes_what_closed_long_ago_and_keeps_what_is_open() {
    let store = store();
    let old_update = item(&store.apply_event(&ready(1), None, T0).unwrap());
    let open_alert = item(&store.apply_event(&down(), None, T0).unwrap());
    let closed = item(&store.apply_event(&failed(), None, T0).unwrap());
    store
        .apply_event(&resolved(failed()), None, T0 + 10 * DAY)
        .unwrap();
    store.run_due(T0).unwrap();
    let ran = store.run_due(T0 + 91 * DAY).unwrap();
    assert_eq!(ran.deleted, 1);
    assert!(store.item(old_update.id, T0).unwrap().is_none());
    assert!(store.item(open_alert.id, T0).unwrap().is_some());
    assert!(store.item(closed.id, T0).unwrap().is_some());
    // Once a day.
    assert!(
        store
            .run_due(T0 + 91 * DAY + HOUR)
            .unwrap()
            .ran
            .iter()
            .all(|(_, w)| *w != Due::Retention)
    );
    store.run_due(T0 + 101 * DAY).unwrap();
    assert!(store.item(closed.id, T0).unwrap().is_none());
    assert!(store.item(open_alert.id, T0).unwrap().is_some());
}

#[test]
fn next_due_says_when_to_wake() {
    let store = store();
    store.run_due(T0).unwrap();
    active(&store, T0, None);
    store.apply_event(&ready(1), None, T0).unwrap();
    assert_eq!(store.next_due(T0).unwrap(), Some(T0 + 90 * SECOND));
    let due = store.due(T0 + 90 * SECOND).unwrap();
    assert_eq!(due.len(), 1);
}

#[test]
fn the_registry_lists_every_source() {
    let store = store();
    let sources = store.sources().unwrap();
    let names: Vec<&str> = sources.iter().map(|s| s.source.as_str()).collect();
    assert_eq!(
        names,
        ["assistant", "backup", "beacon", "monitor", "ship", "trail"]
    );
    let beacon = &sources[2];
    assert!(beacon.builtin && beacon.published.is_none() && beacon.observed);
    assert_eq!(sources[1].published, Some(T0 - DAY));
    assert!(store.unpublish("ship").unwrap());
    assert!(!store.unpublish("ship").unwrap());
    assert_eq!(
        store.apply_event(&ready(1), None, T0).unwrap_err().kind,
        ErrorKind::Undeclared
    );
    let beacon = parse(
        beacon_core::builtin::BEACON,
        &Check {
            builtin: true,
            ..Check::default()
        },
    )
    .unwrap();
    assert_eq!(store.publish(&beacon, T0).unwrap_err().kind, ErrorKind::Conflict);
}

// ---------- devices.db, categories, context ----------

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("beacon-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Every text value in every table of the SQLite file at `path`, opened on its own.
fn every_text(path: &std::path::Path) -> String {
    let conn = rusqlite::Connection::open(path).unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut out = String::new();
    for table in tables {
        let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
        let n = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for i in 0..n {
                if let Ok(text) = row.get::<_, String>(i) {
                    out.push_str(&text);
                    out.push('\n');
                }
            }
        }
    }
    out
}

#[test]
fn tokens_live_only_in_devices_db() {
    let dir = temp_dir("devices");
    let path = dir.join("beacon.db");
    let token = "ab12".repeat(16);
    {
        let store = Store::open(&path, Settings::default()).unwrap();
        store
            .publish(&parse(BACKUP, &Check::default()).unwrap(), T0)
            .unwrap();
        store
            .register_device(&token, "production", Some("iPhone"), T0)
            .unwrap();
        store.apply_event(&failed(), None, T0).unwrap();
        let job = &store.pushes_due(T0).unwrap()[0];
        assert_eq!(job.devices, [(token.clone(), "production".to_string())]);
        let attempt = Attempt {
            token: token.clone(),
            environment: "production".into(),
            status: "accepted".into(),
            reason: None,
        };
        store
            .push_attempted(job.id, &[attempt], PushNext::Sent, T0)
            .unwrap();
        let detail = store.item_detail(job.item, T0).unwrap().unwrap();
        assert_eq!(detail.attempts[0].device, "…12ab12");
        assert!(store.set_device_environment(&token, "sandbox", T0).unwrap());
        assert_eq!(store.devices().unwrap()[0].environment, "sandbox");
        assert_eq!(
            store.devices().unwrap()[0].last_status.as_deref(),
            Some("accepted")
        );
    }
    assert!(!every_text(&path).contains(&token), "beacon.db holds no token");
    assert!(every_text(&dir.join("devices.db")).contains(&token));
    let store = Store::open(&path, Settings::default()).unwrap();
    assert_eq!(store.devices().unwrap().len(), 1, "devices survive a restart");
}

#[test]
fn a_version_1_database_moves_its_devices_out() {
    let dir = temp_dir("migrate");
    let path = dir.join("beacon.db");
    let token = "cd34".repeat(16);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        conn.execute(
            "INSERT INTO devices (token, environment, name, created, updated) VALUES (?1, 'production', 'iPhone', 1, 2)",
            [&token],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO attempts (push, item, at, device, environment, attempt, status) \
             VALUES (1, 1, 1, ?1, 'production', 1, 'accepted')",
            [&token],
        )
        .unwrap();
    }
    let store = Store::open(&path, Settings::default()).unwrap();
    let devices = store.devices().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name.as_deref(), Some("iPhone"));
    drop(store);
    assert!(!every_text(&path).contains(&token));
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, 2);
    let device: String = conn
        .query_row("SELECT device FROM attempts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(device, "34cd34");
}

const APPROVALS: &str = r#"
source = "approvals"
name = "Approvals"
icon = "shield"

[kinds."approvals.needed"]
class = "action"
push = "normal"
title = "Approval needed"
body = "{what}"
slots = { what = 80 }
actions = ["done", "dismiss"]
context = true
categories = ["APPROVAL", "APPROVAL_LONG"]
"#;

#[test]
fn a_push_carries_the_kinds_category_or_the_events_choice_and_its_context() {
    let store = store();
    store
        .publish(&parse(APPROVALS, &Check::default()).unwrap(), T0)
        .unwrap();
    let mut e = event("approvals.needed", "1", json!({}), json!({"what": "Deploy web"}));
    e.context = Some("assistant:thread:42".into());
    store.apply_event(&e, None, T0).unwrap();
    let job = &store.pushes_due(T0).unwrap()[0];
    assert_eq!(job.category.as_deref(), Some("APPROVAL"));
    assert_eq!(job.data["context"], "assistant:thread:42");
    e.key = "2".into();
    e.category = Some("APPROVAL_LONG".into());
    store.apply_event(&e, None, T0).unwrap();
    let job = &store.pushes_due(T0).unwrap()[1];
    assert_eq!(job.category.as_deref(), Some("APPROVAL_LONG"));
    e.key = "3".into();
    e.category = Some("APROVAL".into());
    let err = store.apply_event(&e, None, T0).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Undeclared);
    assert!(err.problem().fix.contains("did you mean `APPROVAL`"), "{err}");
    let mut plain = failed();
    plain.category = Some("APPROVAL".into());
    let err = store.apply_event(&plain, None, T0).unwrap_err();
    assert_eq!(err.problem().at, "event category");
    assert!(err.problem().fix.contains("declare `categories"), "{err}");
    let job = &store.pushes_due(T0).unwrap()[0];
    assert!(job.data.get("context").is_some());
    let failed_job = store.apply_event(&failed(), None, T0).unwrap();
    let jobs = store.pushes_due(T0).unwrap();
    let backup = jobs.iter().find(|j| j.item == item(&failed_job).id).unwrap();
    assert_eq!(
        backup.category.as_deref(),
        Some("backup.failed"),
        "no categories: the kind's name"
    );
    assert!(backup.data.get("context").is_none());
}

#[test]
fn a_silence_with_an_idempotency_key_opens_once() {
    let store = store();
    let new = NewSilence {
        matcher: SilenceMatch {
            source: Some("backup".into()),
            ..SilenceMatch::default()
        },
        until: T0 + HOUR,
        reason: "Deploying".into(),
        client: "api".into(),
    };
    let (first, repeat) = store
        .open_silence_once(&new, "k1", "{\"for\":\"1h\"}", T0)
        .unwrap();
    assert!(!repeat);
    let later = NewSilence {
        until: T0 + MINUTE + HOUR,
        ..new.clone()
    };
    let (again, repeat) = store
        .open_silence_once(&later, "k1", "{\"for\":\"1h\"}", T0 + MINUTE)
        .unwrap();
    assert!(repeat);
    assert_eq!(again.id, first.id);
    assert_eq!(store.silences(false, T0 + MINUTE).unwrap().len(), 1);
    let err = store
        .open_silence_once(&new, "k1", "{\"for\":\"2h\"}", T0 + MINUTE)
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
    let err = store.open_silence_once(&new, "a key", "{}", T0).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Invalid);
}
