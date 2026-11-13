//! Beacon's store: one SQLite database (write-ahead log, `synchronous = FULL`) holding the
//! registry, items with their lifecycle and push, history, the push queue and attempts, silences,
//! presence, devices, Idempotency-Keys, snapshots per source and the budget.
//!
//! Every write is one transaction that applies a transition from `beacon-core` and records the
//! decision it led to, so work that is owed (a held push, a repeat, a snooze, the catch-up, a
//! retry) is in SQLite with what caused it, and [`Store::run_due`] finds it again after a restart.
//! The store never reads the clock: every method that decides takes `now`.
//!
//! `README.md` beside this crate lists the public methods by the route or job that calls them.
//!
//! - `rows`: an item's row, read and written whole.
//! - `engine`: deciding, recording, and the side effects a transition owes, inside a transaction.
//! - `producers`: events, snapshots, Trail's notices, the registry.
//! - `clients`: presses, seen, presence, silences, devices.
//! - `reads`: items, the inbox.
//! - `jobs`: due work, the push queue, retention.

mod clients;
mod engine;
mod error;
mod jobs;
mod producers;
mod reads;
mod rows;

#[cfg(test)]
mod tests;

pub use clients::{Heartbeat, NewSilence, PressAnswer, SeenTarget, SilenceView};
pub use error::{Error, ErrorKind, Result};
pub use jobs::{Attempt, Due, PushJob, PushNext, Ran};
pub use producers::{Event, EventAnswer, EventOutcome, EventState, Snapshot, SnapshotAnswer, SnapshotEntry};
pub use reads::{
    Acted, AttemptView, DeviceView, HistoryEntry, Inbox, InboxQuery, ItemDetail, SourceView, VisitSummary,
};

use beacon_core::decl::Declarations;
use beacon_core::delivery::Settings;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// The schema version this crate writes (`PRAGMA user_version`).
const SCHEMA_VERSION: i64 = 2;
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// Beacon's store. Open it once and share it; it is `Send + Sync`. Calls are serialized on one
/// connection: Beacon's traffic is small, and one writer keeps every transition atomic.
pub struct Store {
    settings: Settings,
    inner: Mutex<Inner>,
}

pub(crate) struct Inner {
    pub conn: Connection,
    /// Every source's declarations: Beacon's own, then the published ones. Read from the
    /// database at open and after each publish.
    pub registry: BTreeMap<String, Declarations>,
    /// Beacon's own declarations, with `[trail] open` applied.
    pub builtin: Vec<Declarations>,
}

impl Store {
    /// Opens (creating if needed) the database at `path` (`data_dir/beacon.db`), with the devices
    /// in `devices.db` beside it.
    pub fn open(path: &Path, settings: Settings) -> Result<Store> {
        Store::open_with_devices(path, &path.with_file_name("devices.db"), settings)
    }

    /// Opens `beacon.db` at `path` and the devices database at `devices`. Push tokens live only in
    /// `devices`, so `beacon.db` can be shared read-only without exposing them.
    pub fn open_with_devices(path: &Path, devices: &Path, settings: Settings) -> Result<Store> {
        let conn = Connection::open(path).map_err(|e| Error::sql("can't open beacon.db", &e))?;
        let devices = devices.to_str().ok_or_else(|| {
            Error::new(
                ErrorKind::Storage,
                "devices.db",
                "the devices database's path is not UTF-8",
                "put devices_db at a path of plain characters",
            )
        })?;
        conn.execute("ATTACH DATABASE ?1 AS dev", [devices])
            .map_err(|e| Error::sql("can't open devices.db", &e))?;
        Store::start(conn, settings, true)
    }

    /// A store in memory, for tests and `beacon check`.
    pub fn open_in_memory(settings: Settings) -> Result<Store> {
        let conn = Connection::open_in_memory().map_err(|e| Error::sql("can't open a database", &e))?;
        conn.execute_batch("ATTACH DATABASE ':memory:' AS dev")
            .map_err(|e| Error::sql("can't open a database", &e))?;
        Store::start(conn, settings, false)
    }

    fn start(mut conn: Connection, settings: Settings, wal: bool) -> Result<Store> {
        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| Error::sql("can't set the busy timeout", &e))?;
        migrate(&mut conn, wal)?;
        let builtin = beacon_core::builtin::declarations().map_err(|problems| {
            Error::new(
                ErrorKind::Storage,
                "builtin",
                format!(
                    "Beacon's own declarations fail their checks: {}",
                    problems
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                "this is a bug in beacon-core: report it",
            )
        })?;
        let mut inner = Inner {
            conn,
            registry: BTreeMap::new(),
            builtin,
        };
        inner.reload_registry()?;
        Ok(Store {
            settings,
            inner: Mutex::new(inner),
        })
    }

