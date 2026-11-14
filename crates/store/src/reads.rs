//! Reads: one item with its history, the inbox, the registry's sources, devices. Reading changes
//! nothing.

use crate::engine::meta;
use crate::error::Result;
use crate::rows::{self, Row};
use crate::{Store, Unreadable};
use beacon_core::Problem;
use beacon_core::decl::Declarations;
use beacon_core::delivery::Settings;
use beacon_core::duration::{DAY, Duration};
use beacon_core::item::Item;
use beacon_core::model::{Class, Icon};
use rusqlite::{OptionalExtension, Transaction, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// `GET /v1/items/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemDetail {
    pub item: Item,
    pub facts: Map<String, Value>,
    pub slots: Map<String, Value>,
    pub acted: Vec<Acted>,
    pub history: Vec<HistoryEntry>,
    pub attempts: Vec<AttemptView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acted {
    pub action: String,
    pub at: i64,
    pub client: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub at: i64,
    pub what: String,
    pub client: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptView {
    pub at: i64,
    /// The token's last six characters.
    pub device: String,
    pub environment: String,
    pub attempt: i64,
    pub status: String,
    pub reason: Option<String>,
}

/// `GET /v1/inbox`'s parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxQuery {
    /// Activity only: these sources; empty for all.
    pub sources: Vec<String>,
    /// Activity only: these classes.
    pub classes: Vec<Class>,
    /// Activity only: ids below this.
    pub before: Option<i64>,
    /// Activity page size, 1 to 200.
    pub limit: u32,
    /// `held` only: items changed after this version.
    pub since: i64,
}

impl Default for InboxQuery {
    fn default() -> InboxQuery {
        InboxQuery {
            sources: Vec::new(),
            classes: vec![Class::Update],
            before: None,
            limit: 50,
            since: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    pub version: i64,
    pub now: i64,
    pub dot: bool,
    pub needs_you: Vec<Item>,
    pub alerts: Alerts,
    pub activity: Activity,
    pub held: Vec<Item>,
    pub since_last_visit: VisitSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alerts {
    pub open: Vec<Item>,
    pub resolved: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub items: Vec<Item>,
    pub next: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisitSummary {
    /// The end of your previous visit; null when there was none (then the last 24 hours count).
    pub since: Option<i64>,
    pub count: i64,
    pub sources: Vec<SourceCount>,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCount {
    pub source: String,
    pub name: String,
    pub icon: Icon,
    pub count: i64,
}

/// One source in `GET /v1/registry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceView {
    pub source: String,
    pub name: String,
    pub icon: Icon,
    /// The service its items are labelled with (`service`).
    pub service: String,
    pub kinds: usize,
    pub builtin: bool,
    pub published: Option<i64>,
    pub observed: bool,
    pub reason: Option<String>,
    pub last_snapshot: Option<i64>,
    pub stale_after: Option<Duration>,
    /// Set when Beacon can't read back what was published for it, or some of its kinds: events
    /// for those are refused until it is published again.
    pub problem: Option<Problem>,
}

/// One device in `GET /v1/devices`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceView {
    /// The token's last six characters.
    pub device: String,
    pub environment: String,
    pub name: Option<String>,
    pub created: i64,
    pub updated: i64,
    pub last_push: Option<i64>,
    pub last_status: Option<String>,
}

/// A source's row: observed, reason, last snapshot, stale after.
type SourceState = (bool, Option<String>, Option<i64>, Option<i64>);

/// Most items in `needs_you` and `alerts.open`.
const NEEDS_YOU_MAX: i64 = 100;
const RESOLVED_MAX: i64 = 50;

pub(crate) fn last_six(token: &str) -> String {
    format!("…{}", last_six_raw(token))
}

/// A token's last six characters: all of it that beacon.db keeps (in push attempts).
pub(crate) fn last_six_raw(token: &str) -> &str {
    let start = token.len().saturating_sub(6);
    token.get(start..).unwrap_or(token)
}

fn items(rows: Vec<Row>, now: i64, s: &Settings) -> Vec<Item> {
    rows.iter().map(|row| row.item(now, s)).collect()
}

impl Store {
    /// One item, as `now` shows it (openings count within the flap window).
    pub fn item(&self, id: i64, now: i64) -> Result<Option<Item>> {
        self.read(|tx, _| Ok(rows::load(tx, id)?.map(|row| row.item(now, self.settings()))))
    }

    /// `GET /v1/items/{id}`: the item with its facts, slots, acted, history and push attempts.
    pub fn item_detail(&self, id: i64, now: i64) -> Result<Option<ItemDetail>> {
        self.read(|tx, _| {
            let Some(row) = rows::load(tx, id)? else {
                return Ok(None);
            };
            let mut acted = Vec::new();
            let mut stmt = tx.prepare_cached("SELECT action, at, client FROM acted WHERE item = ?1 ORDER BY id")?;
            for row in stmt.query_map([id], |r| {
                Ok(Acted {
                    action: r.get(0)?,
                    at: r.get(1)?,
                    client: r.get(2)?,
                })
            })? {
                acted.push(row?);
            }
            let mut history = Vec::new();
            let mut stmt = tx.prepare_cached("SELECT at, what, client FROM history WHERE item = ?1 ORDER BY id")?;
            for row in stmt.query_map([id], |r| {
                Ok(HistoryEntry {
                    at: r.get(0)?,
                    what: r.get(1)?,
                    client: r.get(2)?,
                })
            })? {
                history.push(row?);
            }
            let mut attempts = Vec::new();
            let mut stmt = tx.prepare_cached(
                "SELECT at, device, environment, attempt, status, reason FROM attempts WHERE item = ?1 ORDER BY id",
            )?;
            for row in stmt.query_map([id], |r| {
                Ok(AttemptView {
                    at: r.get(0)?,
                    device: last_six(&r.get::<_, String>(1)?),
                    environment: r.get(2)?,
                    attempt: r.get(3)?,
                    status: r.get(4)?,
                    reason: r.get(5)?,
                })
            })? {
                attempts.push(row?);
            }
            Ok(Some(ItemDetail {
                item: row.item(now, self.settings()),
                facts: row.facts,
                slots: row.slots,
                acted,
                history,
                attempts,
            }))
        })
    }

    /// `GET /v1/inbox`.
    pub fn inbox(&self, query: &InboxQuery, now: i64) -> Result<Inbox> {
        let s = self.settings();
        self.read(|tx, registry| {
            let version = meta(tx, "version")?.unwrap_or(0);
            let mut needs_you = rows::query(
                tx,
                "WHERE class = 'alert' AND state = 'open' ORDER BY opened DESC, id DESC LIMIT ?1",
                [NEEDS_YOU_MAX],
            )?;
            let room = NEEDS_YOU_MAX - i64::try_from(needs_you.len()).unwrap_or(NEEDS_YOU_MAX);
            needs_you.extend(rows::query(
                tx,
                "WHERE class = 'action' AND state = 'open' ORDER BY id DESC LIMIT ?1",
                [room],
            )?);
            let open = rows::query(
                tx,
                "WHERE class = 'alert' AND state IN ('open', 'dismissed') ORDER BY opened DESC, id DESC LIMIT ?1",
                [NEEDS_YOU_MAX],
            )?;
            let resolved = rows::query(
                tx,
                "WHERE class = 'alert' AND state = 'resolved' AND resolved > ?1 ORDER BY resolved DESC, id DESC LIMIT ?2",
                rusqlite::params![now - s.resolved_for, RESOLVED_MAX],
            )?;
            let activity = activity(tx, query)?;
            let held = rows::query(
                tx,
                "WHERE push_status = 'held' AND push_reason = 'active' AND version > ?1 ORDER BY id DESC",
                [query.since],
            )?;
            let since_last_visit = visit_summary(tx, registry, now, s)?;
            Ok(Inbox {
                version,
                now,
                dot: !needs_you.is_empty(),
                needs_you: items(needs_you, now, s),
                alerts: Alerts {
                    open: items(open, now, s),
                    resolved: items(resolved, now, s),
                },
                activity: Activity {
                    items: items(activity.0, now, s),
                    next: activity.1,
                },
                held: items(held, now, s),
                since_last_visit,
            })
        })
    }

    /// `GET /v1/registry`: every source, Beacon's own included, sorted by name.
    pub fn sources(&self) -> Result<Vec<SourceView>> {
        let unreadable: BTreeMap<String, Unreadable> = self
            .unreadable()?
            .into_iter()
            .map(|u| (u.source.clone(), u))
            .collect();
        self.read(|tx, registry| {
            let state = |source: &str| -> Result<SourceState> {
                let state: Option<SourceState> = tx
                    .prepare_cached(
                        "SELECT observed, reason, last_snapshot, stale_after FROM sources WHERE source = ?1",
                    )?
                    .query_row([source], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                    .optional()?;
                Ok(state.unwrap_or((true, None, None, None)))
            };
            let mut out = Vec::new();
            for decl in registry.values() {
                let builtin = decl.source == "beacon" || decl.source == "trail";
                let published: Option<i64> = tx
                    .prepare_cached("SELECT published FROM registry WHERE source = ?1")?
                    .query_row([&decl.source], |r| r.get(0))
                    .optional()?;
                let (observed, reason, last_snapshot, stale_after) = state(&decl.source)?;
                out.push(SourceView {
                    source: decl.source.clone(),
                    name: decl.name.clone(),
                    icon: decl.icon,
                    service: decl.service().to_string(),
                    kinds: decl.kinds.len(),
                    builtin,
                    // `trail` is published when kinds were published for it beside Beacon's own.
                    published: if decl.source == "beacon" { None } else { published },
                    observed: observed || decl.source == "beacon",
                    reason,
                    last_snapshot,
                    stale_after: last_snapshot.and(stale_after).and_then(Duration::from_ms),
                    problem: unreadable.get(&decl.source).map(|u| u.problem.clone()),
                });
            }
            // A source none of whose declarations can be read is still listed, with its problem.
            for (source, u) in &unreadable {
                if registry.contains_key(source) {
                    continue;
                }
                let (observed, reason, last_snapshot, stale_after) = state(source)?;
                out.push(SourceView {
                    source: source.clone(),
                    name: source.clone(),
                    icon: Icon::Bell,
                    service: source.clone(),
                    kinds: 0,
                    builtin: false,
                    published: Some(u.published),
                    observed,
                    reason,
                    last_snapshot,
                    stale_after: last_snapshot.and(stale_after).and_then(Duration::from_ms),
                    problem: Some(u.problem.clone()),
                });
            }
            out.sort_by(|a, b| a.source.cmp(&b.source));
            Ok(out)
        })
    }

    /// `GET /v1/devices`, most recently updated first.
    pub fn devices(&self) -> Result<Vec<DeviceView>> {
        self.read(|tx, _| {
            let mut stmt = tx.prepare_cached(
                "SELECT token, environment, name, created, updated, last_push, last_status FROM dev.devices \
                 ORDER BY updated DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(DeviceView {
                    device: last_six(&r.get::<_, String>(0)?),
                    environment: r.get(1)?,
                    name: r.get(2)?,
                    created: r.get(3)?,
                    updated: r.get(4)?,
                    last_push: r.get(5)?,
                    last_status: r.get(6)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
}

/// One page of activity, and the `before` for the next.
fn activity(tx: &Transaction<'_>, query: &InboxQuery) -> Result<(Vec<Row>, Option<i64>)> {
    let mut clause = String::from("WHERE id < ?");
    let mut params: Vec<rusqlite::types::Value> = vec![query.before.unwrap_or(i64::MAX).into()];
    let classes: Vec<String> = query.classes.iter().map(|c| c.as_str().to_string()).collect();
    clause.push_str(&format!(
        " AND class IN ({})",
        vec!["?"; classes.len().max(1)].join(", ")
    ));
    if classes.is_empty() {
        params.push(String::new().into());
    }
    params.extend(classes.into_iter().map(Into::into));
    if !query.sources.is_empty() {
        clause.push_str(&format!(
            " AND source IN ({})",
            vec!["?"; query.sources.len()].join(", ")
        ));
        params.extend(query.sources.iter().cloned().map(Into::into));
    }
    let limit = query.limit.clamp(1, 200);
    clause.push_str(" ORDER BY id DESC LIMIT ?");
    params.push(i64::from(limit + 1).into());
    let mut page = rows::query(tx, &clause, params_from_iter(params))?;
    let next = if page.len() > limit as usize {
        page.truncate(limit as usize);
        page.last().map(|row| row.id)
    } else {
        None
    };
    Ok((page, next))
}

/// Updates created since the end of the previous visit (or the last 24 hours).
fn visit_summary(
    tx: &Transaction<'_>,
    registry: &BTreeMap<String, Declarations>,
    now: i64,
    s: &Settings,
) -> Result<VisitSummary> {
    let since = meta(tx, "previous_visit_end")?;
    let from = since.unwrap_or(now - DAY);
    let mut counts: Vec<SourceCount> = Vec::new();
    let mut stmt = tx.prepare_cached(
        "SELECT source, count(*) FROM items WHERE class = 'update' AND created > ?1 GROUP BY source",
    )?;
    let rows = stmt.query_map([from], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (source, count) = row?;
        let (name, icon) = registry
            .get(&source)
            .map_or((source.clone(), Icon::Bell), |decl| {
                (decl.name.clone(), decl.icon)
            });
        counts.push(SourceCount {
            source,
            name,
            icon,
            count,
        });
    }
    counts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.source.cmp(&b.source)));
    let count = counts.iter().map(|c| c.count).sum();
    let newest = rows::query(
        tx,
        "WHERE class = 'update' AND created > ?1 ORDER BY id DESC LIMIT ?2",
        rusqlite::params![from, s.since_last_visit_items],
    )?;
    Ok(VisitSummary {
        since,
        count,
        sources: counts,
        items: items(newest, now, s),
    })
}
