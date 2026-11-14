//! Every step of the decision and its skip rules, with a fake clock (`spec/delivery.md`).

use super::*;
use crate::model::PushReason as R;
use crate::quiet::ClockTime;

/// 2026-11-13 12:00 UTC: midday, outside quiet hours.
const NOON: i64 = 1_794_571_200_000 - (1_794_571_200_000 % DAY) + 12 * HOUR;
/// 01:00 the next night: inside quiet hours (23:00–08:00 UTC).
const NIGHT: i64 = NOON + 13 * HOUR;
const MORNING: i64 = NOON + 20 * HOUR;

fn labels() -> BTreeMap<String, String> {
    BTreeMap::from([("service".to_string(), "backup".to_string())])
}

struct Case {
    class: Class,
    push: PushLevel,
    labels: BTreeMap<String, String>,
    context: Option<&'static str>,
    time_bound: bool,
    throttle: Option<i64>,
    flapping: bool,
    seen: bool,
    seen_only_on_web: bool,
    open: bool,
    settings: Settings,
    silences: Vec<Silence>,
    unobserved: bool,
    last_push: Option<i64>,
    presence: Vec<Presence>,
}

fn case(class: Class) -> Case {
    Case {
        class,
        push: if class == Class::Alert {
            PushLevel::Urgent
        } else if class == Class::Record {
            PushLevel::None
        } else {
            PushLevel::Normal
        },
        labels: labels(),
        context: None,
        time_bound: false,
        throttle: Some(10 * MINUTE),
        flapping: false,
        seen: false,
        seen_only_on_web: false,
        open: true,
        settings: Settings::default(),
        silences: Vec::new(),
        unobserved: false,
        last_push: None,
        presence: Vec::new(),
    }
}

impl Case {
    fn decide(&self, trigger: Trigger, now: i64) -> Decision {
        let subject = Subject {
            class: self.class,
            push: self.push,
            kind: "backup.failed",
            key: "nightly",
            source: "backup",
            labels: &self.labels,
            context: self.context,
            time_bound: self.time_bound,
            throttle: self.throttle,
            flapping: self.flapping,
            seen: self.seen,
            seen_only_on_web: self.seen_only_on_web,
            open: self.open,
        };
        let state = State {
            settings: &self.settings,
            silences: &self.silences,
            source_unobserved: self.unobserved,
            last_push: self.last_push,
        };
        decide(&subject, trigger, &state, &self.presence, now)
    }

    /// The iPhone app is in the foreground.
    fn active(mut self, now: i64, viewing: Option<&str>) -> Case {
        self.presence.push(Presence {
            device: "ios-1".into(),
            ios: true,
            active: true,
            last_heartbeat: now - 10 * SECOND,
            viewing: viewing.map(str::to_string),
        });
        self
    }

    /// A web tab is visible, focused and in use.
    fn active_web(mut self, now: i64) -> Case {
        self.presence.push(Presence {
            device: "web-1".into(),
            ios: false,
            active: true,
            last_heartbeat: now - 10 * SECOND,
            viewing: None,
        });
        self
    }

    fn silenced(mut self, matcher: SilenceMatch, until: i64) -> Case {
        self.silences.push(Silence {
            id: 7,
            matcher,
            until,
            ended: None,
        });
        self
    }
}

const ALL: [Trigger; 8] = [
    Trigger::Opened,
    Trigger::Reopened,
    Trigger::Released,
    Trigger::Woke,
    Trigger::Unmuted,
    Trigger::Settled,
    Trigger::CaughtUp,
    Trigger::Repeat,
];

#[test]
fn an_open_item_with_nothing_in_the_way_sends() {
    for class in [Class::Alert, Class::Action, Class::Update] {
        for trigger in ALL {
            assert_eq!(
                case(class).decide(trigger, NOON),
                Decision::Send,
                "{class:?} {trigger:?}"
            );
        }
    }
}

