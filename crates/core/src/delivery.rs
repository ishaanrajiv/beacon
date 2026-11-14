//! The delivery decision (`spec/delivery.md`, "The decision"): whether and when an item's push
//! goes out. [`decide`] is a pure function of the item, why it runs, the stored state, presence
//! and the clock; the store runs it and records what it says in the same transaction as what
//! caused it. Also here: the numbers it reads ([`Settings`]), silences, presence, the catch-up
//! selection and the budget.

use crate::duration::{DAY, HOUR, MINUTE, SECOND};
use crate::model::{Class, PushLevel, PushReason, Trigger};
use crate::quiet::QuietHours;
use crate::template::truncate_words;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The delivery numbers from beacon.toml (`spec/config.md`), in milliseconds. `Default` is the
/// spec's defaults; the server builds one from its config.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `timezone`: quiet hours, the catch-up and the budget's day.
    pub tz: TimeZone,
    /// `push`: false decides and records everything and sends nothing.
    pub push: bool,
    pub quiet_hours: QuietHours,
    /// `presence.active_window`.
    pub active_window: i64,
    /// `delivery.hold`: an action or update waits this long while you're active.
    pub hold: i64,
    /// `delivery.alert_hold`.
    pub alert_hold: i64,
    pub flap_window: i64,
    /// More openings than this within `flap_window` is flapping.
    pub flap_openings: u32,
    pub flap_stable: i64,
    /// Non-alert pushes a local day before `beacon.budget` is recorded.
    pub budget: u32,
    /// `delivery.snooze`: a snooze with no `until`.
    pub snooze: i64,
    /// `delivery.snapshot_stale_after`.
    pub snapshot_stale_after: i64,
    /// `trail.stale_after`.
    pub trail_stale_after: i64,
    /// `delivery.late`: a Trail notice older than this is recorded, not pushed.
    pub late: i64,
    /// `apns.give_up`: no attempt starts this long after a push was queued.
    pub give_up: i64,
    /// `retention`.
    pub retention: i64,
    /// `idempotency`.
    pub idempotency: i64,
    /// `inbox.resolved_days`, as a duration.
    pub resolved_for: i64,
    /// `inbox.since_last_visit_items`.
    pub since_last_visit_items: u32,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            tz: TimeZone::UTC,
            push: true,
            quiet_hours: QuietHours::default(),
            active_window: 75 * SECOND,
            hold: 90 * SECOND,
            alert_hold: 120 * SECOND,
            flap_window: 30 * MINUTE,
            flap_openings: 3,
            flap_stable: 15 * MINUTE,
            budget: 5,
            snooze: HOUR,
            snapshot_stale_after: 15 * MINUTE,
            trail_stale_after: 11 * MINUTE,
            late: HOUR,
            give_up: HOUR,
            retention: 90 * DAY,
            idempotency: DAY,
            resolved_for: 7 * DAY,
            since_last_visit_items: 5,
        }
    }
}

/// Silences that ended more than this long ago are deleted.
pub const SILENCE_KEPT: i64 = 7 * DAY;
/// Devices' presence older than this is deleted.
pub const PRESENCE_KEPT: i64 = DAY;
/// How long `silence_1h` silences for.
pub const SILENCE_1H: i64 = HOUR;
/// The reason a `silence_1h` silence carries.
pub const SILENCE_1H_REASON: &str = "Silenced from a notification";

/// What the decision reads about the item.
#[derive(Debug, Clone)]
pub struct Subject<'a> {
    pub class: Class,
    /// The item's push level: its kind's when it was made.
    pub push: PushLevel,
    pub kind: &'a str,
    pub key: &'a str,
    pub source: &'a str,
    pub labels: &'a BTreeMap<String, String>,
    pub context: Option<&'a str>,
    /// The kind passes quiet hours.
    pub time_bound: bool,
    /// The kind's throttle in ms; None never throttles.
    pub throttle: Option<i64>,
    /// An alert that is flapping.
    pub flapping: bool,
    /// Seen in its current state.
    pub seen: bool,
    /// Every `seen` since its state last changed came from a web client. An urgent push held
    /// for an active iPhone is not suppressed by a web tab marking it seen.
    pub seen_only_on_web: bool,
    /// Still `open`: an alert neither dismissed nor resolved, an action neither done, dismissed,
    /// snoozed nor resolved. Always true for updates and records.
    pub open: bool,
}

/// What a silence matches: every field given must equal the item's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SilenceMatch {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl SilenceMatch {
    /// At least one field is given.
    pub fn is_empty(&self) -> bool {
        self.source.is_none() && self.kind.is_none() && self.key.is_none() && self.labels.is_empty()
    }

    pub fn matches(&self, item: &Subject<'_>) -> bool {
        self.source.as_deref().is_none_or(|s| s == item.source)
            && self.kind.as_deref().is_none_or(|k| k == item.kind)
            && self.key.as_deref().is_none_or(|k| k == item.key)
            && self
                .labels
                .iter()
                .all(|(name, value)| item.labels.get(name) == Some(value))
    }
}

