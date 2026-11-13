//! Owed work (`spec/delivery.md`, "Background work"): every piece is a time stored in SQLite with
//! what caused it, so [`Store::run_due`] finds it again after a restart and runs it in time order.
//! Also the push queue the APNs sender works through, and retention.

use crate::Store;
use crate::engine::{NewItem, Tx, meta, set_meta};
use crate::error::Result;
use crate::producers::STALE_REASON;
use crate::rows::{self, from_word, word};
use beacon_core::builtin;
use beacon_core::delivery::{self, CatchUpPlan, Held, PRESENCE_KEPT, SILENCE_KEPT};
use beacon_core::duration::DAY;
use beacon_core::lifecycle::Input;
use beacon_core::model::{PushLevel, PushReason, PushStatus, Trigger};
use beacon_core::quiet::local_day;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// One piece of owed work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "work", content = "of")]
pub enum Due {
    /// A hold (`active`) reached its `release_at`: decide again (`released`).
    Release(i64),
    /// The end of quiet hours, for what they held.
    CatchUp,
    /// A snoozed action's time came.
    Wake(i64),
    /// An alert's next repeat.
    Repeat(i64),
    /// A flapping alert's stable time.
    Settle(i64),
    /// A silence ended: decide again what it muted.
    SilenceEnd(i64),
    /// A source's snapshots went stale.
    Stale(String),
    /// A queued push older than `apns.give_up`: failed `expired`.
    Expire(i64),
    /// The daily retention pass.
    Retention,
}

/// What [`Store::run_due`] did, in the order it did it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ran {
    pub ran: Vec<(i64, Due)>,
    /// Items deleted by retention.
    pub deleted: u64,
}

/// A push for the sender: send it to every device in `devices`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushJob {
    pub id: i64,
    pub item: i64,
    /// `item`, or `recovery` (passive, no category).
    pub purpose: String,
    pub title: String,
    pub body: String,
    pub level: PushLevel,
    pub collapse_id: String,
    /// The item's kind: its notification category. None for a recovery.
    pub category: Option<String>,
    /// The item's source: its `thread-id`.
    pub thread: String,
    pub url: Option<String>,
    /// `{id, kind, key, collapse_id}`.
    pub data: Value,
    pub queued_at: i64,
    /// Attempts made so far.
    pub attempts: i64,
    /// Every registered device: `(token, environment)`.
    pub devices: Vec<(String, String)>,
}

/// One device's attempt at a push (`spec/apns.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub token: String,
    pub environment: String,
    /// `accepted`, `rejected` or `uncertain`.
    pub status: String,
    pub reason: Option<String>,
}

/// What happens to a push after an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushNext {
    /// A device took it.
    Sent,
    /// Every device failed for good: `rejected`, `no_devices`, `unconfigured` (or `expired`).
    Failed(PushReason),
    /// A retryable failure: try again at this time.
    RetryAt(i64),
}

/// Every piece of work with the time it comes due, in time order.
fn schedule(conn: &Connection, s: &delivery::Settings, now: i64) -> Result<Vec<(i64, Due)>> {
    let mut out: Vec<(i64, Due)> = Vec::new();
    let mut pairs = |sql: &str, make: &dyn Fn(i64) -> Due, param: Option<i64>| -> Result<()> {
        let mut stmt = conn.prepare_cached(sql)?;
        let params: Vec<i64> = param.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (at, id) = row?;
            out.push((at, make(id)));
        }
        Ok(())
    };
    pairs(
        "SELECT release_at, id FROM items WHERE push_status = 'held' AND push_reason = 'active' AND release_at IS NOT NULL",
        &Due::Release,
        None,
    )?;
    pairs(
        "SELECT snoozed_until, id FROM items WHERE state = 'snoozed' AND snoozed_until IS NOT NULL",
        &Due::Wake,
        None,
    )?;
    pairs(
        "SELECT next_repeat, id FROM items WHERE next_repeat IS NOT NULL AND state = 'open'",
        &Due::Repeat,
        None,
    )?;
    pairs(
        "SELECT last_change + ?1, id FROM items WHERE flapping = 1",
        &Due::Settle,
        Some(s.flap_stable),
    )?;
    pairs(
        "SELECT CASE WHEN ended IS NOT NULL AND ended < until THEN ended ELSE until END, id FROM silences WHERE unmuted = 0",
        &Due::SilenceEnd,
        None,
    )?;
    pairs(
        "SELECT queued_at + ?1, id FROM pushes WHERE status = 'queued'",
        &Due::Expire,
        Some(s.give_up),
    )?;
    let catch_up: Option<i64> = conn
        .prepare_cached(
            "SELECT min(release_at) FROM items WHERE push_status = 'held' AND push_reason = 'quiet_hours'",
        )?
        .query_row([], |r| r.get(0))?;
    if let Some(at) = catch_up {
        out.push((at, Due::CatchUp));
    }
    let mut stmt = conn.prepare_cached(
        "SELECT last_snapshot + stale_after, source FROM sources WHERE observed = 1 AND last_snapshot IS NOT NULL \
         AND stale_after IS NOT NULL",
    )?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (at, source) = row?;
        out.push((at, Due::Stale(source)));
    }
    let last = meta(conn, "last_retention")?;
    out.push((last.map_or(now, |at| at + DAY), Due::Retention));
    out.sort_by_key(|(at, _)| *at);
    Ok(out)
}

