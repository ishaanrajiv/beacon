//! Inside one write transaction: making items, applying a core transition to one, running the
//! decision and recording what it said, and the side effects a step owes (a held push suppressed,
//! a recovery, `beacon.flapping`, a silence, the budget). Everything here writes through `Tx`,
//! so it commits or rolls back with what caused it.

use crate::error::{Error, Result};
use crate::rows::{self, Row, word};
use beacon_core::builtin;
use beacon_core::decl::{Declarations, Kind};
use beacon_core::delivery::{
    self, Decision, KindCount, Presence, SILENCE_1H, SILENCE_1H_REASON, Settings, Silence, SilenceMatch,
    State,
};
use beacon_core::ids;
use beacon_core::item::Push;
use beacon_core::lifecycle::{Button, Input, Life, Outcome, Step, Text};
use beacon_core::model::{Class, PushLevel, PushReason, PushStatus, Trigger};
use beacon_core::quiet::local_day;
use beacon_core::render::{self, Clock};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// One write transaction, with the clock and the numbers.
pub(crate) struct Tx<'a, 'r> {
    pub conn: &'a Connection,
    pub s: &'r Settings,
    pub registry: &'r BTreeMap<String, Declarations>,
    pub now: i64,
    /// A late Trail notice: its decision becomes `late` (`spec/api.md`, `POST /v1/trail`).
    pub late: bool,
    version: Option<i64>,
}

/// Reads a value from `meta`.
pub(crate) fn meta(conn: &Connection, key: &str) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
        .query_row([key], |r| r.get::<_, Option<i64>>(0))
        .optional()?
        .flatten())
}

pub(crate) fn set_meta(conn: &Connection, key: &str, value: Option<i64>) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = ?2",
    )?
    .execute(params![key, value])?;
    Ok(())
}

/// What to make a new item from.
pub(crate) struct NewItem<'v> {
    pub kind: &'v str,
    pub key: &'v str,
    pub facts: Map<String, Value>,
    pub slots: Map<String, Value>,
    pub labels: BTreeMap<String, String>,
    pub context: Option<String>,
    pub category: Option<String>,
    pub url: Option<String>,
    /// A push level other than the kind's (the catch-up's).
    pub push: Option<PushLevel>,
    /// A collapse id other than the class's (`beacon.flapping` carries its alert's).
    pub collapse_id: Option<String>,
}

impl<'v> NewItem<'v> {
    pub fn new(kind: &'v str, key: &'v str) -> NewItem<'v> {
        NewItem {
            kind,
            key,
            facts: Map::new(),
            slots: Map::new(),
            labels: BTreeMap::new(),
            context: None,
            category: None,
            url: None,
            push: None,
            collapse_id: None,
        }
    }
}

impl<'a, 'r> Tx<'a, 'r> {
    pub fn new(
        conn: &'a Connection,
        s: &'r Settings,
        registry: &'r BTreeMap<String, Declarations>,
        now: i64,
    ) -> Self {
        Tx {
            conn,
            s,
            registry,
            now,
            late: false,
            version: None,
        }
    }

    /// The version this transaction's changes carry: the counter goes up once per transaction.
    pub fn version(&mut self) -> Result<i64> {
        if let Some(version) = self.version {
            return Ok(version);
        }
        let version = meta(self.conn, "version")?.unwrap_or(0) + 1;
        set_meta(self.conn, "version", Some(version))?;
        self.version = Some(version);
        Ok(version)
    }

    pub fn clock(&self) -> Clock {
        Clock {
            now_ms: self.now,
            tz: self.s.tz.clone(),
        }
    }

    /// A kind and its source's declarations.
    pub fn kind(&self, name: &str) -> Option<(&'r Declarations, &'r Kind)> {
        let registry: &'r BTreeMap<String, Declarations> = self.registry;
        registry
            .values()
            .find_map(|decl| decl.kinds.get(name).map(|kind| (decl, kind)))
    }

