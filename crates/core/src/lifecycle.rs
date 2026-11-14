//! The alert and action lifecycles (`spec/delivery.md`, "Alerts", "Actions", "Updates and
//! records"). [`Life`] is the part of an item that moves; [`Life::apply`] takes one input and
//! says what happened and what the caller owes: a decision to run, a held push to suppress, a
//! recovery push, a `beacon.flapping` to record, a silence to open. Pure: the clock comes in.

use crate::delivery::Settings;
use crate::model::{BuiltinAction, Class, ItemState, Trigger};
use serde::{Deserialize, Serialize};

/// When each step of an alert or action happened; null if it hasn't. `opened` is the latest opening.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Times {
    pub opened: Option<i64>,
    pub acknowledged: Option<i64>,
    pub dismissed: Option<i64>,
    pub snoozed_until: Option<i64>,
    pub done: Option<i64>,
    pub resolved: Option<i64>,
}

/// A button: a built-in action or a custom one, with its rendered link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Button {
    pub id: String,
    pub label: String,
    pub url: Option<String>,
}

impl Button {
    pub fn builtin(&self) -> Option<BuiltinAction> {
        BuiltinAction::parse(&self.id)
    }
}

/// The moving part of an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Life {
    pub class: Class,
    /// None for updates and records.
    pub state: Option<ItemState>,
    pub times: Times,
    pub seen: bool,
    pub one_off: bool,
    /// Alerts: the times of the openings within the flap window, oldest first.
    pub openings: Vec<i64>,
    pub flapping: bool,
    /// Alerts: the kind's `repeat` intervals, in ms.
    pub repeat: Vec<i64>,
    pub repeats_sent: u32,
    pub next_repeat: Option<i64>,
    /// The last opening or resolution: a flapping alert settles `flap_stable` after it.
    pub last_change: i64,
}

/// Something that happens to an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// An alert event or snapshot entry, firing.
    Fire,
    /// An action event or snapshot entry, open.
    Open,
    /// An event with `state: "resolved"`, or absent from its source's snapshot.
    Resolve,
    /// Its kind was removed by a publish: nothing can resolve it any more, so it closes as
    /// resolved, keeping its own text (`spec/declarations.md`, "Reaching the server").
    KindRemoved,
    /// A button.
    Press(Press),
    /// A snoozed action's time came.
    Wake,
    /// A flapping alert's stable time came.
    Settle,
    /// An alert's next repeat came due.
    Repeat,
}

/// A button press, resolved to what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    Done,
    Acknowledge,
    Snooze {
        until: i64,
    },
    Dismiss,
    Silence1h,
    /// A custom action; `url` says whether it is a link.
    Custom {
        url: bool,
    },
}

/// What happened: the history step (`spec/api.md`, `GET /v1/items/{id}`) and the event outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Opened,
    Reopened,
    Updated,
    Resolved,
    /// Nothing changed (`outcome: "ignored"`, or an action whose effect already holds).
    Ignored,
    Acknowledged,
    Dismissed,
    Done,
    Snoozed,
    Woke,
    /// A `silence_1h` press: a silence opens, the item doesn't change.
    Silenced,
    /// A custom action recorded in `acted`.
    Action,
    Settled,
    Repeated,
    /// Closed because a publish removed its kind.
    KindRemoved,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Opened => "opened",
            Outcome::Reopened => "reopened",
            Outcome::Updated => "updated",
            Outcome::Resolved => "resolved",
            Outcome::Ignored => "ignored",
            Outcome::Acknowledged => "acknowledged",
            Outcome::Dismissed => "dismissed",
            Outcome::Done => "done",
            Outcome::Snoozed => "snoozed",
            Outcome::Woke => "woke",
            Outcome::Silenced => "silenced",
            Outcome::Action => "action",
            Outcome::Settled => "settled",
            Outcome::Repeated => "repeated",
            Outcome::KindRemoved => "kind_removed",
        }
    }
}

/// Which text the item shows after a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Text {
    /// Unchanged.
    Keep,
    /// Rendered again from the kind's title and body with the latest facts and slots.
    Firing,
    /// The kind's recovery text, with `{lasted}` from `times.opened` to now.
    Recovery,
}

/// What a step did and what the caller owes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub outcome: Outcome,
    /// Run the decision with this trigger.
    pub decide: Option<Trigger>,
    /// The item left `open`: a held push of it is suppressed (`closed`).
    pub close_held: bool,
    /// An alert resolved while not flapping: if its push was `sent`, a recovery push goes out.
    pub recovery: bool,
    /// The alert just became flapping: record one `beacon.flapping`.
    pub flapping: bool,
    /// Open a one-hour silence for the item's source, kind and key.
    pub silence: bool,
    /// Record the press in `acted`.
    pub acted: bool,
    pub text: Text,
}