/// A silence as the decision reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Silence {
    pub id: i64,
    pub matcher: SilenceMatch,
    pub until: i64,
    /// When it was ended early.
    pub ended: Option<i64>,
}

impl Silence {
    /// Before `until` and not ended.
    pub fn is_active(&self, now: i64) -> bool {
        now < self.until && self.ended.is_none_or(|ended| now < ended)
    }

    /// When it stops muting: `until`, or when it was ended early.
    pub fn end(&self) -> i64 {
        self.ended.map_or(self.until, |ended| ended.min(self.until))
    }
}

/// One device's presence: its last heartbeat and what it was viewing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    pub device: String,
    /// The heartbeat came from the iPhone app (`client: "ios"`), not a web client.
    pub ios: bool,
    /// The last heartbeat said `active: true`.
    pub active: bool,
    pub last_heartbeat: i64,
    pub viewing: Option<String>,
}

impl Presence {
    /// Active for `active_window` after a heartbeat with `active: true`.
    pub fn is_active(&self, now: i64, active_window: i64) -> bool {
        self.active && now < self.last_heartbeat + active_window
    }

    /// When it stops counting as active; None when it isn't.
    pub fn active_until(&self, active_window: i64) -> Option<i64> {
        self.active.then_some(self.last_heartbeat + active_window)
    }
}

/// The stored state the decision reads besides the item.
#[derive(Debug, Clone)]
pub struct State<'a> {
    pub settings: &'a Settings,
    /// Every silence; only active ones count.
    pub silences: &'a [Silence],
    /// The item's source is unobserved.
    pub source_unobserved: bool,
    /// When a push for the item's `(kind, key)` last went out.
    pub last_push: Option<i64>,
    /// The last push a device has, or may have, for this alert is its recovery: the phone says
    /// it is back.
    pub recovery_shown: bool,
}

/// What the decision says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Push now: `push.status` becomes `queued`.
    Send,
    /// Wait until a time: `push.status` becomes `held`, with `active` or `quiet_hours`.
    Hold { reason: PushReason, until: i64 },
    /// Never meant to push: `push.status` `none`, reason `declared`.
    None,
    /// Decided against: `push.status` `suppressed`.
    Suppress(PushReason),
    /// A repeat in quiet hours: this repeat is skipped, and nothing about the item's push changes.
    Skip,
    /// The silence that matched, for `matched` counting: suppressed `silenced`.
    Silenced(i64),
}

impl Decision {
    /// The reason it records, if any.
    pub fn reason(self) -> Option<PushReason> {
        match self {
            Decision::Send | Decision::Skip => None,
            Decision::Hold { reason, .. } | Decision::Suppress(reason) => Some(reason),
            Decision::None => Some(PushReason::Declared),
            Decision::Silenced(_) => Some(PushReason::Silenced),
        }
    }
}

/// The first active silence that matches the item.
pub fn silenced_by(item: &Subject<'_>, silences: &[Silence], now: i64) -> Option<i64> {
    silences
        .iter()
        .find(|s| s.is_active(now) && s.matcher.matches(item))
        .map(|s| s.id)
}

