//! What producers send: the registry, events, snapshots and Trail's webhook (`spec/api.md`,
//! "Producers"). Each is checked against the source's declarations and applied in one
//! transaction; a snapshot is all or nothing.

use crate::Store;
use crate::engine::{NewItem, Tx, open_input};
use crate::error::{Error, ErrorKind, Result};
use crate::rows::{self, Row};
use beacon_core::Problem;
use beacon_core::builtin;
use beacon_core::decl::{Declarations, Kind};
use beacon_core::duration::{Duration, MINUTE};
use beacon_core::item::Item;
use beacon_core::lifecycle::{FireTarget, Input, Life, Outcome, fire_target};
use beacon_core::model::{Class, PushReason, PushStatus, Trigger};
use beacon_core::names::{RESERVED_SOURCES, is_ident, is_key, source_of};
use beacon_core::problem::closest;
use beacon_core::render::check_values;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The state an event sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventState {
    Firing,
    Open,
    Resolved,
}

/// `POST /v1/events`. Facts and slots left out of a resolving event keep the item's latest.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub kind: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub facts: Option<Map<String, Value>>,
    #[serde(default)]
    pub slots: Option<Map<String, Value>>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub state: Option<EventState>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// One of the kind's declared `categories`: the push's category instead of the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventOutcome {
    Opened,
    Reopened,
    Updated,
    Resolved,
    Recorded,
    Ignored,
}

/// The answer to an event; kept with its Idempotency-Key and answered again for a repeat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventAnswer {
    pub outcome: EventOutcome,
    pub item: Option<Item>,
    pub repeat: bool,
}

/// One entry of a snapshot's `firing`: an event without a state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotEntry {
    pub kind: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub facts: Map<String, Value>,
    #[serde(default)]
    pub slots: Map<String, Value>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

fn yes() -> bool {
    true
}

/// `POST /v1/snapshots`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub source: String,
    #[serde(default = "yes")]
    pub observed: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub stale_after: Option<Duration>,
    #[serde(default)]
    pub sent_at: Option<i64>,
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    #[serde(default)]
    pub firing: Vec<SnapshotEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotAnswer {
    pub ok: bool,
    pub opened: Vec<i64>,
    pub reopened: Vec<i64>,
    pub updated: Vec<i64>,
    pub resolved: Vec<i64>,
    pub observed: bool,
}

/// A snapshot is refused when `sent_at` is more than this old...
const SNAPSHOT_MAX_AGE: i64 = 5 * MINUTE;
/// ...or more than this far ahead.
const SNAPSHOT_MAX_AHEAD: i64 = MINUTE;
const LABELS_MAX: usize = 16;
const LABEL_VALUE_MAX: usize = 128;
const CONTEXT_MAX: usize = 128;
const URL_MAX: usize = 1024;
const REASON_MAX: usize = 100;
/// The reason a source that stopped posting snapshots is unobserved.
pub const STALE_REASON: &str = "It stopped reporting.";

/// What a publish did: how many kinds the source now has, which it kept (`keep`), and the open
/// alerts and actions it closed because their kind is gone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Published {
    pub kinds: usize,
    pub kept: Vec<String>,
    pub closed: Vec<i64>,
}

/// A published source whose stored declarations, or some of whose kinds, Beacon can't read back
/// (written by a version that stored them differently). It is kept and reported, never dropped:
/// `GET /v1/registry` shows its `problem`, `GET /v1/health` counts it, and events for what can't
/// be read are refused with a fix that says to publish it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unreadable {
    pub source: String,
    /// The kinds that can't be read; empty when the source itself can't be.
    pub kinds: Vec<String>,
    pub problem: Problem,
    /// Why, in a few words.
    #[serde(skip)]
    pub why: String,
    pub published: i64,
}

impl Unreadable {
    fn new(source: &str, kinds: Vec<String>, why: &str, published: i64) -> Unreadable {
        let what = if kinds.is_empty() {
            format!("its declarations as stored can't be read: {why}")
        } else {
            let names: Vec<String> = kinds.iter().map(|k| format!("`{k}`")).collect();
            format!("{} as stored can't be read: {why}", names.join(", "))
        };
        Unreadable {
            source: source.to_string(),
            kinds,
            problem: Problem::new(
                format!("registry source {source}"),
                what,
                format!(
                    "publish `{source}` again (`beacon publish <its folder>`); until then its events are refused"
                ),
            ),
            why: why.to_string(),
            published,
        }
    }

    /// Whether events of `kind` are refused because of it.
    pub fn covers(&self, kind: &str) -> bool {
        self.kinds.is_empty() || self.kinds.iter().any(|k| k == kind)
    }
}

