//! The words Beacon sorts by: a kind's class and push level, an item's state, what happened to its
//! push, the actions a person can take, and the icons a source may show. Each is a closed set,
//! spelled on the wire exactly as `as_str` returns it (`spec/declarations.md`, `spec/api.md`).

use serde::{Deserialize, Serialize};

/// What a notification asks of you. It decides where the item lives and whether it has a lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Something is broken. Opens, resolves, can flap.
    Alert,
    /// Needs your decision. Open until done, dismissed, snoozed or resolved by its source.
    Action,
    /// Worth knowing. One-off.
    Update,
    /// Audit trail only. One-off, never pushed.
    Record,
}

impl Class {
    pub const ALL: [Class; 4] = [Class::Alert, Class::Action, Class::Update, Class::Record];

    pub fn as_str(self) -> &'static str {
        match self {
            Class::Alert => "alert",
            Class::Action => "action",
            Class::Update => "update",
            Class::Record => "record",
        }
    }

    pub fn parse(text: &str) -> Option<Class> {
        Class::ALL.into_iter().find(|c| c.as_str() == text)
    }

    /// Whether items of this class have a state (`open`, `resolved`, ...). Updates and records don't.
    pub fn has_lifecycle(self) -> bool {
        matches!(self, Class::Alert | Class::Action)
    }
}

/// How a kind may reach the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PushLevel {
    /// Time Sensitive: breaks through Focus. Alerts only.
    Urgent,
    /// A banner with sound.
    Normal,
    /// Passive: no sound, no lit screen; lands in the notification list.
    Silent,
    /// Never pushed.
    None,
}

impl PushLevel {
    pub const ALL: [PushLevel; 4] = [
        PushLevel::Urgent,
        PushLevel::Normal,
        PushLevel::Silent,
        PushLevel::None,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PushLevel::Urgent => "urgent",
            PushLevel::Normal => "normal",
            PushLevel::Silent => "silent",
            PushLevel::None => "none",
        }
    }

    pub fn parse(text: &str) -> Option<PushLevel> {
        PushLevel::ALL.into_iter().find(|p| p.as_str() == text)
    }

    /// The APNs `interruption-level` (`spec/apns.md`). None for `none`, which is never sent.
    pub fn interruption_level(self) -> Option<&'static str> {
        match self {
            PushLevel::Urgent => Some("time-sensitive"),
            PushLevel::Normal => Some("active"),
            PushLevel::Silent => Some("passive"),
            PushLevel::None => None,
        }
    }
}

/// Where an alert or action item is in its lifecycle (`spec/delivery.md`). Updates and records
/// have no state: their `state` is null on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemState {
    /// An alert that is firing, or an action waiting for you.
    Open,
    /// An alert whose source reported recovery, or an action its source closed.
    Resolved,
    /// An action you handled.
    Done,
    /// An action hidden until a time, when it opens again.
    Snoozed,
    /// An action you won't do, or a firing alert you hid from Needs you.
    Dismissed,
}

impl ItemState {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemState::Open => "open",
            ItemState::Resolved => "resolved",
            ItemState::Done => "done",
            ItemState::Snoozed => "snoozed",
            ItemState::Dismissed => "dismissed",
        }
    }
}

/// What happened to an item's push (`spec/api.md`, "Item").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PushStatus {
    /// Never meant to push: a record, or a kind declared `push = "none"`.
    None,
    /// Waiting: you're active somewhere, or it is quiet hours. `release_at` says until when.
    Held,
    /// Being sent to Apple, or waiting for a retry.
    Queued,
    /// Apple took it for at least one device.
    Sent,
    /// No device took it, and Beacon has stopped trying.
    Failed,
    /// Decided against: `reason` says why.
    Suppressed,
}

/// Why a push is held, suppressed, or failed. Spelled in snake_case on the wire (`quiet_hours`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushReason {
    /// none: the kind's push is `none`, or its class is `record`.
    Declared,
    /// suppressed: a silence matched.
    Silenced,
    /// suppressed: its source is unobserved, which mutes the source's alerts.
    Unobserved,
    /// suppressed: the alert is flapping.
    Flapping,
    /// suppressed: a client was viewing the item's context.
    Viewing,
    /// held: you were active on a client.
    Active,
    /// held: quiet hours.
    QuietHours,
    /// suppressed: seen while it was held.
    Seen,
    /// suppressed: done, dismissed, snoozed or resolved while it was held.
    Closed,
    /// suppressed: the kind and key pushed within the throttle.
    Throttled,
    /// suppressed: the morning catch-up push stood in for it.
    CatchUp,
    /// suppressed: an event more than an hour old (a late Trail notice).
    Late,
    /// suppressed: `push = false` in beacon.toml.
    PushOff,
    /// failed: no device is registered.
    NoDevices,
    /// failed: Apple refused it for every device.
    Rejected,
    /// failed: still not taken an hour after it was queued.
    Expired,
    /// failed: no APNs signing key is configured.
    Unconfigured,
}

/// Why the delivery decision runs for an item (`spec/delivery.md`, "The decision").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// An alert or action opened, or an update was recorded.
    Opened,
    /// A resolved alert fired again within the flap window.
    Reopened,
    /// A hold reached its release time.
    Released,
    /// A snoozed action's time came.
    Woke,
    /// A silence that muted an open alert ended, or its source was observed again.
    Unmuted,
    /// A flapping alert settled while firing.
    Settled,
    /// The end of quiet hours, for the one item they held.
    CaughtUp,
    /// An unacknowledged alert's next repeat came due.
    Repeat,
}

