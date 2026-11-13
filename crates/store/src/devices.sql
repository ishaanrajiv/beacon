-- devices.db, attached to beacon.db's connection as `dev`: the registered iPhones and their APNs
-- tokens, which are secrets. Nothing else is kept here, and nothing in beacon.db holds a token.
CREATE TABLE IF NOT EXISTS dev.devices (
    token TEXT PRIMARY KEY,
    environment TEXT NOT NULL,
    name TEXT,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL,
    last_push INTEGER,
    last_status TEXT
);
