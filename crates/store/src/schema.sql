-- Beacon's database, version 1 (PRAGMA user_version). A later version adds a migration beside
-- this file rather than editing it. Times are integer milliseconds since 1970 UTC.

-- Counters and marks: `version` (the server's one change counter), visits, the last retention run.
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value INTEGER
);
INSERT INTO meta (key, value) VALUES ('version', 0);

-- Published declarations, one row per source, in their JSON form. Beacon's own are not stored.
CREATE TABLE registry (
    source TEXT PRIMARY KEY,
    decl TEXT NOT NULL,
    published INTEGER NOT NULL
);

-- What Beacon knows about each source that posted a snapshot or went unobserved.
CREATE TABLE sources (
    source TEXT PRIMARY KEY,
    observed INTEGER NOT NULL DEFAULT 1,
    reason TEXT,
    last_snapshot INTEGER,
    stale_after INTEGER,
    last_sent_at INTEGER
);

-- The (kind, key) pairs the last observed snapshot covering each kind listed: a closed action stays
-- closed while they still list it, and a Trail warning is recorded once per appearance. Kept per
-- (source, kind): a snapshot replaces only the rows of the kinds it covers.
CREATE TABLE listed (
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    PRIMARY KEY (source, kind, key)
);

CREATE TABLE items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    version INTEGER NOT NULL,
    kind TEXT NOT NULL,
    class TEXT NOT NULL,
    source TEXT NOT NULL,
    source_name TEXT NOT NULL,
    icon TEXT NOT NULL,
    key TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    preview TEXT,
    url TEXT,
    url_override INTEGER NOT NULL DEFAULT 0,
    context TEXT,
    labels TEXT NOT NULL,        -- JSON object of strings
    facts TEXT NOT NULL,         -- JSON object
    slots TEXT NOT NULL,         -- JSON object
    kind_decl TEXT NOT NULL,     -- the Kind it was made with, JSON: items keep their text and push
    buttons TEXT NOT NULL,       -- the declared buttons, rendered, JSON
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL,
    -- the lifecycle (crates/core/src/lifecycle.rs)
    state TEXT,
    opened INTEGER,
    acknowledged INTEGER,
    dismissed INTEGER,
    snoozed_until INTEGER,
    done INTEGER,
    resolved INTEGER,
    seen INTEGER NOT NULL DEFAULT 0,
    one_off INTEGER NOT NULL DEFAULT 0,
    openings TEXT NOT NULL DEFAULT '[]',
    flapping INTEGER NOT NULL DEFAULT 0,
    repeat TEXT NOT NULL DEFAULT '[]',
    repeats_sent INTEGER NOT NULL DEFAULT 0,
    next_repeat INTEGER,
    last_change INTEGER NOT NULL,
    -- the push
    collapse_id TEXT NOT NULL,
    push_level TEXT NOT NULL,
    push_status TEXT NOT NULL,
    push_reason TEXT,
    push_at INTEGER,
    release_at INTEGER,
    covered_by INTEGER,
    push_job INTEGER             -- the pushes row its status follows
);
CREATE INDEX items_kind_key ON items (kind, key, id);
CREATE INDEX items_class ON items (class, id);
CREATE INDEX items_source ON items (source, id);
CREATE INDEX items_version ON items (version);
CREATE INDEX items_held ON items (release_at) WHERE push_status = 'held';
CREATE INDEX items_repeat ON items (next_repeat) WHERE next_repeat IS NOT NULL;
CREATE INDEX items_snoozed ON items (snoozed_until) WHERE state = 'snoozed';
CREATE INDEX items_flapping ON items (last_change) WHERE flapping = 1;
CREATE INDEX items_resolved ON items (resolved) WHERE resolved IS NOT NULL;
CREATE INDEX items_unseen ON items (id) WHERE seen = 0;

CREATE TABLE history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    item INTEGER NOT NULL,
    at INTEGER NOT NULL,
    what TEXT NOT NULL,
    client TEXT
);
CREATE INDEX history_item ON history (item, id);

CREATE TABLE acted (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    item INTEGER NOT NULL,
    action TEXT NOT NULL,
    at INTEGER NOT NULL,
    client TEXT
);
CREATE INDEX acted_item ON acted (item);

-- When a push for (kind, key) last went out, for the throttle.
CREATE TABLE last_push (
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (kind, key)
);

-- Pushes owed to Apple: queued until a device takes one or every device fails for good.
CREATE TABLE pushes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    item INTEGER NOT NULL,
    purpose TEXT NOT NULL,       -- item | recovery
    queued_at INTEGER NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    level TEXT NOT NULL,
    collapse_id TEXT NOT NULL,
    category TEXT,
    thread TEXT NOT NULL,
    url TEXT,
    data TEXT NOT NULL,          -- JSON object
    status TEXT NOT NULL,        -- queued | sent | failed | cancelled (its item closed, or a newer push replaced it)
    reason TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt INTEGER NOT NULL,
    finished INTEGER
);
CREATE INDEX pushes_due ON pushes (next_attempt) WHERE status = 'queued';
CREATE INDEX pushes_item ON pushes (item);

CREATE TABLE attempts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    push INTEGER NOT NULL,
    item INTEGER NOT NULL,
    at INTEGER NOT NULL,
    device TEXT NOT NULL,
    environment TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    status TEXT NOT NULL,        -- accepted | rejected | uncertain
    reason TEXT
);
CREATE INDEX attempts_item ON attempts (item);

CREATE TABLE silences (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    match TEXT NOT NULL,         -- JSON {source, kind, key, labels}
    reason TEXT NOT NULL,
    created INTEGER NOT NULL,
    until INTEGER NOT NULL,
    ended INTEGER,
    created_by TEXT NOT NULL,
    matched INTEGER NOT NULL DEFAULT 0,
    unmuted INTEGER NOT NULL DEFAULT 0   -- its end was handled: what it muted was decided again
);

-- The alerts each silence muted, to decide again when it ends.
CREATE TABLE muted (
    silence INTEGER NOT NULL,
    item INTEGER NOT NULL,
    PRIMARY KEY (silence, item)
);

CREATE TABLE presence (
    device TEXT PRIMARY KEY,
    client TEXT NOT NULL,
    active INTEGER NOT NULL,
    viewing TEXT,
    last_heartbeat INTEGER NOT NULL
);

CREATE TABLE devices (
    token TEXT PRIMARY KEY,
    environment TEXT NOT NULL,
    name TEXT,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL,
    last_push INTEGER,
    last_status TEXT
);

CREATE TABLE idempotency (
    source TEXT NOT NULL,
    key TEXT NOT NULL,
    body TEXT NOT NULL,
    answer TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (source, key)
);

-- Trail notices recorded, by notice id, for as long as their item is kept.
CREATE TABLE notices (
    id TEXT PRIMARY KEY,
    item INTEGER NOT NULL
);

-- Pushes sent per local day and kind, for the budget.
CREATE TABLE budget (
    day TEXT NOT NULL,
    kind TEXT NOT NULL,
    count INTEGER NOT NULL,
    last_at INTEGER NOT NULL,
    PRIMARY KEY (day, kind)
);
CREATE TABLE budget_recorded (
    day TEXT PRIMARY KEY,
    item INTEGER NOT NULL
);