impl Step {
    fn new(outcome: Outcome) -> Step {
        Step {
            outcome,
            decide: None,
            close_held: false,
            recovery: false,
            flapping: false,
            silence: false,
            acted: false,
            text: Text::Keep,
        }
    }

    /// Nothing changed.
    pub fn ignored() -> Step {
        Step::new(Outcome::Ignored)
    }

    /// Whether the item changed at all.
    pub fn changed(&self) -> bool {
        !matches!(self.outcome, Outcome::Ignored | Outcome::Silenced)
    }
}

/// Why a press is refused (`409`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The kind declares no action with that id.
    Unknown,
    /// Declared, but it doesn't apply in the item's state; the buttons that do.
    NotOffered(Vec<Button>),
}

/// What an alert event or snapshot entry with `firing` does to the latest item of its `(kind, key)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireTarget {
    /// Make a new item (and run [`Life::new_alert`]).
    New,
    /// Apply [`Input::Fire`] to the latest item: it reopens (it resolved within the flap window)
    /// or updates (it is open or dismissed).
    Latest,
}

/// Where an alert firing goes, given the latest item of its `(kind, key)`.
pub fn fire_target(latest: Option<&Life>, now: i64, settings: &Settings) -> FireTarget {
    match latest {
        Some(life) if life.state == Some(ItemState::Resolved) => {
            let resolved = life.times.resolved.unwrap_or(i64::MIN);
            if now - resolved < settings.flap_window {
                FireTarget::Latest
            } else {
                FireTarget::New
            }
        }
        Some(life) if life.is_open() => FireTarget::Latest,
        _ => FireTarget::New,
    }
}

impl Life {
    /// A new alert: `open`, one opening, repeats scheduled from now. Then decide (`opened`).
    pub fn new_alert(now: i64, repeat: Vec<i64>, one_off: bool) -> (Life, Step) {
        let next_repeat = if one_off {
            None
        } else {
            repeat.first().map(|d| now + d)
        };
        let life = Life {
            class: Class::Alert,
            state: Some(ItemState::Open),
            times: Times {
                opened: Some(now),
                ..Times::default()
            },
            seen: false,
            one_off,
            openings: vec![now],
            flapping: false,
            repeat,
            repeats_sent: 0,
            next_repeat,
            last_change: now,
        };
        (life, opened())
    }

    /// A new action: `open`. Then decide (`opened`).
    pub fn new_action(now: i64) -> (Life, Step) {
        let life = Life {
            class: Class::Action,
            state: Some(ItemState::Open),
            times: Times {
                opened: Some(now),
                ..Times::default()
            },
            seen: false,
            one_off: false,
            openings: Vec::new(),
            flapping: false,
            repeat: Vec::new(),
            repeats_sent: 0,
            next_repeat: None,
            last_change: now,
        };
        (life, opened())
    }

    /// An update or record: no state. An update is decided (`opened`); a record's decision is
    /// `none` (`declared`), so it is decided too, and the decision says so.
    pub fn record(class: Class, now: i64) -> (Life, Step) {
        let life = Life {
            class,
            state: None,
            times: Times::default(),
            seen: false,
            one_off: false,
            openings: Vec::new(),
            flapping: false,
            repeat: Vec::new(),
            repeats_sent: 0,
            next_repeat: None,
            last_change: now,
        };
        (life, opened())
    }

    /// "Open" in the sense of `POST /v1/events`: an alert `open` or `dismissed`, an action
    /// `open` or `snoozed`. The item an event for its `(kind, key)` updates.
    pub fn is_open(&self) -> bool {
        matches!(
            (self.class, self.state),
            (Class::Alert, Some(ItemState::Open | ItemState::Dismissed))
                | (Class::Action, Some(ItemState::Open | ItemState::Snoozed))
        )
    }

    /// In state `open` exactly, or an update or record: what a held push needs to still go out.
    pub fn is_pushable(&self) -> bool {
        self.state.is_none_or(|state| state == ItemState::Open)
    }

    /// The buttons that apply now, from the kind's declared ones (`spec/api.md`, "The item").
    pub fn buttons(&self, declared: &[Button]) -> Vec<Button> {
        declared
            .iter()
            .filter(|button| self.offers(button))
            .cloned()
            .collect()
    }

    fn offers(&self, button: &Button) -> bool {
        let builtin = button.builtin();
        let link_only = builtin.is_none() && button.url.is_some();
        match (self.class, self.state) {
            (Class::Alert, Some(ItemState::Open)) => {
                !(builtin == Some(BuiltinAction::Acknowledge) && self.times.acknowledged.is_some())
            }
            (Class::Alert, Some(ItemState::Dismissed)) => {
                !matches!(builtin, Some(BuiltinAction::Dismiss))
                    && !(builtin == Some(BuiltinAction::Acknowledge) && self.times.acknowledged.is_some())
            }
            (Class::Action, Some(ItemState::Open)) => true,
            (Class::Action, Some(ItemState::Snoozed)) => builtin != Some(BuiltinAction::Snooze),
            (Class::Update, _) => true,
            _ => link_only,
        }
    }

