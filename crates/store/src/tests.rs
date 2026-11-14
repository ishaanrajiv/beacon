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

/// The iPhone app is in the foreground.
fn phone_active(store: &Store, now: i64) {
    store
        .heartbeat(
            &Heartbeat {
                client: "ios".into(),
                device: "ios-1".into(),
                active: true,
                viewing: None,
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
fn firing_again_unchanged_writes_nothing() {
    let store = store();
    let first = item(&store.apply_event(&failed(), None, T0).unwrap());
    let version = store.version().unwrap();
    let before = history(&store, first.id);
    let answer = store.apply_event(&failed(), None, T0 + MINUTE).unwrap();
    assert_eq!(answer.outcome, EventOutcome::Updated);
    assert_eq!(item(&answer), first, "the item as it is");
    assert_eq!(store.version().unwrap(), version, "no new inbox ETag");
    assert_eq!(
        history(&store, first.id),
        before,
        "no history row, so no copy to Trail"
    );
    assert_eq!(store.item(first.id, T0).unwrap().unwrap().version, first.version);
    // A snapshot listing it again writes nothing either.
    let entry = SnapshotEntry {
        kind: "backup.failed".into(),
        key: "nightly".into(),
        facts: json!({"last_ok": T0 - 10 * HOUR}).as_object().cloned().unwrap(),
        slots: json!({"reason": "The disk is full"})
            .as_object()
            .cloned()
            .unwrap(),
        ..SnapshotEntry::default()
    };
    let mut snap = snapshot(vec![entry.clone()]);
    snap.kinds = Some(vec!["backup.failed".into()]);
    let answer = store.apply_snapshot(&snap, T0 + 2 * MINUTE).unwrap();
    assert_eq!(answer.updated, [first.id]);
    assert_eq!(store.version().unwrap(), version);
    assert_eq!(history(&store, first.id), before);
    // A label, a fact or a link that changes is an update.
    let mut labelled = entry.clone();
    labelled.labels = std::collections::BTreeMap::from([("disk".to_string(), "b".to_string())]);
    let mut snap = snapshot(vec![labelled]);
    snap.kinds = Some(vec!["backup.failed".into()]);
    store.apply_snapshot(&snap, T0 + 3 * MINUTE).unwrap();
    assert!(store.version().unwrap() > version);
    assert_eq!(history(&store, first.id).len(), before.len() + 1);
    // An action too.
    let r = item(&store.apply_event(&reminder("r1"), None, T0).unwrap());
    let version = store.version().unwrap();
    store.apply_event(&reminder("r1"), None, T0 + MINUTE).unwrap();
    assert_eq!(store.version().unwrap(), version);
    assert_eq!(history(&store, r.id), ["opened"]);
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

fn attempt(status: &str) -> Attempt {
    Attempt {
        token: "a".repeat(64),
        environment: "production".into(),
        status: status.into(),
        reason: (status != "accepted").then(|| "timeout".into()),
    }
}

#[test]
fn a_queued_push_no_device_took_is_cancelled_when_its_alert_resolves() {
    let store = store();
    let it = item(&store.apply_event(&failed(), None, T0).unwrap());
    assert_eq!(it.push.status, S::Queued);
    // Apple rejected it with a retryable answer: no device has it.
    let job = store.pushes_due(T0).unwrap()[0].id;
    store
        .push_attempted(
            job,
            &[attempt("rejected")],
            PushNext::RetryAt(T0 + 10 * SECOND),
            T0,
        )
        .unwrap();
    let after = item(
        &store
            .apply_event(&resolved(failed()), None, T0 + 5 * SECOND)
            .unwrap(),
    );
    assert_eq!(after.push.status, S::Suppressed);
    assert_eq!(after.push.reason, Some(R::Closed));
    // No retry of the firing text, and no recovery for a banner nobody got.
    assert!(store.pushes_due(T0 + HOUR / 2).unwrap().is_empty());
    assert_eq!(history(&store, it.id), ["opened", "resolved", "push_suppressed"]);
}

#[test]
fn a_queued_push_a_device_may_have_is_followed_by_the_recovery() {
    let store = store();
    let it = item(&store.apply_event(&failed(), None, T0).unwrap());
    let job = store.pushes_due(T0).unwrap()[0].id;
    // No clear answer from Apple: it may be on the phone, and is waiting to retry.
    store
        .push_attempted(
            job,
            &[attempt("uncertain")],
            PushNext::RetryAt(T0 + 10 * SECOND),
            T0,
        )
        .unwrap();
    let after = item(
        &store
            .apply_event(&resolved(failed()), None, T0 + 5 * SECOND)
            .unwrap(),
    );
    assert_eq!(after.push.status, S::Queued);
    let due = store.pushes_due(T0 + HOUR / 2).unwrap();
    assert_eq!(
        due.len(),
        1,
        "only the recovery: the firing push makes no more attempts"
    );
    assert_eq!(due[0].purpose, "recovery");
    assert_eq!(due[0].title, after.title);
    assert_eq!(due[0].collapse_id, it.collapse_id);
    sent(&store, T0 + 10 * SECOND);
    let done = store.item(it.id, T0).unwrap().unwrap();
    assert_eq!(done.push.status, S::Sent);
    assert!(history(&store, it.id).contains(&"recovery_sent".to_string()));
}

#[test]
fn a_push_in_flight_when_its_alert_resolves_is_followed_by_the_recovery_if_taken() {
    for (answer, next, recovery) in [
        ("accepted", PushNext::Sent, true),
        ("uncertain", PushNext::RetryAt(T0 + 10 * SECOND), true),
        ("rejected", PushNext::Failed(R::Rejected), false),
    ] {
        let store = store();
        let it = item(&store.apply_event(&failed(), None, T0).unwrap());
        // The sender has taken the job and is talking to Apple...
        let job = store.pushes_due(T0).unwrap()[0].id;
        // ...when the alert resolves: no attempt is recorded yet, so it is cancelled.
        let after = item(&store.apply_event(&resolved(failed()), None, T0 + SECOND).unwrap());
        assert_eq!(after.push.status, S::Suppressed);
        // Then Apple answers.
        store
            .push_attempted(job, &[attempt(answer)], next, T0 + 2 * SECOND)
            .unwrap();
        let due = store.pushes_due(T0 + HOUR / 2).unwrap();
        if recovery {
            assert_eq!(due.len(), 1, "{answer}");
            assert_eq!(due[0].purpose, "recovery");
            let row = store.item(it.id, T0).unwrap().unwrap();
            assert_eq!(row.push.status, S::Queued);
        } else {
            assert!(due.is_empty(), "{answer}");
            let row = store.item(it.id, T0).unwrap().unwrap();
            assert_eq!(row.push.status, S::Suppressed);
        }
    }
}

#[test]
fn a_reopening_after_its_recovery_was_sent_is_not_throttled() {
    let store = store();
    let it = item(&store.apply_event(&failed(), None, T0).unwrap());
    sent(&store, T0);
    store.apply_event(&resolved(failed()), None, T0 + MINUTE).unwrap();
    sent(&store, T0 + MINUTE);
    // Within the kind's 10-minute throttle: the phone says it recovered, so it pushes.
    let back = item(&store.apply_event(&failed(), None, T0 + 2 * MINUTE).unwrap());
    assert_eq!(back.id, it.id);
    assert_eq!(back.state, Some(ItemState::Open));
    assert_eq!(back.push.status, S::Queued);
    let due = store.pushes_due(T0 + 2 * MINUTE).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].purpose, "item");
    assert_eq!(due[0].title, back.title);

    // A recovery that never went out leaves the firing banner up: the throttle applies.
    let store = self::store();
    store.apply_event(&failed(), None, T0).unwrap();
    sent(&store, T0);
    store.apply_event(&resolved(failed()), None, T0 + MINUTE).unwrap();
    let back = item(&store.apply_event(&failed(), None, T0 + 2 * MINUTE).unwrap());
    assert_eq!(
        (back.push.status, back.push.reason),
        (S::Suppressed, Some(R::Throttled))
    );
}

#[test]
fn a_recovery_in_flight_when_its_alert_reopens_is_put_right_if_taken() {
    for (answer, next, taken) in [
        ("accepted", PushNext::Sent, true),
        ("uncertain", PushNext::RetryAt(T0 + 3 * MINUTE), true),
        ("rejected", PushNext::Failed(R::Rejected), false),
    ] {
        let store = store();
        let it = item(&store.apply_event(&failed(), None, T0).unwrap());
        sent(&store, T0);
        store.apply_event(&resolved(failed()), None, T0 + MINUTE).unwrap();
        // The sender has taken the recovery and is talking to Apple...
        let recovery = store.pushes_due(T0 + MINUTE).unwrap()[0].id;
        // ...when the alert fires again, within its throttle: nothing says a device has the
        // recovery yet, so the reopening is throttled and the recovery cancelled.
        let back = item(&store.apply_event(&failed(), None, T0 + 2 * MINUTE).unwrap());
        assert_eq!(
            (back.push.status, back.push.reason),
            (S::Suppressed, Some(R::Throttled))
        );
        // Then Apple answers.
        store
            .push_attempted(recovery, &[attempt(answer)], next, T0 + 2 * MINUTE + SECOND)
            .unwrap();
        let row = store.item(it.id, T0).unwrap().unwrap();
        let due = store.pushes_due(T0 + 2 * MINUTE + SECOND).unwrap();
        if taken {
            // The phone says it is back while it is open: the firing text goes out after all.
            assert_eq!(row.push.status, S::Queued, "{answer}");
            assert_eq!(due.len(), 1, "{answer}");
            assert_eq!(due[0].purpose, "item");
            assert_eq!(due[0].title, back.title);
        } else {
            assert_eq!(
                (row.push.status, row.push.reason),
                (S::Suppressed, Some(R::Throttled)),
                "{answer}"
            );
            assert!(due.is_empty(), "{answer}");
        }
    }
}

#[test]
fn a_reopening_held_after_its_recovery_was_sent_pushes_when_released() {
    let store = store();
    let it = item(&store.apply_event(&failed(), None, T0).unwrap());
    sent(&store, T0);
    store.apply_event(&resolved(failed()), None, T0 + MINUTE).unwrap();
    sent(&store, T0 + MINUTE);
    // The phone is in hand when it fires again: held, then released unseen within the throttle.
    phone_active(&store, T0 + 2 * MINUTE);
    let back = item(&store.apply_event(&failed(), None, T0 + 2 * MINUTE).unwrap());
    assert_eq!(back.push.status, S::Held);
    store.run_due(T0 + 4 * MINUTE).unwrap();
    let after = store.item(it.id, T0).unwrap().unwrap();
    assert_eq!(after.push.status, S::Queued);
    assert_eq!(store.pushes_due(T0 + 4 * MINUTE).unwrap()[0].purpose, "item");
}

#[test]
fn a_dismissed_alert_whose_push_a_device_may_have_still_gets_its_recovery() {
    let store = store();
    let it = item(&store.apply_event(&down(), None, T0).unwrap());
    let job = store.pushes_due(T0).unwrap()[0].id;
    // No clear answer from Apple: it may be on the phone, and is waiting to retry.
    store
        .push_attempted(
            job,
            &[attempt("uncertain")],
            PushNext::RetryAt(T0 + 10 * SECOND),
            T0,
        )
        .unwrap();
    // Dismissed: the queued push stops, and nothing replaces its banner yet.
    store.press(it.id, "dismiss", None, "web", T0 + SECOND).unwrap();
    let dismissed = store.item(it.id, T0).unwrap().unwrap();
    assert_eq!(
        (dismissed.push.status, dismissed.push.reason),
        (S::Suppressed, Some(R::Closed))
    );
    assert!(store.pushes_due(T0 + MINUTE).unwrap().is_empty());
    // Its source resolves it: the banner a device may have is replaced by the recovery.
    let after = item(&store.apply_event(&resolved(down()), None, T0 + MINUTE).unwrap());
    assert_eq!(after.push.status, S::Queued);
    let due = store.pushes_due(T0 + MINUTE).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].purpose, "recovery");

    // A dismissed alert no device has resolves without one.
    let store = self::store();
    let it = item(&store.apply_event(&down(), None, T0).unwrap());
    store.press(it.id, "dismiss", None, "web", T0 + SECOND).unwrap();
    let after = item(&store.apply_event(&resolved(down()), None, T0 + MINUTE).unwrap());
    assert_eq!(after.push.status, S::Suppressed);
    assert!(store.pushes_due(T0 + MINUTE).unwrap().is_empty());
}