/// Every published source's declarations, read tolerantly, and the sources that can't all be
/// read. Nothing stored can fail the start.
pub(crate) fn load_registry(conn: &Connection) -> Result<(Vec<Declarations>, Vec<Unreadable>)> {
    let mut stmt = conn.prepare("SELECT source, decl, published FROM registry ORDER BY source")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    let mut unreadable = Vec::new();
    for row in rows {
        let (source, text, published) = row?;
        let read = serde_json::from_str::<Value>(&text)
            .map_err(|e| format!("it is not JSON: {e}"))
            .and_then(|value| Declarations::from_stored(&value));
        match read {
            Err(why) => unreadable.push(Unreadable::new(&source, Vec::new(), &why, published)),
            Ok(stored) if stored.decl.source != source => unreadable.push(Unreadable::new(
                &source,
                Vec::new(),
                &format!("they name source `{}`", stored.decl.source),
                published,
            )),
            Ok(stored) => {
                if let Some((_, why)) = stored.unreadable.iter().next() {
                    let kinds = stored.unreadable.keys().cloned().collect();
                    unreadable.push(Unreadable::new(&source, kinds, why, published));
                }
                out.push(stored.decl);
            }
        }
    }
    Ok((out, unreadable))
}

impl Store {
    // ---------- the registry ----------

    /// `PUT /v1/registry/{source}`: stores declarations that passed their checks (the server
    /// runs them with its `banned_words`), replacing the source's earlier ones.
    pub fn publish(&self, decl: &Declarations, now: i64) -> Result<Published> {
        self.publish_with(decl, false, now)
    }

    /// `PUT /v1/registry/{source}`, with `keep` (`?keep=true`): the body's kinds are added or
    /// replaced by name, and kinds already published that the body lacks stay. Without it the
    /// body replaces the source's declarations, and every alert or action still open of a kind
    /// it no longer has closes in the same transaction (`Input::KindRemoved`): nothing could
    /// resolve it any more.
    pub fn publish_with(&self, decl: &Declarations, keep: bool, now: i64) -> Result<Published> {
        // `trail` may be published: its kinds join Beacon's own Trail kinds.
        if RESERVED_SOURCES.contains(&decl.source.as_str()) && decl.source != builtin::TRAIL_SOURCE {
            return Err(Error::new(
                ErrorKind::Conflict,
                "registry source",
                format!("`{}` is Beacon's own source", decl.source),
                "publish your service under its own source name",
            ));
        }
        let mut inner = self.lock()?;
        let crate::Inner {
            conn,
            registry,
            unreadable,
            ..
        } = &mut *inner;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| Error::sql("can't start a write", &e))?;
        let published = {
            let mut w = Tx::new(&tx, &self.settings, registry, now);
            w.unreadable = unreadable;
            w.store_declarations(decl, keep)?
        };
        tx.commit().map_err(|e| Error::sql("can't commit", &e))?;
        inner.reload_registry()?;
        Ok(published)
    }

    /// `DELETE /v1/registry/{source}`: false when nobody published it. Its items stay.
    pub fn unpublish(&self, source: &str) -> Result<bool> {
        let mut inner = self.lock()?;
        let n = inner
            .conn
            .prepare_cached("DELETE FROM registry WHERE source = ?1")?
            .execute([source])?;
        if n > 0 {
            bump(&inner.conn)?;
            inner.reload_registry()?;
        }
        Ok(n > 0)
    }

    /// One source's declarations, Beacon's own included.
    pub fn declarations(&self, source: &str) -> Result<Option<Declarations>> {
        Ok(self.lock()?.registry.get(source).cloned())
    }

    /// Every source's declarations, Beacon's own first, then by source.
    pub fn all_declarations(&self) -> Result<Vec<Declarations>> {
        Ok(self.lock()?.registry.values().cloned().collect())
    }

    /// The published sources Beacon can't (all) read back, by source.
    pub fn unreadable(&self) -> Result<Vec<Unreadable>> {
        Ok(self.lock()?.unreadable.values().cloned().collect())
    }

    // ---------- events ----------

    /// `POST /v1/events`. With an Idempotency-Key, the same key and event within `idempotency`
    /// answers the first answer again (`repeat: true`) and changes nothing; another event under
    /// the same key is `409`. Keys are scoped to the kind's source.
    pub fn apply_event(&self, event: &Event, idempotency_key: Option<&str>, now: i64) -> Result<EventAnswer> {
        self.write(now, |w| {
            let source = source_of(&event.kind).to_string();
            let body = serde_json::to_string(event)?;
            if let Some(key) = idempotency_key {
                if !is_key(key) || key.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Invalid,
                        "header Idempotency-Key",
                        "an Idempotency-Key is 1 to 128 printable ASCII characters with no spaces",
                        "send a fresh random id, like a UUID",
                    ));
                }
                let stored: Option<(String, String)> = w
                    .conn
                    .prepare_cached("SELECT body, answer FROM idempotency WHERE source = ?1 AND key = ?2 AND at > ?3")?
                    .query_row(params![source, key, now - w.s.idempotency], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?;
                if let Some((first, answer)) = stored {
                    if first != body {
                        return Err(Error::new(
                            ErrorKind::Conflict,
                            "header Idempotency-Key",
                            format!("`{key}` was used for another event in the last day"),
                            "send a new Idempotency-Key for each new event; reuse one only to retry the same event",
                        ));
                    }
                    let mut answer: EventAnswer = serde_json::from_str(&answer)?;
                    answer.repeat = true;
                    return Ok(answer);
                }
            }
            let answer = w.event(event, "event", true)?;
            if let Some(key) = idempotency_key {
                w.conn
                    .prepare_cached(
                        "INSERT INTO idempotency (source, key, body, answer, at) VALUES (?1, ?2, ?3, ?4, ?5) \
                         ON CONFLICT (source, key) DO UPDATE SET body = ?3, answer = ?4, at = ?5",
                    )?
                    .execute(params![source, key, body, serde_json::to_string(&answer)?, now])?;
            }
            Ok(answer)
        })
    }

    // ---------- snapshots ----------

    /// `POST /v1/snapshots`: all or nothing.
    pub fn apply_snapshot(&self, snapshot: &Snapshot, now: i64) -> Result<SnapshotAnswer> {
        if RESERVED_SOURCES.contains(&snapshot.source.as_str()) {
            return Err(Error::new(
                ErrorKind::Undeclared,
                "snapshot source",
                format!("`{}` is Beacon's own source", snapshot.source),
                "post snapshots under your service's own source",
            ));
        }
        self.write(now, |w| w.snapshot(snapshot, false))
    }
}