impl Store {
    /// The work due at `now`, in time order. Reads only.
    pub fn due(&self, now: i64) -> Result<Vec<(i64, Due)>> {
        self.read(|tx, _| {
            Ok(schedule(tx, self.settings(), now)?
                .into_iter()
                .filter(|(at, _)| *at <= now)
                .collect())
        })
    }

    /// When the next piece of work comes due after `now`, for a job that sleeps until then.
    pub fn next_due(&self, now: i64) -> Result<Option<i64>> {
        self.read(|tx, _| {
            Ok(schedule(tx, self.settings(), now)?
                .into_iter()
                .map(|(at, _)| at)
                .find(|at| *at > now))
        })
    }

    /// Runs everything due at `now`, oldest first, each in its own transaction: holds released,
    /// the catch-up, snoozes woken, repeats, flapping alerts settled, silences ended, stale
    /// sources, pushes given up, retention. Safe to call at any time and as often as wanted; work
    /// that was due while Beacon was down runs at the first call.
    pub fn run_due(&self, now: i64) -> Result<Ran> {
        let mut ran = Ran::default();
        for (at, work) in self.due(now)? {
            let deleted = self.write(now, |w| w.run(&work))?;
            ran.deleted += deleted;
            ran.ran.push((at, work));
        }
        Ok(ran)
    }

    /// Pushes waiting to be sent at `now` (queued, next attempt due, not yet given up), oldest
    /// first, with every registered device.
    pub fn pushes_due(&self, now: i64) -> Result<Vec<PushJob>> {
        let give_up = self.settings().give_up;
        self.read(|tx, _| {
            let devices: Vec<(String, String)> = {
                let mut stmt = tx.prepare_cached("SELECT token, environment FROM devices ORDER BY updated DESC")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let mut stmt = tx.prepare_cached(
                "SELECT id, item, purpose, title, body, level, collapse_id, category, thread, url, data, queued_at, \
                 attempts FROM pushes WHERE status = 'queued' AND next_attempt <= ?1 AND queued_at + ?2 > ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![now, give_up], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, String>(10)?,
                    r.get::<_, i64>(11)?,
                    r.get::<_, i64>(12)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, item, purpose, title, body, level, collapse_id, category, thread, url, data, queued_at, attempts) =
                    row?;
                out.push(PushJob {
                    id,
                    item,
                    purpose,
                    title,
                    body,
                    level: from_word(&level)?,
                    collapse_id,
                    category,
                    thread,
                    url,
                    data: serde_json::from_str(&data)?,
                    queued_at,
                    attempts,
                    devices: devices.clone(),
                });
            }
            Ok(out)
        })
    }