/// The actions Beacon itself understands. A kind may also declare custom actions (links).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinAction {
    Done,
    Acknowledge,
    Snooze,
    Dismiss,
    #[serde(rename = "silence_1h")]
    Silence1h,
}

impl BuiltinAction {
    /// In the order buttons are shown.
    pub const ALL: [BuiltinAction; 5] = [
        BuiltinAction::Done,
        BuiltinAction::Acknowledge,
        BuiltinAction::Snooze,
        BuiltinAction::Dismiss,
        BuiltinAction::Silence1h,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BuiltinAction::Done => "done",
            BuiltinAction::Acknowledge => "acknowledge",
            BuiltinAction::Snooze => "snooze",
            BuiltinAction::Dismiss => "dismiss",
            BuiltinAction::Silence1h => "silence_1h",
        }
    }

    pub fn parse(text: &str) -> Option<BuiltinAction> {
        BuiltinAction::ALL.into_iter().find(|a| a.as_str() == text)
    }

    /// The button's label, the same on the web, in the app and on a notification.
    pub fn label(self) -> &'static str {
        match self {
            BuiltinAction::Done => "Done",
            BuiltinAction::Acknowledge => "Acknowledge",
            BuiltinAction::Snooze => "Snooze",
            BuiltinAction::Dismiss => "Dismiss",
            BuiltinAction::Silence1h => "Silence 1h",
        }
    }

    /// The classes a kind may offer this action on.
    pub fn allowed_on(self) -> &'static [Class] {
        match self {
            BuiltinAction::Done | BuiltinAction::Snooze => &[Class::Action],
            BuiltinAction::Acknowledge => &[Class::Alert],
            BuiltinAction::Dismiss => &[Class::Alert, Class::Action],
            BuiltinAction::Silence1h => &[Class::Alert, Class::Action, Class::Update],
        }
    }
}

/// The icons a source may show, each an SF Symbol on iOS; the web draws its own for each name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Icon {
    Archive,
    Bell,
    Book,
    Calendar,
    Code,
    Cpu,
    Gauge,
    Globe,
    House,
    Key,
    Server,
    Ship,
    Shield,
    Sparkles,
    Thermometer,
}

impl Icon {
    pub const ALL: [Icon; 15] = [
        Icon::Archive,
        Icon::Bell,
        Icon::Book,
        Icon::Calendar,
        Icon::Code,
        Icon::Cpu,
        Icon::Gauge,
        Icon::Globe,
        Icon::House,
        Icon::Key,
        Icon::Server,
        Icon::Ship,
        Icon::Shield,
        Icon::Sparkles,
        Icon::Thermometer,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Icon::Archive => "archive",
            Icon::Bell => "bell",
            Icon::Book => "book",
            Icon::Calendar => "calendar",
            Icon::Code => "code",
            Icon::Cpu => "cpu",
            Icon::Gauge => "gauge",
            Icon::Globe => "globe",
            Icon::House => "house",
            Icon::Key => "key",
            Icon::Server => "server",
            Icon::Ship => "ship",
            Icon::Shield => "shield",
            Icon::Sparkles => "sparkles",
            Icon::Thermometer => "thermometer",
        }
    }

    pub fn parse(text: &str) -> Option<Icon> {
        Icon::ALL.into_iter().find(|i| i.as_str() == text)
    }

    /// The SF Symbol the iPhone app draws (iOS 17 and later).
    pub fn sf_symbol(self) -> &'static str {
        match self {
            Icon::Archive => "archivebox",
            Icon::Bell => "bell",
            Icon::Book => "book",
            Icon::Calendar => "calendar",
            Icon::Code => "chevron.left.forwardslash.chevron.right",
            Icon::Cpu => "cpu",
            Icon::Gauge => "gauge.with.dots.needle.33percent",
            Icon::Globe => "globe",
            Icon::House => "house",
            Icon::Key => "key",
            Icon::Server => "server.rack",
            Icon::Ship => "shippingbox",
            Icon::Shield => "checkmark.shield",
            Icon::Sparkles => "sparkles",
            Icon::Thermometer => "thermometer.medium",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for class in Class::ALL {
            assert_eq!(Class::parse(class.as_str()), Some(class));
            assert_eq!(serde_json::to_value(class).unwrap(), class.as_str());
        }
        for level in PushLevel::ALL {
            assert_eq!(PushLevel::parse(level.as_str()), Some(level));
            assert_eq!(serde_json::to_value(level).unwrap(), level.as_str());
        }
        for action in BuiltinAction::ALL {
            assert_eq!(BuiltinAction::parse(action.as_str()), Some(action));
            assert_eq!(serde_json::to_value(action).unwrap(), action.as_str());
        }
        for icon in Icon::ALL {
            assert_eq!(Icon::parse(icon.as_str()), Some(icon));
            assert_eq!(serde_json::to_value(icon).unwrap(), icon.as_str());
        }
        assert_eq!(
            serde_json::to_value(PushReason::QuietHours).unwrap(),
            "quiet_hours"
        );
        assert_eq!(
            serde_json::to_value(PushStatus::Suppressed).unwrap(),
            "suppressed"
        );
    }

    #[test]
    fn interruption_levels_follow_the_push_level() {
        assert_eq!(PushLevel::Urgent.interruption_level(), Some("time-sensitive"));
        assert_eq!(PushLevel::Normal.interruption_level(), Some("active"));
        assert_eq!(PushLevel::Silent.interruption_level(), Some("passive"));
        assert_eq!(PushLevel::None.interruption_level(), None);
    }
}