fn bump(conn: &Connection) -> Result<()> {
    conn.prepare_cached("UPDATE meta SET value = value + 1 WHERE key = 'version'")?
        .execute([])?;
    Ok(())
}

fn invalid(at: String, message: impl Into<String>, fix: impl Into<String>) -> Problem {
    Problem::new(at, message, fix)
}

/// The checks that don't depend on facts and slots: key, labels, context, url. `400`.
#[allow(clippy::too_many_arguments)] // reason: the fields of an event or a snapshot entry, which differ in type
fn check_shape(
    at: &str,
    kind_name: &str,
    kind: &Kind,
    key: &str,
    check_key: bool,
    labels: &BTreeMap<String, String>,
    context: Option<&str>,
    url: Option<&str>,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    if check_key && !is_key(key) {
        problems.push(invalid(
            format!("{at} key"),
            "a key is 0 to 128 printable ASCII characters with no spaces",
            "name the thing the notification is about, like `nightly` or `pr-42`",
        ));
    }
    if labels.len() > LABELS_MAX {
        problems.push(invalid(
            format!("{at} labels"),
            format!("{} labels; at most {LABELS_MAX}", labels.len()),
            "keep the labels a silence would match on",
        ));
    }
    for (name, value) in labels {
        if !is_ident(name) {
            problems.push(invalid(
                format!("{at} labels.{name}"),
                "a label name is snake_case, at most 32 characters",
                "rename it, like `service`",
            ));
        }
        if value.chars().count() > LABEL_VALUE_MAX {
            problems.push(invalid(
                format!("{at} labels.{name}"),
                format!("a label value is at most {LABEL_VALUE_MAX} characters"),
                "shorten it; longer text goes in a slot",
            ));
        }
    }
    match (kind.context, context) {
        (true, None) => problems.push(invalid(
            format!("{at} context"),
            format!("{kind_name} declares `context = true`, so each event needs one"),
            "send the context a client shows it in, like `assistant:thread:42`",
        )),
        (true, Some(c)) if c.is_empty() || c.chars().count() > CONTEXT_MAX => problems.push(invalid(
            format!("{at} context"),
            format!("a context is 1 to {CONTEXT_MAX} characters"),
            "send a short id, like `assistant:thread:42`",
        )),
        (false, Some(_)) => problems.push(invalid(
            format!("{at} context"),
            format!("{kind_name} doesn't declare `context = true`"),
            "leave context out, or declare `context = true` and publish",
        )),
        _ => {}
    }
    if let Some(url) = url
        && (!(url.starts_with('/') || url.starts_with("https://")) || url.chars().count() > URL_MAX)
    {
        problems.push(invalid(
            format!("{at} url"),
            format!("a link starts with `/` or `https://` and is at most {URL_MAX} characters"),
            "send a path like `/system`, or an https:// link",
        ));
    }
    problems
}

/// Every problem one snapshot entry has against its kind: its shape, its category, its values.
pub(crate) fn entry_problems(at: &str, entry: &SnapshotEntry, kind: &Kind, check_key: bool) -> Vec<Problem> {
    let mut problems = check_shape(
        at,
        &entry.kind,
        kind,
        &entry.key,
        check_key,
        &entry.labels,
        entry.context.as_deref(),
        entry.url.as_deref(),
    );
    problems.extend(check_category(at, &entry.kind, kind, entry.category.as_deref()));
    problems.extend(check_values(at, &entry.kind, kind, &entry.facts, &entry.slots));
    problems
}

