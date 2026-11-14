//! Trail's webhook (`spec/api.md`, `POST /v1/trail`): each snapshot entry and notice becomes an
//! item of source `trail`. Without `kind` it is the built-in kind its severity picks; with a
//! `kind` published for `trail`, it is that kind, with its facts taken from Trail's fields, its
//! labels and, for a notice, its event's properties. Trail drops a body answered with anything
//! but `408`, `429` or `5xx`, so a kind that doesn't fit never refuses the body: the entry falls
//! back to its severity's built-in kind and the answer lists the fallback. The mapping runs inside
//! the write transaction, against the registry that transaction sees, so a publish can't slip in
//! between the check and the write.

use crate::engine::Tx;
use crate::error::Result;
use crate::producers::{EventAnswer, EventOutcome, Snapshot, SnapshotAnswer, SnapshotEntry, entry_problems};
use crate::rows;
use crate::{Event, Store};
use beacon_core::Problem;
use beacon_core::builtin::{self, TRAIL_BUILTIN, TRAIL_SOURCE};
use beacon_core::decl::{FactBase, FactType, Kind};
use beacon_core::duration::Duration;
use beacon_core::model::Class;
use beacon_core::problem::closest;
use beacon_core::template::truncate_words;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::collections::{BTreeMap, BTreeSet};

/// One of Trail's firing entries, or a notice, as the server read it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrailAlert {
    /// Trail's `key`, or the notice's `id`.
    pub key: String,
    /// Trail's `kind`, as sent (`None` when absent).
    pub kind: Option<Value>,
    /// The built-in kind its severity picks: what it becomes without a usable `kind`.
    pub builtin: String,
    pub title: String,
    pub body: String,
    /// The item's labels, already made from Trail's (`trail_source`, `trail_alert`, `service`...).
    pub labels: BTreeMap<String, String>,
    /// Trail's `labels`, as sent: facts are read from them by name.
    pub trail_labels: Map<String, Value>,
    /// Trail's `source` and `alert`.
    pub source: String,
    pub alert: String,
    /// A snapshot entry's `value` and `since`.
    pub value: Option<Value>,
    pub since: Option<Value>,
    /// A notice's `event.props`.
    pub props: Map<String, Value>,
}

/// An entry that named a kind it couldn't become, and became its severity's built-in kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fallback {
    pub key: String,
    /// The kind Trail named (its text, or the JSON it sent when that isn't a string).
    pub kind: String,
    /// The built-in kind it became instead.
    pub became: String,
    pub problem: Problem,
}

impl Store {
    /// `POST /v1/trail` with `type: "snapshot"`. Covers the built-in `trail.severe`,
    /// `trail.warning` and `trail.info`, and every kind published for `trail` but one-off
    /// alerts (those are for notices): alert and action kinds open and resolve; update and
    /// record kinds are recorded once per appearance. Observes `trail` with `[trail] stale_after`.
    pub fn apply_trail_snapshot(
        &self,
        sent_at: i64,
        firing: Vec<TrailAlert>,
        now: i64,
    ) -> Result<(SnapshotAnswer, Vec<Fallback>)> {
        let stale_after = Duration::from_ms(self.settings().trail_stale_after);
        self.write(now, |w| {
            let covered = w.trail_covered();
            let open = w.open_trail_kinds()?;
            let mut entries = Vec::new();
            let mut fallbacks = Vec::new();
            for (i, alert) in firing.iter().enumerate() {
                let at = format!("trail firing[{i}]");
                let (mut entry, fallback) = w.trail_entry(&at, alert, &covered, false);
                // Already open as another kind (its rule gained, lost or changed `kind` while
                // firing): it is the same incident, so it stays the kind it opened as until it
                // clears. Otherwise the old item would recover and a new one open and push, for
                // a rule that only changed its words.
                if let Some(kind) = open.get(&alert.key)
                    && *kind != entry.kind
                    && let Some(same) = w.trail_entry_as(&at, alert, kind, &covered)
                {
                    entry = same;
                }
                entries.push(entry);
                fallbacks.extend(fallback);
            }
            // Trail may list one key twice; the snapshot would refuse that.
            let mut seen = BTreeSet::new();
            entries.retain(|e| seen.insert((e.kind.clone(), e.key.clone())));
            let snapshot = Snapshot {
                source: TRAIL_SOURCE.into(),
                observed: true,
                reason: None,
                stale_after,
                sent_at: Some(sent_at),
                kinds: Some(covered.into_iter().collect()),
                firing: entries,
            };
            Ok((w.snapshot(&snapshot, true)?, fallbacks))
        })
    }

