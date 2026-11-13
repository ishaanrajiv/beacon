//! What clients send: button presses, seen, presence heartbeats, silences and devices
//! (`spec/api.md`, "Clients").

use crate::Store;
use crate::engine::{Tx, meta, set_meta};
use crate::error::{Error, ErrorKind, Result};
use crate::rows;
use beacon_core::delivery::SilenceMatch;
use beacon_core::duration::{DAY, MINUTE};
use beacon_core::item::Item;
use beacon_core::lifecycle::{Outcome, Refused};
use beacon_core::model::{PushReason, Trigger};
use rusqlite::{OptionalExtension, Row as SqlRow, params};
use serde::{Deserialize, Serialize};

/// `POST /v1/items/{id}/actions` answers this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PressAnswer {
    pub item: Item,
    pub silence: Option<SilenceView>,
}

/// `POST /v1/seen`: exactly one of the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeenTarget {
    /// Every item with an id up to this one.
    Through(i64),
    /// These items, at most 500.
    Ids(Vec<i64>),
}

/// `POST /v1/presence`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    pub client: String,
    pub device: String,
    pub active: bool,
    #[serde(default)]
    pub viewing: Option<String>,
}

/// `POST /v1/silences`, with `for` already turned into `until`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSilence {
    pub matcher: SilenceMatch,
    pub until: i64,
    pub reason: String,
    /// `web`, `ios` or `api`: becomes `created_by`.
    pub client: String,
}

/// A silence as the API sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SilenceView {
    pub id: i64,
    #[serde(rename = "match")]
    pub matcher: SilenceMatch,
    pub reason: String,
    pub created: i64,
    pub until: i64,
    pub ended: Option<i64>,
    pub active: bool,
    pub created_by: String,
    pub matched: i64,
}

const SEEN_IDS_MAX: usize = 500;
const SILENCE_MAX: i64 = 30 * DAY;
const REASON_MAX: usize = 100;
const VIEWING_MAX: usize = 128;
const SNOOZE_MIN: i64 = MINUTE;
const SNOOZE_MAX: i64 = 30 * DAY;

fn bad(at: &str, message: impl Into<String>, fix: impl Into<String>) -> Error {
    Error::new(ErrorKind::Invalid, at, message, fix)
}

fn silence_from(r: &SqlRow<'_>, now: i64) -> rusqlite::Result<SilenceView> {
    let matcher: String = r.get("match")?;
    let until: i64 = r.get("until")?;
    let ended: Option<i64> = r.get("ended")?;
    Ok(SilenceView {
        id: r.get("id")?,
        matcher: serde_json::from_str(&matcher).unwrap_or_default(),
        reason: r.get("reason")?,
        created: r.get("created")?,
        until,
        ended,
        active: ended.is_none() && now < until,
        created_by: r.get("created_by")?,
        matched: r.get("matched")?,
    })
}

const SILENCE_COLUMNS: &str = "id, match, reason, created, until, ended, created_by, matched";

impl Store {
    /// `POST /v1/items/{id}/actions`. `until` is for `snooze` only (default now +
    /// `delivery.snooze`, between 1 minute and 30 days ahead). A press whose effect already holds
    /// answers the item unchanged; one the item doesn't offer now is `409`.
    pub fn press(
        &self,
        id: i64,
        action: &str,
        until: Option<i64>,
        client: &str,
        now: i64,
    ) -> Result<PressAnswer> {
        if let Some(until) = until {
            if action != "snooze" {
                return Err(bad(
                    "action until",
                    "`until` goes with `snooze` only",
                    "leave until out",
                ));
            }
            if until < now + SNOOZE_MIN || until > now + SNOOZE_MAX {
                return Err(bad(
                    "action until",
                    "a snooze ends between 1 minute and 30 days from now",
                    "send a time in that range, or leave until out for the default",
                ));
            }
        }
        self.write(now, |w| {
            let mut row = rows::must_load(w.conn, id)?;
            let step = row
                .life
                .press(&row.buttons, action, until, now, w.s)
                .map_err(|refused| {
                    let offered = match &refused {
                        Refused::Unknown => row.life.buttons(&row.buttons),
                        Refused::NotOffered(buttons) => buttons.clone(),
                    };
                    let names: Vec<String> = offered.iter().map(|b| format!("`{}`", b.id)).collect();
                    Error::new(
                        ErrorKind::Conflict,
                        "action action",
                        format!("item {id} doesn't offer `{action}` now"),
                        if names.is_empty() {
                            "it offers no actions now".to_string()
                        } else {
                            format!("it offers {}", names.join(", "))
                        },
                    )
                })?;
            let silence = w.finish(&mut row, step, Some(action), Some(client))?;
            let silence = match silence {
                Some(silence) => w.silence_view(silence)?,
                None => None,
            };
            if step.outcome == Outcome::Silenced || step.outcome == Outcome::Action {
                // The item itself didn't change, but `acted` or the silence did.
                w.version()?;
            }
            Ok(PressAnswer {
                item: row.item(now, w.s),
                silence,
            })
        })
    }