#[test]
fn a_push_cancelled_after_the_sender_listed_it_is_no_longer_queued() {
    let store = store();
    let first = item(&store.apply_event(&failed(), None, T0).unwrap());
    let second = item(&store.apply_event(&down(), None, T0).unwrap());
    // The sender lists both, and is still sending the first...
    let batch = store.pushes_due(T0).unwrap();
    assert_eq!(batch.len(), 2);
    let (a, b) = (&batch[0], &batch[1]);
    assert_eq!((a.item, b.item), (first.id, second.id));
    // ...when the second's alert resolves.
    store.apply_event(&resolved(down()), None, T0 + SECOND).unwrap();
    assert!(store.push_queued(a.id).unwrap());
    assert!(!store.push_queued(b.id).unwrap(), "the sender must skip it");
}

#[test]
fn a_reopening_cancels_a_recovery_still_waiting() {
    let store = store();
    let it = item(&store.apply_event(&failed(), None, T0).unwrap());
    sent(&store, T0);
    store.apply_event(&resolved(failed()), None, T0 + MINUTE).unwrap();
    assert_eq!(store.pushes_due(T0 + MINUTE).unwrap()[0].purpose, "recovery");
    store.apply_event(&failed(), None, T0 + 2 * MINUTE).unwrap();
    assert!(
        store
            .pushes_due(T0 + 2 * MINUTE)
            .unwrap()
            .iter()
            .all(|job| job.purpose != "recovery" || job.item != it.id)
    );
}