    /// Records one round of attempts at a push and what happens next. The item's `push.status`
    /// follows its latest push: `sent` (history `push_sent`, or `recovery_sent`), `failed`
    /// (`push_failed`), or still `queued` for a retry.
    pub fn push_attempted(&self, push: i64, attempts: &[Attempt], next: PushNext, now: i64) -> Result<()> {
        self.write(now, |w| {
            let (item, purpose, count): (i64, String, i64) = w
                .conn
                .prepare_cached("SELECT item, purpose, attempts FROM pushes WHERE id = ?1")?
                .query_row([push], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            for attempt in attempts {
                w.conn
                    .prepare_cached(
                        "INSERT INTO attempts (push, item, at, device, environment, attempt, status, reason) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    )?
                    .execute(params![
                        push,
                        item,
                        now,
                        attempt.token,
                        attempt.environment,
                        count + 1,
                        attempt.status,
                        attempt.reason
                    ])?;
                w.conn
                    .prepare_cached("UPDATE devices SET last_push = ?2, last_status = ?3 WHERE token = ?1")?
                    .execute(params![attempt.token, now, attempt.status])?;
            }
            match next {
                PushNext::RetryAt(at) => {
                    w.conn
                        .prepare_cached(
                            "UPDATE pushes SET attempts = attempts + 1, next_attempt = ?2 WHERE id = ?1",
                        )?
                        .execute(params![push, at])?;
                    Ok(())
                }
                PushNext::Sent => {
                    w.finish_push(push, item, PushStatus::Sent, None)?;
                    let what = if purpose == "recovery" {
                        "recovery_sent"
                    } else {
                        "push_sent"
                    };
                    w.history(item, what, None)
                }
                PushNext::Failed(reason) => {
                    w.finish_push(push, item, PushStatus::Failed, Some(reason))?;
                    w.history(item, "push_failed", None)
                }
            }
        })
    }
}

impl Tx<'_, '_> {
    fn finish_push(
        &mut self,
        push: i64,
        item: i64,
        status: PushStatus,
        reason: Option<PushReason>,
    ) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE pushes SET status = ?2, reason = ?3, finished = ?4, attempts = attempts + 1 WHERE id = ?1")?
            .execute(params![push, word(status), reason.map(word), self.now])?;
        if let Some(mut row) = rows::load(self.conn, item)?
            && row.push_job == Some(push)
        {
            row.push.status = status;
            row.push.reason = reason;
            row.push.at = Some(self.now);
            self.touch(&mut row)?;
        }
        Ok(())
    }

    /// Runs one piece of work if it is still owed. Returns items deleted (retention).
    fn run(&mut self, work: &Due) -> Result<u64> {
        let now = self.now;
        match work {
            Due::Release(id) => {
                if let Some(mut row) = rows::load(self.conn, *id)?
                    && row.push.status == PushStatus::Held
                    && row.push.reason == Some(PushReason::Active)
                    && row.push.release_at.is_some_and(|at| at <= now)
                {
                    self.history(row.id, "push_released", None)?;
                    self.decide(&mut row, Trigger::Released)?;
                    self.touch(&mut row)?;
                }
            }
            Due::CatchUp => self.catch_up()?,
            Due::Wake(id) => self.step(*id, Input::Wake)?,
            Due::Repeat(id) => self.step(*id, Input::Repeat)?,
            Due::Settle(id) => self.step(*id, Input::Settle)?,
            Due::SilenceEnd(id) => self.unmute(*id)?,
            Due::Stale(source) => {
                let stale: bool = self
                    .conn
                    .prepare_cached(
                        "SELECT count(*) FROM sources WHERE source = ?1 AND observed = 1 \
                         AND last_snapshot + stale_after <= ?2",
                    )?
                    .query_row(params![source, now], |r| r.get::<_, i64>(0))?
                    > 0;
                if stale {
                    self.unobserve(source, STALE_REASON)?;
                }
            }
            Due::Expire(push) => {
                let (item, status): (i64, String) = self
                    .conn
                    .prepare_cached("SELECT item, status FROM pushes WHERE id = ?1")?
                    .query_row([push], |r| Ok((r.get(0)?, r.get(1)?)))?;
                if status == "queued" {
                    self.finish_push(*push, item, PushStatus::Failed, Some(PushReason::Expired))?;
                    self.history(item, "push_failed", None)?;
                }
            }
            Due::Retention => return self.retention(),
        }
        Ok(0)
    }

    fn step(&mut self, id: i64, input: Input) -> Result<()> {
        if let Some(mut row) = rows::load(self.conn, id)? {
            self.apply(&mut row, input, None)?;
        }
        Ok(())
    }

    /// `spec/delivery.md`, "Catch-up".
    fn catch_up(&mut self) -> Result<()> {
        let mut held_rows = rows::query(
            self.conn,
            "WHERE push_status = 'held' AND push_reason = 'quiet_hours' AND release_at <= ?1 ORDER BY id",
            [self.now],
        )?;
        let held: Vec<Held> = held_rows
            .iter()
            .map(|row| Held {
                id: row.id,
                title: row.title.clone(),
                push: row.push.level,
                seen: row.life.seen,
                open: row.life.is_pushable(),
                created: row.created,
            })
            .collect();
        let plan = delivery::catch_up(&held);
        for (id, reason) in &plan.suppressed {
            if let Some(row) = held_rows.iter_mut().find(|row| row.id == *id) {
                self.suppress(row, *reason)?;
                self.touch(row)?;
            }
        }
        match plan.plan {
            CatchUpPlan::Nothing => {}
            CatchUpPlan::One(id) => {
                if let Some(row) = held_rows.iter_mut().find(|row| row.id == id) {
                    self.history(row.id, "push_released", None)?;
                    self.decide(row, Trigger::CaughtUp)?;
                    self.touch(row)?;
                }
            }
            CatchUpPlan::Many {
                ids,
                count,
                titles,
                push,
            } => {
                let day = local_day(self.now, &self.s.tz);
                let mut new = NewItem::new(builtin::CATCH_UP, &day);
                new.facts.insert("count".into(), json!(count));
                new.slots.insert("titles".into(), json!(titles));
                new.push = Some(push);
                let summary = self.record_builtin(new)?;
                for row in held_rows.iter_mut().filter(|row| ids.contains(&row.id)) {
                    self.suppress(row, PushReason::CatchUp)?;
                    row.push.covered_by = Some(summary.id);
                    self.touch(row)?;
                }
            }
        }
        Ok(())
    }

    /// Deletes what has been kept long enough (`spec/delivery.md`, "Retention").
    fn retention(&mut self) -> Result<u64> {
        let cutoff = self.now - self.s.retention;
        let doomed = "SELECT id FROM items WHERE \
             (class = 'alert' AND state = 'resolved' AND resolved < ?1) \
             OR (class = 'action' AND state IN ('done', 'dismissed', 'resolved') \
                 AND coalesce(done, dismissed, resolved) < ?1) \
             OR (class IN ('update', 'record') AND created < ?1)";
        for table in ["history", "acted", "attempts", "pushes", "muted", "notices"] {
            let sql = format!("DELETE FROM {table} WHERE item IN ({doomed})");
            self.conn.prepare_cached(&sql)?.execute([cutoff])?;
        }
        let deleted = self
            .conn
            .prepare_cached(&format!("DELETE FROM items WHERE id IN ({doomed})"))?
            .execute([cutoff])?;
        let silences = "SELECT id FROM silences WHERE unmuted = 1 \
             AND (CASE WHEN ended IS NOT NULL AND ended < until THEN ended ELSE until END) < ?1";
        self.conn
            .prepare_cached(&format!("DELETE FROM muted WHERE silence IN ({silences})"))?
            .execute([self.now - SILENCE_KEPT])?;
        self.conn
            .prepare_cached(&format!("DELETE FROM silences WHERE id IN ({silences})"))?
            .execute([self.now - SILENCE_KEPT])?;
        self.conn
            .prepare_cached("DELETE FROM idempotency WHERE at <= ?1")?
            .execute([self.now - self.s.idempotency])?;
        self.conn
            .prepare_cached("DELETE FROM presence WHERE last_heartbeat < ?1")?
            .execute([self.now - PRESENCE_KEPT])?;
        let old_day = local_day(self.now - 7 * DAY, &self.s.tz);
        self.conn
            .prepare_cached("DELETE FROM budget WHERE day < ?1")?
            .execute([&old_day])?;
        self.conn
            .prepare_cached("DELETE FROM budget_recorded WHERE day < ?1")?
            .execute([&old_day])?;
        set_meta(self.conn, "last_retention", Some(self.now))?;
        if deleted > 0 {
            self.version()?;
        }
        Ok(deleted as u64)
    }
}