    pub fn history(&self, item: i64, what: &str, client: Option<&str>) -> Result<()> {
        self.conn
            .prepare_cached("INSERT INTO history (item, at, what, client) VALUES (?1, ?2, ?3, ?4)")?
            .execute(params![item, self.now, what, client])?;
        Ok(())
    }

    /// Makes, decides and stores a new item of a declared kind with a fresh lifecycle.
    pub fn create(&mut self, new: NewItem<'_>, life: Life, step: Step, client: Option<&str>) -> Result<Row> {
        let (decl, kind) = self.kind(new.kind).ok_or_else(|| {
            Error::new(
                crate::ErrorKind::Undeclared,
                "kind",
                format!("`{}` is not a declared kind", new.kind),
                "declare it in kinds.toml and run `beacon publish`",
            )
        })?;
        let version = self.version()?;
        let class = life.class;
        let level = if class == Class::Record {
            PushLevel::None
        } else {
            new.push.unwrap_or(kind.push)
        };
        let mut row = Row {
            id: 0,
            version,
            kind: new.kind.to_string(),
            source: decl.source.clone(),
            source_name: decl.name.clone(),
            icon: decl.icon,
            key: new.key.to_string(),
            title: String::new(),
            body: String::new(),
            preview: kind.preview.clone(),
            url: new.url.clone(),
            url_override: new.url.is_some(),
            context: new.context,
            category: new.category,
            labels: new.labels,
            facts: new.facts,
            slots: new.slots,
            decl: kind.clone(),
            buttons: Vec::new(),
            created: self.now,
            updated: self.now,
            life,
            collapse_id: String::new(),
            push: Push {
                level,
                status: PushStatus::None,
                reason: None,
                at: None,
                release_at: None,
                covered_by: None,
            },
            push_job: None,
        };
        self.render_firing(&mut row);
        rows::insert(self.conn, &mut row)?;
        row.collapse_id = match new.collapse_id {
            Some(id) => id,
            None if class.has_lifecycle() => ids::collapse_id(&row.kind, &row.key),
            None => ids::item_collapse_id(row.id),
        };
        let what = if class.has_lifecycle() {
            "opened"
        } else {
            "recorded"
        };
        self.history(row.id, what, client)?;
        self.finish(&mut row, step, None, client)?;
        Ok(row)
    }

    /// Renders the kind's title, body, link and buttons from the row's facts and slots.
    pub fn render_firing(&self, row: &mut Row) {
        let clock = self.clock();
        let text = render::render(&row.decl, &row.facts, &row.slots, &clock);
        row.title = text.title;
        row.body = text.body;
        if !row.url_override {
            row.url = text.url;
        }
        row.buttons = row
            .decl
            .actions
            .iter()
            .map(|action| Button {
                id: action.id().to_string(),
                label: action.label().to_string(),
                url: match action {
                    beacon_core::decl::Action::Custom(custom) => custom
                        .url
                        .as_deref()
                        .map(|url| render::link(url, &row.decl, &row.facts, &clock)),
                    beacon_core::decl::Action::Builtin(_) => None,
                },
            })
            .collect();
    }

    fn render_recovery(&self, row: &mut Row) {
        let lasted = self.now - row.life.times.opened.unwrap_or(row.created);
        if let Some((title, body)) =
            render::render_recovery(&row.decl, &row.facts, &row.slots, lasted, &self.clock())
        {
            row.title = title;
            row.body = body;
        }
    }

    /// Applies one input to a stored item: the transition, then what it owes. Returns the step.
    pub fn apply(&mut self, row: &mut Row, input: Input, client: Option<&str>) -> Result<Step> {
        let step = row.life.apply(input, self.now, self.s);
        self.finish(row, step, None, client)?;
        Ok(step)
    }