#[test]
fn step_1_a_matching_silence_mutes_every_class() {
    for class in [Class::Alert, Class::Action, Class::Update] {
        let c = case(class).silenced(
            SilenceMatch {
                source: Some("backup".into()),
                ..SilenceMatch::default()
            },
            NOON + HOUR,
        );
        for trigger in ALL {
            assert_eq!(
                c.decide(trigger, NOON),
                Decision::Silenced(7),
                "{class:?} {trigger:?}"
            );
            assert_eq!(c.decide(trigger, NOON).reason(), Some(R::Silenced));
        }
        // Over at `until`.
        assert_eq!(c.decide(Trigger::Opened, NOON + HOUR), Decision::Send);
    }
}

#[test]
fn step_1_silences_match_on_every_field_given() {
    let m =
        |source: Option<&str>, kind: Option<&str>, key: Option<&str>, labels: &[(&str, &str)]| SilenceMatch {
            source: source.map(Into::into),
            kind: kind.map(Into::into),
            key: key.map(Into::into),
            labels: labels
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        };
    let matching = [
        m(Some("backup"), None, None, &[]),
        m(None, Some("backup.failed"), None, &[]),
        m(None, Some("backup.failed"), Some("nightly"), &[]),
        m(None, None, None, &[("service", "backup")]),
        m(
            Some("backup"),
            Some("backup.failed"),
            Some("nightly"),
            &[("service", "backup")],
        ),
    ];
    for matcher in matching {
        let c = case(Class::Alert).silenced(matcher.clone(), NOON + HOUR);
        assert_eq!(
            c.decide(Trigger::Opened, NOON),
            Decision::Silenced(7),
            "{matcher:?}"
        );
    }
    let missing = [
        m(Some("ship"), None, None, &[]),
        m(None, Some("backup.failed"), Some("weekly"), &[]),
        m(None, None, None, &[("service", "ship")]),
        m(None, None, None, &[("host", "backup")]),
        m(Some("backup"), Some("backup.stale"), None, &[]),
    ];
    for matcher in missing {
        let c = case(Class::Alert).silenced(matcher.clone(), NOON + HOUR);
        assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Send, "{matcher:?}");
    }
    assert!(SilenceMatch::default().is_empty());
}

#[test]
fn step_1_an_ended_or_expired_silence_mutes_nothing() {
    let mut c = case(Class::Alert).silenced(
        SilenceMatch {
            source: Some("backup".into()),
            ..SilenceMatch::default()
        },
        NOON + HOUR,
    );
    c.silences[0].ended = Some(NOON - MINUTE);
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Send);
    assert_eq!(c.silences[0].end(), NOON - MINUTE);
    c.silences[0].ended = None;
    c.silences[0].until = NOON;
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Send);
}

#[test]
fn step_1_an_unobserved_source_mutes_its_alerts_only() {
    let mut c = case(Class::Alert);
    c.unobserved = true;
    for trigger in ALL {
        assert_eq!(
            c.decide(trigger, NOON),
            Decision::Suppress(R::Unobserved),
            "{trigger:?}"
        );
    }
    for class in [Class::Action, Class::Update] {
        let mut c = case(class);
        c.unobserved = true;
        assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Send, "{class:?}");
    }
}

#[test]
fn step_2_records_and_push_none_are_declared_quiet() {
    let c = case(Class::Record);
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::None);
    assert_eq!(Decision::None.reason(), Some(R::Declared));
    let mut c = case(Class::Update);
    c.push = PushLevel::None;
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::None);
    // A silence comes first.
    let c = case(Class::Record).silenced(
        SilenceMatch {
            kind: Some("backup.failed".into()),
            ..SilenceMatch::default()
        },
        NOON + HOUR,
    );
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Silenced(7));
}

#[test]
fn step_3_a_flapping_alert_is_suppressed() {
    let mut c = case(Class::Alert);
    c.flapping = true;
    for trigger in ALL {
        assert_eq!(
            c.decide(trigger, NOON),
            Decision::Suppress(R::Flapping),
            "{trigger:?}"
        );
    }
}