    /// `POST /v1/trail` with `type: "notice"`: an event keyed by the notice id, of the kind it
    /// names (a one-off alert, an update or a record published for `trail`) or of its severity's
    /// built-in kind (`trail.severe_notice`, `trail.warning`, `trail.info`). Recorded once per id:
    /// a repeat answers the item with `repeat: true`. A `late` notice (its `ts` more than
    /// `delivery.late` old) is recorded with its push suppressed `late`.
    pub fn apply_trail_notice(
        &self,
        notice: &TrailAlert,
        late: bool,
        now: i64,
    ) -> Result<(EventAnswer, Vec<Fallback>)> {
        self.write(now, |w| {
            let known: Option<i64> = w
                .conn
                .prepare_cached("SELECT item FROM notices WHERE id = ?1")?
                .query_row([&notice.key], |r| r.get(0))
                .optional()?;
            if let Some(item) = known {
                let item = rows::load(w.conn, item)?.map(|row| row.item(now, w.s));
                let answer = EventAnswer {
                    outcome: EventOutcome::Ignored,
                    item,
                    repeat: true,
                };
                return Ok((answer, Vec::new()));
            }
            let (entry, fallback) = w.trail_entry("trail", notice, &BTreeSet::new(), true);
            let event = Event {
                kind: entry.kind,
                key: entry.key,
                facts: Some(entry.facts),
                slots: Some(entry.slots),
                labels: entry.labels,
                ..Event::default()
            };
            w.late = late;
            let answer = w.event(&event, "notice", false)?;
            w.late = false;
            if let Some(item) = &answer.item {
                w.conn
                    .prepare_cached("INSERT INTO notices (id, item) VALUES (?1, ?2)")?
                    .execute(params![notice.key, item.id])?;
            }
            Ok((answer, fallback.into_iter().collect()))
        })
    }
}

impl Tx<'_, '_> {
    /// The kinds Trail's snapshot is complete for: the built-in ones that aren't for notices,
    /// and every kind published for `trail` that isn't a one-off alert.
    fn trail_covered(&self) -> BTreeSet<String> {
        let mut covered: BTreeSet<String> =
            [builtin::TRAIL_SEVERE, builtin::TRAIL_WARNING, builtin::TRAIL_INFO]
                .into_iter()
                .map(String::from)
                .collect();
        if let Some(trail) = self.registry.get(TRAIL_SOURCE) {
            for (name, kind) in &trail.kinds {
                if !TRAIL_BUILTIN.contains(&name.as_str()) && !kind.one_off {
                    covered.insert(name.clone());
                }
            }
        }
        covered
    }

    /// The kind of each alert or action of `trail` that is open now, by Trail's key.
    fn open_trail_kinds(&self) -> Result<BTreeMap<String, String>> {
        let open = rows::query(
            self.conn,
            "WHERE source = ?1 AND state IN ('open', 'dismissed', 'snoozed') ORDER BY id",
            [TRAIL_SOURCE],
        )?;
        let mut kinds = BTreeMap::new();
        for row in open {
            if row.life.is_open() {
                kinds.entry(row.key).or_insert(row.kind);
            }
        }
        Ok(kinds)
    }

    /// Trail's firing entry as `kind`, the kind its open item has: None when the snapshot no
    /// longer covers that kind (a publish removed it) or the entry can't be it.
    fn trail_entry_as(
        &self,
        at: &str,
        alert: &TrailAlert,
        kind: &str,
        covered: &BTreeSet<String>,
    ) -> Option<SnapshotEntry> {
        if !covered.contains(kind) {
            return None;
        }
        if TRAIL_BUILTIN.contains(&kind) {
            return Some(builtin_entry(alert, kind));
        }
        self.named_trail_entry(at, alert, &Value::String(kind.to_string()), covered, false)
            .ok()
    }

    /// The entry Trail's alert becomes: the kind it names when that fits, else its severity's
    /// built-in kind and the problem.
    fn trail_entry(
        &self,
        at: &str,
        alert: &TrailAlert,
        covered: &BTreeSet<String>,
        notice: bool,
    ) -> (SnapshotEntry, Option<Fallback>) {
        let builtin_entry = || builtin_entry(alert, &alert.builtin);
        let Some(named) = &alert.kind else {
            return (builtin_entry(), None);
        };
        match self.named_trail_entry(at, alert, named, covered, notice) {
            Ok(entry) => (entry, None),
            Err(problem) => {
                let fallback = Fallback {
                    key: alert.key.clone(),
                    kind: named.as_str().map_or_else(|| named.to_string(), String::from),
                    became: alert.builtin.clone(),
                    problem,
                };
                (builtin_entry(), Some(fallback))
            }
        }
    }