#[test]
fn an_alert_that_never_pushed_recovers_quietly() {
    let store = store();
    phone_active(&store, T0);
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
    phone_active(&store, T0);
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
fn an_open_web_tab_never_holds_or_sees_away_an_urgent_alert() {
    // Only a web tab is active: the urgent alert goes straight out.
    let store = store();
    active(&store, NIGHT, None);
    let it = item(&store.apply_event(&failed(), None, NIGHT).unwrap());
    assert_eq!(it.push.level, PushLevel::Urgent);
    assert_eq!(it.push.status, S::Queued);
    // A normal push is still held by the tab.
    let pr = item(&store.apply_event(&ready(1), None, T0).unwrap());
    assert_eq!((pr.push.status, pr.push.reason), (S::Held, Some(R::Active)));

    // The phone in hand holds it; the tab's banner marking it seen doesn't suppress it.
    let store = self::store();
    active(&store, T0, None);
    phone_active(&store, T0);
    let it = item(&store.apply_event(&failed(), None, T0).unwrap());
    assert_eq!((it.push.status, it.push.reason), (S::Held, Some(R::Active)));
    store
        .mark_seen(&SeenTarget::Ids(vec![it.id]), "web", T0 + SECOND)
        .unwrap();
    let held = store.item(it.id, T0).unwrap().unwrap();
    assert!(held.seen);
    assert_eq!(held.push.status, S::Held);
    store.run_due(T0 + 3 * MINUTE).unwrap();
    let after = store.item(it.id, T0).unwrap().unwrap();
    assert_eq!(after.push.status, S::Queued);

    // Seen on the phone, it is suppressed at once, as before.
    let store = self::store();
    phone_active(&store, T0);
    let it = item(&store.apply_event(&failed(), None, T0).unwrap());
    store
        .mark_seen(&SeenTarget::Ids(vec![it.id]), "ios", T0 + SECOND)
        .unwrap();
    let after = store.item(it.id, T0).unwrap().unwrap();
    assert_eq!(
        (after.push.status, after.push.reason),
        (S::Suppressed, Some(R::Seen))
    );

    // A web seen, then the phone's: suppressed when the phone's comes.
    let store = self::store();
    phone_active(&store, T0);
    let it = item(&store.apply_event(&failed(), None, T0).unwrap());
    store
        .mark_seen(&SeenTarget::Through(it.id), "web", T0 + SECOND)
        .unwrap();
    assert_eq!(store.item(it.id, T0).unwrap().unwrap().push.status, S::Held);
    // Not newly seen, but the phone's mark still counts.
    assert_eq!(
        store
            .mark_seen(&SeenTarget::Through(it.id), "ios", T0 + 2 * SECOND)
            .unwrap(),
        0
    );
    let after = store.item(it.id, T0).unwrap().unwrap();
    assert_eq!(
        (after.push.status, after.push.reason),
        (S::Suppressed, Some(R::Seen))
    );
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
    // An event for it updates the snoozed item (the same one changes nothing, so writes nothing).
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
            "woke",
            "done",
            "push_suppressed"
        ]
    );
    // The push the wake queued was never taken: closing it cancelled it.
    assert_eq!(
        (done.push.status, done.push.reason),
        (S::Suppressed, Some(R::Closed))
    );
    assert!(store.pushes_due(T0 + 3 * HOUR).unwrap().is_empty());
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
fn two_checks_of_one_source_keep_their_own_bookkeeping() {
    // One check of `assistant` covers its reminders, another its questions. A snapshot only
    // touches the kinds it covers: before, the questions' snapshot forgot that the reminder was
    // listed, so the next reminders' snapshot opened the reminder you had closed again.
    let store = store();
    let reminder = SnapshotEntry {
        kind: "assistant.reminder".into(),
        key: "r1".into(),
        slots: json!({"text": "Call"}).as_object().cloned().unwrap(),
        ..SnapshotEntry::default()
    };
    let covering = |kind: &str, firing: Vec<SnapshotEntry>| Snapshot {
        source: "assistant".into(),
        kinds: Some(vec![kind.into()]),
        ..snapshot(firing)
    };
    let first = store
        .apply_snapshot(&covering("assistant.reminder", vec![reminder.clone()]), T0)
        .unwrap();
    let id = first.opened[0];
    store.press(id, "done", None, "ios", T0 + MINUTE).unwrap();
    store
        .apply_snapshot(&covering("assistant.question", vec![]), T0 + 2 * MINUTE)
        .unwrap();
    let again = store
        .apply_snapshot(&covering("assistant.reminder", vec![reminder]), T0 + 3 * MINUTE)
        .unwrap();
    assert!(again.opened.is_empty(), "{again:?}");
    assert_eq!(store.item(id, T0).unwrap().unwrap().state, Some(ItemState::Done));
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
    // Unobserved twice opens one alert, which pushes: it belongs to `beacon`, never muted by
    // the rule it reports on.
    store.apply_snapshot(&s, T0 + MINUTE).unwrap();
    let open = store
        .inbox(&InboxQuery::default(), T0 + MINUTE)
        .unwrap()
        .alerts
        .open;
    let stopped: Vec<_> = open
        .iter()
        .filter(|i| i.kind == "beacon.source_stopped")
        .collect();
    assert_eq!(stopped.len(), 1);
    let stopped = stopped[0].clone();
    assert_eq!(stopped.source, "beacon");
    assert_eq!(stopped.key, "backup");
    assert_eq!(stopped.title, "Can't check Backups");
    assert_eq!(
        stopped.body,
        "The bucket didn't answer. Its alerts are on hold until it reports again."
    );
    assert_eq!(stopped.push.level, PushLevel::Normal);
    assert_eq!(stopped.push.status, S::Queued);
    sent(&store, T0 + MINUTE);
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
    // Observed again: the alert resolves with its recovery, in place of the record.
    let back = store.item(stopped.id, T0).unwrap().unwrap();
    assert_eq!(back.state, Some(ItemState::Resolved));
    assert_eq!(back.title, "Checking Backups again");
    assert_eq!(back.body, "Its alerts are back on. Lasted 2 min.");
    assert_eq!(back.push.status, S::Queued, "the recovery");
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
    assert!(kinds.is_empty(), "{kinds:?}");
}