    /// Everything a step owes, then the row saved with a new version. `action` names the
    /// pressed button for `acted`. Returns the silence a `silence_1h` opened.
    pub fn finish(
        &mut self,
        row: &mut Row,
        step: Step,
        action: Option<&str>,
        client: Option<&str>,
    ) -> Result<Option<i64>> {
        if step.outcome == Outcome::Ignored {
            return Ok(None);
        }
        let mut silence = None;
        if step.silence {
            let matcher = SilenceMatch {
                source: Some(row.source.clone()),
                kind: Some(row.kind.clone()),
                key: (!row.key.is_empty()).then(|| row.key.clone()),
                labels: BTreeMap::new(),
            };
            silence = Some(self.open_silence(
                &matcher,
                SILENCE_1H_REASON,
                self.now + SILENCE_1H,
                client.unwrap_or("api"),
            )?);
        }
        if step.acted
            && let Some(action) = action
        {
            self.conn
                .prepare_cached("INSERT INTO acted (item, action, at, client) VALUES (?1, ?2, ?3, ?4)")?
                .execute(params![row.id, action, self.now, client])?;
        }
        if step.outcome == Outcome::Silenced {
            return Ok(silence);
        }
        if !matches!(step.outcome, Outcome::Opened) {
            self.history(row.id, step.outcome.as_str(), client)?;
        }
        if step.close_held && row.push.status == PushStatus::Held {
            self.suppress(row, PushReason::Closed)?;
        }
        // A push still queued for an item that left open makes no more attempts. If a device
        // may already have it, the recovery below replaces its banner.
        let mut may_have = false;
        if step.close_held && row.push.status == PushStatus::Queued {
            may_have = self.cancel_queued(row)?;
            if !(may_have && step.recovery) {
                self.suppress(row, PushReason::Closed)?;
            }
        }
        match step.text {
            Text::Keep => {}
            Text::Firing => self.render_firing(row),
            Text::Recovery => self.render_recovery(row),
        }
        if step.recovery && (row.push.status == PushStatus::Sent || may_have) {
            self.recovery(row)?;
        }
        // A recovery still waiting when the alert fires again would say it is back while it is
        // open: the reopening's own decision stands instead.
        if step.outcome == Outcome::Reopened {
            self.supersede_queued(row.id)?;
        }
        if step.flapping {
            self.history(row.id, "flapping", None)?;
            self.record_flapping(row)?;
        }
        if let Some(trigger) = step.decide {
            self.decide(row, trigger)?;
        }
        self.touch(row)?;
        Ok(silence)
    }

    /// Saves the row with this transaction's version.
    pub fn touch(&mut self, row: &mut Row) -> Result<()> {
        row.version = self.version()?;
        row.updated = self.now;
        rows::save(self.conn, row)
    }

    // ---------- the decision ----------