    fn named_trail_entry(
        &self,
        at: &str,
        alert: &TrailAlert,
        named: &Value,
        covered: &BTreeSet<String>,
        notice: bool,
    ) -> std::result::Result<SnapshotEntry, Problem> {
        let at_kind = format!("{at} kind");
        let Some(name) = named.as_str() else {
            return Err(Problem::new(
                at_kind,
                format!("{named} is not a kind's name"),
                "set the rule's `kind` to a kind published for trail, like \"trail.disk_full\"",
            ));
        };
        let published: Vec<&str> = self
            .registry
            .get(TRAIL_SOURCE)
            .map(|d| {
                d.kinds
                    .keys()
                    .map(String::as_str)
                    .filter(|k| !TRAIL_BUILTIN.contains(k))
                    .collect()
            })
            .unwrap_or_default();
        let kind = match self.kind(name) {
            Some((decl, kind)) if decl.source == TRAIL_SOURCE && published.contains(&name) => kind,
            _ => {
                return Err(Problem::new(
                    at_kind,
                    format!("`{name}` is not a kind published for trail"),
                    match closest(name, published.iter().copied()) {
                        Some(near) => format!(
                            "did you mean `{near}`? Otherwise declare it in the trail declarations folder and run `beacon publish`"
                        ),
                        None => "declare it in a declarations folder with source = \"trail\" and run `beacon publish`"
                            .to_string(),
                    },
                ));
            }
        };
        let fits = if notice {
            (kind.class == Class::Alert && kind.one_off) || !kind.class.has_lifecycle()
        } else {
            covered.contains(name)
        };
        if !fits {
            return Err(Problem::new(
                at_kind,
                if notice {
                    format!(
                        "`{name}` is an {} that stays open, and nothing would resolve it",
                        kind.class.as_str()
                    )
                } else {
                    format!("`{name}` is a one-off alert, which is for event rules")
                },
                if notice {
                    "name a one-off alert (one_off = true), an update or a record from an event rule"
                } else {
                    "name an alert, action, update or record that isn't one-off from a metric rule"
                },
            ));
        }
        let (facts, mut problems) = trail_facts(at, alert, kind, notice);
        let mut slots = Map::new();
        for (slot, max) in &kind.slots {
            let text = match slot.as_str() {
                "title" => Some(alert.title.clone()),
                "body" => Some(alert.body.clone()),
                other => text_of(alert, other, notice),
            };
            if let Some(text) = text {
                let cap = usize::try_from(*max).unwrap_or(usize::MAX);
                slots.insert(slot.clone(), Value::String(truncate_words(&text, cap)));
            }
        }
        let entry = SnapshotEntry {
            kind: name.to_string(),
            key: alert.key.clone(),
            facts,
            slots,
            labels: alert.labels.clone(),
            ..SnapshotEntry::default()
        };
        problems.extend(entry_problems(at, &entry, kind, false));
        match problems.into_iter().next() {
            Some(problem) => Err(problem),
            None => Ok(entry),
        }
    }
}

/// Trail's alert as one of Beacon's built-in Trail kinds, which carry its title and body in slots.
fn builtin_entry(alert: &TrailAlert, kind: &str) -> SnapshotEntry {
    let mut slots = Map::new();
    slots.insert("title".into(), Value::String(alert.title.clone()));
    slots.insert("body".into(), Value::String(alert.body.clone()));
    SnapshotEntry {
        kind: kind.to_string(),
        key: alert.key.clone(),
        slots,
        labels: alert.labels.clone(),
        ..SnapshotEntry::default()
    }
}

/// A value Trail sent under `name`, beside `value`, `since`, `alert` and `trail_source`: a
/// snapshot entry's label, or a notice's event property.
fn named_value<'a>(alert: &'a TrailAlert, name: &str, notice: bool) -> Option<&'a Value> {
    if notice {
        alert.props.get(name)
    } else {
        alert.trail_labels.get(name)
    }
}