#[test]
fn step_4_an_item_being_viewed_is_suppressed_except_for_repeats() {
    let mut c = case(Class::Update).active(NOON, Some("assistant:thread:42"));
    c.context = Some("assistant:thread:42");
    for trigger in ALL {
        let expected = if trigger == Trigger::Repeat {
            Decision::Send
        } else {
            Decision::Suppress(R::Viewing)
        };
        assert_eq!(c.decide(trigger, NOON), expected, "{trigger:?}");
    }
    // Another context, or a device no longer active, doesn't count.
    c.context = Some("assistant:thread:43");
    assert_eq!(
        c.decide(Trigger::Opened, NOON),
        Decision::Hold {
            reason: R::Active,
            until: NOON + 90 * SECOND
        }
    );
    c.context = Some("assistant:thread:42");
    assert_eq!(c.decide(Trigger::Opened, NOON + 66 * SECOND), Decision::Send);
    // An item without a context is never "viewed".
    let c = case(Class::Update).active(NOON, None);
    assert!(matches!(c.decide(Trigger::Opened, NOON), Decision::Hold { .. }));
}

#[test]
fn step_4_an_inactive_device_viewing_is_ignored() {
    let mut c = case(Class::Update);
    c.context = Some("assistant:thread:42");
    c.presence.push(Presence {
        device: "ios-1".into(),
        ios: true,
        active: false,
        last_heartbeat: NOON,
        viewing: Some("assistant:thread:42".into()),
    });
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Send);
}

#[test]
fn step_5_holds_while_you_are_active() {
    let alert = case(Class::Alert).active(NOON, None);
    let hold_alert = Decision::Hold {
        reason: R::Active,
        until: NOON + 120 * SECOND,
    };
    let hold = Decision::Hold {
        reason: R::Active,
        until: NOON + 90 * SECOND,
    };
    for trigger in ALL {
        let expected = if matches!(trigger, Trigger::Released | Trigger::Repeat) {
            Decision::Send
        } else {
            hold_alert
        };
        assert_eq!(alert.decide(trigger, NOON), expected, "{trigger:?}");
    }
    for class in [Class::Action, Class::Update] {
        let c = case(class).active(NOON, None);
        assert_eq!(c.decide(Trigger::Opened, NOON), hold, "{class:?}");
        assert_eq!(c.decide(Trigger::Released, NOON), Decision::Send, "{class:?}");
    }
    // Active for 75 s after the last heartbeat.
    let c = case(Class::Update).active(NOON, None);
    assert!(matches!(
        c.decide(Trigger::Opened, NOON + 64 * SECOND),
        Decision::Hold { .. }
    ));
    assert_eq!(c.decide(Trigger::Opened, NOON + 65 * SECOND), Decision::Send);
}

#[test]
fn step_5_web_presence_never_holds_an_urgent_push() {
    // A laptop left open on the app at 3am: the urgent alert goes out.
    let c = case(Class::Alert).active_web(NIGHT);
    assert_eq!(c.push, PushLevel::Urgent);
    assert_eq!(c.decide(Trigger::Opened, NIGHT), Decision::Send);
    assert_eq!(c.decide(Trigger::Reopened, NOON), Decision::Send);
    // The phone in hand holds it: the app shows it in its own banner.
    let c = case(Class::Alert).active_web(NOON).active(NOON, None);
    assert_eq!(
        c.decide(Trigger::Opened, NOON),
        Decision::Hold {
            reason: R::Active,
            until: NOON + 120 * SECOND
        }
    );
    // Other levels are held by any active client, as before.
    for (class, push, wait) in [
        (Class::Alert, PushLevel::Normal, 120 * SECOND),
        (Class::Alert, PushLevel::Silent, 120 * SECOND),
        (Class::Action, PushLevel::Normal, 90 * SECOND),
        (Class::Update, PushLevel::Silent, 90 * SECOND),
    ] {
        let mut c = case(class).active_web(NOON);
        c.push = push;
        assert_eq!(
            c.decide(Trigger::Opened, NOON),
            Decision::Hold {
                reason: R::Active,
                until: NOON + wait
            },
            "{class:?} {push:?}"
        );
    }
    // An urgent action is held by the phone only, too.
    let mut c = case(Class::Action).active_web(NOON);
    c.push = PushLevel::Urgent;
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Send);
}

