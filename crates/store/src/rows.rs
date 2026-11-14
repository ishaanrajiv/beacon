//! An item's row: read whole, changed in memory by a core transition, written whole.

use crate::error::{Error, ErrorKind, Result};
use beacon_core::decl::Kind;
use beacon_core::delivery::{Settings, Subject};
use beacon_core::item::{AlertInfo, Item, Push};
use beacon_core::lifecycle::{Button, Life, Times};
use beacon_core::model::{Class, Icon, ItemState, PushLevel, PushReason, PushStatus};
use rusqlite::{Connection, OptionalExtension, Row as SqlRow, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub(crate) const COLUMNS: &str = "id, version, kind, class, source, source_name, icon, key, title, body, preview, url, \
     url_override, context, labels, facts, slots, kind_decl, buttons, created, updated, state, opened, acknowledged, \
     dismissed, snoozed_until, done, resolved, seen, one_off, openings, flapping, repeat, repeats_sent, next_repeat, \
     last_change, collapse_id, push_level, push_status, push_reason, push_at, release_at, covered_by, push_job, category";

/// Everything stored about one item.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub id: i64,
    pub version: i64,
    pub kind: String,
    pub source: String,
    pub source_name: String,
    pub icon: Icon,
    pub key: String,
    pub title: String,
    pub body: String,
    pub preview: Option<String>,
    pub url: Option<String>,
    /// The event set `url`: it isn't rendered from the kind's `open`.
    pub url_override: bool,
    pub context: Option<String>,
    /// The notification category the event chose among its kind's `categories`.
    pub category: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub facts: Map<String, Value>,
    pub slots: Map<String, Value>,
    pub decl: Kind,
    pub buttons: Vec<Button>,
    pub created: i64,
    pub updated: i64,
    pub life: Life,
    pub collapse_id: String,
    pub push: Push,
    pub push_job: Option<i64>,
}

impl Row {
    pub fn class(&self) -> Class {
        self.life.class
    }

    pub fn subject(&self) -> Subject<'_> {
        Subject {
            class: self.life.class,
            push: self.push.level,
            kind: &self.kind,
            key: &self.key,
            source: &self.source,
            labels: &self.labels,
            context: self.context.as_deref(),
            time_bound: self.decl.time_bound,
            throttle: self.decl.throttle.map(|d| d.ms()),
            flapping: self.life.flapping,
            seen: self.life.seen,
            seen_only_on_web: false,
            open: self.life.is_pushable(),
        }
    }

    pub fn item(&self, now: i64, settings: &Settings) -> Item {
        let lifecycle = self.life.class.has_lifecycle();
        Item {
            id: self.id,
            version: self.version,
            kind: self.kind.clone(),
            class: self.life.class,
            source: self.source.clone(),
            source_name: self.source_name.clone(),
            icon: self.icon,
            key: self.key.clone(),
            title: self.title.clone(),
            body: self.body.clone(),
            preview: self.preview.clone(),
            url: self.url.clone(),
            context: self.context.clone(),
            labels: self.labels.clone(),
            created: self.created,
            updated: self.updated,
            state: self.life.state,
            times: lifecycle.then(|| self.life.times.clone()),
            alert: (self.life.class == Class::Alert).then(|| AlertInfo {
                openings: self.life.openings_within(now, settings),
                flapping: self.life.flapping,
                one_off: self.life.one_off,
                repeats_sent: self.life.repeats_sent,
                next_repeat: self.life.next_repeat,
            }),
            actions: self.life.buttons(&self.buttons),
            seen: self.life.seen,
            collapse_id: self.collapse_id.clone(),
            push: self.push.clone(),
        }
    }
}

fn json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| Error::json("can't write a value", &e))
}

fn parse<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|e| Error::json("can't read a stored value", &e))
}

/// The wire spelling of a serde enum, like `quiet_hours`.
pub(crate) fn word<T: Serialize>(value: T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(text)) => text,
        _ => String::new(),
    }
}

pub(crate) fn from_word<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_value(Value::String(text.to_string()))
        .map_err(|e| Error::json(&format!("can't read `{text}`"), &e))
}

fn bad(column: &str, e: &Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("{column}: {}", e.problem().message).into(),
    )
}

fn get_json<T: DeserializeOwned>(r: &SqlRow<'_>, column: &str) -> rusqlite::Result<T> {
    let text: String = r.get(column)?;
    parse(&text).map_err(|e| bad(column, &e))
}

fn get_word<T: DeserializeOwned>(r: &SqlRow<'_>, column: &str) -> rusqlite::Result<T> {
    let text: String = r.get(column)?;
    from_word(&text).map_err(|e| bad(column, &e))
}

fn get_opt_word<T: DeserializeOwned>(r: &SqlRow<'_>, column: &str) -> rusqlite::Result<Option<T>> {
    let text: Option<String> = r.get(column)?;
    text.map(|t| from_word(&t).map_err(|e| bad(column, &e)))
        .transpose()
}