fn text_of(alert: &TrailAlert, name: &str, notice: bool) -> Option<String> {
    match named_value(alert, name, notice)? {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// The facts a kind declares, by name, from Trail's fields; and the problems of those missing
/// or unreadable.
fn trail_facts(
    at: &str,
    alert: &TrailAlert,
    kind: &Kind,
    notice: bool,
) -> (Map<String, Value>, Vec<Problem>) {
    let mut facts = Map::new();
    let mut problems = Vec::new();
    for (name, fact) in &kind.facts {
        let sent = match name.as_str() {
            "alert" => Some(Value::String(alert.alert.clone())),
            "trail_source" => Some(Value::String(alert.source.clone())),
            "value" if !notice => alert.value.clone(),
            "since" if !notice => alert.since.clone(),
            other => named_value(alert, other, notice).cloned(),
        };
        let sent = sent.filter(|v| !v.is_null());
        match sent.map(|v| as_fact(&v, *fact)) {
            Some(Ok(value)) => {
                facts.insert(name.clone(), value);
            }
            Some(Err(message)) => problems.push(Problem::new(
                format!("{at} facts.{name}"),
                message,
                format!("make the rule send `{name}` as a {}", fact.base.name()),
            )),
            None if fact.optional => {}
            None => problems.push(Problem::new(
                format!("{at} facts.{name}"),
                format!("the kind needs the fact `{name}`, and Trail sent none"),
                if notice {
                    format!(
                        "put `{name}` in the event's props, or make the fact optional (`{name} = \"{}?\"`)",
                        fact.base.name()
                    )
                } else {
                    format!(
                        "give the rule a `{name}` label, or make the fact optional (`{name} = \"{}?\"`)",
                        fact.base.name()
                    )
                },
            )),
        }
    }
    (facts, problems)
}

/// Trail's value as the fact's type: a string (a label) is read as that type; a number is kept,
/// and a whole `float` given to an `int` becomes the integer.
fn as_fact(value: &Value, fact: FactType) -> std::result::Result<Value, String> {
    match (value, fact.base) {
        (Value::String(text), FactBase::String) => Ok(Value::String(text.clone())),
        (Value::String(text), base) => from_text(text.trim(), base)
            .ok_or_else(|| format!("`{text}` doesn't read as the fact's type, {}", base.name())),
        (Value::Number(n), FactBase::Int | FactBase::Duration | FactBase::Time) if n.as_i64().is_none() => {
            match n.as_f64() {
                // reason: a whole float in range converts exactly
                #[allow(clippy::cast_possible_truncation)]
                Some(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Ok(Value::from(f as i64)),
                _ => Err(format!("{n} is not a whole number")),
            }
        }
        (Value::Number(_), FactBase::String) | (Value::Bool(_), FactBase::String) => {
            Ok(Value::String(value.to_string()))
        }
        _ => Ok(value.clone()),
    }
}

fn from_text(text: &str, base: FactBase) -> Option<Value> {
    match base {
        FactBase::String => Some(Value::String(text.to_string())),
        FactBase::Int => text.parse::<i64>().ok().map(Value::from),
        FactBase::Float => text
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number),
        FactBase::Duration => text
            .parse::<i64>()
            .ok()
            .or_else(|| Duration::parse(text).ok().map(Duration::ms))
            .map(Value::from),
        FactBase::Time => text
            .parse::<i64>()
            .ok()
            .or_else(|| {
                text.parse::<jiff::Timestamp>()
                    .ok()
                    .map(jiff::Timestamp::as_millisecond)
            })
            .map(Value::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trail_values_become_the_facts_type() {
        let ty = |text: &str| FactType::parse(text).unwrap();
        assert_eq!(as_fact(&Value::from("42"), ty("int")), Ok(Value::from(42)));
        assert_eq!(as_fact(&Value::from(150.0), ty("int")), Ok(Value::from(150)));
        assert!(as_fact(&Value::from(1.5), ty("int")).is_err());
        assert_eq!(as_fact(&Value::from("95.5"), ty("float")), Ok(Value::from(95.5)));
        assert_eq!(
            as_fact(&Value::from("5m"), ty("duration")),
            Ok(Value::from(300_000))
        );
        assert_eq!(
            as_fact(&Value::from("2026-11-13T12:00:00Z"), ty("time")),
            Ok(Value::from(1_794_571_200_000_i64))
        );
        assert_eq!(as_fact(&Value::from(7), ty("string")), Ok(Value::from("7")));
        assert!(
            as_fact(&Value::from("lots"), ty("int"))
                .unwrap_err()
                .contains("doesn't read as")
        );
    }
}