#[test]
fn a_web_seen_never_suppresses_a_held_urgent_push() {
    let mut c = case(Class::Alert);
    c.seen = true;
    c.seen_only_on_web = true;
    assert_eq!(c.decide(Trigger::Released, NOON), Decision::Send);
    // Seen on the phone (or by the API) suppresses it as before.
    c.seen_only_on_web = false;
    assert_eq!(c.decide(Trigger::Released, NOON), Decision::Suppress(R::Seen));
    // Below urgent, a web client's seen still counts.
    c.push = PushLevel::Normal;
    c.seen_only_on_web = true;
    assert_eq!(c.decide(Trigger::Released, NOON), Decision::Suppress(R::Seen));
    // Closed still wins.
    let mut c = case(Class::Alert);
    c.seen = true;
    c.seen_only_on_web = true;
    c.open = false;
    assert_eq!(c.decide(Trigger::Released, NOON), Decision::Suppress(R::Closed));
}

#[test]
fn released_suppresses_what_was_seen_or_closed() {
    let mut c = case(Class::Action).active(NOON, None);
    c.seen = true;
    assert_eq!(c.decide(Trigger::Released, NOON), Decision::Suppress(R::Seen));
    c.seen = false;
    c.open = false;
    assert_eq!(c.decide(Trigger::Released, NOON), Decision::Suppress(R::Closed));
    // Seen wins over closed.
    c.seen = true;
    assert_eq!(c.decide(Trigger::Released, NOON), Decision::Suppress(R::Seen));
    // Otherwise it is decided again without step 5: it pushes though you're still active.
    c.seen = false;
    c.open = true;
    assert_eq!(c.decide(Trigger::Released, NOON), Decision::Send);
    // ...but the other steps still run.
    c.unobserved = true;
    c.class = Class::Alert;
    assert_eq!(
        c.decide(Trigger::Released, NOON),
        Decision::Suppress(R::Unobserved)
    );
}

#[test]
fn step_6_quiet_hours_hold_until_their_end() {
    let until = MORNING; // 08:00
    for class in [Class::Action, Class::Update] {
        let c = case(class);
        for trigger in [
            Trigger::Opened,
            Trigger::Released,
            Trigger::Woke,
            Trigger::Unmuted,
        ] {
            assert_eq!(
                c.decide(trigger, NIGHT),
                Decision::Hold {
                    reason: R::QuietHours,
                    until
                },
                "{class:?} {trigger:?}"
            );
        }
    }
    let mut alert = case(Class::Alert);
    alert.push = PushLevel::Normal;
    assert_eq!(
        alert.decide(Trigger::Reopened, NIGHT),
        Decision::Hold {
            reason: R::QuietHours,
            until
        }
    );
}

#[test]
fn step_6_urgent_and_time_bound_pass_quiet_hours() {
    let urgent = case(Class::Alert);
    assert_eq!(urgent.decide(Trigger::Opened, NIGHT), Decision::Send);
    assert_eq!(urgent.decide(Trigger::Repeat, NIGHT), Decision::Send);
    let mut bound = case(Class::Action);
    bound.time_bound = true;
    assert_eq!(bound.decide(Trigger::Opened, NIGHT), Decision::Send);
    let mut silent = case(Class::Update);
    silent.push = PushLevel::Silent;
    silent.time_bound = true;
    assert_eq!(silent.decide(Trigger::Opened, NIGHT), Decision::Send);
}