/// A chosen category must be one the kind declares (`422`).
fn check_category(at: &str, kind_name: &str, kind: &Kind, category: Option<&str>) -> Vec<Problem> {
    let Some(category) = category else {
        return Vec::new();
    };
    if kind.categories.iter().any(|c| c == category) {
        return Vec::new();
    }
    let fix = if kind.categories.is_empty() {
        format!("leave category out, or declare `categories = [\"{category}\"]` on {kind_name} and publish")
    } else {
        let names: Vec<String> = kind.categories.iter().map(|c| format!("`{c}`")).collect();
        match closest(category, kind.categories.iter().map(String::as_str)) {
            Some(near) => format!("did you mean `{near}`? {kind_name} declares {}", names.join(", ")),
            None => format!("send one of {}, or declare it and publish", names.join(", ")),
        }
    };
    vec![Problem::new(
        format!("{at} category"),
        format!("`{category}` is not a category {kind_name} declares"),
        fix,
    )]
}

/// `at` names the field, like `event kind`.
fn undeclared(at: &str, name: &str, registry: &BTreeMap<String, Declarations>) -> Error {
    let all: Vec<&str> = registry
        .values()
        .flat_map(|decl| decl.kinds.keys().map(String::as_str))
        .collect();
    let fix = match closest(name, all.iter().copied()) {
        Some(near) => {
            format!("did you mean `{near}`? Declare a new kind in kinds.toml and run `beacon publish`")
        }
        None => "declare it in kinds.toml and run `beacon publish`".to_string(),
    };
    Error::new(
        ErrorKind::Undeclared,
        at,
        format!("`{name}` is not a declared kind"),
        fix,
    )
}

impl Tx<'_, '_> {
    /// Stores a source's declarations (merged with what it had, with `keep`), and closes what a
    /// replacing publish removed the kind of.
    fn store_declarations(&mut self, decl: &Declarations, keep: bool) -> Result<Published> {
        let mut stored = decl.to_stored();
        let mut kept = Vec::new();
        if keep {
            let old: Option<String> = self
                .conn
                .prepare_cached("SELECT decl FROM registry WHERE source = ?1")?
                .query_row([&decl.source], |r| r.get(0))
                .optional()?;
            let old: Option<Value> = old.and_then(|text| serde_json::from_str(&text).ok());
            if let (Some(Value::Object(old_kinds)), Some(Value::Object(kinds))) =
                (old.as_ref().and_then(|v| v.get("kinds")), stored.get_mut("kinds"))
            {
                // Kept as stored, even one this version can't read: `keep` removes nothing.
                for (name, kind) in old_kinds {
                    if !kinds.contains_key(name) {
                        kinds.insert(name.clone(), kind.clone());
                        kept.push(name.clone());
                    }
                }
            }
        }
        let kinds = stored.get("kinds").and_then(Value::as_object).map_or(0, Map::len);
        self.conn
            .prepare_cached(
                "INSERT INTO registry (source, decl, published) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (source) DO UPDATE SET decl = ?2, published = ?3",
            )?
            .execute(params![decl.source, stored.to_string(), self.now])?;
        self.version()?;
        let mut closed = Vec::new();
        if !keep {
            let open = rows::query(
                self.conn,
                "WHERE source = ?1 AND state IN ('open', 'dismissed', 'snoozed') ORDER BY id",
                [&decl.source],
            )?;
            for mut row in open {
                let stays = decl.kinds.contains_key(&row.kind)
                    || (decl.source == builtin::TRAIL_SOURCE
                        && builtin::TRAIL_BUILTIN.contains(&row.kind.as_str()));
                if !row.life.is_open() || stays {
                    continue;
                }
                self.apply(&mut row, Input::KindRemoved, None)?;
                closed.push(row.id);
            }
        }
        Ok(Published { kinds, kept, closed })
    }

    /// The problem of a stored source that can't be read, when it covers `kind`.
    fn unreadable_kind(&self, at: &str, kind: &str) -> Option<Problem> {
        let unreadable = self.unreadable.get(source_of(kind))?;
        unreadable.covers(kind).then(|| {
            Problem::new(
                at,
                format!(
                    "`{kind}` was published, but Beacon can't read it as stored: {}",
                    unreadable.why
                ),
                unreadable.problem.fix.clone(),
            )
        })
    }

    /// The error for a kind the registry doesn't have: unreadable as stored, or undeclared.
    fn refuse_kind(&self, at: &str, kind: &str) -> Error {
        match self.unreadable_kind(at, kind) {
            Some(problem) => Error::many(ErrorKind::Undeclared, vec![problem]),
            None => undeclared(at, kind, self.registry),
        }
    }