pub(crate) fn from_sql(r: &SqlRow<'_>) -> rusqlite::Result<Row> {
    let class: Class = get_word(r, "class")?;
    let state: Option<ItemState> = get_opt_word(r, "state")?;
    Ok(Row {
        id: r.get("id")?,
        version: r.get("version")?,
        kind: r.get("kind")?,
        source: r.get("source")?,
        source_name: r.get("source_name")?,
        icon: get_word(r, "icon")?,
        key: r.get("key")?,
        title: r.get("title")?,
        body: r.get("body")?,
        preview: r.get("preview")?,
        url: r.get("url")?,
        url_override: r.get("url_override")?,
        context: r.get("context")?,
        category: r.get("category")?,
        labels: get_json(r, "labels")?,
        facts: get_json(r, "facts")?,
        slots: get_json(r, "slots")?,
        decl: get_json(r, "kind_decl")?,
        buttons: get_json(r, "buttons")?,
        created: r.get("created")?,
        updated: r.get("updated")?,
        life: Life {
            class,
            state,
            times: Times {
                opened: r.get("opened")?,
                acknowledged: r.get("acknowledged")?,
                dismissed: r.get("dismissed")?,
                snoozed_until: r.get("snoozed_until")?,
                done: r.get("done")?,
                resolved: r.get("resolved")?,
            },
            seen: r.get("seen")?,
            one_off: r.get("one_off")?,
            openings: get_json(r, "openings")?,
            flapping: r.get("flapping")?,
            repeat: get_json(r, "repeat")?,
            repeats_sent: r.get("repeats_sent")?,
            next_repeat: r.get("next_repeat")?,
            last_change: r.get("last_change")?,
        },
        collapse_id: r.get("collapse_id")?,
        push: Push {
            level: get_word::<PushLevel>(r, "push_level")?,
            status: get_word::<PushStatus>(r, "push_status")?,
            reason: get_opt_word::<PushReason>(r, "push_reason")?,
            at: r.get("push_at")?,
            release_at: r.get("release_at")?,
            covered_by: r.get("covered_by")?,
        },
        push_job: r.get("push_job")?,
    })
}

/// One item by id.
pub(crate) fn load(conn: &Connection, id: i64) -> Result<Option<Row>> {
    let sql = format!("SELECT {COLUMNS} FROM items WHERE id = ?1");
    Ok(conn.prepare_cached(&sql)?.query_row([id], from_sql).optional()?)
}

pub(crate) fn must_load(conn: &Connection, id: i64) -> Result<Row> {
    load(conn, id)?.ok_or_else(|| Error::not_found("item", format!("no item {id}")))
}

/// Items matching a `WHERE` clause (with `?1`... params), in the order given.
pub(crate) fn query(conn: &Connection, clause: &str, params: impl rusqlite::Params) -> Result<Vec<Row>> {
    let sql = format!("SELECT {COLUMNS} FROM items {clause}");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params, from_sql)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The latest item of a `(kind, key)`.
pub(crate) fn latest(conn: &Connection, kind: &str, key: &str) -> Result<Option<Row>> {
    Ok(query(
        conn,
        "WHERE kind = ?1 AND key = ?2 ORDER BY id DESC LIMIT 1",
        params![kind, key],
    )?
    .into_iter()
    .next())
}

/// Inserts a new item; its id is assigned and `row.id` set.
pub(crate) fn insert(conn: &Connection, row: &mut Row) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO items (version, kind, class, source, source_name, icon, key, title, body, labels, facts, slots, \
         kind_decl, buttons, created, updated, last_change, collapse_id, push_level, push_status) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '', '', '{}', '{}', '{}', '{}', '[]', ?8, ?8, ?8, '', ?9, 'none')",
    )?
    .execute(params![
        row.version,
        row.kind,
        word(row.life.class),
        row.source,
        row.source_name,
        word(row.icon),
        row.key,
        row.created,
        word(row.push.level),
    ])?;
    row.id = conn.last_insert_rowid();
    save(conn, row)
}

/// Writes every column of the row.
pub(crate) fn save(conn: &Connection, row: &Row) -> Result<()> {
    let n = conn
        .prepare_cached(
            "UPDATE items SET version = ?2, title = ?3, body = ?4, preview = ?5, url = ?6, url_override = ?7, \
             context = ?8, labels = ?9, facts = ?10, slots = ?11, kind_decl = ?12, buttons = ?13, updated = ?14, \
             state = ?15, opened = ?16, acknowledged = ?17, dismissed = ?18, snoozed_until = ?19, done = ?20, \
             resolved = ?21, seen = ?22, one_off = ?23, openings = ?24, flapping = ?25, repeat = ?26, \
             repeats_sent = ?27, next_repeat = ?28, last_change = ?29, collapse_id = ?30, push_level = ?31, \
             push_status = ?32, push_reason = ?33, push_at = ?34, release_at = ?35, covered_by = ?36, \
             push_job = ?37, category = ?38 WHERE id = ?1",
        )?
        .execute(params![
            row.id,
            row.version,
            row.title,
            row.body,
            row.preview,
            row.url,
            row.url_override,
            row.context,
            json(&row.labels)?,
            json(&row.facts)?,
            json(&row.slots)?,
            json(&row.decl)?,
            json(&row.buttons)?,
            row.updated,
            row.life.state.map(word),
            row.life.times.opened,
            row.life.times.acknowledged,
            row.life.times.dismissed,
            row.life.times.snoozed_until,
            row.life.times.done,
            row.life.times.resolved,
            row.life.seen,
            row.life.one_off,
            json(&row.life.openings)?,
            row.life.flapping,
            json(&row.life.repeat)?,
            row.life.repeats_sent,
            row.life.next_repeat,
            row.life.last_change,
            row.collapse_id,
            word(row.push.level),
            word(row.push.status),
            row.push.reason.map(word),
            row.push.at,
            row.push.release_at,
            row.push.covered_by,
            row.push_job,
            row.category,
        ])?;
    if n == 0 {
        return Err(Error::new(
            ErrorKind::NotFound,
            "item",
            format!("item {} was deleted", row.id),
            "it was removed by retention; nothing to do",
        ));
    }
    Ok(())
}