    /// Sets the link every `trail.*` item opens (`[trail] open` in beacon.toml).
    pub fn set_trail_open(&self, open: &str) -> Result<()> {
        let mut inner = self.lock()?;
        for decl in &mut inner.builtin {
            if decl.source == "trail" {
                for kind in decl.kinds.values_mut() {
                    kind.open = Some(open.to_string());
                }
            }
        }
        inner.reload_registry()
    }

    /// The numbers the store decides with.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        self.inner.lock().map_err(|_| {
            Error::new(
                ErrorKind::Storage,
                "store",
                "a call failed part-way and left the store's lock poisoned",
                "restart Beacon; the database is consistent (SQLite rolled the write back)",
            )
        })
    }

    /// Runs `f` in one write transaction, committed when it returns Ok.
    pub(crate) fn write<T>(
        &self,
        now: i64,
        f: impl FnOnce(&mut engine::Tx<'_, '_>) -> Result<T>,
    ) -> Result<T> {
        let mut inner = self.lock()?;
        let Inner {
            conn,
            registry,
            builtin: _,
        } = &mut *inner;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::sql("can't start a write", &e))?;
        let value = {
            let mut w = engine::Tx::new(&tx, &self.settings, registry, now);
            f(&mut w)?
        };
        tx.commit().map_err(|e| Error::sql("can't commit", &e))?;
        Ok(value)
    }

    /// Runs `f` in one read transaction.
    pub(crate) fn read<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>, &BTreeMap<String, Declarations>) -> Result<T>,
    ) -> Result<T> {
        let mut inner = self.lock()?;
        let Inner { conn, registry, .. } = &mut *inner;
        let tx = conn
            .transaction()
            .map_err(|e| Error::sql("can't start a read", &e))?;
        f(&tx, registry)
    }

    /// The server's change counter: an inbox's `version` and ETag.
    pub fn version(&self) -> Result<i64> {
        self.read(|tx, _| engine::meta(tx, "version").map(|v| v.unwrap_or(0)))
    }
}

impl Inner {
    pub(crate) fn reload_registry(&mut self) -> Result<()> {
        let mut registry = BTreeMap::new();
        for decl in &self.builtin {
            registry.insert(decl.source.clone(), decl.clone());
        }
        for decl in producers::load_registry(&self.conn)? {
            registry.insert(decl.source.clone(), decl);
        }
        self.registry = registry;
        Ok(())
    }
}

/// Sets the connection up and brings the schema to [`SCHEMA_VERSION`].
fn migrate(conn: &mut Connection, wal: bool) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| Error::sql("can't read the schema version", &e))?;
    if version > SCHEMA_VERSION {
        return Err(Error::new(
            ErrorKind::Storage,
            "beacon.db",
            format!("the database is schema version {version}; this Beacon knows up to {SCHEMA_VERSION}"),
            "run the newer Beacon that wrote it, or restore a backup made before the upgrade",
        ));
    }
    if wal {
        for schema in ["main", "dev"] {
            let mode: String = conn
                .query_row(&format!("PRAGMA {schema}.journal_mode = WAL"), [], |r| r.get(0))
                .map_err(|e| Error::sql("can't switch to WAL", &e))?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(Error::new(
                    ErrorKind::Storage,
                    "beacon.db",
                    format!("SQLite kept journal mode `{mode}` instead of WAL"),
                    "put data_dir on a local disk: WAL needs shared memory, which network file systems lack",
                ));
            }
        }
    }
    // An accepted event survives a power cut (spec/config.md, "Storage"). Deleted rows are
    // overwritten, so a token moved out of beacon.db doesn't linger in its free pages.
    conn.execute_batch(
        "PRAGMA main.synchronous = FULL; PRAGMA dev.synchronous = FULL; PRAGMA foreign_keys = ON; \
         PRAGMA temp_store = MEMORY; PRAGMA main.secure_delete = ON; PRAGMA dev.secure_delete = ON;",
    )
    .map_err(|e| Error::sql("can't set pragmas", &e))?;
    let tx = conn
        .transaction()
        .map_err(|e| Error::sql("can't start the schema", &e))?;
    tx.execute_batch(include_str!("devices.sql"))
        .map_err(|e| Error::sql("can't create devices.db's schema", &e))?;
    if version == 0 {
        tx.execute_batch(include_str!("schema.sql"))
            .map_err(|e| Error::sql("can't create the schema", &e))?;
    }
    if version < 2 {
        tx.execute_batch(include_str!("migrate_2.sql"))
            .map_err(|e| Error::sql("can't move to schema version 2", &e))?;
    }
    if version < SCHEMA_VERSION {
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .map_err(|e| Error::sql("can't set the schema version", &e))?;
    }
    tx.commit()
        .map_err(|e| Error::sql("can't commit the schema", &e))?;
    if wal && version > 0 && version < 2 {
        // The moved tokens' old pages leave the log too.
        conn.execute_batch("PRAGMA main.wal_checkpoint(TRUNCATE)")
            .map_err(|e| Error::sql("can't checkpoint beacon.db", &e))?;
    }
    Ok(())
}