    /// Checks and applies one event.
    pub(crate) fn event(&mut self, event: &Event, at: &str, check_key: bool) -> Result<EventAnswer> {
        let Some((decl, kind)) = self.kind(&event.kind) else {
            return Err(self.refuse_kind(&format!("{at} kind"), &event.kind));
        };
        if let Some(source) = &event.source
            && source != &decl.source
        {
            return Err(Error::new(
                ErrorKind::Undeclared,
                format!("{at} source"),
                format!(
                    "`{}` belongs to source `{}`, not `{source}`",
                    event.kind, decl.source
                ),
                format!("leave source out, or send `{}`", decl.source),
            ));
        }
        let resolving = event.state == Some(EventState::Resolved);
        let state_ok = matches!(
            (kind.class, event.state),
            (_, None)
                | (Class::Alert, Some(EventState::Firing | EventState::Resolved))
                | (Class::Action, Some(EventState::Open | EventState::Resolved))
        );
        let mut problems = Vec::new();
        if !state_ok {
            problems.push(invalid(
                format!("{at} state"),
                format!(
                    "a {} can't have state `{}`",
                    kind.class.as_str(),
                    event.state.map(rows::word).unwrap_or_default()
                ),
                match kind.class {
                    Class::Alert => "send `firing` or `resolved`",
                    Class::Action => "send `open` or `resolved`",
                    _ => "leave state out: updates and records have none",
                },
            ));
        }
        problems.extend(check_shape(
            at,
            &event.kind,
            kind,
            &event.key,
            check_key,
            &event.labels,
            event.context.as_deref(),
            event.url.as_deref(),
        ));
        if !problems.is_empty() {
            return Err(Error::many(ErrorKind::Invalid, problems));
        }
        let keep = resolving && event.facts.is_none() && event.slots.is_none();
        let facts = event.facts.clone().unwrap_or_default();
        let slots = event.slots.clone().unwrap_or_default();
        let mut problems = check_category(at, &event.kind, kind, event.category.as_deref());
        if !keep {
            problems.extend(check_values(at, &event.kind, kind, &facts, &slots));
        }
        if !problems.is_empty() {
            return Err(Error::many(ErrorKind::Undeclared, problems));
        }
        let fields = Fields {
            facts: (!keep).then_some(facts),
            slots: (!keep).then_some(slots),
            labels: event.labels.clone(),
            context: event.context.clone(),
            url: event.url.clone(),
            category: event.category.clone(),
        };
        let (outcome, row) = self.route(&event.kind, kind, &event.key, resolving, fields)?;
        Ok(EventAnswer {
            outcome,
            item: row.map(|row| row.item(self.now, self.s)),
            repeat: false,
        })
    }

    /// Sends one event's worth of change to the right item (`spec/api.md`, "What happens").
    fn route(
        &mut self,
        kind_name: &str,
        kind: &Kind,
        key: &str,
        resolving: bool,
        fields: Fields,
    ) -> Result<(EventOutcome, Option<Row>)> {
        let latest = rows::latest(self.conn, kind_name, key)?;
        match kind.class {
            Class::Update | Class::Record => {
                let (life, step) = Life::record(kind.class, self.now);
                let row = self.create(fields.into_new(kind_name, key), life, step, None)?;
                Ok((EventOutcome::Recorded, Some(row)))
            }
            Class::Alert | Class::Action if resolving => match latest {
                Some(mut row) if row.life.is_open() => {
                    fields.apply_to(&mut row);
                    self.apply(&mut row, Input::Resolve, None)?;
                    Ok((EventOutcome::Resolved, Some(row)))
                }
                _ => Ok((EventOutcome::Ignored, None)),
            },
            Class::Alert => {
                let target = fire_target(latest.as_ref().map(|row| &row.life), self.now, self.s);
                match (target, latest) {
                    (FireTarget::Latest, Some(row))
                        if row.life.is_open() && self.unchanged(&row, &fields) =>
                    {
                        Ok((EventOutcome::Updated, Some(row)))
                    }
                    (FireTarget::Latest, Some(mut row)) => {
                        fields.apply_to(&mut row);
                        let step = self.apply(&mut row, Input::Fire, None)?;
                        let outcome = if step.outcome == Outcome::Reopened {
                            EventOutcome::Reopened
                        } else {
                            EventOutcome::Updated
                        };
                        Ok((outcome, Some(row)))
                    }
                    _ => {
                        let repeat = kind.repeat.iter().map(|d| d.ms()).collect();
                        let (life, step) = Life::new_alert(self.now, repeat, kind.one_off);
                        let row = self.create(fields.into_new(kind_name, key), life, step, None)?;
                        Ok((EventOutcome::Opened, Some(row)))
                    }
                }
            }
            Class::Action => match latest {
                Some(row) if row.life.is_open() && self.unchanged(&row, &fields) => {
                    Ok((EventOutcome::Updated, Some(row)))
                }
                Some(mut row) if row.life.is_open() => {
                    fields.apply_to(&mut row);
                    self.apply(&mut row, open_input(Class::Action), None)?;
                    Ok((EventOutcome::Updated, Some(row)))
                }
                _ => {
                    let (life, step) = Life::new_action(self.now);
                    let row = self.create(fields.into_new(kind_name, key), life, step, None)?;
                    Ok((EventOutcome::Opened, Some(row)))
                }
            },
        }
    }

