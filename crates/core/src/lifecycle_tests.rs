//! Every row of the alert and action tables in `spec/delivery.md`, with a fake clock.

use super::*;
use crate::duration::{HOUR, MINUTE};
use crate::model::ItemState::{Dismissed, Done, Open, Resolved, Snoozed};

const T0: i64 = 1_794_571_200_000;

fn s() -> Settings {
    Settings::default()
}

fn alert() -> Life {
    Life::new_alert(T0, vec![30 * MINUTE, 2 * HOUR], false).0
}

fn one_off() -> Life {
    Life::new_alert(T0, Vec::new(), true).0
}

fn action() -> Life {
    Life::new_action(T0).0
}

fn button(id: &str, url: Option<&str>) -> Button {
    Button {
        id: id.into(),
        label: id.into(),
        url: url.map(Into::into),
    }
}

fn ids(buttons: &[Button]) -> Vec<&str> {
    buttons.iter().map(|b| b.id.as_str()).collect()
}

// ---------- alerts ----------

#[test]
fn fire_with_no_item_opens_a_new_one() {
    assert_eq!(fire_target(None, T0, &s()), FireTarget::New);
    let (life, step) = Life::new_alert(T0, vec![30 * MINUTE, 2 * HOUR], false);
    assert_eq!(life.state, Some(Open));
    assert_eq!(life.openings, vec![T0]);
    assert!(!life.seen);
    assert_eq!(life.times.opened, Some(T0));
    assert_eq!(life.next_repeat, Some(T0 + 30 * MINUTE));
    assert_eq!(step.outcome, Outcome::Opened);
    assert_eq!(step.decide, Some(Trigger::Opened));
    assert_eq!(step.text, Text::Firing);
}

#[test]
fn fire_after_the_flap_window_opens_a_new_item() {
    let mut life = alert();
    life.apply(Input::Resolve, T0 + MINUTE, &s());
    assert_eq!(
        fire_target(Some(&life), T0 + 31 * MINUTE - 1, &s()),
        FireTarget::Latest
    );
    assert_eq!(fire_target(Some(&life), T0 + 31 * MINUTE, &s()), FireTarget::New);
}

#[test]
fn fire_within_the_flap_window_reopens_the_same_item() {
    let mut life = alert();
    life.apply(Input::Press(Press::Acknowledge), T0 + MINUTE, &s());
    life.seen = true;
    life.apply(Input::Resolve, T0 + 2 * MINUTE, &s());
    let now = T0 + 10 * MINUTE;
    let step = life.apply(Input::Fire, now, &s());
    assert_eq!(step.outcome, Outcome::Reopened);
    assert_eq!(step.decide, Some(Trigger::Reopened));
    assert_eq!(step.text, Text::Firing);
    assert!(!step.flapping);
    assert_eq!(life.state, Some(Open));
    assert_eq!(life.times.opened, Some(now));
    assert_eq!(life.times.resolved, None);
    assert_eq!(life.times.acknowledged, None);
    assert_eq!(life.times.dismissed, None);
    assert!(!life.seen);
    assert_eq!(life.openings, vec![T0, now]);
    assert_eq!(life.repeats_sent, 0);
    assert_eq!(life.next_repeat, Some(now + 30 * MINUTE));
}

#[test]
fn more_than_three_openings_in_thirty_minutes_is_flapping() {
    let mut life = alert();
    let mut now = T0;
    let mut flapped = Vec::new();
    for _ in 0..5 {
        now += MINUTE;
        life.apply(Input::Resolve, now, &s());
        now += MINUTE;
        let step = life.apply(Input::Fire, now, &s());
        flapped.push(step.flapping);
    }
    // The 4th opening makes it flapping; beacon.flapping is recorded once.
    assert_eq!(flapped, vec![false, false, true, false, false]);
    assert!(life.flapping);
    assert_eq!(life.next_repeat, None);
    assert_eq!(life.openings_within(now, &s()), 6);
    // A flapping alert's resolve sends no recovery.
    let step = life.apply(Input::Resolve, now + MINUTE, &s());
    assert!(!step.recovery);
    assert_eq!(step.text, Text::Recovery);
}