    /// `POST /v1/seen`: how many items were newly marked. A held push of a newly seen item is
    /// suppressed (`seen`).
    pub fn mark_seen(&self, target: &SeenTarget, client: &str, now: i64) -> Result<u32> {
        if let SeenTarget::Ids(ids) = target
            && ids.len() > SEEN_IDS_MAX
        {
            return Err(bad(
                "seen ids",
                format!("{} ids; at most {SEEN_IDS_MAX}", ids.len()),
                "send `through` with the newest id instead",
            ));
        }
        self.write(now, |w| {
            let rows = match target {
                SeenTarget::Through(through) => {
                    rows::query(w.conn, "WHERE seen = 0 AND id <= ?1 ORDER BY id", [through])?
                }
                SeenTarget::Ids(ids) => {
                    let mut out = Vec::new();
                    for id in ids {
                        if let Some(row) = rows::load(w.conn, *id)?
                            && !row.life.seen
                        {
                            out.push(row);
                        }
                    }
                    out
                }
            };
            let mut count = 0;
            for mut row in rows {
                row.life.seen = true;
                w.history(row.id, "seen", Some(client))?;
                w.suppress_held(&mut row, PushReason::Seen)?;
                w.touch(&mut row)?;
                count += 1;
            }
            Ok(count)
        })
    }

    /// `POST /v1/presence`: the device's `active_until`, or None when it said inactive. Records
    /// visits for `since_last_visit`: a visit is a run of time with a device active and no gap
    /// longer than `active_window`.
    pub fn heartbeat(&self, beat: &Heartbeat, now: i64) -> Result<Option<i64>> {
        if beat.client != "web" && beat.client != "ios" {
            return Err(bad(
                "presence client",
                format!("`{}` is not a client", beat.client),
                "send `web` or `ios`",
            ));
        }
        let device_ok = !beat.device.is_empty()
            && beat.device.len() <= 64
            && beat
                .device
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !device_ok {
            return Err(bad(
                "presence device",
                "a device id is 1 to 64 of A-Z a-z 0-9 _ -",
                "send a stable id per browser profile or app install, like `web-3f9a1c`",
            ));
        }
        if beat
            .viewing
            .as_deref()
            .is_some_and(|v| v.chars().count() > VIEWING_MAX)
        {
            return Err(bad(
                "presence viewing",
                format!("viewing is at most {VIEWING_MAX} characters"),
                "send the context's id, like `assistant:thread:42`",
            ));
        }
        self.write(now, |w| {
            let window = w.s.active_window;
            let viewing = if beat.active { beat.viewing.clone() } else { None };
            w.conn
                .prepare_cached(
                    "INSERT INTO presence (device, client, active, viewing, last_heartbeat) VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT (device) DO UPDATE SET client = ?2, active = ?3, viewing = ?4, last_heartbeat = ?5",
                )?
                .execute(params![beat.device, beat.client, beat.active, viewing, now])?;
            let visit_until = meta(w.conn, "visit_until")?;
            if beat.active {
                if visit_until.is_none_or(|until| now >= until) {
                    if let Some(until) = visit_until {
                        set_meta(w.conn, "previous_visit_end", Some(until))?;
                    }
                    set_meta(w.conn, "visit_start", Some(now))?;
                    w.version()?;
                }
                set_meta(w.conn, "visit_until", Some(visit_until.unwrap_or(0).max(now + window)))?;
                Ok(Some(now + window))
            } else {
                // The visit ends now unless another device is still active.
                let others = w
                    .presence()?
                    .into_iter()
                    .filter_map(|p| p.is_active(now, window).then_some(p.last_heartbeat + window))
                    .max();
                if let Some(until) = visit_until
                    && until > now
                {
                    set_meta(w.conn, "visit_until", Some(others.unwrap_or(now)))?;
                }
                Ok(None)
            }
        })
    }