#[test]
fn a_source_with_no_alert_kinds_records_an_update_and_a_record() {
    let store = store();
    let notes = r#"
source = "notes"
name = "Notes"
icon = "book"

[kinds."notes.saved"]
class = "update"
push = "silent"
title = "Note saved"
"#;
    store
        .publish(&parse(notes, &Check::default()).unwrap(), T0 - DAY)
        .unwrap();
    let snap = |observed: bool| Snapshot {
        source: "notes".into(),
        observed,
        reason: (!observed).then(|| "The folder didn't load.".into()),
        stale_after: None,
        sent_at: None,
        kinds: Some(vec![]),
        firing: vec![],
    };
    store.apply_snapshot(&snap(false), T0).unwrap();
    store.apply_snapshot(&snap(true), T0 + MINUTE).unwrap();
    let all = InboxQuery {
        classes: vec![Class::Update, Class::Record],
        ..InboxQuery::default()
    };
    let inbox = store.inbox(&all, T0 + MINUTE).unwrap();
    let kinds: Vec<&str> = inbox.activity.items.iter().map(|i| i.kind.as_str()).collect();
    assert_eq!(kinds, ["beacon.source_observed", "beacon.source_unobserved"]);
    assert_eq!(inbox.activity.items[1].push.status, S::None);
    assert!(inbox.alerts.open.is_empty());
}