#[test]
fn openings_outside_the_window_are_not_counted() {
    let mut life = alert();
    let mut now = T0;
    for _ in 0..6 {
        now += 9 * MINUTE;
        life.apply(Input::Resolve, now, &s());
        now += MINUTE;
        let step = life.apply(Input::Fire, now, &s());
        assert!(!step.flapping, "at {}", (now - T0) / MINUTE);
    }
    assert_eq!(life.openings_within(now, &s()), 3);
    assert!(!life.flapping);
}

#[test]
fn a_flapping_alert_settles_after_fifteen_quiet_minutes() {
    let mut life = alert();
    let mut now = T0;
    for _ in 0..3 {
        now += MINUTE;
        life.apply(Input::Resolve, now, &s());
        now += MINUTE;
        life.apply(Input::Fire, now, &s());
    }
    assert!(life.flapping);
    assert_eq!(life.settles_at(&s()), Some(now + 15 * MINUTE));
    // Not yet.
    assert_eq!(
        life.apply(Input::Settle, now + 15 * MINUTE - 1, &s()).outcome,
        Outcome::Ignored
    );
    // Firing while open isn't an opening: it doesn't move the clock.
    life.apply(Input::Fire, now + 5 * MINUTE, &s());
    let step = life.apply(Input::Settle, now + 15 * MINUTE, &s());
    assert_eq!(step.outcome, Outcome::Settled);
    assert_eq!(step.decide, Some(Trigger::Settled));
    assert!(!life.flapping);
    assert_eq!(life.next_repeat, Some(now + 15 * MINUTE + 30 * MINUTE));
    assert_eq!(life.settles_at(&s()), None);
}

#[test]
fn a_flapping_alert_that_settles_resolved_sends_nothing() {
    let mut life = alert();
    let mut now = T0;
    for _ in 0..3 {
        now += MINUTE;
        life.apply(Input::Resolve, now, &s());
        now += MINUTE;
        life.apply(Input::Fire, now, &s());
    }
    now += MINUTE;
    life.apply(Input::Resolve, now, &s());
    let step = life.apply(Input::Settle, now + 15 * MINUTE, &s());
    assert_eq!(step.outcome, Outcome::Settled);
    assert_eq!(step.decide, None);
    assert!(!life.flapping);
}

#[test]
fn fire_on_open_or_dismissed_updates_without_a_push() {
    for dismissed in [false, true] {
        let mut life = alert();
        if dismissed {
            life.apply(Input::Press(Press::Dismiss), T0 + MINUTE, &s());
        }
        let before = life.clone();
        let step = life.apply(Input::Fire, T0 + 2 * MINUTE, &s());
        assert_eq!(step.outcome, Outcome::Updated);
        assert_eq!(step.decide, None);
        assert_eq!(step.text, Text::Firing);
        assert_eq!(life, before);
    }
}

#[test]
fn resolve_from_open_or_dismissed() {
    for dismissed in [false, true] {
        let mut life = alert();
        if dismissed {
            life.apply(Input::Press(Press::Dismiss), T0 + MINUTE, &s());
        }
        life.seen = true;
        let step = life.apply(Input::Resolve, T0 + HOUR, &s());
        assert_eq!(step.outcome, Outcome::Resolved);
        assert!(step.close_held);
        assert!(step.recovery);
        assert_eq!(step.text, Text::Recovery);
        assert_eq!(step.decide, None);
        assert_eq!(life.state, Some(Resolved));
        assert_eq!(life.times.resolved, Some(T0 + HOUR));
        assert!(!life.seen);
        assert_eq!(life.next_repeat, None);
    }
}

#[test]
fn resolve_on_resolved_is_ignored() {
    let mut life = alert();
    life.apply(Input::Resolve, T0 + MINUTE, &s());
    let before = life.clone();
    assert_eq!(
        life.apply(Input::Resolve, T0 + 2 * MINUTE, &s()).outcome,
        Outcome::Ignored
    );
    assert_eq!(life, before);
}

#[test]
fn acknowledge_stops_repeats() {
    for dismissed in [false, true] {
        let mut life = alert();
        if dismissed {
            life.apply(Input::Press(Press::Dismiss), T0, &s());
            life.next_repeat = Some(T0 + MINUTE);
        }
        let step = life.apply(Input::Press(Press::Acknowledge), T0 + MINUTE, &s());
        assert_eq!(step.outcome, Outcome::Acknowledged);
        assert!(!step.close_held);
        assert_eq!(life.times.acknowledged, Some(T0 + MINUTE));
        assert_eq!(life.next_repeat, None);
        assert_eq!(life.state, Some(if dismissed { Dismissed } else { Open }));
    }
}