    // ---------- silences ----------

    /// `POST /v1/silences`.
    pub fn open_silence(&self, new: &NewSilence, now: i64) -> Result<SilenceView> {
        if new.matcher.is_empty() {
            return Err(bad(
                "silence match",
                "a silence must match on something",
                "give at least one of source, kind, key, labels",
            ));
        }
        if new.matcher.key.is_some() && new.matcher.kind.is_none() {
            return Err(bad(
                "silence match.key",
                "`key` needs `kind`",
                "add the kind the key belongs to",
            ));
        }
        check_until(new.until, now)?;
        check_reason(&new.reason)?;
        if !matches!(new.client.as_str(), "web" | "ios" | "api") {
            return Err(bad(
                "silence client",
                format!("`{}` is not a client", new.client),
                "send web, ios or api",
            ));
        }
        self.write(now, |w| {
            let id = w.open_silence(&new.matcher, &new.reason, new.until, &new.client)?;
            w.silence_view(id)?
                .ok_or_else(|| Error::not_found("silence", format!("no silence {id}")))
        })
    }

    /// `PATCH /v1/silences/{id}`: a new end (a time in the past ends it now) and/or reason.
    /// `409` for one already ended.
    pub fn change_silence(
        &self,
        id: i64,
        until: Option<i64>,
        reason: Option<&str>,
        now: i64,
    ) -> Result<SilenceView> {
        if let Some(reason) = reason {
            check_reason(reason)?;
        }
        if let Some(until) = until
            && until > now + SILENCE_MAX
        {
            return Err(bad(
                "silence until",
                "a silence lasts at most 30 days",
                "send an end within 30 days",
            ));
        }
        self.write(now, |w| {
            let view = w
                .silence_view(id)?
                .ok_or_else(|| Error::not_found("silence", format!("no silence {id}")))?;
            if !view.active {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "silence",
                    format!("silence {id} has already ended"),
                    "open a new silence",
                ));
            }
            if let Some(reason) = reason {
                w.conn
                    .prepare_cached("UPDATE silences SET reason = ?2 WHERE id = ?1")?
                    .execute(params![id, reason])?;
            }
            if let Some(until) = until {
                if until <= now {
                    w.end_silence(id)?;
                } else {
                    w.conn
                        .prepare_cached("UPDATE silences SET until = ?2 WHERE id = ?1")?
                        .execute(params![id, until])?;
                }
            }
            w.version()?;
            w.silence_view(id)?
                .ok_or_else(|| Error::not_found("silence", format!("no silence {id}")))
        })
    }

    /// `DELETE /v1/silences/{id}`: ends it now; ending one already ended changes nothing.
    pub fn end_silence(&self, id: i64, now: i64) -> Result<SilenceView> {
        self.write(now, |w| {
            let view = w
                .silence_view(id)?
                .ok_or_else(|| Error::not_found("silence", format!("no silence {id}")))?;
            if view.active {
                w.end_silence(id)?;
            }
            w.silence_view(id)?
                .ok_or_else(|| Error::not_found("silence", format!("no silence {id}")))
        })
    }

    /// `GET /v1/silences/{id}`.
    pub fn silence(&self, id: i64, now: i64) -> Result<Option<SilenceView>> {
        self.read(|tx, _| {
            let sql = format!("SELECT {SILENCE_COLUMNS} FROM silences WHERE id = ?1");
            Ok(tx
                .prepare_cached(&sql)?
                .query_row([id], |r| silence_from(r, now))
                .optional()?)
        })
    }

    /// `GET /v1/silences`: the active ones, soonest-ending first; with `all`, then those that
    /// ended in the last 7 days, newest first.
    pub fn silences(&self, all: bool, now: i64) -> Result<Vec<SilenceView>> {
        self.read(|tx, _| {
            let mut out = Vec::new();
            let sql =
                format!("SELECT {SILENCE_COLUMNS} FROM silences WHERE ended IS NULL AND until > ?1 ORDER BY until, id");
            let mut stmt = tx.prepare_cached(&sql)?;
            for row in stmt.query_map([now], |r| silence_from(r, now))? {
                out.push(row?);
            }
            if all {
                let sql = format!(
                    "SELECT {SILENCE_COLUMNS} FROM silences WHERE (ended IS NOT NULL OR until <= ?1) \
                     AND min(coalesce(ended, until), until) > ?2 ORDER BY min(coalesce(ended, until), until) DESC, id DESC"
                );
                let mut stmt = tx.prepare_cached(&sql)?;
                for row in stmt.query_map([now, now - 7 * DAY], |r| silence_from(r, now))? {
                    out.push(row?);
                }
            }
            Ok(out)
        })
    }

    // ---------- devices ----------

    /// `POST /v1/devices`: adds the device, or updates its environment and name.
    pub fn register_device(
        &self,
        token: &str,
        environment: &str,
        name: Option<&str>,
        now: i64,
    ) -> Result<()> {
        let token_ok = (64..=400).contains(&token.len())
            && token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !token_ok {
            return Err(bad(
                "device token",
                "an APNs device token is 64 to 400 lowercase hex characters",
                "send the token the app got from registerForRemoteNotifications, hex-encoded in lowercase",
            ));
        }
        if environment != "sandbox" && environment != "production" {
            return Err(bad(
                "device environment",
                format!("`{environment}` is not an APNs environment"),
                "send `sandbox` for a debug build, `production` otherwise",
            ));
        }
        if name.is_some_and(|n| n.chars().count() > 64) {
            return Err(bad(
                "device name",
                "a device name is at most 64 characters",
                "shorten it",
            ));
        }
        self.write(now, |w| {
            w.conn
                .prepare_cached(
                    "INSERT INTO devices (token, environment, name, created, updated) VALUES (?1, ?2, ?3, ?4, ?4) \
                     ON CONFLICT (token) DO UPDATE SET environment = ?2, name = coalesce(?3, name), updated = ?4",
                )?
                .execute(params![token, environment, name, now])?;
            Ok(())
        })
    }

    /// `DELETE /v1/devices/{token}`: false when it wasn't registered. Also what the push sender
    /// calls when Apple says a token is dead.
    pub fn forget_device(&self, token: &str, now: i64) -> Result<bool> {
        self.write(now, |w| {
            Ok(w.conn
                .prepare_cached("DELETE FROM devices WHERE token = ?1")?
                .execute([token])?
                > 0)
        })
    }
}