#[test]
fn a_silence_still_mutes_source_stopped() {
    let store = store();
    store
        .open_silence(
            &NewSilence {
                matcher: SilenceMatch {
                    kind: Some("beacon.source_stopped".into()),
                    ..SilenceMatch::default()
                },
                reason: "Moving the backups".into(),
                until: T0 + HOUR,
                client: "api".into(),
            },
            T0,
        )
        .unwrap();
    let mut s = snapshot(vec![]);
    s.observed = false;
    s.reason = Some("The bucket didn't answer.".into());
    store.apply_snapshot(&s, T0).unwrap();
    let open = store.inbox(&InboxQuery::default(), T0).unwrap().alerts.open;
    let stopped = open.iter().find(|i| i.kind == "beacon.source_stopped").unwrap();
    assert_eq!(stopped.push.reason, Some(R::Silenced));
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
    let stopped = store
        .inbox(&InboxQuery::default(), T0 + 5 * MINUTE)
        .unwrap()
        .alerts
        .open
        .into_iter()
        .find(|i| i.kind == "beacon.source_stopped")
        .unwrap();
    assert_eq!(stopped.title, "Can't check Backups");
    assert_eq!(
        stopped.body,
        "It stopped reporting. Its alerts are on hold until it reports again."
    );
    assert_eq!(stopped.push.status, S::Queued);
}