    /// Whether a press's effect already holds: a second tap answers `200` and changes nothing.
    pub fn effect_holds(&self, press: Press) -> bool {
        let resolved_one_off = self.one_off && self.state == Some(ItemState::Resolved);
        match press {
            Press::Done => self.state == Some(ItemState::Done),
            Press::Acknowledge => self.times.acknowledged.is_some() || resolved_one_off,
            Press::Snooze { .. } => self.state == Some(ItemState::Snoozed),
            Press::Dismiss => self.state == Some(ItemState::Dismissed) || resolved_one_off,
            Press::Silence1h | Press::Custom { .. } => false,
        }
    }

    /// Presses the button `id`: refused when the kind doesn't declare it, `Ignored` when its
    /// effect already holds, refused when it doesn't apply now, else applied.
    pub fn press(
        &mut self,
        declared: &[Button],
        id: &str,
        until: Option<i64>,
        now: i64,
        settings: &Settings,
    ) -> Result<Step, Refused> {
        let Some(button) = declared.iter().find(|b| b.id == id) else {
            return Err(Refused::Unknown);
        };
        let press = match button.builtin() {
            Some(BuiltinAction::Done) => Press::Done,
            Some(BuiltinAction::Acknowledge) => Press::Acknowledge,
            Some(BuiltinAction::Snooze) => Press::Snooze {
                until: until.unwrap_or(now + settings.snooze),
            },
            Some(BuiltinAction::Dismiss) => Press::Dismiss,
            Some(BuiltinAction::Silence1h) => Press::Silence1h,
            None => Press::Custom {
                url: button.url.is_some(),
            },
        };
        if self.effect_holds(press) {
            return Ok(Step::ignored());
        }
        if !self.offers(button) {
            return Err(Refused::NotOffered(self.buttons(declared)));
        }
        Ok(self.apply(Input::Press(press), now, settings))
    }

    /// Applies one input. An input that doesn't apply in the current state is `Ignored`.
    pub fn apply(&mut self, input: Input, now: i64, settings: &Settings) -> Step {
        match self.class {
            Class::Alert => self.alert(input, now, settings),
            Class::Action => self.action(input, now),
            Class::Update | Class::Record => match input {
                Input::Press(Press::Silence1h) => silenced(),
                Input::Press(Press::Custom { .. }) => acted(),
                _ => Step::ignored(),
            },
        }
    }

    fn alert(&mut self, input: Input, now: i64, s: &Settings) -> Step {
        use ItemState::{Dismissed, Open, Resolved};
        let state = self.state.unwrap_or(Resolved);
        match (state, input) {
            (Open | Dismissed, Input::Fire) => {
                let mut step = Step::new(Outcome::Updated);
                step.text = Text::Firing;
                step
            }
            (Resolved, Input::Fire) => self.reopen(now, s),
            (Open | Dismissed, Input::Resolve) => {
                self.resolve(now);
                self.seen = false;
                let mut step = Step::new(Outcome::Resolved);
                step.close_held = true;
                if !self.one_off {
                    step.text = Text::Recovery;
                    step.recovery = !self.flapping;
                }
                step
            }
            (Open | Dismissed, Input::KindRemoved) => {
                // Nothing recovered: it keeps its text and `seen`. A device that has or may have
                // its firing push still gets the usual recovery, which replaces that banner.
                self.resolve(now);
                let mut step = Step::new(Outcome::KindRemoved);
                step.close_held = true;
                step.recovery = !self.one_off && !self.flapping;
                step
            }
            (Open | Dismissed, Input::Press(Press::Acknowledge)) => {
                self.times.acknowledged = Some(now);
                self.next_repeat = None;
                let mut step = Step::new(Outcome::Acknowledged);
                if self.one_off {
                    self.resolve(now);
                    step.close_held = true;
                }
                step
            }
            (Open, Input::Press(Press::Dismiss)) => {
                let mut step = Step::new(Outcome::Dismissed);
                step.close_held = true;
                self.next_repeat = None;
                if self.one_off {
                    self.resolve(now);
                } else {
                    self.state = Some(Dismissed);
                    self.times.dismissed = Some(now);
                }
                step
            }
            (Open | Dismissed, Input::Press(Press::Silence1h)) => silenced(),
            (_, Input::Press(Press::Custom { .. })) => acted(),
            (_, Input::Settle) if self.flapping && now - self.last_change >= s.flap_stable => {
                self.flapping = false;
                let mut step = Step::new(Outcome::Settled);
                if state == Open {
                    step.decide = Some(Trigger::Settled);
                    if self.times.acknowledged.is_none() {
                        self.next_repeat = self.repeat.get(self.repeats_sent as usize).map(|d| now + d);
                    }
                }
                step
            }
            (Open, Input::Repeat)
                if self.times.acknowledged.is_none()
                    && !self.flapping
                    && self.next_repeat.is_some_and(|due| due <= now) =>
            {
                self.repeats_sent += 1;
                self.next_repeat = self.repeat.get(self.repeats_sent as usize).map(|d| now + d);
                let mut step = Step::new(Outcome::Repeated);
                step.decide = Some(Trigger::Repeat);
                step
            }
            _ => Step::ignored(),
        }
    }