#[test]
fn step_6_a_repeat_in_quiet_hours_is_skipped_not_held() {
    let mut c = case(Class::Alert);
    c.push = PushLevel::Normal;
    assert_eq!(c.decide(Trigger::Repeat, NIGHT), Decision::Skip);
    assert_eq!(Decision::Skip.reason(), None);
    assert_eq!(c.decide(Trigger::Repeat, NOON), Decision::Send);
}

#[test]
fn step_6_follows_the_configured_zone_and_window() {
    let mut c = case(Class::Update);
    c.settings.tz = TimeZone::get("Asia/Kolkata").unwrap();
    // NOON UTC is 17:30 in Kolkata: quiet from 17:00 to 18:00 there.
    c.settings.quiet_hours = QuietHours {
        start: ClockTime::parse("17:00").unwrap(),
        end: ClockTime::parse("18:00").unwrap(),
    };
    assert_eq!(
        c.decide(Trigger::Opened, NOON),
        Decision::Hold {
            reason: R::QuietHours,
            until: NOON + 30 * MINUTE
        }
    );
    // Off when start equals end.
    c.settings.quiet_hours.end = c.settings.quiet_hours.start;
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Send);
}

#[test]
fn active_comes_before_quiet_hours() {
    let c = case(Class::Update).active(NIGHT, None);
    assert_eq!(
        c.decide(Trigger::Opened, NIGHT),
        Decision::Hold {
            reason: R::Active,
            until: NIGHT + 90 * SECOND
        }
    );
}

#[test]
fn step_7_throttles_within_the_kind_s_throttle() {
    let mut c = case(Class::Update);
    c.last_push = Some(NOON - 9 * MINUTE);
    for trigger in ALL {
        let expected = if matches!(trigger, Trigger::Repeat | Trigger::Woke | Trigger::CaughtUp) {
            Decision::Send
        } else {
            Decision::Suppress(R::Throttled)
        };
        assert_eq!(c.decide(trigger, NOON), expected, "{trigger:?}");
    }
    c.last_push = Some(NOON - 10 * MINUTE);
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Send);
    // throttle = "none" never throttles.
    c.last_push = Some(NOON - SECOND);
    c.throttle = None;
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Send);
}

#[test]
fn step_8_push_off_suppresses_what_would_send() {
    let mut c = case(Class::Alert);
    c.settings.push = false;
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Suppress(R::PushOff));
    // Everything before still decides as usual.
    c.flapping = true;
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Suppress(R::Flapping));
    c.flapping = false;
    let c = Case {
        settings: Settings {
            push: false,
            ..Settings::default()
        },
        ..case(Class::Update)
    }
    .active(NOON, None);
    assert!(matches!(c.decide(Trigger::Opened, NOON), Decision::Hold { .. }));
}

#[test]
fn the_first_step_that_applies_decides() {
    // Silenced beats flapping, viewing, active, quiet hours and throttle.
    let mut c = case(Class::Alert).active(NIGHT, Some("x")).silenced(
        SilenceMatch {
            source: Some("backup".into()),
            ..SilenceMatch::default()
        },
        NIGHT + HOUR,
    );
    c.flapping = true;
    c.context = Some("x");
    c.last_push = Some(NIGHT);
    assert_eq!(c.decide(Trigger::Opened, NIGHT), Decision::Silenced(7));
    c.silences.clear();
    assert_eq!(c.decide(Trigger::Opened, NIGHT), Decision::Suppress(R::Flapping));
    c.flapping = false;
    assert_eq!(c.decide(Trigger::Opened, NIGHT), Decision::Suppress(R::Viewing));
    c.context = None;
    assert!(matches!(
        c.decide(Trigger::Opened, NIGHT),
        Decision::Hold {
            reason: R::Active,
            ..
        }
    ));
    c.presence.clear();
    c.push = PushLevel::Normal;
    assert!(matches!(
        c.decide(Trigger::Opened, NIGHT),
        Decision::Hold {
            reason: R::QuietHours,
            ..
        }
    ));
    assert_eq!(c.decide(Trigger::Opened, NOON), Decision::Suppress(R::Throttled));
}