#[test]
fn trail_going_quiet_opens_source_stopped() {
    let store = store();
    store.apply_trail_snapshot(T0, vec![], T0).unwrap();
    store.run_due(T0 + 11 * MINUTE).unwrap();
    let stopped = store
        .inbox(&InboxQuery::default(), T0 + 11 * MINUTE)
        .unwrap()
        .alerts
        .open
        .into_iter()
        .find(|i| i.kind == "beacon.source_stopped")
        .unwrap();
    assert_eq!(stopped.key, "trail");
    assert_eq!(stopped.push.status, S::Queued);
    store
        .apply_trail_snapshot(T0 + 12 * MINUTE, vec![], T0 + 12 * MINUTE)
        .unwrap();
    let after = store.item(stopped.id, T0).unwrap().unwrap();
    assert_eq!(after.state, Some(ItemState::Resolved));
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

// ---------- stored declarations ----------

/// Rewrites a stored declarations row (or an item's kind) with `edit`, as another version might
/// have written it, with the store closed.
fn rewrite_stored(path: &std::path::Path, sql: &str, edit: impl Fn(&mut Value)) {
    let conn = rusqlite::Connection::open(path).unwrap();
    let rows: Vec<(String, String)> = conn
        .prepare(sql)
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for (id, text) in rows {
        let mut value: Value = serde_json::from_str(&text).unwrap();
        edit(&mut value);
        let table = if sql.contains("registry") {
            "registry SET decl"
        } else {
            "items SET kind_decl"
        };
        let column = if sql.contains("registry") { "source" } else { "id" };
        conn.execute(
            &format!("UPDATE {table} = ?1 WHERE {column} = ?2"),
            [value.to_string(), id],
        )
        .unwrap();
    }
}

#[test]
fn stored_declarations_carry_a_format_and_read_back_tolerantly() {
    let dir = temp_dir("stored");
    let path = dir.join("beacon.db");
    {
        let store = Store::open(&path, Settings::default()).unwrap();
        store
            .publish(&parse(BACKUP, &Check::default()).unwrap(), T0)
            .unwrap();
        store.apply_event(&failed(), None, T0).unwrap();
    }
    let conn = rusqlite::Connection::open(&path).unwrap();
    let stored: String = conn
        .query_row("SELECT decl FROM registry WHERE source = 'backup'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&stored).unwrap()["format"], 1);
    let kind: String = conn
        .query_row("SELECT kind_decl FROM items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&kind).unwrap()["format"], 1);
    drop(conn);

    // A newer version wrote fields this one doesn't know: before, the whole source was dropped.
    rewrite_stored(&path, "SELECT source, decl FROM registry", |v| {
        v["colour"] = "teal".into();
        v["kinds"]["backup.failed"]["escalate_after"] = "1h".into();
    });
    rewrite_stored(&path, "SELECT CAST(id AS TEXT), kind_decl FROM items", |v| {
        v["escalate_after"] = "1h".into();
    });
    let store = Store::open(&path, Settings::default()).unwrap();
    assert!(store.unreadable().unwrap().is_empty());
    let answer = store.apply_event(&failed(), None, T0 + MINUTE).unwrap();
    assert_eq!(answer.outcome, EventOutcome::Updated);
    let backup = store.declarations("backup").unwrap().unwrap();
    assert_eq!(backup, parse(BACKUP, &Check::default()).unwrap());
}

#[test]
fn a_stored_source_that_cant_be_read_is_kept_and_reported() {
    let dir = temp_dir("unreadable");
    let path = dir.join("beacon.db");
    let id;
    {
        let store = Store::open(&path, Settings::default()).unwrap();
        for text in [BACKUP, SHIP] {
            store
                .publish(&parse(text, &Check::default()).unwrap(), T0)
                .unwrap();
        }
        id = item(&store.apply_event(&failed(), None, T0).unwrap()).id;
    }
    // One kind of backup can't be read; ship can't be read at all; an item's kind is garbage.
    rewrite_stored(&path, "SELECT source, decl FROM registry", |v| {
        if v["source"] == "backup" {
            v["kinds"]["backup.failed"]["class"] = "page".into();
        } else {
            *v = json!(["not", "declarations"]);
        }
    });
    rewrite_stored(&path, "SELECT CAST(id AS TEXT), kind_decl FROM items", |v| {
        *v = json!({"class": 3});
    });
    let store = Store::open(&path, Settings::default()).unwrap();
    let unreadable = store.unreadable().unwrap();
    assert_eq!(unreadable.len(), 2);
    assert_eq!(unreadable[0].kinds, ["backup.failed"]);
    assert!(unreadable[1].kinds.is_empty());

    // Both still show in the registry, with their problem.
    let sources = store.sources().unwrap();
    let backup = sources.iter().find(|s| s.source == "backup").unwrap();
    assert!(backup.problem.as_ref().unwrap().message.contains("backup.failed"));
    assert_eq!(
        backup.kinds,
        parse(BACKUP, &Check::default()).unwrap().kinds.len() - 1
    );
    let ship = sources.iter().find(|s| s.source == "ship").unwrap();
    assert!(
        ship.problem
            .as_ref()
            .unwrap()
            .fix
            .contains("publish `ship` again")
    );
    assert_eq!((ship.kinds, ship.published), (0, Some(T0)));

    // Events for what can't be read are refused with the fix; the rest of the source works.
    let e = store.apply_event(&failed(), None, T0).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Undeclared);
    assert!(
        e.problems[0].message.contains("can't read it as stored"),
        "{:?}",
        e.problems
    );
    assert!(e.problems[0].fix.contains("publish `backup` again"));
    let e = store.apply_event(&ready(1), None, T0).unwrap_err();
    assert!(
        e.problems[0].fix.contains("publish `ship` again"),
        "{:?}",
        e.problems
    );
    let finished = event(
        "backup.finished",
        "",
        json!({"size": "2 GB", "took": 60_000}),
        json!({}),
    );
    store.apply_event(&finished, None, T0).unwrap();
    let mut snap = Snapshot {
        source: "ship".into(),
        observed: true,
        reason: None,
        stale_after: None,
        sent_at: None,
        kinds: None,
        firing: vec![],
    };
    let e = store.apply_snapshot(&snap, T0).unwrap_err();
    assert!(e.problems[0].fix.contains("publish `ship` again"));
    snap.source = "backup".into();
    snap.kinds = Some(vec!["backup.failed".into()]);
    let e = store.apply_snapshot(&snap, T0).unwrap_err();
    assert!(
        e.problems[0].fix.contains("publish `backup` again"),
        "{:?}",
        e.problems
    );

    // The item whose kind can't be read still loads, with its text and buttons.
    let detail = store.item_detail(id, T0).unwrap().unwrap();
    assert_eq!(detail.item.title, "Backup failed");
    assert!(!detail.item.actions.is_empty());
    assert!(
        !store
            .inbox(&InboxQuery::default(), T0)
            .unwrap()
            .needs_you
            .is_empty()
    );

    // Publishing again puts it right.
    store
        .publish(&parse(BACKUP, &Check::default()).unwrap(), T0 + MINUTE)
        .unwrap();
    assert_eq!(store.unreadable().unwrap().len(), 1);
    store.apply_event(&failed(), None, T0 + MINUTE).unwrap();
}

// ---------- publishing ----------

/// The backup example without some of its kinds, and under another display name.
fn backup_without(kinds: &[&str], name: &str) -> beacon_core::decl::Declarations {
    let mut decl = parse(BACKUP, &Check::default()).unwrap();
    for kind in kinds {
        decl.kinds.remove(*kind);
    }
    decl.name = name.into();
    decl
}

#[test]
fn a_keep_publish_adds_and_replaces_kinds_and_keeps_the_rest() {
    let store = store();
    let all = parse(BACKUP, &Check::default()).unwrap();
    let mut body = backup_without(
        &["backup.failed", "backup.stale", "backup.space"],
        "Nightly backups",
    );
    let mut renamed = all.kinds["backup.space"].clone();
    renamed.title = "Backup disk is filling".into();
    body.kinds.insert("backup.disk".into(), renamed);
    let published = store.publish_with(&body, true, T0).unwrap();
    assert_eq!(published.kept, ["backup.failed", "backup.space", "backup.stale"]);
    assert_eq!(published.kinds, all.kinds.len() + 1);
    assert!(published.closed.is_empty());
    let now = store.declarations("backup").unwrap().unwrap();
    assert_eq!(
        now.name, "Nightly backups",
        "the source's own fields are the body's"
    );
    assert_eq!(now.kinds["backup.failed"], all.kinds["backup.failed"]);
    assert!(now.kinds.contains_key("backup.disk"));
    // The old code and the new can both send.
    store.apply_event(&failed(), None, T0).unwrap();
    let disk = event("backup.disk", "", json!({"used_percent": 91}), json!({}));
    store.apply_event(&disk, None, T0).unwrap();
    // A keep publish of the same kind replaces it by name.
    let mut changed = parse(BACKUP, &Check::default()).unwrap();
    changed.kinds.get_mut("backup.failed").unwrap().push = PushLevel::Normal;
    store.publish_with(&changed, true, T0 + MINUTE).unwrap();
    assert_eq!(
        store.declarations("backup").unwrap().unwrap().kinds["backup.failed"].push,
        PushLevel::Normal
    );
}

#[test]
fn a_replacing_publish_closes_what_it_removed_the_kind_of() {
    let store = store();
    store
        .register_device(&"ab".repeat(32), "production", None, T0)
        .unwrap();
    // An alert whose push went out, one that never pushed, and an action, of kinds about to go.
    let pushed = item(&store.apply_event(&failed(), None, T0).unwrap());
    sent(&store, T0);
    phone_active(&store, T0);
    let quiet = event(
        "backup.stale",
        "",
        json!({"since": DAY, "last_ok": T0 - DAY}),
        json!({}),
    );
    let held = item(&store.apply_event(&quiet, None, T0).unwrap());
    assert_eq!(held.push.status, S::Held);
    let asked = item(&store.apply_event(&reminder("r1"), None, T0).unwrap());
    let done = item(&store.apply_event(&reminder("r2"), None, T0).unwrap());
    store.press(done.id, "done", None, "api", T0).unwrap();
    // A kind that stays is untouched.
    let space = event("backup.space", "", json!({"used_percent": 95}), json!({}));
    let staying = item(&store.apply_event(&space, None, T0).unwrap());

    let published = store
        .publish(
            &backup_without(&["backup.failed", "backup.stale"], "Backups"),
            T0 + HOUR,
        )
        .unwrap();
    assert_eq!(published.closed, [pushed.id, held.id]);
    for id in [pushed.id, held.id] {
        let it = store.item(id, T0 + HOUR).unwrap().unwrap();
        assert_eq!(it.state, Some(ItemState::Resolved));
        assert_eq!(it.times.unwrap().resolved, Some(T0 + HOUR));
        assert!(history(&store, id).contains(&"kind_removed".to_string()));
    }
    let after = store.item(pushed.id, T0 + HOUR).unwrap().unwrap();
    assert_eq!(after.title, pushed.title, "no recovery text is invented");
    assert_eq!(after.body, pushed.body);
    // Its firing push reached the phone: the usual recovery replaces the banner, with its text.
    let jobs = store.pushes_due(T0 + HOUR).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!((jobs[0].item, jobs[0].purpose.as_str()), (pushed.id, "recovery"));
    assert_eq!(jobs[0].title, pushed.title);
    // The held one never pushed: suppressed, closed.
    let held = store.item(held.id, T0 + HOUR).unwrap().unwrap();
    assert_eq!(
        (held.push.status, held.push.reason),
        (S::Suppressed, Some(R::Closed))
    );
    assert!(history(&store, held.id).contains(&"kind_removed".to_string()));
    // Other sources and kept kinds are untouched.
    for id in [asked.id, staying.id] {
        assert!(store.item(id, T0 + HOUR).unwrap().unwrap().state == Some(ItemState::Open));
    }
    // The assistant's action closes when its own source drops the kind.
    let mut assistant = parse(ASSISTANT, &Check::default()).unwrap();
    assistant.kinds.remove("assistant.reminder");
    let published = store.publish(&assistant, T0 + 2 * HOUR).unwrap();
    assert_eq!(published.closed, [asked.id]);
    let closed = store.item(asked.id, T0 + 2 * HOUR).unwrap().unwrap();
    assert_eq!(closed.state, Some(ItemState::Resolved));
    assert_eq!(
        store.item(done.id, T0 + 2 * HOUR).unwrap().unwrap().state,
        Some(ItemState::Done)
    );
}