    pub fn silences(&self) -> Result<Vec<Silence>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, match, until, ended FROM silences WHERE ended IS NULL AND until > ?1",
        )?;
        let rows = stmt.query_map([self.now], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, matcher, until, ended) = row?;
            out.push(Silence {
                id,
                matcher: serde_json::from_str(&matcher)?,
                until,
                ended,
            });
        }
        Ok(out)
    }

    pub fn presence(&self) -> Result<Vec<Presence>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT device, active, last_heartbeat, viewing, client FROM presence")?;
        let rows = stmt.query_map([], |r| {
            Ok(Presence {
                device: r.get(0)?,
                ios: r.get::<_, String>(4)? == "ios",
                active: r.get(1)?,
                last_heartbeat: r.get(2)?,
                viewing: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn unobserved(&self, source: &str) -> Result<bool> {
        if source == "beacon" {
            return Ok(false);
        }
        let observed: Option<bool> = self
            .conn
            .prepare_cached("SELECT observed FROM sources WHERE source = ?1")?
            .query_row([source], |r| r.get(0))
            .optional()?;
        Ok(observed == Some(false))
    }

    /// The item is seen, and every `seen` since its state last changed (it opened, reopened,
    /// resolved, woke or was recorded) came from a web client.
    pub fn seen_only_on_web(&self, row: &Row) -> Result<bool> {
        if !row.life.seen {
            return Ok(false);
        }
        let elsewhere: i64 = self
            .conn
            .prepare_cached(
                "SELECT count(*) FROM history WHERE item = ?1 AND what = 'seen' AND coalesce(client, '') != 'web' \
                 AND id > (SELECT coalesce(max(id), 0) FROM history WHERE item = ?1 \
                 AND what IN ('opened', 'reopened', 'resolved', 'woke', 'recorded'))",
            )?
            .query_row([row.id], |r| r.get(0))?;
        Ok(elsewhere == 0)
    }

    /// The latest push of the item that a device has or may have (sent, or an attempt accepted
    /// or without a clear answer) is its recovery.
    fn recovery_shown(&self, item: i64) -> Result<bool> {
        let purpose: Option<String> = self
            .conn
            .prepare_cached(
                "SELECT purpose FROM pushes p WHERE item = ?1 AND (status = 'sent' OR EXISTS \
                 (SELECT 1 FROM attempts a WHERE a.push = p.id AND a.status IN ('accepted', 'uncertain'))) \
                 ORDER BY id DESC LIMIT 1",
            )?
            .query_row([item], |r| r.get(0))
            .optional()?;
        Ok(purpose.as_deref() == Some("recovery"))
    }

    fn last_push(&self, kind: &str, key: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .prepare_cached("SELECT at FROM last_push WHERE kind = ?1 AND key = ?2")?
            .query_row([kind, key], |r| r.get(0))
            .optional()?)
    }

    /// Runs the decision for the row and records what it says on the row (not yet saved).
    pub fn decide(&mut self, row: &mut Row, trigger: Trigger) -> Result<Decision> {
        let silences = self.silences()?;
        let presence = self.presence()?;
        let state = State {
            settings: self.s,
            silences: &silences,
            source_unobserved: self.unobserved(&row.source)?,
            last_push: self.last_push(&row.kind, &row.key)?,
            recovery_shown: self.recovery_shown(row.id)?,
        };
        let mut subject = row.subject();
        subject.seen_only_on_web = self.seen_only_on_web(row)?;
        let mut decision = delivery::decide(&subject, trigger, &state, &presence, self.now);
        if self.late && decision != Decision::None {
            decision = Decision::Suppress(PushReason::Late);
        }
        // A repeat that doesn't send changes nothing about the item's push.
        if trigger == Trigger::Repeat && decision != Decision::Send {
            return Ok(decision);
        }
        match decision {
            Decision::Send => self.send(row)?,
            Decision::Hold { reason, until } => {
                self.set_push(row, PushStatus::Held, Some(reason), Some(until));
                self.history(row.id, "push_held", None)?;
            }
            Decision::None => self.set_push(row, PushStatus::None, Some(PushReason::Declared), None),
            Decision::Suppress(reason) => self.suppress(row, reason)?,
            Decision::Silenced(silence) => {
                self.suppress(row, PushReason::Silenced)?;
                let added = self
                    .conn
                    .prepare_cached("INSERT OR IGNORE INTO muted (silence, item) VALUES (?1, ?2)")?
                    .execute([silence, row.id])?;
                if added > 0 {
                    self.conn
                        .prepare_cached("UPDATE silences SET matched = matched + 1 WHERE id = ?1")?
                        .execute([silence])?;
                }
            }
            Decision::Skip => {}
        }
        Ok(decision)
    }

    fn set_push(
        &self,
        row: &mut Row,
        status: PushStatus,
        reason: Option<PushReason>,
        release_at: Option<i64>,
    ) {
        row.push.status = status;
        row.push.reason = reason;
        row.push.at = Some(self.now);
        row.push.release_at = release_at;
        if reason != Some(PushReason::CatchUp) {
            row.push.covered_by = None;
        }
    }

    pub fn suppress(&self, row: &mut Row, reason: PushReason) -> Result<()> {
        self.set_push(row, PushStatus::Suppressed, Some(reason), None);
        self.history(row.id, "push_suppressed", None)
    }

    /// Queues the item's push for every device.
    fn send(&mut self, row: &mut Row) -> Result<()> {
        let data = push_data(row);
        let category = beacon_core::decl::push_category(&row.kind, &row.decl, row.category.as_deref());
        let job = self.queue(
            row,
            "item",
            &row.title,
            &row.body,
            row.push.level,
            Some(category),
            &data,
        )?;
        self.set_push(row, PushStatus::Queued, None, None);
        row.push_job = Some(job);
        self.conn
            .prepare_cached(
                "INSERT INTO last_push (kind, key, at) VALUES (?1, ?2, ?3) ON CONFLICT (kind, key) DO UPDATE SET at = ?3",
            )?
            .execute(params![row.kind, row.key, self.now])?;
        if delivery::counts_toward_budget(row.class()) {
            self.count_budget(row)?;
        }
        Ok(())
    }

    /// Cancels the item's queued push (`spec/delivery.md`, "Sending"): no more attempts. True
    /// when a device may already have it: an attempt was accepted, or got no clear answer.
    fn cancel_queued(&self, row: &Row) -> Result<bool> {
        let Some(job) = row.push_job else {
            return Ok(false);
        };
        let may_have = may_have_delivered(self.conn, job)?;
        self.conn
            .prepare_cached(
                "UPDATE pushes SET status = 'cancelled', reason = 'closed', finished = ?2 \
                 WHERE id = ?1 AND status = 'queued'",
            )?
            .execute(params![job, self.now])?;
        Ok(may_have)
    }

    /// Cancels every push of the item still waiting to be sent: something newer stands.
    fn supersede_queued(&self, item: i64) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE pushes SET status = 'cancelled', reason = 'superseded', finished = ?2 \
                 WHERE item = ?1 AND status = 'queued'",
            )?
            .execute(params![item, self.now])?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // reason: one row of the push queue, field by field
    fn queue(
        &self,
        row: &Row,
        purpose: &str,
        title: &str,
        body: &str,
        level: PushLevel,
        category: Option<&str>,
        data: &Value,
    ) -> Result<i64> {
        // The newest push about an item replaces the last one's banner (the collapse id), so an
        // older one still waiting to retry would only put stale text back over it.
        self.supersede_queued(row.id)?;
        self.conn
            .prepare_cached(
                "INSERT INTO pushes (item, purpose, queued_at, title, body, level, collapse_id, category, thread, url, \
                 data, status, next_attempt) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'queued', ?3)",
            )?
            .execute(params![
                row.id,
                purpose,
                self.now,
                title,
                body,
                word(level),
                row.collapse_id,
                category,
                row.source,
                row.url,
                data.to_string(),
            ])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// A resolved alert whose push was sent, or may have reached a device: the recovery text,
    /// passive, same collapse id. Only `push = false` stops it.
    pub(crate) fn recovery(&mut self, row: &mut Row) -> Result<()> {
        if !self.s.push {
            return self.suppress(row, PushReason::PushOff);
        }
        let data = push_data(row);
        let job = self.queue(
            row,
            "recovery",
            &row.title,
            &row.body,
            PushLevel::Silent,
            None,
            &data,
        )?;
        self.set_push(row, PushStatus::Queued, None, None);
        row.push_job = Some(job);
        Ok(())
    }

    // ---------- Beacon's own items ----------

    /// Records one of Beacon's updates or records.
    pub fn record_builtin(&mut self, new: NewItem<'_>) -> Result<Row> {
        let class = self.kind(new.kind).map_or(Class::Update, |(_, kind)| kind.class);
        let (life, step) = Life::record(class, self.now);
        self.create(new, life, step, None)
    }

    fn record_flapping(&mut self, alert: &Row) -> Result<()> {
        let key = format!("{}/{}", alert.kind, alert.key);
        // The slot is the firing title, which the reopening just rendered.
        let mut new = NewItem::new(builtin::FLAPPING, &key);
        new.facts.insert(
            "openings".into(),
            json!(alert.life.openings_within(self.now, self.s)),
        );
        new.facts.insert("window".into(), json!(self.s.flap_window));
        new.facts.insert("stable".into(), json!(self.s.flap_stable));
        let title = render::render(&alert.decl, &alert.facts, &alert.slots, &self.clock()).title;
        new.slots.insert("alert".into(), json!(title));
        new.collapse_id = Some(alert.collapse_id.clone());
        self.record_builtin(new)?;
        Ok(())
    }

    /// Counts a sent action or update toward today's budget; the push past it records
    /// `beacon.budget`.
    fn count_budget(&mut self, row: &Row) -> Result<()> {
        let day = local_day(self.now, &self.s.tz);
        self.conn
            .prepare_cached(
                "INSERT INTO budget (day, kind, count, last_at) VALUES (?1, ?2, 1, ?3) \
                 ON CONFLICT (day, kind) DO UPDATE SET count = count + 1, last_at = ?3",
            )?
            .execute(params![day, row.kind, self.now])?;
        let counts: Vec<(String, u32, i64)> = {
            let mut stmt = self
                .conn
                .prepare_cached("SELECT kind, count, last_at FROM budget WHERE day = ?1")?;
            let rows = stmt.query_map([&day], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let total: u32 = counts.iter().map(|(_, count, _)| count).sum();
        if !delivery::budget_exceeded(total, self.s.budget) {
            return Ok(());
        }
        let recorded: Option<i64> = self
            .conn
            .prepare_cached("SELECT item FROM budget_recorded WHERE day = ?1")?
            .query_row([&day], |r| r.get(0))
            .optional()?;
        if recorded.is_some() {
            return Ok(());
        }
        let list: Vec<KindCount<'_>> = counts
            .iter()
            .map(|(kind, count, last_push)| KindCount {
                kind,
                count: *count,
                last_push: *last_push,
            })
            .collect();
        let Some(noisy) = delivery::noisiest(&list).map(str::to_string) else {
            return Ok(());
        };
        let latest: Option<(String, String)> = if noisy == row.kind {
            Some((row.source_name.clone(), row.title.clone()))
        } else {
            self.conn
                .prepare_cached(
                    "SELECT source_name, title FROM items WHERE kind = ?1 ORDER BY id DESC LIMIT 1",
                )?
                .query_row([&noisy], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
        };
        let (source_name, title) = latest.unwrap_or_default();
        let mut new = NewItem::new(builtin::BUDGET, &day);
        new.facts.insert("count".into(), json!(total));
        new.facts.insert("source_name".into(), json!(source_name));
        new.slots.insert("kind_title".into(), json!(title));
        let budget = self.record_builtin(new)?;
        self.conn
            .prepare_cached("INSERT INTO budget_recorded (day, item) VALUES (?1, ?2)")?
            .execute(params![day, budget.id])?;
        Ok(())
    }

    // ---------- silences ----------

    pub fn open_silence(
        &mut self,
        matcher: &SilenceMatch,
        reason: &str,
        until: i64,
        created_by: &str,
    ) -> Result<i64> {
        self.conn
            .prepare_cached(
                "INSERT INTO silences (match, reason, created, until, created_by) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![serde_json::to_string(matcher)?, reason, self.now, until, created_by])?;
        self.version()?;
        Ok(self.conn.last_insert_rowid())
    }
}

/// Whether a device may have push `job`: an attempt Apple accepted, or one with no clear answer.
pub(crate) fn may_have_delivered(conn: &Connection, job: i64) -> Result<bool> {
    let n: i64 = conn
        .prepare_cached(
            "SELECT count(*) FROM attempts WHERE push = ?1 AND status IN ('accepted', 'uncertain')",
        )?
        .query_row([job], |r| r.get(0))?;
    Ok(n > 0)
}

/// A push's `data` (`spec/apns.md`): the item's id, kind, key and collapse id, and its context
/// when it has one. Never a token.
fn push_data(row: &Row) -> Value {
    let mut data = json!({"id": row.id, "kind": row.kind, "key": row.key, "collapse_id": row.collapse_id});
    if let (Some(context), Some(map)) = (&row.context, data.as_object_mut()) {
        map.insert("context".into(), json!(context));
    }
    data
}

/// The input an alert or action event's open state means.
pub(crate) fn open_input(class: Class) -> Input {
    if class == Class::Alert {
        Input::Fire
    } else {
        Input::Open
    }
}