#[test]
fn acknowledge_or_dismiss_resolves_a_one_off_keeping_its_text() {
    for press in [Press::Acknowledge, Press::Dismiss] {
        let mut life = one_off();
        life.seen = true;
        let step = life.apply(Input::Press(press), T0 + MINUTE, &s());
        assert_eq!(life.state, Some(Resolved));
        assert_eq!(life.times.resolved, Some(T0 + MINUTE));
        assert!(step.close_held);
        assert!(!step.recovery);
        assert_eq!(step.text, Text::Keep);
        assert!(life.seen, "you resolved it: nothing to miss");
        // A second tap: the effect holds.
        assert!(life.effect_holds(press));
    }
    // An event with state resolved resolves it too, with no recovery.
    let mut life = one_off();
    let step = life.apply(Input::Resolve, T0 + MINUTE, &s());
    assert_eq!(life.state, Some(Resolved));
    assert!(!step.recovery);
    assert_eq!(step.text, Text::Keep);
    assert_eq!(life.next_repeat, None);
}

#[test]
fn dismiss_an_open_alert() {
    let mut life = alert();
    let step = life.apply(Input::Press(Press::Dismiss), T0 + MINUTE, &s());
    assert_eq!(step.outcome, Outcome::Dismissed);
    assert!(step.close_held);
    assert_eq!(life.state, Some(Dismissed));
    assert_eq!(life.times.dismissed, Some(T0 + MINUTE));
    assert_eq!(life.next_repeat, None);
    // A second dismiss changes nothing.
    assert_eq!(
        life.apply(Input::Press(Press::Dismiss), T0 + 2 * MINUTE, &s())
            .outcome,
        Outcome::Ignored
    );
}

#[test]
fn silence_1h_opens_a_silence_and_changes_nothing() {
    for dismissed in [false, true] {
        let mut life = alert();
        if dismissed {
            life.apply(Input::Press(Press::Dismiss), T0, &s());
        }
        let before = life.clone();
        let step = life.apply(Input::Press(Press::Silence1h), T0 + MINUTE, &s());
        assert_eq!(step.outcome, Outcome::Silenced);
        assert!(step.silence);
        assert!(!step.changed());
        assert_eq!(life, before);
    }
}

#[test]
fn repeats_follow_the_list_then_stop() {
    let mut life = alert();
    // Not due yet.
    assert_eq!(
        life.apply(Input::Repeat, T0 + 29 * MINUTE, &s()).outcome,
        Outcome::Ignored
    );
    let first = T0 + 30 * MINUTE;
    let step = life.apply(Input::Repeat, first, &s());
    assert_eq!(step.outcome, Outcome::Repeated);
    assert_eq!(step.decide, Some(Trigger::Repeat));
    assert_eq!(life.repeats_sent, 1);
    assert_eq!(life.next_repeat, Some(first + 2 * HOUR));
    let step = life.apply(Input::Repeat, first + 2 * HOUR, &s());
    assert_eq!(step.outcome, Outcome::Repeated);
    assert_eq!(life.repeats_sent, 2);
    assert_eq!(life.next_repeat, None);
    assert_eq!(
        life.apply(Input::Repeat, first + 9 * HOUR, &s()).outcome,
        Outcome::Ignored
    );
}

#[test]
fn no_repeats_when_acknowledged_dismissed_flapping_or_one_off() {
    let mut life = alert();
    life.times.acknowledged = Some(T0);
    assert_eq!(
        life.apply(Input::Repeat, T0 + HOUR, &s()).outcome,
        Outcome::Ignored
    );
    let mut life = alert();
    life.flapping = true;
    assert_eq!(
        life.apply(Input::Repeat, T0 + HOUR, &s()).outcome,
        Outcome::Ignored
    );
    let mut life = alert();
    life.state = Some(Dismissed);
    assert_eq!(
        life.apply(Input::Repeat, T0 + HOUR, &s()).outcome,
        Outcome::Ignored
    );
    assert_eq!(one_off().next_repeat, None);
    assert_eq!(Life::new_alert(T0, Vec::new(), false).0.next_repeat, None);
}