fn held(id: i64, title: &str, push: PushLevel, created: i64) -> Held {
    Held {
        id,
        title: title.into(),
        push,
        seen: false,
        open: true,
        created,
    }
}

#[test]
fn catch_up_with_nothing_left() {
    let mut a = held(1, "A", PushLevel::Normal, 1);
    a.seen = true;
    let mut b = held(2, "B", PushLevel::Normal, 2);
    b.open = false;
    let plan = catch_up(&[a, b]);
    assert_eq!(plan.suppressed, vec![(1, R::Seen), (2, R::Closed)]);
    assert_eq!(plan.plan, CatchUpPlan::Nothing);
    assert_eq!(catch_up(&[]).plan, CatchUpPlan::Nothing);
}

#[test]
fn catch_up_with_one_left_decides_it_again() {
    let mut a = held(1, "A", PushLevel::Normal, 1);
    a.seen = true;
    let plan = catch_up(&[a, held(2, "B", PushLevel::Silent, 2)]);
    assert_eq!(plan.suppressed, vec![(1, R::Seen)]);
    assert_eq!(plan.plan, CatchUpPlan::One(2));
}

#[test]
fn catch_up_with_several_records_one_summary() {
    let plan = catch_up(&[
        held(3, "Third", PushLevel::Silent, 30),
        held(1, "First", PushLevel::Silent, 10),
        held(2, "Second", PushLevel::Silent, 20),
    ]);
    assert_eq!(
        plan.plan,
        CatchUpPlan::Many {
            ids: vec![1, 2, 3],
            count: 3,
            titles: "First; Second; Third".into(),
            push: PushLevel::Silent,
        }
    );
    let plan = catch_up(&[
        held(1, "First", PushLevel::Silent, 10),
        held(2, "Second", PushLevel::Normal, 20),
    ]);
    assert!(matches!(
        plan.plan,
        CatchUpPlan::Many {
            push: PushLevel::Normal,
            ..
        }
    ));
    // Titles are cut to 150.
    let many: Vec<Held> = (0..20)
        .map(|i| held(i, "A long title of some words", PushLevel::Silent, i))
        .collect();
    let CatchUpPlan::Many { titles, count, .. } = catch_up(&many).plan else {
        panic!("expected a summary");
    };
    assert_eq!(count, 20);
    assert!(titles.chars().count() <= 150, "{titles}");
    assert!(titles.ends_with('…'));
}

#[test]
fn the_budget_counts_the_sixth_push() {
    assert!(!budget_exceeded(5, 5));
    assert!(budget_exceeded(6, 5));
    assert!(!budget_exceeded(7, 5));
    assert!(budget_exceeded(1, 0));
    assert!(counts_toward_budget(Class::Action));
    assert!(counts_toward_budget(Class::Update));
    assert!(!counts_toward_budget(Class::Alert));
    assert!(!counts_toward_budget(Class::Record));
    let counts = [
        KindCount {
            kind: "ship.deployed",
            count: 3,
            last_push: 10,
        },
        KindCount {
            kind: "backup.done",
            count: 3,
            last_push: 20,
        },
        KindCount {
            kind: "assistant.reply",
            count: 1,
            last_push: 30,
        },
    ];
    assert_eq!(noisiest(&counts), Some("backup.done"));
    assert_eq!(noisiest(&[]), None);
}

#[test]
fn presence_counts_for_the_active_window() {
    let p = Presence {
        device: "d".into(),
        ios: false,
        active: true,
        last_heartbeat: NOON,
        viewing: None,
    };
    assert!(p.is_active(NOON + 74 * SECOND, 75 * SECOND));
    assert!(!p.is_active(NOON + 75 * SECOND, 75 * SECOND));
    assert_eq!(p.active_until(75 * SECOND), Some(NOON + 75 * SECOND));
    let off = Presence { active: false, ..p };
    assert!(!off.is_active(NOON, 75 * SECOND));
    assert_eq!(off.active_until(75 * SECOND), None);
}