    /// Whether an event or snapshot entry for an open item would change nothing it shows or
    /// stores: facts, slots, labels, rendered text and buttons, link, category, context. Then it
    /// writes nothing (`spec/delivery.md`, "Alerts"): no history, no version, no copy to Trail.
    fn unchanged(&self, row: &Row, fields: &Fields) -> bool {
        let mut probe = row.clone();
        fields.clone().apply_to(&mut probe);
        self.render_firing(&mut probe);
        probe.facts == row.facts
            && probe.slots == row.slots
            && probe.labels == row.labels
            && probe.title == row.title
            && probe.body == row.body
            && probe.url == row.url
            && probe.url_override == row.url_override
            && probe.category == row.category
            && probe.context == row.context
            && probe.buttons == row.buttons
    }

    /// Checks and applies a snapshot. `builtin` is Trail's, which may cover update and record
    /// kinds and has already been mapped.
    pub(crate) fn snapshot(&mut self, snap: &Snapshot, builtin: bool) -> Result<SnapshotAnswer> {
        let Some(decl) = self.registry.get(&snap.source) else {
            if let Some(unreadable) = self.unreadable.get(&snap.source) {
                let mut problem = unreadable.problem.clone();
                problem.at = "snapshot source".into();
                return Err(Error::many(ErrorKind::Undeclared, vec![problem]));
            }
            return Err(Error::new(
                ErrorKind::Undeclared,
                "snapshot source",
                format!("nobody published source `{}`", snap.source),
                "run `beacon publish` for the service first",
            ));
        };
        let last_sent: Option<i64> = self
            .conn
            .prepare_cached("SELECT last_sent_at FROM sources WHERE source = ?1")?
            .query_row([&snap.source], |r| r.get(0))
            .optional()?
            .flatten();
        if let Some(sent_at) = snap.sent_at {
            let late = last_sent.is_some_and(|last| sent_at < last);
            if late || sent_at < self.now - SNAPSHOT_MAX_AGE || sent_at > self.now + SNAPSHOT_MAX_AHEAD {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "snapshot sent_at",
                    if late {
                        "this snapshot is older than one already taken from this source".to_string()
                    } else {
                        "sent_at is more than 5 minutes old or more than 1 minute ahead".to_string()
                    },
                    "send a fresh snapshot made now; check the sender's clock",
                ));
            }
        }
        let mut problems = Vec::new();
        if !snap.observed {
            let ok = snap
                .reason
                .as_deref()
                .is_some_and(|r| !r.trim().is_empty() && r.chars().count() <= REASON_MAX);
            if !ok {
                problems.push(invalid(
                    "snapshot reason".into(),
                    format!("with `observed: false`, a reason of 1 to {REASON_MAX} characters is required"),
                    "say what the source couldn't see, like \"The usage page didn't load.\"",
                ));
            }
        }
        if let Some(stale) = snap.stale_after
            && !(MINUTE..=7 * beacon_core::duration::DAY).contains(&stale.ms())
        {
            problems.push(invalid(
                "snapshot stale_after".into(),
                format!("`{stale}` is outside 1m to 7d"),
                "send a duration from 1m to 7d, a little longer than the time between snapshots",
            ));
        }
        if !problems.is_empty() {
            return Err(Error::many(ErrorKind::Invalid, problems));
        }
        let covered: Vec<String> = match &snap.kinds {
            Some(kinds) => kinds.clone(),
            None => decl
                .kinds
                .iter()
                .filter(|(_, kind)| kind.class.has_lifecycle())
                .map(|(name, _)| name.clone())
                .collect(),
        };
        let mut unfit = Vec::new();
        for (i, name) in covered.iter().enumerate() {
            let at = format!("snapshot kinds[{i}]");
            match decl.kinds.get(name) {
                Some(kind) if builtin || kind.class.has_lifecycle() => {}
                None if self.unreadable_kind(&at, name).is_some() => {
                    unfit.extend(self.unreadable_kind(&at, name));
                }
                _ => unfit.push(invalid(
                    at,
                    format!(
                        "`{name}` is not an alert or action kind of source `{}`",
                        snap.source
                    ),
                    "list only the source's alert and action kinds, or leave kinds out to cover them all",
                )),
            }
        }
        if !unfit.is_empty() {
            return Err(Error::many(ErrorKind::Undeclared, unfit));
        }
        if !snap.observed {
            let reason = snap.reason.clone().unwrap_or_default();
            self.unobserve(&snap.source, &reason)?;
            self.conn
                .prepare_cached(
                    "UPDATE sources SET last_sent_at = coalesce(?2, last_sent_at) WHERE source = ?1",
                )?
                .execute(params![snap.source, snap.sent_at])?;
            return Ok(SnapshotAnswer {
                ok: true,
                observed: false,
                ..SnapshotAnswer::default()
            });
        }
        // Check every entry before changing anything.
        let mut shape = Vec::new();
        let mut values = Vec::new();
        let mut seen = BTreeSet::new();
        for (i, entry) in snap.firing.iter().enumerate() {
            let at = format!("snapshot firing[{i}]");
            if !covered.contains(&entry.kind) {
                shape.push(invalid(
                    format!("{at} kind"),
                    format!("`{}` is not among the kinds this snapshot covers", entry.kind),
                    "add it to kinds, or leave kinds out",
                ));
                continue;
            }
            if !seen.insert((entry.kind.as_str(), entry.key.as_str())) {
                shape.push(invalid(
                    format!("{at} key"),
                    format!("({}, {:?}) is listed twice", entry.kind, entry.key),
                    "list each kind and key once",
                ));
            }
            let Some(kind) = decl.kinds.get(&entry.kind) else {
                continue;
            };
            shape.extend(check_shape(
                &at,
                &entry.kind,
                kind,
                &entry.key,
                !builtin,
                &entry.labels,
                entry.context.as_deref(),
                entry.url.as_deref(),
            ));
            values.extend(check_category(&at, &entry.kind, kind, entry.category.as_deref()));
            values.extend(check_values(&at, &entry.kind, kind, &entry.facts, &entry.slots));
        }
        if !shape.is_empty() {
            return Err(Error::many(ErrorKind::Invalid, shape));
        }
        if !values.is_empty() {
            return Err(Error::many(ErrorKind::Undeclared, values));
        }
        let listed = self.listed(&snap.source)?;
        let mut answer = SnapshotAnswer {
            ok: true,
            observed: true,
            ..SnapshotAnswer::default()
        };
        // 1. Each entry opens or updates its item.
        for entry in &snap.firing {
            let Some(kind) = decl.kinds.get(&entry.kind) else {
                continue;
            };
            let was_listed = listed.contains(&(entry.kind.clone(), entry.key.clone()));
            let fields = Fields {
                facts: Some(entry.facts.clone()),
                slots: Some(entry.slots.clone()),
                labels: entry.labels.clone(),
                context: entry.context.clone(),
                url: entry.url.clone(),
                category: entry.category.clone(),
            };
            let latest = rows::latest(self.conn, &entry.kind, &entry.key)?;
            let skip = match kind.class {
                // 3. An action you closed stays closed while the snapshot still lists it.
                Class::Action => {
                    was_listed
                        && latest.as_ref().is_some_and(|row| {
                            matches!(
                                row.life.state,
                                Some(
                                    beacon_core::model::ItemState::Done
                                        | beacon_core::model::ItemState::Dismissed
                                )
                            )
                        })
                }
                // Trail's warnings and info: once per appearance.
                Class::Update | Class::Record => was_listed,
                Class::Alert => false,
            };
            if skip {
                continue;
            }
            let (outcome, row) = self.route(&entry.kind, kind, &entry.key, false, fields)?;
            let id = row.map(|row| row.id).unwrap_or_default();
            match outcome {
                EventOutcome::Opened | EventOutcome::Recorded => answer.opened.push(id),
                EventOutcome::Reopened => answer.reopened.push(id),
                EventOutcome::Updated => answer.updated.push(id),
                EventOutcome::Resolved | EventOutcome::Ignored => {}
            }
        }
        // 2. Everything open of a covered kind with no entry resolves.
        let present: BTreeSet<(String, String)> = snap
            .firing
            .iter()
            .map(|entry| (entry.kind.clone(), entry.key.clone()))
            .collect();
        for name in &covered {
            let open = rows::query(
                self.conn,
                "WHERE kind = ?1 AND state IN ('open', 'dismissed', 'snoozed') ORDER BY id",
                [name],
            )?;
            for mut row in open {
                if !row.life.is_open() || present.contains(&(row.kind.clone(), row.key.clone())) {
                    continue;
                }
                self.apply(&mut row, Input::Resolve, None)?;
                answer.resolved.push(row.id);
            }
        }
        // The listed set of each covered kind becomes this snapshot's; another check's kinds of
        // the same source keep theirs.
        for name in &covered {
            self.conn
                .prepare_cached("DELETE FROM listed WHERE source = ?1 AND kind = ?2")?
                .execute(params![snap.source, name])?;
        }
        for (kind, key) in &present {
            self.conn
                .prepare_cached("INSERT INTO listed (source, kind, key) VALUES (?1, ?2, ?3)")?
                .execute(params![snap.source, kind, key])?;
        }
        // 4. Observed again.
        let stale_after = snap.stale_after.map_or(self.s.snapshot_stale_after, |d| d.ms());
        self.observe(&snap.source, stale_after, snap.sent_at)?;
        Ok(answer)
    }

    fn listed(&self, source: &str) -> Result<BTreeSet<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT kind, key FROM listed WHERE source = ?1")?;
        let rows = stmt.query_map([source], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn source_name(&self, source: &str) -> String {
        self.registry
            .get(source)
            .map_or_else(|| source.to_string(), |decl| decl.name.clone())
    }

    /// A snapshot was taken: the source is observed, with a fresh `last_snapshot`. If it wasn't,
    /// its open `beacon.source_stopped` resolves (or, with none, `beacon.source_observed` is
    /// recorded) and its alerts muted `unobserved` are decided again.
    pub(crate) fn observe(&mut self, source: &str, stale_after: i64, sent_at: Option<i64>) -> Result<()> {
        let was_unobserved = self.unobserved(source)?;
        self.conn
            .prepare_cached(
                "INSERT INTO sources (source, observed, reason, last_snapshot, stale_after, last_sent_at) \
                 VALUES (?1, 1, NULL, ?2, ?3, ?4) ON CONFLICT (source) DO UPDATE SET observed = 1, reason = NULL, \
                 last_snapshot = ?2, stale_after = ?3, last_sent_at = coalesce(?4, last_sent_at)",
            )?
            .execute(params![source, self.now, stale_after, sent_at])?;
        if !was_unobserved {
            return Ok(());
        }
        self.version()?;
        let stopped =
            rows::latest(self.conn, builtin::SOURCE_STOPPED, source)?.filter(|row| row.life.is_open());
        match (stopped, self.kind(builtin::SOURCE_STOPPED)) {
            (Some(_), Some((_, kind))) => {
                let keep = Fields {
                    facts: None,
                    slots: None,
                    labels: BTreeMap::new(),
                    context: None,
                    url: None,
                    category: None,
                };
                self.route(builtin::SOURCE_STOPPED, kind, source, true, keep)?;
            }
            _ => {
                let mut new = NewItem::new(builtin::SOURCE_OBSERVED, source);
                new.facts
                    .insert("source_name".into(), json!(self.source_name(source)));
                self.record_builtin(new)?;
            }
        }
        let muted = rows::query(
            self.conn,
            "WHERE source = ?1 AND class = 'alert' AND state = 'open' AND push_status = 'suppressed' \
             AND push_reason = 'unobserved' ORDER BY id",
            [source],
        )?;
        for mut row in muted {
            self.decide(&mut row, Trigger::Unmuted)?;
            self.touch(&mut row)?;
        }
        Ok(())
    }

    /// The source can't see what it watches, or went stale: unobserved with `reason`. When it
    /// was observed, opens `beacon.source_stopped` if it declares an alert kind, else records
    /// `beacon.source_unobserved`.
    pub(crate) fn unobserve(&mut self, source: &str, reason: &str) -> Result<()> {
        if source == "beacon" {
            return Ok(());
        }
        let was_unobserved = self.unobserved(source)?;
        self.conn
            .prepare_cached(
                "INSERT INTO sources (source, observed, reason) VALUES (?1, 0, ?2) \
                 ON CONFLICT (source) DO UPDATE SET observed = 0, reason = ?2",
            )?
            .execute(params![source, reason])?;
        self.version()?;
        if was_unobserved {
            return Ok(());
        }
        // A source whose alerts are now on hold says so with an alert of Beacon's own, which
        // its own source being unobserved can't mute.
        let has_alerts = self
            .registry
            .get(source)
            .is_some_and(|decl| decl.kinds.values().any(|kind| kind.class == Class::Alert));
        if has_alerts && let Some((_, kind)) = self.kind(builtin::SOURCE_STOPPED) {
            let fields = Fields {
                facts: Some(Map::from_iter([(
                    "source_name".to_string(),
                    json!(self.source_name(source)),
                )])),
                slots: Some(Map::from_iter([("reason".to_string(), json!(reason))])),
                labels: BTreeMap::new(),
                context: None,
                url: None,
                category: None,
            };
            self.route(builtin::SOURCE_STOPPED, kind, source, false, fields)?;
            return Ok(());
        }
        let mut new = NewItem::new(builtin::SOURCE_UNOBSERVED, source);
        new.facts
            .insert("source_name".into(), json!(self.source_name(source)));
        new.slots.insert("reason".into(), json!(reason));
        self.record_builtin(new)?;
        Ok(())
    }

    /// A held push is suppressed at once when its item is seen (`seen`) or leaves open (`closed`).
    pub(crate) fn suppress_held(&mut self, row: &mut Row, reason: PushReason) -> Result<bool> {
        if row.push.status != PushStatus::Held {
            return Ok(false);
        }
        self.suppress(row, reason)?;
        Ok(true)
    }
}

/// The values an event or snapshot entry sets on an item.
#[derive(Clone)]
struct Fields {
    /// None keeps the item's latest (a resolving event without facts or slots).
    facts: Option<Map<String, Value>>,
    slots: Option<Map<String, Value>>,
    labels: BTreeMap<String, String>,
    context: Option<String>,
    url: Option<String>,
    category: Option<String>,
}

impl Fields {
    fn into_new<'v>(self, kind: &'v str, key: &'v str) -> NewItem<'v> {
        let mut new = NewItem::new(kind, key);
        new.facts = self.facts.unwrap_or_default();
        new.slots = self.slots.unwrap_or_default();
        new.labels = self.labels;
        new.context = self.context;
        new.category = self.category;
        new.url = self.url;
        new
    }

    fn apply_to(self, row: &mut Row) {
        let keep = self.facts.is_none() && self.slots.is_none();
        if let Some(facts) = self.facts {
            row.facts = facts;
        }
        if let Some(slots) = self.slots {
            row.slots = slots;
        }
        if keep {
            return;
        }
        row.labels = self.labels;
        row.context = self.context;
        row.category = self.category;
        row.url_override = self.url.is_some();
        if self.url.is_some() {
            row.url = self.url;
        }
    }
}