fn check_until(until: i64, now: i64) -> Result<()> {
    if until < now + MINUTE || until > now + SILENCE_MAX {
        return Err(bad(
            "silence for",
            "a silence lasts from 1 minute to 30 days",
            "send `for` like \"30m\", or an `until` within the next 30 days",
        ));
    }
    Ok(())
}

fn check_reason(reason: &str) -> Result<()> {
    if reason.trim().is_empty() || reason.chars().count() > REASON_MAX {
        return Err(bad(
            "silence reason",
            format!("a reason is 1 to {REASON_MAX} characters"),
            "say why, like \"Deploying dashboard\"",
        ));
    }
    Ok(())
}

impl Tx<'_, '_> {
    pub(crate) fn silence_view(&self, id: i64) -> Result<Option<SilenceView>> {
        let sql = format!("SELECT {SILENCE_COLUMNS} FROM silences WHERE id = ?1");
        let now = self.now;
        Ok(self
            .conn
            .prepare_cached(&sql)?
            .query_row([id], |r| silence_from(r, now))
            .optional()?)
    }

    /// Ends a silence now and decides again each alert it muted that is still open.
    pub(crate) fn end_silence(&mut self, id: i64) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE silences SET ended = ?2 WHERE id = ?1 AND ended IS NULL")?
            .execute(params![id, self.now])?;
        self.unmute(id)
    }

    /// What a silence muted, decided again once it is over: each alert still `open` whose push
    /// was suppressed `silenced` (`unmuted`). Another silence may still mute it.
    pub(crate) fn unmute(&mut self, id: i64) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE silences SET unmuted = 1 WHERE id = ?1")?
            .execute([id])?;
        self.version()?;
        let muted = rows::query(
            self.conn,
            "WHERE id IN (SELECT item FROM muted WHERE silence = ?1) AND class = 'alert' AND state = 'open' \
             AND push_status = 'suppressed' AND push_reason = 'silenced' ORDER BY id",
            [id],
        )?;
        for mut row in muted {
            self.decide(&mut row, Trigger::Unmuted)?;
            self.touch(&mut row)?;
        }
        Ok(())
    }
}