/// Runs the steps of `spec/delivery.md`, "The decision", in order; the first that applies decides.
/// For `released` it first suppresses an item that was seen (`seen`; for an urgent push, seen on
/// something other than a web client) or left `open` (`closed`).
pub fn decide(
    item: &Subject<'_>,
    trigger: Trigger,
    state: &State<'_>,
    presence: &[Presence],
    now: i64,
) -> Decision {
    let s = state.settings;
    if matches!(trigger, Trigger::Released | Trigger::CaughtUp) {
        if item.seen && !(item.push == PushLevel::Urgent && item.seen_only_on_web) {
            return Decision::Suppress(PushReason::Seen);
        }
        if !item.open {
            return Decision::Suppress(PushReason::Closed);
        }
    }
    // 1. Muted?
    if let Some(id) = silenced_by(item, state.silences, now) {
        return Decision::Silenced(id);
    }
    if item.class == Class::Alert && state.source_unobserved {
        return Decision::Suppress(PushReason::Unobserved);
    }
    // 2. Declared quiet?
    if item.class == Class::Record || item.push == PushLevel::None {
        return Decision::None;
    }
    // 3. Flapping?
    if item.class == Class::Alert && item.flapping {
        return Decision::Suppress(PushReason::Flapping);
    }
    let active: Vec<&Presence> = presence
        .iter()
        .filter(|p| p.is_active(now, s.active_window))
        .collect();
    // 4. Being viewed?
    if trigger != Trigger::Repeat
        && let Some(context) = item.context
        && active.iter().any(|p| p.viewing.as_deref() == Some(context))
    {
        return Decision::Suppress(PushReason::Viewing);
    }
    // 5. You're active? An urgent push is held only for an active iPhone, which shows it in the
    // app: a web tab may be open with nobody in front of it.
    let holding = active.iter().any(|p| p.ios || item.push != PushLevel::Urgent);
    if !matches!(trigger, Trigger::Released | Trigger::Repeat) && holding {
        let wait = if item.class == Class::Alert {
            s.alert_hold
        } else {
            s.hold
        };
        return Decision::Hold {
            reason: PushReason::Active,
            until: now + wait,
        };
    }
    // 6. Quiet hours?
    if s.quiet_hours.contains(now, &s.tz) && item.push != PushLevel::Urgent && !item.time_bound {
        if trigger == Trigger::Repeat {
            return Decision::Skip;
        }
        if let Some(until) = s.quiet_hours.next_end(now, &s.tz) {
            return Decision::Hold {
                reason: PushReason::QuietHours,
                until,
            };
        }
    }
    // 7. Throttled? Not a reopening whose recovery is what the phone shows.
    let recovered = trigger == Trigger::Reopened && state.recovery_shown;
    if !matches!(trigger, Trigger::Repeat | Trigger::Woke | Trigger::CaughtUp)
        && !recovered
        && let (Some(throttle), Some(last)) = (item.throttle, state.last_push)
        && now - last < throttle
    {
        return Decision::Suppress(PushReason::Throttled);
    }
    // 8. Send.
    if !s.push {
        return Decision::Suppress(PushReason::PushOff);
    }
    Decision::Send
}

/// An item held with `quiet_hours`, as the catch-up reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub id: i64,
    pub title: String,
    pub push: PushLevel,
    pub seen: bool,
    pub open: bool,
    pub created: i64,
}

/// What the catch-up does (`spec/delivery.md`, "Catch-up").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchUp {
    /// Items suppressed first, with `seen` or `closed`.
    pub suppressed: Vec<(i64, PushReason)>,
    pub plan: CatchUpPlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatchUpPlan {
    /// Nothing is left.
    Nothing,
    /// One item is left: decide it again with trigger `caught_up`.
    One(i64),
    /// Two or more: record one `beacon.catch_up` with these facts and slots, decide it
    /// (`opened`), and suppress each item (`catch_up`, `covered_by` the new item).
    Many {
        ids: Vec<i64>,
        count: usize,
        titles: String,
        push: PushLevel,
    },
}

/// The most characters of `titles` in a `beacon.catch_up`.
pub const CATCH_UP_TITLES_MAX: usize = 150;

/// Chooses what the catch-up does with the items quiet hours held.
pub fn catch_up(held: &[Held]) -> CatchUp {
    let mut suppressed = Vec::new();
    let mut left: Vec<&Held> = Vec::new();
    for item in held {
        if item.seen {
            suppressed.push((item.id, PushReason::Seen));
        } else if !item.open {
            suppressed.push((item.id, PushReason::Closed));
        } else {
            left.push(item);
        }
    }
    left.sort_by_key(|item| (item.created, item.id));
    let plan = match left.as_slice() {
        [] => CatchUpPlan::Nothing,
        [one] => CatchUpPlan::One(one.id),
        many => {
            let titles: Vec<&str> = many.iter().map(|item| item.title.as_str()).collect();
            let loud = many
                .iter()
                .any(|item| matches!(item.push, PushLevel::Urgent | PushLevel::Normal));
            CatchUpPlan::Many {
                ids: many.iter().map(|item| item.id).collect(),
                count: many.len(),
                titles: truncate_words(&titles.join("; "), CATCH_UP_TITLES_MAX),
                push: if loud {
                    PushLevel::Normal
                } else {
                    PushLevel::Silent
                },
            }
        }
    };
    CatchUp { suppressed, plan }
}

/// One kind's pushes today, for the budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindCount<'a> {
    pub kind: &'a str,
    pub count: u32,
    pub last_push: i64,
}

/// Whether the push that made today's count `count` is the one that records `beacon.budget`:
/// the `budget + 1`th.
pub fn budget_exceeded(count: u32, budget: u32) -> bool {
    count == budget.saturating_add(1)
}

/// The kind `beacon.budget` names: the most pushes today, and on a tie the one that pushed last.
pub fn noisiest<'a>(counts: &[KindCount<'a>]) -> Option<&'a str> {
    counts
        .iter()
        .max_by_key(|c| (c.count, c.last_push))
        .map(|c| c.kind)
}

/// Whether a push counts toward the budget: actions and updates, not alerts.
pub fn counts_toward_budget(class: Class) -> bool {
    matches!(class, Class::Action | Class::Update)
}

#[cfg(test)]
#[path = "delivery_tests.rs"]
mod tests;
