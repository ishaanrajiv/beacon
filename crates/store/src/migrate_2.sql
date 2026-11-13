-- Version 2. Push tokens leave beacon.db, so it can be opened read-only by an assistant without
-- exposing them: devices live in devices.db (attached as `dev`, devices.sql), and a push attempt
-- keeps only the token's last six characters. Items keep the notification category an event chose.
INSERT OR IGNORE INTO dev.devices (token, environment, name, created, updated, last_push, last_status)
    SELECT token, environment, name, created, updated, last_push, last_status FROM main.devices;
DROP TABLE main.devices;
UPDATE attempts SET device = substr(device, -6) WHERE length(device) > 6;
ALTER TABLE items ADD COLUMN category TEXT;