#[test]
fn alert_buttons_follow_the_state() {
    let declared = vec![
        button("acknowledge", None),
        button("dismiss", None),
        button("silence_1h", None),
        button("logs", Some("https://example.com/logs")),
        button("retry", None),
    ];
    let mut life = alert();
    assert_eq!(
        ids(&life.buttons(&declared)),
        ["acknowledge", "dismiss", "silence_1h", "logs", "retry"]
    );
    life.apply(Input::Press(Press::Acknowledge), T0, &s());
    assert_eq!(
        ids(&life.buttons(&declared)),
        ["dismiss", "silence_1h", "logs", "retry"]
    );
    let mut life = alert();
    life.apply(Input::Press(Press::Dismiss), T0, &s());
    assert_eq!(
        ids(&life.buttons(&declared)),
        ["acknowledge", "silence_1h", "logs", "retry"]
    );
    life.apply(Input::Resolve, T0, &s());
    assert_eq!(ids(&life.buttons(&declared)), ["logs"]);
}

#[test]
fn presses_that_already_hold_answer_ok_and_others_are_refused() {
    let declared = vec![button("acknowledge", None), button("dismiss", None)];
    let mut life = alert();
    life.press(&declared, "acknowledge", None, T0, &s()).unwrap();
    // Acknowledge again: the effect holds.
    assert_eq!(
        life.press(&declared, "acknowledge", None, T0, &s())
            .unwrap()
            .outcome,
        Outcome::Ignored
    );
    life.apply(Input::Resolve, T0, &s());
    // Dismissing a resolved alert doesn't apply.
    let refused = life.press(&declared, "dismiss", None, T0, &s()).unwrap_err();
    assert_eq!(refused, Refused::NotOffered(Vec::new()));
    assert_eq!(
        life.press(&declared, "snooze", None, T0, &s()).unwrap_err(),
        Refused::Unknown
    );
}

// ---------- actions ----------

#[test]
fn open_with_no_item_opens_a_new_action() {
    let (life, step) = Life::new_action(T0);
    assert_eq!(life.state, Some(Open));
    assert!(!life.seen);
    assert_eq!(step.decide, Some(Trigger::Opened));
}

#[test]
fn open_on_open_or_snoozed_updates_without_a_push() {
    for snoozed in [false, true] {
        let mut life = action();
        if snoozed {
            life.apply(Input::Press(Press::Snooze { until: T0 + HOUR }), T0, &s());
        }
        let before = life.clone();
        let step = life.apply(Input::Open, T0 + MINUTE, &s());
        assert_eq!(step.outcome, Outcome::Updated);
        assert_eq!(step.decide, None);
        assert_eq!(life, before);
    }
}

#[test]
fn done_dismiss_and_resolve_close_open_or_snoozed_actions() {
    let cases: [(Input, ItemState, Outcome); 3] = [
        (Input::Press(Press::Done), Done, Outcome::Done),
        (Input::Press(Press::Dismiss), Dismissed, Outcome::Dismissed),
        (Input::Resolve, Resolved, Outcome::Resolved),
    ];
    for snoozed in [false, true] {
        for (input, to, outcome) in cases {
            let mut life = action();
            life.seen = true;
            if snoozed {
                life.apply(Input::Press(Press::Snooze { until: T0 + HOUR }), T0, &s());
            }
            let step = life.apply(input, T0 + MINUTE, &s());
            assert_eq!(step.outcome, outcome);
            assert!(step.close_held);
            assert_eq!(life.state, Some(to));
            assert!(life.seen, "seen is unchanged");
            let at = match to {
                Done => life.times.done,
                Dismissed => life.times.dismissed,
                _ => life.times.resolved,
            };
            assert_eq!(at, Some(T0 + MINUTE));
        }
    }
}

#[test]
fn snooze_then_wake() {
    let mut life = action();
    life.seen = true;
    let step = life.apply(Input::Press(Press::Snooze { until: T0 + HOUR }), T0, &s());
    assert_eq!(step.outcome, Outcome::Snoozed);
    assert!(step.close_held);
    assert_eq!(life.state, Some(Snoozed));
    assert_eq!(life.times.snoozed_until, Some(T0 + HOUR));
    // Snoozing a snoozed action is not offered; its effect holds.
    assert!(life.effect_holds(Press::Snooze { until: T0 }));
    assert_eq!(
        life.apply(Input::Wake, T0 + HOUR - 1, &s()).outcome,
        Outcome::Ignored
    );
    let step = life.apply(Input::Wake, T0 + HOUR, &s());
    assert_eq!(step.outcome, Outcome::Woke);
    assert_eq!(step.decide, Some(Trigger::Woke));
    assert_eq!(life.state, Some(Open));
    assert!(!life.seen);
}