    fn resolve(&mut self, now: i64) {
        self.state = Some(ItemState::Resolved);
        self.times.resolved = Some(now);
        self.next_repeat = None;
        self.last_change = now;
    }

    fn reopen(&mut self, now: i64, s: &Settings) -> Step {
        self.openings.retain(|at| now - at < s.flap_window);
        self.openings.push(now);
        self.state = Some(ItemState::Open);
        self.times.opened = Some(now);
        self.times.resolved = None;
        self.times.acknowledged = None;
        self.times.dismissed = None;
        self.seen = false;
        self.last_change = now;
        self.repeats_sent = 0;
        let mut step = Step::new(Outcome::Reopened);
        step.text = Text::Firing;
        step.decide = Some(Trigger::Reopened);
        let count = u32::try_from(self.openings.len()).unwrap_or(u32::MAX);
        if count > s.flap_openings && !self.flapping {
            self.flapping = true;
            step.flapping = true;
        }
        self.next_repeat = if self.flapping || self.one_off {
            None
        } else {
            self.repeat.first().map(|d| now + d)
        };
        step
    }

    /// Openings within the flap window as of `now`: the item's `alert.openings`.
    pub fn openings_within(&self, now: i64, settings: &Settings) -> u32 {
        let n = self
            .openings
            .iter()
            .filter(|at| now - **at < settings.flap_window)
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// When a flapping alert settles, if it is flapping.
    pub fn settles_at(&self, settings: &Settings) -> Option<i64> {
        self.flapping.then_some(self.last_change + settings.flap_stable)
    }

    fn action(&mut self, input: Input, now: i64) -> Step {
        use ItemState::{Dismissed, Done, Open, Resolved, Snoozed};
        let Some(state) = self.state else {
            return Step::ignored();
        };
        let closing = |life: &mut Life, to: ItemState, outcome: Outcome| {
            life.state = Some(to);
            let mut step = Step::new(outcome);
            step.close_held = true;
            step
        };
        match (state, input) {
            (Open | Snoozed, Input::Open) => {
                let mut step = Step::new(Outcome::Updated);
                step.text = Text::Firing;
                step
            }
            (Open | Snoozed, Input::Resolve) => {
                self.times.resolved = Some(now);
                closing(self, Resolved, Outcome::Resolved)
            }
            (Open | Snoozed, Input::KindRemoved) => {
                self.times.resolved = Some(now);
                closing(self, Resolved, Outcome::KindRemoved)
            }
            (Open | Snoozed, Input::Press(Press::Done)) => {
                self.times.done = Some(now);
                closing(self, Done, Outcome::Done)
            }
            (Open | Snoozed, Input::Press(Press::Dismiss)) => {
                self.times.dismissed = Some(now);
                closing(self, Dismissed, Outcome::Dismissed)
            }
            (Open, Input::Press(Press::Snooze { until })) => {
                self.times.snoozed_until = Some(until);
                closing(self, Snoozed, Outcome::Snoozed)
            }
            (Snoozed, Input::Wake) if self.times.snoozed_until.is_some_and(|t| t <= now) => {
                self.state = Some(Open);
                self.seen = false;
                let mut step = Step::new(Outcome::Woke);
                step.decide = Some(Trigger::Woke);
                step
            }
            (Open | Snoozed, Input::Press(Press::Silence1h)) => silenced(),
            (Open | Snoozed, Input::Press(Press::Custom { url: false })) => {
                self.times.done = Some(now);
                let mut step = closing(self, Done, Outcome::Done);
                step.acted = true;
                step
            }
            (_, Input::Press(Press::Custom { .. })) => acted(),
            _ => Step::ignored(),
        }
    }
}

fn opened() -> Step {
    let mut step = Step::new(Outcome::Opened);
    step.decide = Some(Trigger::Opened);
    step.text = Text::Firing;
    step
}

fn silenced() -> Step {
    let mut step = Step::new(Outcome::Silenced);
    step.silence = true;
    step
}

fn acted() -> Step {
    let mut step = Step::new(Outcome::Action);
    step.acted = true;
    step
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