#[test]
fn snooze_without_until_uses_the_default() {
    let declared = vec![button("snooze", None)];
    let mut life = action();
    life.press(&declared, "snooze", None, T0, &s()).unwrap();
    assert_eq!(life.times.snoozed_until, Some(T0 + HOUR));
}

#[test]
fn closed_actions_answer_ok_when_the_effect_holds_else_refuse() {
    let declared = vec![
        button("done", None),
        button("dismiss", None),
        button("snooze", None),
    ];
    for (close, holds) in [
        (Input::Press(Press::Done), "done"),
        (Input::Press(Press::Dismiss), "dismiss"),
        (Input::Resolve, ""),
    ] {
        for press in ["done", "dismiss", "snooze"] {
            let mut life = action();
            life.apply(close, T0, &s());
            let before = life.clone();
            let result = life.press(&declared, press, None, T0 + MINUTE, &s());
            if press == holds {
                assert_eq!(result.unwrap().outcome, Outcome::Ignored);
            } else {
                assert!(matches!(result, Err(Refused::NotOffered(_))), "{close:?} {press}");
            }
            assert_eq!(life, before);
        }
    }
}

#[test]
fn a_custom_action_without_a_url_is_done() {
    let declared = vec![
        button("approve", None),
        button("open", Some("https://example.com/x")),
    ];
    let mut life = action();
    let step = life.press(&declared, "approve", None, T0, &s()).unwrap();
    assert_eq!(step.outcome, Outcome::Done);
    assert!(step.acted);
    assert!(step.close_held);
    assert_eq!(life.state, Some(Done));
    // A link is recorded only, and still offered on a closed item.
    let step = life.press(&declared, "open", None, T0, &s()).unwrap();
    assert_eq!(step.outcome, Outcome::Action);
    assert!(step.acted);
    assert_eq!(life.state, Some(Done));
    assert!(matches!(
        life.press(&declared, "approve", None, T0, &s()),
        Err(Refused::NotOffered(_))
    ));
}

#[test]
fn action_buttons_follow_the_state() {
    let declared = vec![
        button("done", None),
        button("snooze", None),
        button("dismiss", None),
        button("open", Some("https://example.com/x")),
    ];
    let mut life = action();
    assert_eq!(
        ids(&life.buttons(&declared)),
        ["done", "snooze", "dismiss", "open"]
    );
    life.apply(Input::Press(Press::Snooze { until: T0 + HOUR }), T0, &s());
    assert_eq!(ids(&life.buttons(&declared)), ["done", "dismiss", "open"]);
    life.apply(Input::Press(Press::Done), T0, &s());
    assert_eq!(ids(&life.buttons(&declared)), ["open"]);
}

// ---------- updates and records ----------

#[test]
fn updates_and_records_are_recorded_once() {
    let (mut update, step) = Life::record(Class::Update, T0);
    assert_eq!(update.state, None);
    assert_eq!(step.decide, Some(Trigger::Opened));
    assert!(update.is_pushable());
    for input in [
        Input::Fire,
        Input::Open,
        Input::Resolve,
        Input::Wake,
        Input::Repeat,
        Input::Settle,
    ] {
        assert_eq!(update.apply(input, T0, &s()).outcome, Outcome::Ignored);
    }
    assert!(update.apply(Input::Press(Press::Silence1h), T0, &s()).silence);
    let declared = vec![
        button("silence_1h", None),
        button("open", Some("https://example.com")),
    ];
    assert_eq!(ids(&update.buttons(&declared)), ["silence_1h", "open"]);
    let (record, _) = Life::record(Class::Record, T0);
    assert_eq!(ids(&record.buttons(&declared)), ["open"]);
}

#[test]
fn open_and_pushable() {
    let mut life = alert();
    assert!(life.is_open() && life.is_pushable());
    life.apply(Input::Press(Press::Dismiss), T0, &s());
    assert!(life.is_open() && !life.is_pushable());
    let mut life = action();
    life.apply(Input::Press(Press::Snooze { until: T0 + 1 }), T0, &s());
    assert!(life.is_open() && !life.is_pushable());
    life.apply(Input::Press(Press::Done), T0, &s());
    assert!(!life.is_open());
}
